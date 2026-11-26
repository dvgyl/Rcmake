use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use rcmake_base::BString;
use rcmake_build::BuildOutcome;
use rcmake_diag::{Effect, MessageKind};
use rcmake_eval::{Cx, LedgerLevel, ProbeCacheMode, Services};
use rcmake_host::{FileKind, Follow, Host, ReadRange, path};
use rcmake_store::{Digest, Domain, KeyHasher, StoreConfig, StoreMode, file_digest};

use crate::configure::build_tool_facts::BuildToolFacts;

use super::args::{Options, Signature, Source};
use super::cache::{Artifact, ObservedOutcome, ProbeCache, VerifyContext};
use super::journal::{CompileIntervals, Journal, JournalHost};
use super::plan::{
    Admission, PathBindings, ProbeEligibility, ProbeRecipe, RcmakeBuildId, ReuseScope, Templated,
    compute_probe_key, process_environment_fingerprint,
};
use super::seed::TryCompileSeed;
use super::template::TemplateCapture;
use super::{ProbeDirs, ProbeResult};

mod compiler;
mod remote;
mod speculate;

pub(in crate::configure) struct ProbeRuntime {
    host: Arc<dyn Host>,
    journal_host: Arc<dyn Host>,
    store: StoreConfig,
    cache: ProbeCache,
    mode: ProbeCacheMode,
    build: PathBuf,
    executable: OnceLock<Option<Digest>>,
    compiler: compiler::SessionEvidence,
    intervals: Option<Arc<CompileIntervals>>,
    purity_log: Option<PathBuf>,
    remote: Option<remote::RemoteProbe>,
}

impl ProbeRuntime {
    pub(in crate::configure) fn new(
        host: Arc<dyn Host>,
        build: &Path,
        services: &Services,
        generation: Option<Arc<rcmake_host::MemoHost<dyn Host>>>,
    ) -> Option<Self> {
        let mode = services.speed.probe_cache;
        if mode == ProbeCacheMode::Off
            && !services.speed.probe_jobs.is_some_and(|jobs| jobs.get() > 1)
        {
            return None;
        }
        let mut store = StoreConfig::from_host(host.as_ref());
        if let Some(root) = host
            .env_get(b"RCMAKE_PROBE_CACHE_DIR")
            .filter(|v| !v.is_empty())
        {
            store.root = host.initial_cwd().join(path::to_native(&root));
        }
        if !store.root.is_absolute() {
            return None;
        }
        store.mode = match mode {
            ProbeCacheMode::Off => StoreMode::Off,
            ProbeCacheMode::ReadWrite => StoreMode::ReadWrite,
            ProbeCacheMode::ReadOnly => StoreMode::ReadOnly,
            ProbeCacheMode::Verify => StoreMode::Verify,
        };
        let remote = (mode != ProbeCacheMode::Off)
            .then(|| remote::RemoteProbe::new(host.as_ref()))
            .flatten();
        let intervals = (host
            .env_get(b"RCMAKE_MEASURE_PROBE_INTERVALS")
            .is_some_and(|value| value == "1"))
        .then(|| Arc::new(CompileIntervals::default()));
        let purity_log = host
            .env_get(b"RCMAKE_PROBE_PURITY_LOG")
            .filter(|value| !value.is_empty())
            .map(|value| host.initial_cwd().join(path::to_native(&value)));
        let mut cache = ProbeCache::for_session(mode);
        let journal_host = generation
            .as_ref()
            .map_or_else(|| host.clone(), |memo| memo.clone() as Arc<dyn Host>);
        if let Some(host) = generation {
            cache.memo = Arc::new(rcmake_store::TraceMemo::with_generation(host));
        }
        Some(Self {
            host,
            journal_host,
            intervals,
            purity_log,
            store,
            cache,
            mode,
            build: build.to_owned(),
            executable: OnceLock::new(),
            compiler: compiler::SessionEvidence::default(),
            remote,
        })
    }

    fn executable(&self) -> Option<Digest> {
        *self.executable.get_or_init(|| {
            let exe = self.host.current_exe().ok()?;
            file_digest(self.host.as_ref(), &exe).ok()
        })
    }
}

