//! Evaluate a configure request and resolve its build graph through a host.

#![forbid(unsafe_code)]

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

use rcmake_base::{BString, list};
use rcmake_diag::{Diagnostic, Effect, Location, MessageKind, SourceMap};
use rcmake_eval::{
    BuiltinFn, CmdError, Cx, IncludeOptions, Interp, InterpConfig, Registry, RunMode, StopReason,
};
use rcmake_genex::ConfigId;
use rcmake_host::{FileKind, Follow, Host, no_process::NoProcessHost, path};
use rcmake_model::{
    Cache, CacheEntry, CacheType, DirId, GeneratorKind, GeneratorSpec, ProjectModel,
};
use rcmake_resolve::{ResolveError, ResolveOptions, ide::IdeFlavor, resolve_with_options};

use crate::{
    ApiError, CompletionState, ConfigureRequest, ConfigureResult, LedgerLevel, PreparedConfigure,
    ResolvedEntry, SourceLocation, VarLedgerSnapshot,
};

/// Stack budget for an evaluation on a caller thread without a budget. It fits in the
/// 2 MiB default stack of a Rust thread and leaves room for native recursion. The
/// caller must make sure that its thread has at least this much free stack.
const API_STACK_BUDGET: usize = 1024 * 1024;

mod facts;
use facts::{SourceReads, evaluation_facts, merge_diagnostics};

/// Evaluate CMakeLists.txt and resolve the configured project's build graphs.
///
/// All file and process operations use the prepared session host. The result
/// contains the evaluated model, any resolved graphs, and the requested index.
/// A denied process makes the result partial. A stop during evaluation returns
/// the facts collected so far when the evaluator reached the root listfile.
/// Cancellation and command limits stop evaluation before resolution.
///
/// # Errors
///
/// Return an error for an invalid request, an unavailable host operation, or an
/// evaluation or resolution failure that cannot provide a usable snapshot.
pub fn run_configure(req: ConfigureRequest) -> Result<ConfigureResult, ApiError> {
    let prepared = req.prepare()?;
    let result = prepared.evaluate()?;
    Ok(prepared.finish(result))
}

impl PreparedConfigure {
    /// Evaluate this session and resolve its build graphs.
    ///
    /// Call [Self::finish] to attach the host observations and build the index.
    ///
    /// On a native target, the calling thread must have at least 2 MiB of free stack
    /// (the default for a Rust thread). Without a budget from
    /// `rcmake_eval::stack_guard::with_budget`, the evaluation uses a 1 MiB stack budget.
    /// A caller with a larger known stack can set a larger budget around this call.
    ///
    /// # Errors
    ///
    /// Return an error if the host, evaluator, or resolver fails before a usable
    /// result is available.
    pub fn evaluate(&self) -> Result<ConfigureResult, ApiError> {
        // The stack of an embedder thread is unknown. Without a budget from the caller,
        // use a budget that fits the smallest default thread stack.
        if rcmake_eval::stack_guard::is_active() {
            self.evaluate_with_budget()
        } else {
            rcmake_eval::stack_guard::with_budget(API_STACK_BUDGET, || self.evaluate_with_budget())
        }
    }

