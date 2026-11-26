use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};

use rcmake_base::BString;
use rcmake_build::BuildOutcome;
use rcmake_diag::{Effect, MessageKind};
use rcmake_eval::{Cx, LedgerLevel, ProbeCacheMode, Services};
use rcmake_host::{FileKind, Follow, Host, ReadRange, path};
use rcmake_store::{Digest, Domain, KeyHasher, StoreConfig, StoreMode, TraceMemo};

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
    source: PathBuf,
    build: PathBuf,
    executable: OnceLock<Option<Digest>>,
    compiler: compiler::SessionEvidence,
    intervals: Option<Arc<CompileIntervals>>,
    purity_log: Option<PathBuf>,
    remote: Option<remote::RemoteProbe>,
    /// The background toolchain prefetch, once it starts.
    prefetch: OnceLock<Prefetch>,
}

/// A background toolchain prefetch and its cancel flag.
struct Prefetch {
    cancel: Arc<AtomicBool>,
    worker: std::sync::Mutex<Option<std::thread::JoinHandle<()>>>,
}

impl Drop for ProbeRuntime {
    fn drop(&mut self) {
        // Stop the prefetch at its next step, then wait for its bounded current step.
        if let Some(prefetch) = self.prefetch.get() {
            prefetch.cancel.store(true, Ordering::Relaxed);
            if let Some(worker) = prefetch
                .worker
                .lock()
                .ok()
                .and_then(|mut worker| worker.take())
            {
                let _ = worker.join();
            }
        }
    }
}

impl ProbeRuntime {
    pub(in crate::configure) fn new(
        host: Arc<dyn Host>,
        source: &Path,
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
        if mode == ProbeCacheMode::Verify {
            // Verification checks every input again with fresh reads.
            cache.memo = Arc::new(TraceMemo::verifying(Arc::clone(&host)));
        } else if let Some(host) = generation {
            cache.memo = Arc::new(TraceMemo::with_generation(host));
        }
        Some(Self {
            host,
            journal_host,
            intervals,
            purity_log,
            store,
            cache,
            mode,
            source: source.to_owned(),
            build: build.to_owned(),
            executable: OnceLock::new(),
            compiler: compiler::SessionEvidence::default(),
            remote,
            prefetch: OnceLock::new(),
        })
    }

    /// Return the session memo for file digests and search listings.
    pub(in crate::configure) fn trace_memo(&self) -> &Arc<TraceMemo> {
        &self.cache.memo
    }

    /// Start background work that warms the session memo for a C-family compiler.
    ///
    /// The work overlaps compiler identification. It hashes the Rcmake executable
    /// and, on Darwin, the SDK settings file, and it walks the SDK search
    /// directories of the admitted system root. It runs no process. Every
    /// result stays a candidate that the memo checks again with fresh queries
    /// before use. The work runs only once per session, never in verification
    /// mode, and only on a host with reliable directory stamps.
    pub(in crate::configure) fn prefetch_toolchain(&self, sysroot: Option<PathBuf>) {
        if self.mode == ProbeCacheMode::Verify || !self.host.directory_times_track_names() {
            return;
        }
        let cancel = Arc::new(AtomicBool::new(false));
        let prefetch = Prefetch {
            cancel: Arc::clone(&cancel),
            worker: std::sync::Mutex::new(None),
        };
        if self.prefetch.set(prefetch).is_err() {
            return;
        }
        let live = Arc::clone(&self.host);
        let memo = Arc::clone(&self.cache.memo);
        // A failure to start the thread only removes the speedup.
        let worker = std::thread::Builder::new()
            .name("rcmake-toolchain-prefetch".into())
            .spawn(move || prefetch_toolchain(&live, &memo, sysroot, &cancel));
        if let (Ok(worker), Some(prefetch)) = (worker, self.prefetch.get())
            && let Ok(mut slot) = prefetch.worker.lock()
        {
            *slot = Some(worker);
        }
    }

    fn executable(&self) -> Option<Digest> {
        if self.mode == ProbeCacheMode::Verify {
            // Verification hashes the executable again for each use.
            let exe = self.host.current_exe().ok()?;
            return rcmake_store::file_digest(self.host.as_ref(), &exe).ok();
        }
        *self.executable.get_or_init(|| {
            let exe = self.host.current_exe().ok()?;
            self.cache.memo.file_digest(self.host.as_ref(), &exe).ok()
        })
    }
}