pub(super) struct ProbeGeneratorFacts {
    pub(super) real: Arc<dyn Host>,
    pub(super) facts: Arc<BuildToolFacts>,
    pub(super) journal: Option<Arc<JournalHost<'static>>>,
    pub(super) memo: Arc<rcmake_store::TraceMemo>,
}

impl rcmake_resolve::GeneratorFacts for ProbeGeneratorFacts {
    fn ninja_cxx_dyndep(&self, host: &dyn Host, request: &rcmake_host::RunRequest) -> Option<bool> {
        use rcmake_resolve::query_ninja_cxx_dyndep;

        let program = path::to_native(&request.pipeline.first()?.program);
        let fallback = || query_ninja_cxx_dyndep(host, request);
        if !program.is_absolute() || !compiler::admit_environment(&request.env) {
            return fallback();
        }
        let Some(canonical) = self.real.canonicalize(&program).ok() else {
            return fallback();
        };
        // The capability contract applies to native Ninja programs.
        // A script can read its working directory or run other programs.
        if !native_ninja(self.real.as_ref(), &canonical) {
            return fallback();
        }
        let Some(digest) = self.memo.file_digest(self.real.as_ref(), &program).ok() else {
            return fallback();
        };
        if self.journal.as_ref().is_some_and(|journal| {
            journal
                .record_generator_dependency(&program, digest)
                .is_err()
        }) {
            return fallback();
        }
        let mut request = request.clone();
        super::plan::process_environment(&mut request.env);
        let result = self.facts.ninja_cxx_dyndep(
            self.real.as_ref(),
            &request,
            program,
            digest,
            process_environment_fingerprint(&request.env),
        );
        if let Some(journal) = &self.journal
            && result.is_none()
        {
            journal.reject_generator_fact();
        }
        result
    }
}

pub(in crate::configure) use compiler::admit_environment;

pub(super) fn native_ninja(host: &dyn Host, path: &Path) -> bool {
    crate::configure::build_tool_facts::is_native_ninja(host, path)
}

pub(super) struct PreparedProbe {
    runtime: Arc<ProbeRuntime>,
    recipe: ProbeRecipe,
    compiler: Arc<dyn super::journal::ProcessVerifier>,
    verified: Option<ObservedOutcome>,
    /// Platform files whose contents the seed binds.
    bound: std::collections::BTreeSet<PathBuf>,
}