    fn evaluate_with_budget(&self) -> Result<ConfigureResult, ApiError> {
        let prepared = self;
        let host = prepared.host();
        let generator = select_generator(prepared, host.as_ref())?;
        let command = host.current_exe()?;
        let command = host.canonicalize(&command).unwrap_or(command);
        let reads = Arc::new(Mutex::new(SourceReads::default()));
        let host = if prepared.wants_index()
            || !matches!(prepared.services.provenance, LedgerLevel::Off)
        {
            let reads = Arc::clone(&reads);
            prepared.observed_host(
                move |path, bytes| {
                    reads
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .record(path, bytes);
                },
                |_, _| false,
            )
        } else {
            host
        };
        let mut services = prepared.services.clone();
        if prepared.wants_index()
            || !matches!(services.provenance, LedgerLevel::Off)
            || services.collect_annotations
            || prepared.initial_cache.is_some()
            || prepared
                .defines
                .iter()
                .any(|define| define.name == b"CMAKE_TOOLCHAIN_FILE")
            || [&prepared.source_dir, &prepared.build_dir]
                .into_iter()
                .any(|dir| is_file(host.as_ref(), &dir.join("PreLoad.cmake")))
        {
            Arc::make_mut(&mut services).speed.incremental = false;
        }
        let mut runtime = rcmake_commands::ConfigureRuntime::new(
            host,
            &prepared.source_dir,
            &prepared.build_dir,
            &services,
            prepared.allows_persistence(),
        );
        let result = prepared.evaluate_in(&mut runtime, generator, command, &reads, services);
        let complete = result.as_ref().is_ok_and(|result| {
            result.completion == CompletionState::Complete && prepared.denied_processes().is_empty()
        });
        let finish = runtime.finish(complete);
        match (result, finish) {
            (Ok(result), Some(error)) => Err(ApiError::GenerateFailed {
                message: error.to_string(),
                diagnostics: result.diagnostics,
                sources: result.facts.source_map,
            }),
            (result, _) => result,
        }
    }

    fn evaluate_in(
        &self,
        runtime: &mut rcmake_commands::ConfigureRuntime,
        generator: GeneratorKind,
        command: PathBuf,
        reads: &Mutex<SourceReads>,
        services: Arc<crate::Services>,
    ) -> Result<ConfigureResult, ApiError> {
        let prepared = self;
        let host = runtime.host();
        let config = InterpConfig {
            host: Arc::clone(&host),
            mode: RunMode::Configure,
            cmake_root: prepared.cmake_root.clone(),
            cmake_command: command,
            cwd: prepared.cwd.clone(),
            limits: prepared.limits.clone(),
            diag: prepared.diag.clone(),
            defines: Vec::new(),
            script_args: Vec::new(),
            services,
        };
        let mut interp = Interp::new(config.clone(), register);
        let denial_tracker = prepared.denial_adapter().map(|adapter| {
            let tracker = Arc::new(ProcessDenialTracker::new(adapter));
            interp.add_session_data(Arc::clone(&tracker));
            tracker
        });
        runtime.attach(&mut interp);
        seed_cache(&mut interp, prepared);
        let (preload_diagnostics, preload_sources) = run_preloads(
            runtime,
            &mut interp,
            prepared,
            &config,
            denial_tracker.as_ref(),
        )?;
        initialize(&mut interp, prepared, generator);
        set_directory_vars(&mut interp, prepared);
        if let Some(toolchain) = toolchain_file(&mut interp, prepared, host.as_ref()) {
            let _ = interp
                .cx()
                .include_file(&toolchain, IncludeOptions::default());
        }
        let outcome = if runtime.denied() {
            1
        } else if interp.fatal_occurred() {
            interp.exit_code()
        } else {
            runtime.run(&mut interp, &[]).exit_code
        };
        if let Some(tracker) = denial_tracker {
            let (module_denials, project_sites) = tracker.take();
            prepared.record_denial_attribution(module_denials, project_sites);
        }
        let stop = interp.stop_reason();
        let mut sources = if prepared.wants_index() || prepared.services.collect_annotations {
            interp.take_diagnostic_sources()
        } else {
            SourceMap::default()
        };
        let mut diagnostics = Vec::new();
        merge_diagnostics(
            &mut diagnostics,
            &mut sources,
            &preload_diagnostics,
            &preload_sources,
        );
        diagnostics.extend_from_slice(interp.diag_engine().diagnostics());
        let find_traces = (prepared.services.find_trace != rcmake_eval::FindTraceLevel::Off)
            .then(|| interp.take_find_traces());
        let var_ledger = interp.take_var_ledger().map(|ledger| {
            VarLedgerSnapshot::from_ledger(ledger, prepared.services.provenance.clone())
        });
        let mut result = ConfigureResult {
            diagnostics,
            model: std::mem::take(interp.cx().project()),
            var_ledger,
            ..ConfigureResult::default()
        };
        result.facts = evaluation_facts(
            &result.model,
            result.var_ledger.as_ref(),
            &reads
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
            &prepared.limits,
            sources,
            prepared.wants_index(),
        );
        result.facts.find_traces = find_traces;
        if prepared.wants_index() {
            result.facts.callables = interp
                .callable_definitions()
                .map(
                    |(kind, name, file, line, params)| crate::CallableDefinition {
                        name: name.clone(),
                        kind,
                        location: SourceLocation {
                            path: file.into(),
                            line,
                            byte_offset: None,
                            source_id: None,
                            range: None,
                        },
                        parameters: params.to_vec(),
                        documentation: None,
                    },
                )
                .collect();
        }
        let has_snapshot = result.model.dir(DirId::ROOT).final_policies.is_some()
            || result.model.targets().next().is_some();
        let resolution = match stop {
            Some(StopReason::Cancelled) if !has_snapshot => return Err(ApiError::Cancelled),
            Some(StopReason::Cancelled) => {
                result.completion = partial(true);
                Ok(())
            }
            Some(StopReason::StepBudgetExceeded { limit }) if !has_snapshot => {
                return Err(ApiError::StepBudgetExceeded { limit });
            }
            Some(StopReason::StepBudgetExceeded { limit }) => {
                result.completion = partial(false);
                record_error(
                    &mut result,
                    ApiError::StepBudgetExceeded { limit }.to_string(),
                );
                Ok(())
            }
            Some(StopReason::DeniedProcess) if !has_snapshot => {
                return Err(ApiError::DeniedProcess {
                    commands: prepared.denied_processes(),
                });
            }
            Some(StopReason::DeniedProcess) => {
                result.completion = partial(false);
                Ok(())
            }
            None if outcome == 0 => {
                resolve_project(prepared, host.as_ref(), generator, &mut result)
            }
            None => {
                return Err(ApiError::ConfigureFailed {
                    exit_code: outcome,
                    diagnostics: result.diagnostics,
                    sources: result.facts.source_map,
                    find_traces: result.facts.find_traces.map(Box::new),
                });
            }
        };
        resolution?;
        Ok(result)
    }
}

