//! Evaluate a configure request and resolve its build graph through a host.

#![forbid(unsafe_code)]

use std::collections::BTreeMap;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

use rcmake_base::{BString, Limits, list};
use rcmake_diag::{
    Backtrace, Diagnostic, Effect, FrameKind, Location, MessageKind, SourceId, SourceMap,
    source::SourceKind,
};
use rcmake_eval::{
    BuiltinFn, CmdError, IncludeOptions, Interp, InterpConfig, Registry, RunMode, StopReason,
};
use rcmake_genex::ConfigId;
use rcmake_host::{FileKind, Follow, Host, path};
use rcmake_model::{
    Cache, CacheEntry, CacheType, DirId, GeneratorKind, GeneratorSpec, ProjectModel,
};
use rcmake_resolve::{ResolveError, ResolveOptions, ide::IdeFlavor, resolve_with_options};
use rcmake_syntax::{CommandInvocation, parse_list_file};

use crate::{
    ApiError, CompletionState, ConfigureRequest, ConfigureResult, EvaluationFacts, FileEdge,
    FileEdgeKind, LedgerLevel, PreparedConfigure, PreviousValue, ResolvedEntry, SourceLocation,
    SourceRange, VarLedgerSnapshot, VariableWriteDetails,
};

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
    /// # Errors
    ///
    /// Return an error if the host, evaluator, or resolver fails before a usable
    /// result is available.
    pub fn evaluate(&self) -> Result<ConfigureResult, ApiError> {
        let prepared = self;
        let host = prepared.host();
        let generator = select_generator(prepared, host.as_ref())?;
        let command = host.current_exe()?;
        let command = host.canonicalize(&command).unwrap_or(command);
        let reads = Arc::new(Mutex::new(SourceReads::default()));
        let host = if prepared.wants_index() {
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
        let mut runtime = rcmake_commands::ConfigureRuntime::new(
            host,
            &prepared.source_dir,
            &prepared.build_dir,
            &prepared.services,
            false,
        );
        let result = prepared.evaluate_in(&mut runtime, generator, command, &reads);
        let complete = result.as_ref().is_ok_and(|result| {
            result.completion == CompletionState::Complete && prepared.denied_processes().is_empty()
        });
        runtime.finish(complete);
        result
    }

    fn evaluate_in(
        &self,
        runtime: &mut rcmake_commands::ConfigureRuntime,
        generator: GeneratorKind,
        command: PathBuf,
        reads: &Mutex<SourceReads>,
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
            services: prepared.services.clone(),
        };
        let mut interp = Interp::new(config.clone(), register);
        seed_cache(&mut interp, prepared);
        let mut diagnostics = run_preloads(&mut interp, prepared, &config)?;
        initialize(&mut interp, prepared, generator);
        set_directory_vars(&mut interp, prepared);
        if let Some(toolchain) = toolchain_file(&mut interp, prepared, host.as_ref()) {
            let _ = interp
                .cx()
                .include_file(&toolchain, IncludeOptions::default());
        }
        let outcome = if interp.fatal_occurred() {
            interp.exit_code()
        } else {
            runtime.run(&mut interp, &[]).exit_code
        };
        let stop = interp.stop_reason();
        diagnostics.extend_from_slice(interp.diag_engine().diagnostics());
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
        );
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
            None if outcome == 0 => resolve_project(prepared, generator, &mut result),
            None => {
                return Err(ApiError::ConfigureFailed {
                    exit_code: outcome,
                    diagnostics: result.diagnostics,
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
        cancelled,
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
    interp: &mut Interp,
    prepared: &PreparedConfigure,
    config: &InterpConfig,
) -> Result<Vec<Diagnostic>, ApiError> {
    let mut diagnostics = Vec::new();
    let source = &prepared.source_dir;
    let build = &prepared.build_dir;
    for dir in std::iter::once(source).chain((build != source).then_some(build)) {
        let file = dir.join("PreLoad.cmake");
        if !is_file(config.host.as_ref(), &file) {
            continue;
        }
        let mut preload = Interp::new(config.clone(), register);
        set_directory_vars(&mut preload, prepared);
        *preload.cx().cache() = std::mem::take(interp.cx().cache());
        let _ = preload.cx().include_file(&file, IncludeOptions::default());
        *interp.cx().cache() = std::mem::take(preload.cx().cache());
        diagnostics.extend_from_slice(preload.diag_engine().diagnostics());
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
            None if preload.fatal_occurred() || preload.exit_code() != 0 => {
                return Err(ApiError::ConfigureFailed {
                    exit_code: preload.exit_code(),
                    diagnostics,
                });
            }
            None => {}
        }
    }
    Ok(diagnostics)
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
        ide: match generator {
            GeneratorKind::Xcode => Some(IdeFlavor::Xcode),
            GeneratorKind::VisualStudio(_) => Some(IdeFlavor::VisualStudio),
            _ => None,
        },
    };
    let host = prepared.host();
    let resolved = resolve_with_options(&result.model, host.as_ref(), &configs, &options);
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

const MAX_SOURCE_READS: usize = 4096;
const MAX_SOURCE_BYTES: usize = 32 * 1024 * 1024;

#[derive(Default)]
struct SourceReads {
    files: BTreeMap<PathBuf, Option<Arc<[u8]>>>,
    bytes: usize,
}

impl SourceReads {
    fn record(&mut self, path: &Path, bytes: &[u8]) {
        if let Some(previous) = self.files.get_mut(path) {
            if previous
                .as_deref()
                .is_some_and(|previous| previous != bytes)
            {
                *previous = None;
            }
        } else if self.files.len() < MAX_SOURCE_READS
            && self.bytes.saturating_add(bytes.len()) <= MAX_SOURCE_BYTES
        {
            self.files.insert(path.to_owned(), Some(Arc::from(bytes)));
            self.bytes += bytes.len();
        }
    }
}

struct ParsedSource {
    id: SourceId,
    commands: Arc<[CommandInvocation]>,
}

fn evaluation_facts(
    model: &ProjectModel,
    ledger: Option<&VarLedgerSnapshot>,
    reads: &SourceReads,
    limits: &Limits,
) -> EvaluationFacts {
    let mut facts = EvaluationFacts::default();
    let mut sources = SourceMap::default();
    let mut parsed = BTreeMap::new();
    for file in model.dirs().flat_map(|(_, directory)| &directory.listfiles) {
        if parsed.contains_key(file) {
            continue;
        }
        let Some(Some(bytes)) = reads.files.get(&path::to_native(file)) else {
            continue;
        };
        let Some(id) = sources.add(file.clone(), SourceKind::File, Arc::clone(bytes)) else {
            continue;
        };
        let commands = parse_list_file(bytes, id, SourceKind::File, limits)
            .map_or_else(|_| Arc::from([]), |list| list.commands);
        parsed.insert(file.clone(), ParsedSource { id, commands });
    }
    if !parsed.is_empty() {
        facts.source_map = Some(Arc::new(sources));
    }

    for (id, target) in model.targets() {
        if let Some(mut site) = SourceLocation::from_backtrace(&target.backtrace) {
            let name = target
                .backtrace
                .top()
                .and_then(|frame| frame.name.as_deref());
            let (source, command) = command_at(&parsed, &site, name);
            site.source_id = source;
            site.range = command.and_then(|command| {
                command
                    .args
                    .first()
                    .filter(|arg| arg.text == target.name)
                    .and_then(|arg| source_range(&arg.span))
            });
            facts.target_sites.insert(id, site);
        }
        include_edges(
            model,
            &parsed,
            target.dir,
            &target.backtrace,
            &mut facts.file_edges,
        );
    }
    for (_, dir) in model.dirs() {
        let Some((parent, site)) = dir
            .parent
            .zip(SourceLocation::from_backtrace(&dir.backtrace))
        else {
            continue;
        };
        let destination =
            path::from_native_cmake(&path::to_native(&dir.source_dir).join("CMakeLists.txt"));
        if parsed.contains_key(&destination) {
            add_file_edge(
                &parsed,
                &mut facts.file_edges,
                FileEdgeKind::AddSubdirectory,
                parent,
                site,
                b"add_subdirectory",
                destination,
            );
        }
        include_edges(
            model,
            &parsed,
            parent,
            &dir.backtrace,
            &mut facts.file_edges,
        );
    }
    if let Some(ledger) = ledger {
        for name in ledger.names() {
            for event in ledger.history(name) {
                let dir = write_directory(model, &event.backtrace);
                if let Some(dir) = dir {
                    include_edges(model, &parsed, dir, &event.backtrace, &mut facts.file_edges);
                }
                facts.variable_writes.insert(
                    event.seq,
                    VariableWriteDetails {
                        dir,
                        binding_dir: event.binding_dir,
                        command: event
                            .backtrace
                            .top()
                            .and_then(|frame| frame.name.as_deref())
                            .map(BString::from),
                        previous: event
                            .old_value
                            .clone()
                            .map_or(PreviousValue::Unset, PreviousValue::Value),
                    },
                );
            }
        }
    }
    facts
}

fn write_directory(model: &ProjectModel, trace: &Backtrace) -> Option<DirId> {
    let mut frames = trace.frames().peekable();
    while let Some(frame) = frames.next() {
        if frame.kind != FrameKind::ModuleInclude
            || frames.peek().is_some_and(|caller| {
                !caller
                    .name
                    .as_deref()
                    .is_some_and(|name| name.eq_ignore_ascii_case(b"add_subdirectory"))
            })
        {
            continue;
        }
        let src = path::to_native(&frame.file);
        if src.file_name().is_none_or(|name| name != "CMakeLists.txt") {
            continue;
        }
        let dir = path::from_native_cmake(src.parent()?);
        let mut matching = model
            .dirs()
            .filter(|(_, candidate)| candidate.source_dir == dir);
        if let Some((id, _)) = matching.next() {
            return matching.next().is_none().then_some(id);
        }
    }
    None
}

fn include_edges(
    model: &ProjectModel,
    parsed: &BTreeMap<BString, ParsedSource>,
    dir: DirId,
    trace: &Backtrace,
    edges: &mut Vec<FileEdge>,
) {
    let mut frames = trace.frames().peekable();
    while let Some(frame) = frames.next() {
        if frame.kind != FrameKind::ModuleInclude {
            continue;
        }
        let Some(caller) = frames.peek() else {
            break;
        };
        if !caller
            .name
            .as_deref()
            .is_some_and(|name| name.eq_ignore_ascii_case(b"include"))
            || !model
                .dir(dir)
                .listfiles
                .iter()
                .any(|file| file.as_slice() == frame.file.as_ref())
        {
            continue;
        }
        add_file_edge(
            parsed,
            edges,
            FileEdgeKind::Include,
            dir,
            SourceLocation {
                path: caller.file.as_ref().into(),
                line: caller.line,
                byte_offset: None,
                source_id: None,
                range: None,
            },
            b"include",
            frame.file.as_ref().into(),
        );
    }
}

fn add_file_edge(
    parsed: &BTreeMap<BString, ParsedSource>,
    edges: &mut Vec<FileEdge>,
    kind: FileEdgeKind,
    dir: DirId,
    mut site: SourceLocation,
    name: &[u8],
    destination: BString,
) {
    let (source, command) = command_at(parsed, &site, Some(name));
    let Some(argument_range) = command
        .and_then(|command| command.args.first())
        .and_then(|arg| source_range(&arg.span))
    else {
        return;
    };
    site.source_id = source;
    let edge = FileEdge {
        kind,
        dir,
        site,
        argument_range: Some(argument_range),
        destination,
    };
    if edges.len() < MAX_SOURCE_READS && !edges.contains(&edge) {
        edges.push(edge);
    }
}

fn source_range(range: &Range<usize>) -> Option<SourceRange> {
    Some(SourceRange {
        start: u32::try_from(range.start).ok()?,
        end: u32::try_from(range.end).ok()?,
    })
}

fn command_at<'a>(
    parsed: &'a BTreeMap<BString, ParsedSource>,
    site: &SourceLocation,
    name: Option<&[u8]>,
) -> (Option<SourceId>, Option<&'a CommandInvocation>) {
    let Some(source) = parsed.get(&site.path) else {
        return (None, None);
    };
    let first = source
        .commands
        .partition_point(|command| command.line < site.line);
    let mut commands = source
        .commands
        .iter()
        .skip(first)
        .take_while(|command| command.line == site.line)
        .filter(|command| name.is_some_and(|name| command.name.eq_ignore_ascii_case(name)));
    let command = commands.next().filter(|_| commands.next().is_none());
    (Some(source.id), command)
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