impl PreparedProbe {
    pub(super) fn prepare(
        cx: &mut Cx<'_>,
        signature: &Signature<'_>,
        options: &Options<'_>,
        dirs: &ProbeDirs,
        target: Option<&[u8]>,
        seed: &TryCompileSeed,
        imports: &[(rcmake_model::Target, Vec<BString>)],
    ) -> Option<Self> {
        let runtime = Arc::clone(cx.session_data::<Arc<ProbeRuntime>>()?);

        let Signature::Sources {
            base: None,
            sources,
        } = signature
        else {
            return None;
        };
        let (name, file, external) = match sources.as_slice() {
            [Source::Content(name, _) | Source::Variable(name, _) | Source::Copy(name, _)] => (
                BString::from(*name),
                dirs.source.join(path::to_native(name)),
                false,
            ),
            [Source::File(file)] => {
                let source = path::to_native(file);
                if !source.is_absolute() {
                    return None;
                }
                let name = BString::from(source.file_name()?.as_encoded_bytes());
                (name, source, true)
            }
            _ => return None,
        };
        let store_mode = if runtime.mode == ProbeCacheMode::Off {
            cx.session_data::<crate::configure::probe_pool::ProbeQueue>()?
                .available()
                .then_some(StoreMode::ReadWrite)?
        } else {
            runtime.store.mode
        };
        let eligibility = Admission {
            store_mode,
            debug_try_compile: cx.var_is_on(b"CMAKE_DEBUG_TRY_COMPILE"),
            trace: cx.trace_config().is_some(),
            profiling: cx.observes_commands()
                || !matches!(cx.services().provenance, LedgerLevel::Off)
                || cx.services().collect_annotations,
            sources_signature: true,
            private_scratch: dirs.scratch.is_some(),
            verified_execution: options.unknown.is_empty()
                && options
                    .many(b"CMAKE_FLAGS")
                    .iter()
                    .all(|flag| compiler::admit_cmake_flag(flag))
                && options
                    .many(b"LINK_LIBRARIES")
                    .iter()
                    .all(|flag| compiler::admit_link(flag.trim_ascii()))
                && options
                    .many(b"LINK_OPTIONS")
                    .iter()
                    .all(|flag| compiler::admit_link(flag))
                && options
                    .many(b"COMPILE_DEFINITIONS")
                    .iter()
                    .all(|flag| compiler::admit_definition(flag)),
        }
        .eligibility();
        if eligibility != ProbeEligibility::Eligible {
            return None;
        }
        let host = runtime.host.as_ref();
        let contents = host.read(&file, ReadRange::All { limit: 1 << 20 }).ok()?;
        if !compiler::admit_source(&name, &contents) {
            return None;
        }
        let environment = host.env_all();
        if !compiler::admit_environment(&environment) {
            return None;
        }
        let compiler = compiler::ProbeCompiler::select(
            Arc::clone(&runtime.journal_host),
            cx,
            &runtime.compiler,
            &name,
            &runtime.cache.memo,
            external.then(|| (file, Digest::hash(&contents))),
        )?;
        let cmake_root = path::to_native(&cx.var_or_empty(b"CMAKE_ROOT"));

        let source = path::to_native(&cx.var_or_empty(b"CMAKE_SOURCE_DIR"));
        let bindings = PathBindings::new(&dirs.binary, &runtime.build, &source, &cmake_root)?
            .with_target(target?)?
            .with_canonical(host);
        let project = host
            .read(
                &dirs.source.join("CMakeLists.txt"),
                ReadRange::All { limit: 1 << 20 },
            )
            .ok()?;
        let template = TemplateCapture::new(&bindings);
        let files = BTreeMap::from([
            (BString::from("CMakeLists.txt"), template.capture(&project)?),
            (name, Templated::literal(&contents)?),
        ]);
        let (seed, bound) = seed_digest(
            cx,
            &seed.defines,
            &seed.enabled_languages,
            &seed.probe_languages,
            &template,
            host,
        )?;
        let system = host.system();
        let model = cx.project();

        let generator =
            Digest::hash(format!("{:?}:{:?}", model.generator, model.generator_spec).as_bytes());
        let recipe = ProbeRecipe {
            eligibility,
            rcmake: RcmakeBuildId(runtime.executable()?),
            cmake_root: Digest::hash(&path::from_native(&cmake_root)),
            host: Digest::hash(
                format!(
                    "{:?}:{:?}:{:?}:{:?}",
                    system.sysname, system.release, system.version, system.machine
                )
                .as_bytes(),
            ),
            generator,
            files,
            tools: compiler.identity,
            flags: Vec::new(),
            definitions: Vec::new(),
            link_libraries: Vec::new(),
            target: BString::default(),
            sysroot: cx.var_or_empty(b"CMAKE_OSX_SYSROOT"),
            seed,
            imports: template
                .capture(format!("{imports:?}").as_bytes())?
                .digest(),
            environment: process_environment_fingerprint(&environment),
            scope: ReuseScope::VerifiedRelocation,
            bindings,
            try_run: false,
            speculative_run: None,
        };
        Some(Self {
            runtime,
            recipe,

            compiler: compiler.verifier,
            verified: None,
            bound,
        })
    }