fn cancelled(prepared: &PreparedConfigure) -> bool {
    prepared
        .services
        .cancel
        .as_ref()
        .is_some_and(|token| token.is_cancelled())
}

fn partial(cancelled: bool) -> CompletionState {
    CompletionState::Partial {
        denied_processes: Vec::new(),
        omitted_denials: 0,
        omitted_denial_bytes: 0,
        module_denials: 0,
        project_denial_sites: Vec::new(),
        cancelled,
    }
}

#[derive(Default)]
struct TrackedDenials {
    confirmed_cached_denials: usize,
    project_sites: Vec<(BString, u32)>,
}

struct ProcessDenialTracker {
    adapter: Arc<NoProcessHost<dyn Host>>,
    state: Mutex<TrackedDenials>,
}

impl ProcessDenialTracker {
    fn new(adapter: Arc<NoProcessHost<dyn Host>>) -> Self {
        Self {
            adapter,
            state: Mutex::default(),
        }
    }

    fn total_denied(&self) -> usize {
        self.adapter.total_denied()
    }

    fn record_command_denial(
        &self,
        file: &[u8],
        line: u32,
        count: usize,
        confirmed_cached_reuse: bool,
    ) {
        if count == 0 {
            return;
        }
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if confirmed_cached_reuse {
            state.confirmed_cached_denials = state.confirmed_cached_denials.saturating_add(count);
        } else if !file.is_empty() && state.project_sites.len() < 128 {
            state.project_sites.push((BString::from(file), line));
        }
    }

    fn take(&self) -> (usize, Vec<(BString, u32)>) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let confirmed = state.confirmed_cached_denials;
        let sites = std::mem::take(&mut state.project_sites);
        (confirmed, sites)
    }
}

fn record_denied_command(
    cx: &Cx<'_>,
    tracker: Option<&Arc<ProcessDenialTracker>>,
    before: Option<usize>,
) {
    if let (Some(tracker), Some(before)) = (tracker, before) {
        let added = tracker.total_denied().saturating_sub(before);
        if added > 0
            && let Some(top) = cx.backtrace().top()
        {
            tracker.record_command_denial(&top.file, top.line, added, false);
        }
    }
}

