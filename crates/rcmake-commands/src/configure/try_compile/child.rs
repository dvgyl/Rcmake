use std::path::PathBuf;
use std::sync::Arc;

use rcmake_base::BString;
use rcmake_build::BuildOutcome;
use rcmake_eval::{Cx, Interp, InterpConfig, RunMode, Services, TraceConfig};
use rcmake_host::{Host, WriteMode, path};
use rcmake_model::{DirId, GeneratorKind, GeneratorSpec, Target};

use super::capture::CapturingHost;
use super::run::{ChildError, build_child};
use super::seed::TryCompileSeed;
use super::{ProbeDirs, ProbeResult, probe_cache};

#[derive(Clone)]
pub(super) struct ChildConfig {
    config: InterpConfig,
    generator: Option<GeneratorKind>,
    generator_spec: GeneratorSpec,
    enabled_languages: BString,
    depth: u32,
    trace: Option<TraceConfig>,
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
        let project = cx.project();
        Self {
            config,
            generator: project.generator,
            generator_spec: project.generator_spec.clone(),
            enabled_languages: seed.enabled_languages,
            depth,
            trace,
        }
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
        initialize: impl FnOnce(&mut Interp),
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
        let (built, stdout, diagnostics) = CapturingHost::capture(host, &dirs.binary, |host| {
            let mut config = self.config;
            config.cwd = dirs.binary.clone();
            config.host = host.clone();
            let mut child = Interp::new(config, crate::register_all);
            initialize(&mut child);
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
            if save_cache {
                let data = probe_cache(&mut child.cx());
                host.write(
                    &dirs.binary.join("CMakeCache.txt"),
                    &data,
                    WriteMode::Truncate,
                )
                .map_err(|error| ChildError::Build(error.to_string()))?;
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
                target,
                source_signature,
                generator_facts,
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

pub(super) struct ChildBuild<'a> {
    pub(super) target: Option<&'a [u8]>,
    pub(super) imports: Vec<(Target, Vec<BString>)>,
    pub(super) save_cache: bool,
    pub(super) source_signature: bool,
    pub(super) generator_facts: Option<Arc<dyn rcmake_resolve::GeneratorFacts>>,
}
