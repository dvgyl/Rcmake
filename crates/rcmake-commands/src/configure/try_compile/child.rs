use std::path::PathBuf;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use rcmake_base::BString;
use rcmake_build::BuildOutcome;
use rcmake_eval::{
    CommandProfiler, CommandSample, Cx, Interp, InterpConfig, Registry, RunMode, SampleKind,
    Services, TraceConfig,
};
use rcmake_host::{Host, WriteMode, path};
use rcmake_model::{DirId, GeneratorKind, GeneratorSpec, Target};

use super::capture::CapturingHost;
use super::run::{ChildError, build_child};
use super::seed::TryCompileSeed;
use super::{ProbeDirs, ProbeResult, probe_cache};

/// Shared read-only command registry reused across all `try_compile` child interpreters.
fn child_registry() -> Arc<Registry> {
    static REGISTRY: OnceLock<Arc<Registry>> = OnceLock::new();
    Arc::clone(REGISTRY.get_or_init(|| Interp::build_registry(crate::register_all)))
}

#[derive(Clone)]
pub(super) struct ChildConfig {
    config: InterpConfig,
    generator: Option<GeneratorKind>,
    generator_spec: GeneratorSpec,
    enabled_languages: BString,
    explicit_config: Option<BString>,
    depth: u32,
    trace: Option<TraceConfig>,
    profile: Option<Arc<CommandProfiler>>,
    parse_cache: Option<Arc<rcmake_eval::SharedParseCache>>,
}

impl ChildConfig {
    pub(super) fn new(cx: &mut Cx<'_>, dirs: &ProbeDirs, seed: TryCompileSeed) -> Self {
        let config = InterpConfig {
            services: Arc::new(Services {
                cancel: cx.services().cancel.clone(),
                ..Default::default()
            }),
            host: cx.host_arc(),
            mode: RunMode::Configure,
            cmake_root: path::to_native(&cx.var_or_empty(b"CMAKE_ROOT")),
            cmake_command: path::to_native(&cx.var_or_empty(b"CMAKE_COMMAND")),
            cwd: dirs.binary.clone(),
            limits: cx.limits().clone(),
            diag: Default::default(),
            defines: seed.defines,
            script_args: Vec::new(),
        };
        let depth = cx.recursion_depth();
        let trace = cx.trace_config().cloned();
        let profile = cx.command_profiler().cloned();
        let project = cx.project();
        Self {
            config,
            generator: project.generator,
            generator_spec: project.generator_spec.clone(),
            enabled_languages: seed.enabled_languages,
            explicit_config: seed.explicit_config,
            depth,
            trace,
            profile,
            parse_cache: cx.parse_cache().cloned(),
        }
    }

    #[cfg(all(feature = "native", any(unix, windows)))]
    pub(super) fn seed_inputs(&self) -> (&[rcmake_eval::CacheDefine], &[u8]) {
        (&self.config.defines, &self.enabled_languages)
    }

    #[cfg(all(feature = "native", any(unix, windows)))]
    pub(super) fn retained_bytes(&self) -> usize {
        self.config.defines.iter().fold(4096, |size, define| {
            size.saturating_add(96)
                .saturating_add(define.name.len())
                .saturating_add(define.value.len())
        })
    }

    #[cfg(all(feature = "native", any(unix, windows)))]
    pub(super) fn limit_speculation(
        &mut self,
        duration: std::time::Duration,
        shutdown: rcmake_eval::CancelToken,
    ) {
        self.config.limits.wall_budget = Some(
            self.config
                .limits
                .wall_budget
                .map_or(duration, |budget| budget.min(duration)),
        );
        Arc::make_mut(&mut self.config.services).cancel = Some(shutdown);
    }