fn select_generator(
    prepared: &PreparedConfigure,
    host: &dyn Host,
) -> Result<GeneratorKind, ApiError> {
    let saved = prepared
        .defines
        .iter()
        .rev()
        .find(|def| def.name == b"CMAKE_GENERATOR")
        .map(|def| &def.value)
        .or_else(|| {
            prepared
                .initial_cache
                .as_ref()?
                .get(b"CMAKE_GENERATOR")
                .map(|entry| &entry.value)
        });
    let saved_kind = saved
        .map(|name| {
            GeneratorKind::from_name(name).ok_or_else(|| ApiError::InvalidConfig {
                field: "generator",
                message: format!("Unknown generator {name}."),
            })
        })
        .transpose()?;
    if let (Some(requested), Some(saved)) = (prepared.generator, saved_kind)
        && requested != saved
    {
        return Err(ApiError::InvalidConfig {
            field: "generator",
            message: format!(
                "The build directory uses {}. Select a build directory for {}.",
                saved.name(),
                requested.name()
            ),
        });
    }
    Ok(prepared
        .generator
        .or(saved_kind)
        .or_else(|| {
            host.env_get(b"CMAKE_GENERATOR")
                .and_then(|name| GeneratorKind::from_name(&name))
                .filter(|kind| kind.host_available())
        })
        .unwrap_or(GeneratorKind::UnixMakefiles))
}

fn initialize(interp: &mut Interp, prepared: &PreparedConfigure, generator: GeneratorKind) {
    let mut cx = interp.cx();
    let cache = cx.cache();
    let setting = |key: &[u8]| {
        cache
            .get(key)
            .map(|entry| entry.value.clone())
            .unwrap_or_default()
    };
    let spec = GeneratorSpec {
        toolset: setting(b"CMAKE_GENERATOR_TOOLSET"),
        platform: setting(b"CMAKE_GENERATOR_PLATFORM"),
        instance: setting(b"CMAKE_GENERATOR_INSTANCE"),
        ..GeneratorSpec::default()
    };
    for (key, value) in [
        ("CMAKE_GENERATOR", BString::from(generator.name())),
        (
            "CMAKE_HOME_DIRECTORY",
            path::from_native_cmake(&prepared.source_dir),
        ),
    ] {
        write_cache(cache, key.into(), value, Some(CacheType::Internal));
    }
    let project = cx.project();
    project.generator = Some(generator);
    project.generator_spec = spec;
}