    pub(super) fn hit(
        &mut self,
        cx: &mut Cx<'_>,
        dirs: &ProbeDirs,
        options: &Options<'_>,
    ) -> Option<ProbeResult> {
        let runtime = &self.runtime;
        let host = runtime.host.as_ref();
        let result = runtime
            .cache
            .lookup(host, &runtime.store, &self.recipe)
            .or_else(|| {
                let queue = cx.session_data::<crate::configure::probe_pool::ProbeQueue>()?;
                let spec = queue.take(compute_probe_key(&self.recipe))?;
                if runtime.mode == ProbeCacheMode::Off {
                    return super::cache::rebind_speculation(host, &self.recipe, spec);
                }
                if !runtime
                    .cache
                    .publish_uncounted(
                        host,
                        &runtime.store,
                        &spec.recipe,
                        &spec.outcome,
                        &spec.journal,
                        || {
                            cx.services()
                                .cancel
                                .as_ref()
                                .is_some_and(rcmake_eval::CancelToken::is_cancelled)
                        },
                    )
                    .available()
                {
                    return None;
                }
                runtime
                    .cache
                    .lookup_uncounted(host, &runtime.store, &self.recipe)
            })
            .or_else(|| {
                runtime
                    .remote
                    .as_ref()?
                    .get(host, &runtime.store, &self.recipe)
            })?;
        if cx.recursion_depth() > result.max_start_depth {
            return None;
        }
        if runtime.mode == ProbeCacheMode::Verify {
            self.verified = Some(result);
            return None;
        }
        let artifact = if options.has(b"COPY_FILE") && result.compiled {
            Some(
                result
                    .artifact
                    .as_ref()?
                    .materialize(cx.host(), &dirs.binary)
                    .ok()?,
            )
        } else {
            None
        };
        cx.note_nested_recursion_limit(result.max_start_depth);
        Some(ProbeResult {
            max_start_depth: result.max_start_depth,
            outcome: BuildOutcome {
                success: result.compiled,
                exit_code: result.exit_code,
                ..Default::default()
            },
            artifact,
            output: result.output.into(),
            diagnostics: BString::default(),
            failure: None,
        })
    }

    pub(super) fn trace_memo(&self) -> Arc<rcmake_store::TraceMemo> {
        Arc::clone(&self.runtime.cache.memo)
    }

    pub(super) fn journal(&self, dirs: &ProbeDirs) -> Option<Arc<JournalHost<'static>>> {
        if self.runtime.mode == ProbeCacheMode::Off {
            return None;
        }
        JournalHost::from_shared(
            Arc::clone(&self.runtime.journal_host),
            &dirs.binary,
            Arc::clone(&self.compiler),
            super::journal::JournalMode::Observe,
        )
        .ok()
        .map(|journal| {
            journal
                .with_trace_memo(Arc::clone(&self.runtime.cache.memo))
                .with_bound_inputs(self.bound.clone())
                .with_impurity_sites(self.runtime.purity_log.is_some())
        })
        .map(Arc::new)
    }

    pub(super) fn record(
        &self,
        cx: &mut Cx<'_>,
        dirs: &ProbeDirs,
        name: &[u8],
        built: &ProbeResult,
        journal: Journal,
        eligible: bool,
    ) {
        let runtime = &self.runtime;
        if cx
            .services()
            .cancel
            .as_ref()
            .is_some_and(rcmake_eval::CancelToken::is_cancelled)
        {
            return;
        }
        if !eligible {
            if let Some(log) = &runtime.purity_log {
                self.log_purity(log, name, &journal, false);
            }
            return;
        }
        let host = runtime.host.as_ref();
        let artifact = built
            .artifact
            .as_deref()
            .and_then(|file| read_artifact(host, dirs, file));
        let fresh = ObservedOutcome {
            max_start_depth: built.max_start_depth,
            compiled: built.outcome.success,
            exit_code: built.outcome.exit_code,
            output: built.output.to_vec(),
            artifact,
            diagnostics: Vec::new(),
        };
        if let Some(cached) = &self.verified {
            let backtrace = cx.backtrace();
            let Some(frame) = backtrace.top() else { return };
            if let Some(mismatch) = runtime.cache.verify_mode(
                VerifyContext {
                    variable: name,
                    file: &path::to_native(&frame.file),
                    line: frame.line,
                    key: compute_probe_key(&self.recipe),
                    cached_copy: None,
                    fresh_copy: None,
                },
                cached,
                &fresh,
            ) {
                cx.issue(
                    MessageKind::Warning,
                    Effect::None,
                    None,
                    mismatch.to_string().into(),
                );
            }
        }
        let published = (!fresh.compiled || fresh.artifact.is_some())
            && journal.output().is_empty()
            && runtime
                .cache
                .publish(host, &runtime.store, &self.recipe, &fresh, &journal)
                .available();
        if let Some(log) = &runtime.purity_log {
            self.log_purity(log, name, &journal, published);
        }
        if published && let Some(remote) = &runtime.remote {
            remote.put(host, &runtime.store, &self.recipe);
        }
    }

    fn log_purity(&self, log: &Path, name: &[u8], journal: &Journal, published: bool) {
        use rcmake_json::Value;
        let entry = Value::object_from([
            ("variable", Value::string(name)),
            (
                "key",
                Value::string(compute_probe_key(&self.recipe).to_string()),
            ),
            ("published", Value::Bool(published)),
            (
                "impurities",
                Value::array_from(journal.impurities().iter().map(|(reason, site)| {
                    Value::object_from([
                        ("reason", Value::string(format!("{reason:?}"))),
                        ("file", Value::string(site.file())),
                        ("line", Value::UInt(site.line().into())),
                        ("column", Value::UInt(site.column().into())),
                    ])
                })),
            ),
        ]);
        let mut line = rcmake_json::write_compact(&entry).to_vec();
        line.push(b'\n');
        let _ = self
            .runtime
            .host
            .write(log, &line, rcmake_host::WriteMode::Append);
    }
}