pub(super) struct ProbeGeneratorFacts {
    pub(super) real: Arc<dyn Host>,
    pub(super) facts: Arc<BuildToolFacts>,
    pub(super) journal: Option<Arc<JournalHost<'static>>>,
    pub(super) memo: Arc<rcmake_store::TraceMemo>,
    pub(super) speculative: bool,
    pub(super) preverified_ninja: Option<crate::configure::build_tool_facts::VerifiedNinja>,
}

impl rcmake_resolve::GeneratorFacts for ProbeGeneratorFacts {
    fn ninja_cxx_dyndep(&self, host: &dyn Host, request: &rcmake_host::RunRequest) -> Option<bool> {
        use rcmake_resolve::query_ninja_cxx_dyndep;

        let program = path::to_native(&request.pipeline.first()?.program);
        let fallback = || {
            if self.speculative {
                if let Some(journal) = &self.journal {
                    journal.reject_generator_fact();
                }
                return None;
            }
            query_ninja_cxx_dyndep(host, request)
        };
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
        let env_digest = process_environment_fingerprint(&request.env);
        if self.speculative {
            let result = self
                .preverified_ninja
                .as_ref()
                .filter(|ninja| {
                    ninja.program == program && ninja.digest == digest && ninja.env == env_digest
                })
                .map(crate::configure::build_tool_facts::VerifiedNinja::supports_cxx_dyndep);
            if let Some(journal) = &self.journal
                && result.is_none()
            {
                journal.reject_generator_fact();
            }
            return result;
        }
        let result =
            self.facts
                .ninja_cxx_dyndep(self.real.as_ref(), &request, program, digest, env_digest);
        if let Some(journal) = &self.journal
            && result.is_none()
        {
            journal.reject_generator_fact();
        }
        result
    }
}

pub(in crate::configure) use compiler::admit_environment;
pub(in crate::configure) use compiler::{ReuseWarningReason, compiler_reuse_warning};

pub(super) fn native_ninja(host: &dyn Host, path: &Path) -> bool {
    crate::configure::build_tool_facts::is_native_ninja(host, path)
}

pub(super) struct PreparedProbe {
    runtime: Arc<ProbeRuntime>,
    recipe: ProbeRecipe,
    compiler: Arc<compiler::CompilerEvidence>,
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
        let select = |cx: &mut Cx<'_>| {
            compiler::ProbeCompiler::select(
                Arc::clone(&runtime.journal_host),
                cx,
                &runtime.compiler,
                &name,
                &runtime.cache.memo,
                external.then(|| (file, Digest::hash(&contents))),
                &dirs.binary,
            )
        };
        let compiler = if runtime.executable.get().is_none() {
            // The first probe hashes the Rcmake executable while it selects the compiler.
            crate::configure::concurrent::join(|| runtime.executable(), || select(cx)).1
        } else {
            select(cx)
        }?;
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
        // A read-only store publishes nothing, so only a purity log needs the journal.
        if self.runtime.mode == ProbeCacheMode::Off
            || (self.runtime.mode == ProbeCacheMode::ReadOnly && self.runtime.purity_log.is_none())
        {
            return None;
        }
        JournalHost::from_shared(
            Arc::clone(&self.runtime.journal_host),
            &dirs.binary,
            Arc::clone(&self.compiler) as Arc<dyn super::journal::ProcessVerifier>,
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
    let command = cx.var_or_empty(b"CMAKE_COMMAND");
    let platform = path::to_native(&cx.var_or_empty(b"CMAKE_PLATFORM_INFO_DIR"));
    seed_digest_with(
        cx,
        &command,
        &platform,
        defines,
        enabled_languages,
        probe_languages,
        template,
        host,
    )
}

fn seed_digest_peek(
    cx: &Cx<'_>,
    defines: &[rcmake_eval::CacheDefine],
    enabled_languages: &[u8],
    probe_languages: &[BString],
    template: &TemplateCapture<'_>,
    host: &dyn Host,
) -> Option<(Digest, std::collections::BTreeSet<PathBuf>)> {
    let command = cx
        .peek_var(b"CMAKE_COMMAND")
        .map_or(&[][..], |v| v.as_slice());
    let platform = path::to_native(
        cx.peek_var(b"CMAKE_PLATFORM_INFO_DIR")
            .map_or(&[][..], |v| v.as_slice()),
    );
    seed_digest_with(
        cx,
        command,
        &platform,
        defines,
        enabled_languages,
        probe_languages,
        template,
        host,
    )
}