fn register(registry: &mut Registry) {
    rcmake_commands::register_all(registry);
    static PROJECT: OnceLock<BuiltinFn> = OnceLock::new();
    if let Some((command, flags)) = registry.get(b"project") {
        PROJECT.get_or_init(|| command);
        registry.add(
            "project",
            |cx, args| {
                let model = cx.project();
                if let Some(kind) = model.generator.filter(|kind| !kind.is_ide()) {
                    let spec = &model.generator_spec;
                    if let Err(error) = GeneratorSpec::parse(
                        kind,
                        &spec.toolset,
                        &spec.platform,
                        &spec.instance,
                        None,
                    ) {
                        cx.issue(
                            MessageKind::Error,
                            Effect::Fatal,
                            None,
                            error.to_string().into(),
                        );
                        return Err(CmdError::Reported);
                    }
                }
                PROJECT.get().ok_or(CmdError::Reported)?(cx, args)
            },
            flags,
        );
    }
    static EXECUTE_PROCESS: OnceLock<BuiltinFn> = OnceLock::new();
    if let Some((command, flags)) = registry.get(b"execute_process") {
        EXECUTE_PROCESS.get_or_init(|| command);
        registry.add(
            "execute_process",
            |cx, args| {
                let tracker = cx.session_data::<Arc<ProcessDenialTracker>>().cloned();
                let before = tracker.as_ref().map(|t| t.total_denied());
                let result = EXECUTE_PROCESS.get().ok_or(CmdError::Reported)?(cx, args);
                record_denied_command(cx, tracker.as_ref(), before);
                result
            },
            flags,
        );
    }
    static EXEC_PROGRAM: OnceLock<BuiltinFn> = OnceLock::new();
    if let Some((command, flags)) = registry.get(b"exec_program") {
        EXEC_PROGRAM.get_or_init(|| command);
        registry.add(
            "exec_program",
            |cx, args| {
                let tracker = cx.session_data::<Arc<ProcessDenialTracker>>().cloned();
                let before = tracker.as_ref().map(|t| t.total_denied());
                let result = EXEC_PROGRAM.get().ok_or(CmdError::Reported)?(cx, args);
                record_denied_command(cx, tracker.as_ref(), before);
                result
            },
            flags,
        );
    }
    static TRY_COMPILE: OnceLock<BuiltinFn> = OnceLock::new();
    if let Some((command, flags)) = registry.get(b"try_compile") {
        TRY_COMPILE.get_or_init(|| command);
        registry.add(
            "try_compile",
            |cx, args| {
                let tracker = cx.session_data::<Arc<ProcessDenialTracker>>().cloned();
                let before = tracker.as_ref().map(|t| t.total_denied());
                let result = TRY_COMPILE.get().ok_or(CmdError::Reported)?(cx, args);
                record_denied_command(cx, tracker.as_ref(), before);
                result
            },
            flags,
        );
    }
    static TRY_RUN: OnceLock<BuiltinFn> = OnceLock::new();
    if let Some((command, flags)) = registry.get(b"try_run") {
        TRY_RUN.get_or_init(|| command);
        registry.add(
            "try_run",
            |cx, args| {
                let tracker = cx.session_data::<Arc<ProcessDenialTracker>>().cloned();
                let before = tracker.as_ref().map(|t| t.total_denied());
                let result = TRY_RUN.get().ok_or(CmdError::Reported)?(cx, args);
                record_denied_command(cx, tracker.as_ref(), before);
                result
            },
            flags,
        );
    }
    static FILE_CMD: OnceLock<BuiltinFn> = OnceLock::new();
    if let Some((command, flags)) = registry.get(b"file") {
        FILE_CMD.get_or_init(|| command);
        registry.add(
            "file",
            |cx, args| {
                let tracker = cx.session_data::<Arc<ProcessDenialTracker>>().cloned();
                let before = tracker.as_ref().map(|t| t.total_denied());
                let result = FILE_CMD.get().ok_or(CmdError::Reported)?(cx, args);
                record_denied_command(cx, tracker.as_ref(), before);
                result
            },
            flags,
        );
    }
    static SET: OnceLock<BuiltinFn> = OnceLock::new();
    if let Some((command, flags)) = registry.get(b"set") {
        SET.get_or_init(|| command);
        registry.add(
            "set",
            |cx, args| {
                let existing = cache_declare_name(args.iter().map(|arg| arg.value.as_slice()))
                    .and_then(|name| {
                        cx.cache()
                            .get(name)
                            .filter(|entry| entry.ty != CacheType::Uninitialized)
                            .map(|_| BString::from(name))
                    });
                let result = SET.get().ok_or(CmdError::Reported)?(cx, args);
                if result.is_ok()
                    && let Some(name) = existing
                {
                    cx.record_cache_declare(&name);
                }
                result
            },
            flags,
        );
    }
    static OPTION_CMD: OnceLock<BuiltinFn> = OnceLock::new();
    if let Some((command, flags)) = registry.get(b"option") {
        OPTION_CMD.get_or_init(|| command);
        registry.add(
            "option",
            |cx, args| {
                let existing = args.first().and_then(|name| {
                    cx.cache()
                        .get(&name.value)
                        .filter(|entry| entry.ty != CacheType::Uninitialized)
                        .map(|_| name.value.clone())
                });
                let result = OPTION_CMD.get().ok_or(CmdError::Reported)?(cx, args);
                if result.is_ok()
                    && let Some(name) = existing
                {
                    cx.record_cache_declare(&name);
                }
                result
            },
            flags,
        );
    }
}