    pub(super) fn run(
        self,
        host: Arc<dyn Host>,
        dirs: &ProbeDirs,
        build: ChildBuild<'_>,
        initialize: impl FnOnce(&mut Interp, &Arc<CapturingHost>),
    ) -> (ProbeResult, bool) {
        let ChildBuild {
            target,
            imports,
            save_cache,
            source_signature,
            generator_facts,
        } = build;
        let mut recursion_exceeded = false;
        let mut failure = None;
        let mut max_start_depth = u32::MAX;
        let (built, stdout, diagnostics) = CapturingHost::capture(host, &dirs.binary, |host| {
            let profile = self.profile.map(|record| {
                let host: Arc<dyn Host> = host.clone();
                Arc::new(ProbeProfile {
                    host,
                    depth: self.depth.saturating_add(1),
                    record,
                    prelude_end: OnceLock::new(),
                })
            });
            let configure_start = profile.as_deref().map(ProbeProfile::now);
            let mut config = self.config;
            config.cwd = dirs.binary.clone();
            config.host = host.clone();
            let mut child = Interp::with_registry(config, child_registry());
            initialize(&mut child, &host);
            if let Some(cache) = self.parse_cache {
                child.set_parse_cache(cache);
            }
            if let Some(profile) = &profile {
                child.add_session_data(profile.clone());
            }
            child.set_project_injection(!source_signature);
            child.inherit_recursion_depth(self.depth);
            if let Some(mut trace) = self.trace {
                let redirect = trace.redirect.replace(PathBuf::new());
                child.set_trace_config(Some(trace));
                let _ = host.trace_output.set(redirect);
            }
            let mut context = child.cx();
            let model = context.project();
            model.generator = self.generator;
            model.generator_spec = self.generator_spec;
            model.global.props.set("IN_TRY_COMPILE".into(), "1".into());
            if !self.enabled_languages.is_empty() {
                model
                    .global
                    .props
                    .set("ENABLED_LANGUAGES".into(), self.enabled_languages);
            }
            for (imported, aliases) in imports {
                let id = model
                    .add_target(imported)
                    .map_err(|error| ChildError::Build(error.to_string()))?;
                for alias in aliases {
                    model
                        .add_alias(DirId::ROOT, alias, id)
                        .map_err(|error| ChildError::Build(error.to_string()))?;
                }
            }
            let configure = child.run_configure(&dirs.source, &dirs.binary, &[]);
            max_start_depth = child.max_inherited_recursion_depth();
            if save_cache {
                let data = probe_cache(&mut child.cx());
                host.write(
                    &dirs.binary.join("CMakeCache.txt"),
                    &data,
                    WriteMode::Truncate,
                )
                .map_err(|error| ChildError::Build(error.to_string()))?;
            }
            if let (Some(profile), Some(start)) = (&profile, configure_start) {
                let end = profile.now();
                let prelude_end = profile.prelude_end.get().copied().unwrap_or(end);
                profile.record(b"probe.prelude", start, prelude_end);
                profile.record(b"probe.body", prelude_end, end);
            }
            if configure.exit_code != 0 {
                recursion_exceeded = child.recursion_exceeded();
                failure = Some("Failed to configure test project build system.");
                return Ok((
                    BuildOutcome {
                        exit_code: configure.exit_code,
                        ..Default::default()
                    },
                    None,
                    BString::default(),
                ));
            }
            build_child(
                &mut child.cx(),
                &dirs.binary,
                self.explicit_config.as_ref(),
                target,
                source_signature,
                generator_facts,
                profile.as_deref(),
            )
        });
        let (outcome, artifact, header) = match built {
            Ok(values) => values,
            Err(error) => {
                let output = match error {
                    ChildError::Build(output) => output,
                    ChildError::Generate(output) => {
                        failure = Some("Failed to generate test project build system.");
                        output
                    }
                };
                (
                    BuildOutcome {
                        exit_code: 1,
                        output: output.into(),
                        ..Default::default()
                    },
                    None,
                    BString::default(),
                )
            }
        };
        let built = !header.is_empty();
        let mut output = header;
        output.extend_from_slice(&stdout);
        output.extend_from_slice(&outcome.output);
        if built {
            output.push(b'\n');
        }
        (
            ProbeResult {
                max_start_depth,
                output,
                diagnostics,
                outcome,
                artifact,
                failure,
            },
            recursion_exceeded,
        )
    }
}

pub(crate) fn finish_probe_prelude(cx: &Cx<'_>) {
    if cx.current_dir_id() == DirId::ROOT
        && let Some(profile) = cx.session_data::<Arc<ProbeProfile>>()
    {
        profile.prelude_end.get_or_init(|| profile.now());
    }
}

pub(super) struct ProbeProfile {
    host: Arc<dyn Host>,
    depth: u32,
    record: Arc<CommandProfiler>,
    prelude_end: OnceLock<Duration>,
}

impl ProbeProfile {
    pub(super) fn now(&self) -> Duration {
        self.host.monotonic()
    }

    fn record(&self, name: &[u8], start: Duration, end: Duration) {
        (self.record)(&CommandSample {
            name,
            file: b"",
            line: 0,
            depth: self.depth,
            start,
            elapsed: end.saturating_sub(start),
            kind: SampleKind::ProbePhase,
        });
    }

    pub(super) fn span(&self, name: &[u8], start: Duration) {
        self.record(name, start, self.now());
    }
}

pub(super) struct ChildBuild<'a> {
    pub(super) target: Option<&'a [u8]>,
    pub(super) imports: Vec<(Target, Vec<BString>)>,
    pub(super) save_cache: bool,
    pub(super) source_signature: bool,
    pub(super) generator_facts: Option<Arc<dyn rcmake_resolve::GeneratorFacts>>,
}