#[allow(clippy::too_many_arguments)]
fn seed_digest_with(
    cx: &Cx<'_>,
    command: &[u8],
    platform: &Path,
    defines: &[rcmake_eval::CacheDefine],
    enabled_languages: &[u8],
    probe_languages: &[BString],
    template: &TemplateCapture<'_>,
    host: &dyn Host,
) -> Option<(Digest, std::collections::BTreeSet<PathBuf>)> {
    let mut bound = std::collections::BTreeSet::new();
    let mut key = KeyHasher::new(Domain::ProbeV1);
    key.field(6, format!("{:?}", cx.limits()).as_bytes());
    key.field(7, command);
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
    if platform.is_absolute() {
        if host.metadata(platform, Follow::Yes).ok()?.kind != FileKind::Dir {
            return None;
        }
        bound.insert(platform.to_path_buf());
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

/// Warm `memo` with digests and SDK listings that a C-family probe needs.
fn prefetch_toolchain(
    live: &Arc<dyn Host>,
    memo: &TraceMemo,
    sysroot: Option<PathBuf>,
    cancel: &AtomicBool,
) {
    // The memo keeps a digest only for a file that this user cannot change.
    // A build of Rcmake that this user owns is hashed again on each use.
    if let Ok(exe) = live.current_exe() {
        memo.prefetch_file_digest(live.as_ref(), &exe);
    }
    if live.system().sysname != "Darwin" {
        return;
    }
    // Hash the probable Apple driver and linker before the ABI probe needs
    // them. A wrong guess only wastes background work, because the probe
    // checks each digest again with fresh identity and metadata.
    for file in apple_tool_candidates(live.as_ref()) {
        if cancel.load(Ordering::Relaxed) {
            return;
        }
        memo.prefetch_file_digest(live.as_ref(), &file);
    }
    let Some(sdk) = sysroot.filter(|sdk| sdk.is_absolute()) else {
        return;
    };
    for root in [
        "usr/include",
        "System/Library/Frameworks",
        "System/Library/SubFrameworks",
    ] {
        if cancel.load(Ordering::Relaxed) {
            return;
        }
        memo.prefetch_sdk_digest(live.as_ref(), &sdk.join(root), cancel);
    }
    if !cancel.load(Ordering::Relaxed) {
        let _ = memo.file_digest(live.as_ref(), &sdk.join("SDKSettings.json"));
    }
}

/// Return the probable paths of the Apple compiler driver and linker.
///
/// Find the developer directory in the order that `xcode-select` uses. Use
/// `DEVELOPER_DIR`, then the first `xcode-select` link, then Xcode in
/// `/Applications` if it exists, then the Command Line Tools. Run no process.
fn apple_tool_candidates(host: &dyn Host) -> Vec<PathBuf> {
    let developer = apple_developer_dir(host);
    let toolchain = developer.join("Toolchains/XcodeDefault.xctoolchain/usr/bin");
    let tools = developer.join("usr/bin");
    let mut candidates = Vec::with_capacity(5);
    for dir in [toolchain, tools] {
        for name in ["clang", "ld"] {
            let file = dir.join(name);
            if host
                .metadata(&file, Follow::Yes)
                .is_ok_and(|stat| stat.kind == FileKind::File)
            {
                candidates.push(file);
            }
        }
    }
    candidates.push(PathBuf::from("/usr/bin/xcrun"));
    candidates
}

/// Return the probable developer directory. See [`apple_tool_candidates`].
fn apple_developer_dir(host: &dyn Host) -> PathBuf {
    if let Some(dir) = host
        .env_get(b"DEVELOPER_DIR")
        .filter(|value| !value.is_empty())
        .map(|value| path::to_native(&value))
        .filter(|dir| dir.is_absolute())
    {
        return dir;
    }
    for link in [
        "/var/select/developer_dir",
        "/var/db/xcode_select_link",
        "/usr/share/xcode-select/xcode_dir_link",
    ] {
        if let Ok(dir) = host.read_link(Path::new(link))
            && dir.is_absolute()
        {
            return dir;
        }
    }
    let xcode = PathBuf::from("/Applications/Xcode.app/Contents/Developer");
    if host
        .metadata(&xcode, Follow::Yes)
        .is_ok_and(|stat| stat.kind == FileKind::Dir)
    {
        return xcode;
    }
    PathBuf::from("/Library/Developer/CommandLineTools")
}
