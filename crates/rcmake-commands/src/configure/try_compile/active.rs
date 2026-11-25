use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use rcmake_base::BString;
use rcmake_build::BuildOutcome;
use rcmake_diag::{Effect, MessageKind};
use rcmake_eval::{Cx, LedgerLevel, ProbeCacheMode, Services};
use rcmake_host::{FileKind, Follow, Host, ReadRange, path};
use rcmake_store::{Digest, Domain, KeyHasher, StoreConfig, StoreMode, file_digest};

use super::args::{Options, Signature, Source};
use super::cache::{Artifact, ObservedOutcome, ProbeCache, VerifyContext};
use super::journal::{CompileIntervals, Journal, JournalHost};
use super::plan::{
    Admission, PathBindings, ProbeEligibility, ProbeRecipe, RcmakeBuildId, ReuseScope, Templated,
    compute_probe_key, environment_fingerprint,
};
use super::seed::TryCompileSeed;
use super::{ProbeDirs, ProbeResult};

mod compiler;
mod remote;
mod speculate;

pub(in crate::configure) struct ProbeRuntime {
    host: Arc<dyn Host>,
    store: StoreConfig,
    cache: ProbeCache,
    mode: ProbeCacheMode,
    build: PathBuf,
    executable: OnceLock<Option<Digest>>,
    compiler: compiler::SessionEvidence,
    intervals: Option<Arc<CompileIntervals>>,
    remote: Option<remote::RemoteProbe>,
}

impl ProbeRuntime {
    pub(in crate::configure) fn new(
        host: Arc<dyn Host>,
        build: &Path,
        services: &Services,
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
        Some(Self {
            host,
            intervals,
            store,
            cache: ProbeCache::for_session(mode),
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
                    .chain(options.many(b"LINK_OPTIONS"))
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
            Arc::clone(&runtime.host),
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
        let files = BTreeMap::from([
            (
                BString::from("CMakeLists.txt"),
                Templated::capture(&project, &bindings)?,
            ),
            (name, Templated::literal(&contents)?),
        ]);
        let (seed, bound) = seed_digest(cx, seed, &bindings, host)?;
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
            imports: Templated::capture(format!("{imports:?}").as_bytes(), &bindings)?.digest(),
            environment: environment_fingerprint(&environment),
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
        Some(ProbeResult {
            outcome: BuildOutcome {
                success: result.compiled,
                exit_code: result.exit_code,
                ..Default::default()
            },
            artifact,
            output: result.output.into(),
            diagnostics: BString::default(),
        })
    }

    pub(super) fn journal(&self, dirs: &ProbeDirs) -> Option<Arc<JournalHost<'static>>> {
        if self.runtime.mode == ProbeCacheMode::Off {
            return None;
        }
        JournalHost::from_shared(
            Arc::clone(&self.runtime.host),
            &dirs.binary,
            Arc::clone(&self.compiler),
            super::journal::JournalMode::Observe,
        )
        .ok()
        .map(|journal| {
            journal
                .with_trace_memo(Arc::clone(&self.runtime.cache.memo))
                .with_bound_inputs(self.bound.clone())
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
        let host = runtime.host.as_ref();
        let artifact = built
            .artifact
            .as_deref()
            .and_then(|file| read_artifact(host, dirs, file));
        let fresh = ObservedOutcome {
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
        if (fresh.compiled && fresh.artifact.is_none()) || !journal.output().is_empty() {
            return;
        }
        if runtime
            .cache
            .publish(host, &runtime.store, &self.recipe, &fresh, &journal)
            .available()
            && let Some(remote) = &runtime.remote
        {
            remote.put(host, &runtime.store, &self.recipe);
        }
    }
}

fn seed_digest(
    cx: &Cx<'_>,
    seed: &TryCompileSeed,
    bindings: &PathBindings,
    host: &dyn Host,
) -> Option<(Digest, std::collections::BTreeSet<PathBuf>)> {
    let mut bound = std::collections::BTreeSet::new();
    let mut key = KeyHasher::new(Domain::ProbeV1);
    key.field(5, &cx.recursion_depth().to_le_bytes());
    key.field(6, format!("{:?}", cx.limits()).as_bytes());
    key.field(7, &cx.var_or_empty(b"CMAKE_COMMAND"));
    for define in &seed.defines {
        key.field(8, &define.name);
        key.field(
            9,
            Templated::capture(&define.value, bindings)?
                .digest()
                .as_bytes(),
        );
        key.field(
            10,
            define
                .ty
                .map_or("", rcmake_model::CacheType::as_str)
                .as_bytes(),
        );
    }
    key.field(11, &seed.enabled_languages);
    let platform = path::to_native(&cx.var_or_empty(b"CMAKE_PLATFORM_INFO_DIR"));
    if platform.is_absolute() {
        let mut entries = host.read_dir(&platform).ok()?;
        bound.insert(platform.clone());
        if entries.len() > 256 {
            return None;
        }
        entries.sort_unstable_by(|left, right| left.name.cmp(&right.name));
        for file in entries
            .into_iter()
            .filter(|file| file.name.ends_with(b".cmake"))
        {
            if file.kind != FileKind::File
                || !file
                    .name
                    .iter()
                    .all(|byte| byte.is_ascii_alphanumeric() || b"._-".contains(byte))
            {
                return None;
            }
            let file_path = platform.join(path::to_native(&file.name));
            let bytes = host
                .read(&file_path, ReadRange::All { limit: 1 << 20 })
                .ok()?;
            bound.insert(file_path);
            key.field(12, &file.name);
            key.field(
                13,
                Templated::capture(&bytes, bindings)?.digest().as_bytes(),
            );
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