fn cache_declare_name<'a>(
    args: impl ExactSizeIterator<Item = &'a [u8]> + Clone,
) -> Option<&'a [u8]> {
    let mut iter = args.clone();
    let first = iter.next()?;
    if let Some(inner) = first
        .strip_prefix(b"CACHE{")
        .and_then(|rest| rest.strip_suffix(b"}"))
    {
        let has_force = iter
            .take_while(|arg| *arg != b"VALUE")
            .any(|arg| arg == b"FORCE");
        return (!has_force).then_some(inner);
    }
    let len = args.len();
    if len < 4 || args.clone().last().is_some_and(|arg| arg == b"FORCE") {
        return None;
    }
    let cache_pos = len.checked_sub(3)?;
    args.into_iter()
        .nth(cache_pos)
        .filter(|arg| *arg == b"CACHE")
        .map(|_| first)
}

fn seed_cache(interp: &mut Interp, prepared: &PreparedConfigure) {
    let mut cx = interp.cx();
    let cache = cx.cache();
    *cache = prepared.initial_cache.clone().unwrap_or_default();
    for definition in &prepared.defines {
        write_cache(
            cache,
            definition.name.clone(),
            definition.value.clone(),
            definition.ty,
        );
    }
}

fn write_cache(cache: &mut Cache, name: BString, value: BString, ty: Option<CacheType>) {
    if let Some(entry) = cache.get_mut(&name) {
        entry.value = value;
        if let Some(ty) = ty {
            entry.ty = ty;
        }
    } else {
        cache.insert(
            name,
            CacheEntry {
                value,
                ty: ty.unwrap_or(CacheType::Uninitialized),
                help: BString::default(),
                advanced: false,
                props: Default::default(),
            },
        );
    }
}

fn set_directory_vars(interp: &mut Interp, prepared: &PreparedConfigure) {
    let source = path::from_native_cmake(&prepared.source_dir);
    let binary = path::from_native_cmake(&prepared.build_dir);
    let mut cx = interp.cx();
    cx.set_var(b"CMAKE_SOURCE_DIR", source.clone());
    cx.set_var(b"CMAKE_CURRENT_SOURCE_DIR", source);
    cx.set_var(b"CMAKE_BINARY_DIR", binary.clone());
    cx.set_var(b"CMAKE_CURRENT_BINARY_DIR", binary);
}

fn run_preloads(
    runtime: &rcmake_commands::ConfigureRuntime,
    interp: &mut Interp,
    prepared: &PreparedConfigure,
    config: &InterpConfig,
    denial_tracker: Option<&Arc<ProcessDenialTracker>>,
) -> Result<(Vec<Diagnostic>, SourceMap), ApiError> {
    let mut diagnostics = Vec::new();
    let mut sources = SourceMap::default();
    let source = &prepared.source_dir;
    let build = &prepared.build_dir;
    for dir in std::iter::once(source).chain((build != source).then_some(build)) {
        let file = dir.join("PreLoad.cmake");
        if !is_file(config.host.as_ref(), &file) {
            continue;
        }
        let mut preload = Interp::new(config.clone(), register);
        if let Some(tracker) = denial_tracker {
            preload.add_session_data(Arc::clone(tracker));
        }
        runtime.attach(&mut preload);
        set_directory_vars(&mut preload, prepared);
        *preload.cx().cache() = std::mem::take(interp.cx().cache());
        let _ = preload.cx().include_file(&file, IncludeOptions::default());
        *interp.cx().cache() = std::mem::take(preload.cx().cache());
        let preload_sources = preload.take_diagnostic_sources();
        merge_diagnostics(
            &mut diagnostics,
            &mut sources,
            preload.diag_engine().diagnostics(),
            &preload_sources,
        );
        match preload.stop_reason() {
            Some(StopReason::Cancelled) => return Err(ApiError::Cancelled),
            Some(StopReason::StepBudgetExceeded { limit }) => {
                return Err(ApiError::StepBudgetExceeded { limit });
            }
            Some(StopReason::DeniedProcess) => {
                return Err(ApiError::DeniedProcess {
                    commands: prepared.denied_processes(),
                });
            }
            None if runtime.denied() || preload.fatal_occurred() || preload.exit_code() != 0 => {
                return Err(ApiError::ConfigureFailed {
                    exit_code: if runtime.denied() {
                        1
                    } else {
                        preload.exit_code()
                    },
                    diagnostics,
                    sources: Some(Arc::new(sources)),
                    find_traces: None,
                });
            }
            None => {}
        }
    }
    Ok((diagnostics, sources))
}