fn seed_digest(
    cx: &Cx<'_>,
    defines: &[rcmake_eval::CacheDefine],
    enabled_languages: &[u8],
    probe_languages: &[BString],
    template: &TemplateCapture<'_>,
    host: &dyn Host,
) -> Option<(Digest, std::collections::BTreeSet<PathBuf>)> {
    let mut bound = std::collections::BTreeSet::new();
    let mut key = KeyHasher::new(Domain::ProbeV1);
    key.field(6, format!("{:?}", cx.limits()).as_bytes());
    key.field(7, &cx.var_or_empty(b"CMAKE_COMMAND"));
    for define in defines {
        key.field(8, &define.name);
        key.field(9, template.capture(&define.value)?.digest().as_bytes());
        key.field(
            10,
            define
                .ty
                .map_or("", rcmake_model::CacheType::as_str)
                .as_bytes(),
        );
    }
    key.field(11, enabled_languages);
    let platform = path::to_native(&cx.var_or_empty(b"CMAKE_PLATFORM_INFO_DIR"));
    if platform.is_absolute() {
        if host.metadata(&platform, Follow::Yes).ok()?.kind != FileKind::Dir {
            return None;
        }
        bound.insert(platform.clone());
        let mut files = vec![BString::from("CMakeSystem.cmake")];
        for language in probe_languages {
            if !language.iter().all(u8::is_ascii_alphanumeric) {
                return None;
            }
            files.push(format!("CMake{language}Compiler.cmake").into());
        }
        files.sort_unstable();
        files.dedup();
        for name in files {
            key.field(12, &name);
            let file_path = platform.join(path::to_native(&name));
            match host.metadata(&file_path, Follow::No) {
                Ok(stat) if stat.kind == FileKind::File => {}
                Err(error) if error.kind == std::io::ErrorKind::NotFound => {
                    key.field(14, b"missing");
                    continue;
                }
                _ => return None,
            }
            let bytes = host
                .read(&file_path, ReadRange::All { limit: 1 << 20 })
                .ok()?;
            bound.insert(file_path);
            key.field(13, template.capture(&bytes)?.digest().as_bytes());
        }
    }
    Some((key.finish(), bound))
}

fn read_artifact(host: &dyn Host, dirs: &ProbeDirs, file: &Path) -> Option<Artifact> {
    let relative_path = file.strip_prefix(&dirs.binary).ok()?.to_owned();
    let attr = host.metadata(file, Follow::No).ok()?;
    if attr.kind != FileKind::File || attr.nlink != 1 {
        return None;
    }
    let bytes = host
        .read(
            file,
            ReadRange::All {
                limit: 4 * 1024 * 1024,
            },
        )
        .ok()?;
    Some(Artifact {
        relative_path,
        mode: attr.mode & 0o777,
        bytes,
    })
}

#[cfg(all(test, unix))]
mod tests;