fn is_file(host: &dyn Host, path: &Path) -> bool {
    host.metadata(path, Follow::Yes)
        .is_ok_and(|metadata| metadata.kind == FileKind::File)
}

fn toolchain_file(
    interp: &mut Interp,
    prepared: &PreparedConfigure,
    host: &dyn Host,
) -> Option<PathBuf> {
    let mut cx = interp.cx();
    let cache = cx.cache();
    let entry = cache
        .get_mut(b"CMAKE_TOOLCHAIN_FILE")
        .filter(|entry| !entry.value.is_empty())?;
    if entry.ty == CacheType::Uninitialized {
        entry.ty = CacheType::Filepath;
    }
    let path = path::to_native(&entry.value);
    let file = if path.is_absolute() {
        path
    } else {
        let build = prepared.build_dir.join(&path);
        if is_file(host, &build) {
            build
        } else {
            prepared.source_dir.join(path)
        }
    };
    entry.value = path::from_native_cmake(&file);
    is_file(host, &file).then_some(file)
}

fn resolve_project(
    prepared: &PreparedConfigure,
    host: &dyn Host,
    generator: GeneratorKind,
    result: &mut ConfigureResult,
) -> Result<(), ApiError> {
    if cancelled(prepared) {
        result.completion = partial(true);
        return Ok(());
    }
    let configs = configurations(&result.model, generator);
    let options = ResolveOptions {
        provenance: !matches!(prepared.services.provenance, LedgerLevel::Off),
        generator_facts: None,
        ide: match generator {
            GeneratorKind::Xcode => Some(IdeFlavor::Xcode),
            GeneratorKind::VisualStudio(_) => Some(IdeFlavor::VisualStudio),
            _ => None,
        },
    };
    let resolved = resolve_with_options(&result.model, host, &configs, &options);
    let mut generation = match resolved {
        Ok(generation) if !cancelled(prepared) => generation,
        Ok(_) => {
            result.completion = partial(true);
            return Ok(());
        }
        Err(error) => {
            let recoverable = missing_source(&error) && !prepared.denied_processes().is_empty();
            let message = error.to_string();
            record_error(result, message.clone());
            if recoverable {
                result.completion = partial(cancelled(prepared));
                return Ok(());
            }
            return Err(ApiError::GenerateFailed {
                message,
                diagnostics: std::mem::take(&mut result.diagnostics),
                sources: result.facts.source_map.clone(),
            });
        }
    };
    result.facts.resolved_entries = std::mem::take(&mut generation.provenance_entries)
        .into_iter()
        .map(|entry| ResolvedEntry {
            key: entry.key,
            value: entry.value,
            link_targets: entry.link_targets,
        })
        .collect();
    result.generation = Some(generation);
    Ok(())
}

fn record_error(result: &mut ConfigureResult, message: String) {
    result.diagnostics.push(Diagnostic {
        kind: MessageKind::Error,
        effect: Effect::Fatal,
        category: None,
        policy: None,
        location: Location::None,
        body: message.into(),
        annotations: None,
    });
}

fn missing_source(error: &ResolveError) -> bool {
    match error {
        ResolveError::Message(body) | ResolveError::Reported(body) => {
            body == b"Cannot find source file."
                || body.starts_with(b"Cannot find source file:\n")
                || body.starts_with(b"No SOURCES given to target:")
                || body.starts_with(b"Cannot determine link language for target ")
        }
        _ => false,
    }
}

fn configurations(model: &ProjectModel, generator: GeneratorKind) -> Vec<ConfigId> {
    if !generator.multi_config() {
        // The resolver reads CMAKE_BUILD_TYPE for an empty request.
        return Vec::new();
    }
    match model.definition(DirId::ROOT, b"CMAKE_CONFIGURATION_TYPES") {
        Some(value) => list::split(value, list::Empty::Drop)
            .into_iter()
            .map(ConfigId)
            .collect(),
        None => generator
            .default_configuration_types()
            .iter()
            .map(|name| ConfigId((*name).into()))
            .collect(),
    }
}

#[cfg(test)]
#[path = "runner_tests.rs"]
mod tests;
