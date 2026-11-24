use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use rcmake_base::{BString, truth::is_on};
use rcmake_diag::{Effect, MessageKind};
use rcmake_eval::{BuiltinFn, CmdError, Cx, IncludeOptions, Interp, Registry, RunMode};
use rcmake_host::{FileKind, FileTime, Follow, Host, Stream, WriteMode, path};
use rcmake_instrument::Query;
use rcmake_json::Value;
use rcmake_model::{Cache, CacheType, GeneratorKind};

use crate::options::{self, CacheEdit, DirectoryArg, Options};
use crate::{cache, emit_cli_error, new_interp, new_interp_with_paths, presets, resolve_cli_path};

mod diagnostics;
mod graphviz;

use graphviz::write_graphviz;
mod unused_cli;

pub(crate) use diagnostics::initialize_diagnostics;

const HOME_DIR: &[u8] = b"CMAKE_HOME_DIRECTORY";
const GENERATOR: &[u8] = b"CMAKE_GENERATOR";

pub(crate) fn register(registry: &mut Registry) {
    rcmake_commands::register_all(registry);
    static PROJECT: OnceLock<BuiltinFn> = OnceLock::new();
    if let Some((command, flags)) = registry.get(b"project") {
        PROJECT.get_or_init(|| command);
        registry.add(
            "project",
            |cx, args| {
                validate_generator(cx)?;
                PROJECT.get().ok_or(CmdError::Reported)?(cx, args)
            },
            flags,
        );
    }
}

fn validate_generator(cx: &mut Cx<'_>) -> Result<(), CmdError> {
    let model = cx.project();
    if let Some(kind) = model.generator.filter(|kind| !kind.is_ide()) {
        let spec = &model.generator_spec;
        if let Err(error) = rcmake_model::GeneratorSpec::parse(
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
    Ok(())
}

pub(crate) fn run(argv: &[BString], opts: Options, host: Arc<dyn Host>) -> i32 {
    let (cmake_command, cmake_root) = rcmake_host::discover_cmake_root(host.as_ref(), argv.first());
    let Some(cmake_root) = cmake_root else {
        return crate::missing_cmake_root(host.as_ref(), &cmake_command, true);
    };
    match configure_project(argv, opts, Arc::clone(&host), cmake_command, cmake_root) {
        Ok(()) => 0,
        Err(Exit::Code(code)) => code,
        Err(Exit::Message(message)) => error(host.as_ref(), message),
    }
}

enum Exit {
    Code(i32),
    Message(String),
}

impl Exit {
    fn message(message: impl ToString) -> Self {
        Self::Message(message.to_string())
    }
}

struct ConfigureSession<'a> {
    argv: &'a [BString],
    host: Arc<dyn Host>,
    opts: Options,
    src: PathBuf,
    bin: PathBuf,
    preset: presets::Application,
    profiler: Option<Arc<crate::profile::Recorder>>,
}

struct Configured {
    queries: Vec<Query>,
    sample: bool,
}

fn configure_project(
    argv: &[BString],
    opts: Options,
    host: Arc<dyn Host>,
    cmake_command: PathBuf,
    cmake_root: PathBuf,
) -> Result<(), Exit> {
    let (mut session, saved) = ConfigureSession::resolve(argv, opts, host)?;
    let (mut interp, has_cache) = session.initialize(saved, cmake_command, cmake_root)?;
    let start = session.host.monotonic();
    let time = session.host.now();
    let mut preloads = session.apply_cache_edits(&mut interp);
    let mut results = std::mem::take(&mut preloads.results);
    let outcome = (|| {
        if session.opts.view {
            session.list_cache(&mut interp);
            return Ok(());
        }
        if preloads.fatal {
            return Err(Exit::Code(preloads.exit_code));
        }
        session.check_source()?;
        if preloads.missing {
            return Err(Exit::Code(1));
        }
        let configured = session.configure(&mut interp, has_cache, start, preloads)?;
        session.generate(&mut interp, configured, &mut results)
    })();
    let code = match &outcome {
        Ok(()) => 0,
        Err(Exit::Code(code)) => *code,
        Err(Exit::Message(_)) => 1,
    };
    if let Some((profiler, path)) = session
        .profiler
        .as_ref()
        .zip(session.opts.profile_output.as_deref())
    {
        profiler
            .write(session.host.as_ref(), path)
            .map_err(|error| Exit::Message(format!("Could not write profiling output: {error}")))?;
    }
    session.write_sarif(&mut interp, results, time, code)?;
    outcome
}

impl<'a> ConfigureSession<'a> {
    fn resolve(
        argv: &'a [BString],
        mut opts: Options,
        host: Arc<dyn Host>,
    ) -> Result<(Self, Cache), Exit> {
        let (mut src, mut bin) = resolve_directories(host.as_ref(), &opts.directories);
        let cwd = host.initial_cwd();
        if let Some(listing) = opts.list_presets
            && (src.is_some()
                || bin.is_none()
                || opts
                    .directories
                    .iter()
                    .any(|arg| matches!(arg, DirectoryArg::Binary(_))))
        {
            return Err(Exit::Code(presets::list(
                host.as_ref(),
                src.as_deref().unwrap_or(&cwd),
                opts.presets_file.as_deref(),
                listing,
            )));
        }
        let mut saved = load_cache(host.as_ref(), bin.as_deref().unwrap_or(&cwd))?;
        if src.is_none() {
            src = saved
                .get(HOME_DIR)
                .map(|entry| path::to_native(&entry.value));
        }
        let had_src = src.is_some();
        let had_bin = bin.is_some();
        let src = src.unwrap_or_else(|| cwd.clone());

        if let Some(listing) = opts.list_presets {
            return Err(Exit::Code(presets::list(
                host.as_ref(),
                &src,
                opts.presets_file.as_deref(),
                listing,
            )));
        }
        let mut preset = presets::Application::default();
        if opts.preset.is_some() {
            preset = presets::apply(host.as_ref(), &src, &mut opts).map_err(Exit::Code)?;
            if bin.is_none()
                && let Some(dir) = preset.binary.take()
            {
                if dir != cwd {
                    saved = load_cache(host.as_ref(), &dir)?;
                }
                bin = Some(dir);
            }
        }
        if !had_src && !had_bin && opts.preset.is_none() && opts.list_cache.is_none() {
            options::warn(
                host.as_ref(),
                "No source or binary directory provided.  Both will be assumed to be the\n  same as the current working directory, but note that this warning will\n  become a fatal error in future CMake releases.",
            );
        }
        let bin = bin.unwrap_or(cwd);
        if opts.fresh && !opts.view {
            clear_cache(host.as_ref(), &bin).map_err(|err| {
                Exit::Message(format!(
                    "Could not remove the cache in {}: {err}",
                    bin.display()
                ))
            })?;
            saved = Cache::default();
        }
        let profiler = opts
            .profile_output
            .as_ref()
            .map(|_| Arc::new(crate::profile::Recorder::new(host.as_ref())));
        Ok((
            Self {
                argv,
                host,
                opts,
                src,
                bin,
                preset,
                profiler,
            },
            saved,
        ))
    }

    fn initialize(
        &self,
        saved: Cache,
        cmake_command: PathBuf,
        cmake_root: PathBuf,
    ) -> Result<(Interp, bool), Exit> {
        let host = self.host.as_ref();
        let generator = self
            .opts
            .generator
            .as_ref()
            .or_else(|| saved.get(GENERATOR).map(|entry| &entry.value))
            .cloned()
            .unwrap_or_else(|| options::default_generator(host));
        if !options::is_generator(&generator) {
            options::generator_error(host, Some(&generator));
            return Err(Exit::Code(1));
        }
        if let Some(old) = saved.get(GENERATOR)
            && old.value != generator
        {
            return Err(Exit::Message(format!(
                "Error: generator : {generator}\nDoes not match the generator used previously: {}\nEither remove the CMakeCache.txt file and CMakeFiles directory or choose a different binary directory.",
                old.value
            )));
        }
        if let Some(old) = saved.get(HOME_DIR) {
            let old_src = path::to_native(&old.value);
            if old_src != self.src
                && host
                    .canonicalize(&old_src)
                    .ok()
                    .is_none_or(|old| host.canonicalize(&self.src).ok().as_ref() != Some(&old))
            {
                return Err(Exit::Message(format!(
                    "The source \"{}/CMakeLists.txt\" does not match the source \"{}/CMakeLists.txt\" used to generate cache.  Re-run cmake with a different source directory.",
                    self.src.display(),
                    old_src.display()
                )));
            }
        }

        let generator_kind = GeneratorKind::from_name(&generator);
        let has_cache = saved.contains(HOME_DIR);
        let spec_value = |given: Option<&BString>, key: &[u8]| {
            given
                .or_else(|| saved.get(key).map(|entry| &entry.value))
                .cloned()
                .unwrap_or_default()
        };
        let generator_spec = rcmake_model::GeneratorSpec {
            toolset: spec_value(self.opts.toolset.as_ref(), b"CMAKE_GENERATOR_TOOLSET"),
            platform: spec_value(self.opts.architecture.as_ref(), b"CMAKE_GENERATOR_PLATFORM"),
            instance: spec_value(None, b"CMAKE_GENERATOR_INSTANCE"),
            ..rcmake_model::GeneratorSpec::default()
        };
        let mut interp = new_interp_with_paths(
            &self.opts,
            RunMode::Configure,
            Arc::clone(&self.host),
            cmake_command,
            cmake_root,
            self.argv,
        );
        if let Some(profiler) = &self.profiler {
            profiler.attach(&mut interp);
        }
        *interp.cx().cache() = saved;
        interp.cx().project().generator = generator_kind;
        interp.cx().project().generator_spec = generator_spec;
        cache::insert(
            interp.cx().cache(),
            GENERATOR,
            generator,
            Some(CacheType::Internal),
        );
        cache::insert(
            interp.cx().cache(),
            HOME_DIR,
            path::from_native(&self.src),
            Some(CacheType::Internal),
        );
        for key in [
            b"CMAKE_ROOT".as_slice(),
            b"CMAKE_COMMAND",
            b"CMAKE_CTEST_COMMAND",
            b"CMAKE_CPACK_COMMAND",
        ] {
            let value = interp.cx().var_or_empty(key);
            cache::insert(interp.cx().cache(), key, value, Some(CacheType::Internal));
        }
        let directory = path::from_native(&self.bin);
        for (key, value) in [
            (b"CMAKE_CACHEFILE_DIR".as_slice(), directory),
            (b"CMAKE_CACHE_MAJOR_VERSION", "4".into()),
            (b"CMAKE_CACHE_MINOR_VERSION", "5".into()),
            (b"CMAKE_CACHE_PATCH_VERSION", "1".into()),
        ] {
            cache::insert(interp.cx().cache(), key, value, Some(CacheType::Internal));
        }
        set_directory_vars(&mut interp, &self.src, &self.bin);
        Ok((interp, has_cache))
    }

    fn apply_cache_edits(&mut self, interp: &mut Interp) -> Preloads {
        let host = self.host.as_ref();
        let mut preloads = Preloads::default();
        let mut configured_diagnostics = false;
        for edit in std::mem::take(&mut self.opts.cache_edits) {
            match edit {
                CacheEdit::Define(def) => {
                    let name = def.name.clone();
                    diagnostics::warn_legacy_define(host, &name);
                    if unused_cli::apply_define(interp.cx().cache(), def)
                        && preloads.tracked.insert(name.clone())
                    {
                        interp.track_variable_reads([name]);
                    }
                }
                CacheEdit::Remove(pattern) => {
                    remove_entries(interp, &pattern, &mut preloads.tracked)
                }
                CacheEdit::Preload(script) => {
                    if !configured_diagnostics {
                        self.apply_diagnostics(interp);
                        configured_diagnostics = true;
                    }
                    let path = host.initial_cwd().join(&script);
                    if is_preload_file(host, &path) {
                        self.run_preload(interp, &script, true, &mut preloads);
                    } else {
                        options::out(
                            host,
                            &format!("loading initial cache file {}\n", script.display()),
                        );
                        emit_cli_error(host, format!("Not a file: {}", path.display()));
                        if self.opts.view || is_file(host, &self.src.join("CMakeLists.txt")) {
                            emit_cli_error(
                                host,
                                format!("Error processing file: {}", path.display()),
                            );
                        }
                        preloads.missing = true;
                    }
                }
            }
            if preloads.fatal {
                break;
            }
        }
        if !configured_diagnostics {
            self.apply_diagnostics(interp);
        }
        if !self.preset.log.is_empty() {
            options::out(host, &self.preset.log);
        }
        for (index, dir) in [&self.src, &self.bin].into_iter().enumerate() {
            if index == 1 && self.src == self.bin {
                break;
            }
            let file = dir.join("PreLoad.cmake");
            if is_file(host, &file) {
                self.run_preload(interp, &file, false, &mut preloads);
            }
        }
        preloads
    }

    fn run_preload(
        &self,
        configure: &mut Interp,
        script: &Path,
        initial_cache: bool,
        preloads: &mut Preloads,
    ) {
        let mut interp = new_interp(
            self.argv,
            &self.opts,
            RunMode::Configure,
            Arc::clone(&self.host),
        );
        if let Some(profiler) = &self.profiler {
            profiler.attach(&mut interp);
        }
        *interp.cx().diag_state_mut() = *configure.cx().diag_state();
        *interp.cx().cache() = std::mem::take(configure.cx().cache());
        set_directory_vars(&mut interp, &self.src, &self.bin);
        let pending = configure
            .unread_variables()
            .map(BString::from)
            .collect::<BTreeSet<_>>();
        interp.track_variable_reads(pending.iter().cloned());
        if initial_cache {
            let _ = interp.run_preload_script(script);
        } else {
            let _ = interp.cx().include_file(script, IncludeOptions::default());
        }
        let unread = interp
            .unread_variables()
            .map(BString::from)
            .collect::<BTreeSet<_>>();
        configure.untrack_variable_reads(pending.difference(&unread).cloned());
        *configure.cx().cache() = std::mem::take(interp.cx().cache());
        let code = interp.exit_code();
        if code != 0 {
            preloads.exit_code = code;
        }
        preloads.fatal |= interp.fatal_occurred();
        preloads.results.extend(crate::sarif::take_results(&interp));
    }

    fn apply_diagnostics(&self, interp: &mut Interp) {
        let state = initialize_diagnostics(
            self.host.as_ref(),
            interp.cx().cache(),
            &self.preset.warnings,
            &self.opts.warning_flags,
            true,
        );
        *interp.cx().diag_state_mut() = state;
    }

    fn check_source(&self) -> Result<(), Exit> {
        let host = self.host.as_ref();
        if is_file(host, &self.src.join("CMakeLists.txt")) {
            return Ok(());
        }
        let problem = if is_dir(host, &self.src) {
            "does not appear to contain CMakeLists.txt"
        } else {
            "does not exist"
        };
        Err(Exit::Message(format!(
            "The source directory \"{}\" {problem}.\nSpecify --help for usage, or press the help button on the CMake GUI.",
            self.src.display()
        )))
    }

    fn configure(
        &self,
        interp: &mut Interp,
        has_cache: bool,
        start: Duration,
        preloads: Preloads,
    ) -> Result<Configured, Exit> {
        let host = self.host.as_ref();
        let toolchain =
            apply_environment_defaults(host, interp.cx().cache(), &self.src, &self.bin, has_cache);
        if let Some(toolchain) = toolchain
            && is_file(host, &toolchain)
        {
            let _ = interp
                .cx()
                .include_file(&toolchain, IncludeOptions::default());
        }
        rcmake_instrument::query::clear_generated_queries(host, &self.bin)
            .map_err(Exit::message)?;
        if host
            .env_get(b"CTEST_USE_INSTRUMENTATION")
            .is_some_and(|value| is_on(&value))
        {
            use rcmake_instrument::{DATA_VERSION, InstrumentOption, Query};
            let mut query = Query {
                version: DATA_VERSION,
                callbacks: Vec::new(),
                hooks: Vec::new(),
                options: vec![
                    InstrumentOption::CdashSubmit,
                    InstrumentOption::DynamicSystemInformation,
                ],
            };
            if host
                .env_get(b"CTEST_USE_VERBOSE_INSTRUMENTATION")
                .is_some_and(|value| is_on(&value))
            {
                query.options.push(InstrumentOption::CdashVerbose);
            }
            rcmake_instrument::query::write_generated_query(host, &self.bin, &query)
                .map_err(Exit::message)?;
        }
        // Configure scripts can add a query that requests the initial sample.
        let mut configure_step = crate::InstrumentationStep::start(host, true);
        let key = b"CMAKE_INSTALL_PREFIX";
        if let Some(entry) = interp.cx().cache().get_mut(key)
            && !entry.value.is_empty()
            && !rcmake_base::path_lex::Lexical::new(&entry.value).absolute()
        {
            entry.value =
                path::from_native_cmake(&host.initial_cwd().join(path::to_native(&entry.value)));
        }
        let result = interp.run_configure(&self.src, &self.bin, &[]);
        if result.exit_code == 0
            && rcmake_resolve::validate_codegen_name(interp.cx().project(), host).is_err()
        {
            if self.show_status() {
                options::out(host, "-- Configuring incomplete, errors occurred!\n");
            }
            self.list_cache(interp);
            return Err(Exit::Code(1));
        }
        let queries =
            rcmake_instrument::query::read_queries(host, &self.bin).map_err(Exit::message)?;
        let sample = crate::sample_instrumentation(&queries);
        if !sample {
            configure_step.before = None;
        }
        if !queries.is_empty() {
            configure_step
                .record(
                    host,
                    &self.bin,
                    self.argv,
                    rcmake_instrument::SnippetRole::Configure,
                    &crate::CommandExit::Code(result.exit_code),
                )
                .map_err(Exit::message)?;
        }
        if result.exit_code == 0 && self.show_status() {
            let elapsed = host.monotonic().saturating_sub(start);
            options::out(
                host,
                &format!("-- Configuring done ({:.1}s)\n", elapsed.as_secs_f64()),
            );
        }
        let exit_code = if result.exit_code == 0 {
            preloads.exit_code
        } else {
            result.exit_code
        };
        if exit_code != 0 {
            self.list_cache(interp);
            return Err(Exit::Code(exit_code));
        }
        Ok(Configured { queries, sample })
    }

    fn generate(
        &self,
        interp: &mut Interp,
        configured: Configured,
        results: &mut Vec<Value>,
    ) -> Result<(), Exit> {
        let host = self.host.as_ref();
        let Configured { queries, sample } = configured;
        if let Some(graph) = &self.opts.graphviz {
            write_graphviz(Arc::clone(&self.host), graph, interp)?;
        }
        let show_status = self.show_status();
        if self.opts.debug_output && show_status {
            options::out(
                host,
                &format!(
                    "-- Generating {}\n   Called from: [1]\t{}\n",
                    self.bin.display(),
                    self.src.join("CMakeLists.txt").display()
                ),
            );
        }
        let start = host.monotonic();
        let generate_step = crate::InstrumentationStep::start(host, sample);
        let generated = match crate::generate::run(interp, host, &self.bin, &queries) {
            Ok(generated) => generated,
            Err(failure) => {
                if let crate::generate::Failure::Message(message) = failure {
                    if message.starts_with("CMake Error ") || message.starts_with("CMake Error:") {
                        host.write_out(Stream::Stderr, message.as_bytes());
                        if !message.ends_with('\n') {
                            host.write_out(Stream::Stderr, b"\n");
                        }
                    } else {
                        emit_cli_error(host, message);
                    }
                }
                generation_failed(host);
                return Err(Exit::Code(1));
            }
        };
        let elapsed = host.monotonic().saturating_sub(start);
        if generated && !queries.is_empty() {
            generate_step
                .record(
                    host,
                    &self.bin,
                    self.argv,
                    rcmake_instrument::SnippetRole::Generate,
                    &crate::CommandExit::Code(0),
                )
                .map_err(Exit::message)?;
        }
        cache::save(host, &self.bin, interp.cx().cache())
            .map_err(|err| Exit::Message(format!("Could not write CMakeCache.txt: {err}")))?;
        if generated && show_status {
            options::out(
                host,
                &format!("-- Generating done ({:.1}s)\n", elapsed.as_secs_f64()),
            );
        }
        if unused_cli::report(host, interp, results) {
            generation_failed(host);
            return Err(Exit::Code(1));
        }
        if generated && !queries.is_empty() {
            rcmake_instrument::hooks::run_hook(
                host,
                &self.bin,
                rcmake_instrument::Hook::PostGenerate,
            )
            .map_err(Exit::message)?;
        }
        if generated && show_status {
            options::out(
                host,
                &format!(
                    "-- Build files have been written to: {}\n",
                    path::from_native_cmake(&self.bin)
                ),
            );
        }
        self.list_cache(interp);
        Ok(())
    }

    fn show_status(&self) -> bool {
        self.opts
            .log_level
            .is_none_or(|level| level >= rcmake_eval::LogLevel::Status)
    }

    fn list_cache(&self, interp: &mut Interp) {
        if let Some(list) = &self.opts.list_cache {
            list_cache(self.host.as_ref(), interp, list);
        }
    }

    fn write_sarif(
        &self,
        interp: &mut Interp,
        preloads: Vec<Value>,
        start: FileTime,
        exit_code: i32,
    ) -> Result<(), Exit> {
        let host = self.host.as_ref();
        let default = self.bin.join(".cmake/sarif");
        let default_output;
        let output = match &self.opts.sarif {
            Some(path) => path,
            None if interp
                .cx()
                .cache()
                .get(b"CMAKE_EXPORT_SARIF")
                .is_some_and(|entry| is_on(&entry.value)) =>
            {
                host.create_dir_all(&default).map_err(Exit::message)?;
                default_output = default.join("cmake.sarif");
                &default_output
            }
            None => return Ok(()),
        };
        let command = self
            .argv
            .first()
            .filter(|argument| path::to_native(argument).is_absolute())
            .cloned()
            .unwrap_or_else(|| interp.cx().var_or_empty(b"CMAKE_COMMAND"));
        let invocation = crate::sarif::Invocation {
            command: &command,
            start,
            exit_code,
            directories: Some((&self.src, &self.bin)),
        };
        if crate::sarif::write(host, output, interp, preloads, invocation) {
            Ok(())
        } else {
            Err(Exit::Code(1))
        }
    }
}

#[derive(Default)]
struct Preloads {
    tracked: BTreeSet<BString>,
    results: Vec<Value>,
    exit_code: i32,
    fatal: bool,
    missing: bool,
}

fn load_cache(host: &dyn Host, bin: &Path) -> Result<Cache, Exit> {
    cache::load(host, bin).map_err(|err| {
        Exit::Message(format!(
            "Could not load cache from {}: {err}",
            bin.display()
        ))
    })
}

fn resolve_directories(
    host: &dyn Host,
    args: &[DirectoryArg],
) -> (Option<PathBuf>, Option<PathBuf>) {
    let mut source = None;
    let mut binary = None;
    let explicit_binary = args
        .iter()
        .any(|arg| matches!(arg, DirectoryArg::Binary(_)));
    let mut ignored = Vec::new();
    for (index, arg) in args.iter().enumerate() {
        let (slot, path, slash) = match arg {
            DirectoryArg::Source(path) => (&mut source, path.as_path(), false),
            DirectoryArg::Binary(path) => (&mut binary, path.as_path(), false),
            DirectoryArg::Positional(pos, slash) => {
                let path = match pos.file_name().and_then(|name| name.to_str()) {
                    Some("CMakeCache.txt" | "CMakeLists.txt") => pos.parent().unwrap_or(pos),
                    _ => pos,
                };
                if is_file(host, &path.join("CMakeCache.txt")) && !explicit_binary {
                    (&mut binary, path, *slash)
                } else if is_file(host, &path.join("CMakeLists.txt")) || source.is_none() {
                    (&mut source, path, *slash)
                } else if !explicit_binary && binary.is_none() {
                    (&mut binary, path, *slash)
                } else {
                    ignored.push((index, path, *slash));
                    continue;
                }
            }
        };
        if let Some(old) = slot.replace((index, path, slash))
            && old.1 != path
        {
            ignored.push(old);
        }
    }
    let source = source.map(|(_, path, _)| path);
    let binary = binary.map(|(_, path, _)| path);
    ignored.sort_by_key(|(index, _, _)| *index);
    for (_, path, slash) in ignored {
        if source != Some(path) && binary != Some(path) {
            let suffix = if slash && path.parent().is_some() {
                "/"
            } else {
                ""
            };
            options::warn(
                host,
                &format!(
                    "Ignoring extra path from command line:\n\n   \"{}{suffix}\"",
                    path.display()
                ),
            );
        }
    }
    (source.map(Path::to_path_buf), binary.map(Path::to_path_buf))
}

fn set_directory_vars(interp: &mut Interp, src: &Path, bin: &Path) {
    let mut cx = interp.cx();
    let source = path::from_native_cmake(src);
    let binary = path::from_native_cmake(bin);
    cx.set_var(b"CMAKE_SOURCE_DIR", source.clone());
    cx.set_var(b"CMAKE_CURRENT_SOURCE_DIR", source);
    cx.set_var(b"CMAKE_BINARY_DIR", binary.clone());
    cx.set_var(b"CMAKE_CURRENT_BINARY_DIR", binary);
}

fn clear_cache(host: &dyn Host, bin: &Path) -> rcmake_host::HostResult<()> {
    let ignore_missing = |result: rcmake_host::HostResult<()>| {
        result.or_else(|err| {
            if err.kind == std::io::ErrorKind::NotFound {
                Ok(())
            } else {
                Err(err)
            }
        })
    };
    ignore_missing(host.remove_file(&bin.join("CMakeCache.txt")))?;
    let files = bin.join("CMakeFiles");
    if host
        .metadata(&files, Follow::No)
        .is_ok_and(|m| m.kind == FileKind::Symlink)
    {
        ignore_missing(host.remove_file(&files))
    } else {
        ignore_missing(host.remove_dir_all(&files))
    }
}

fn apply_environment_defaults(
    host: &dyn Host,
    cache: &mut Cache,
    src: &Path,
    bin: &Path,
    has_cache: bool,
) -> Option<PathBuf> {
    for (name, ty) in [
        (b"CMAKE_TOOLCHAIN_FILE".as_slice(), CacheType::Filepath),
        (b"CMAKE_INSTALL_PREFIX", CacheType::Path),
        (b"CMAKE_BUILD_TYPE", CacheType::String),
        (b"CMAKE_EXPORT_COMPILE_COMMANDS", CacheType::Bool),
        (b"CMAKE_COLOR_DIAGNOSTICS", CacheType::Bool),
        (b"CMAKE_CROSSCOMPILING_EMULATOR", CacheType::String),
    ] {
        if !has_cache
            && !cache.contains(name)
            && let Some(value) = host.env_get(name).filter(|v| !v.is_empty())
        {
            let value = if name == b"CMAKE_COLOR_DIAGNOSTICS" {
                BString::from(if is_on(&value) { "ON" } else { "OFF" })
            } else {
                value
            };
            cache::insert(cache, name, value, Some(ty));
        }
        if let Some(entry) = cache.get_mut(name)
            && entry.ty == CacheType::Uninitialized
        {
            entry.ty = ty;
        }
    }
    if cache
        .get(GENERATOR)
        .is_some_and(|g| g.value.windows(4).any(|window| window == b"Make"))
        && !cache.contains(b"CMAKE_COLOR_MAKEFILE")
    {
        cache::insert(
            cache,
            b"CMAKE_COLOR_MAKEFILE",
            "ON".into(),
            Some(CacheType::Bool),
        );
    }
    let entry = cache
        .get_mut(b"CMAKE_TOOLCHAIN_FILE")
        .filter(|entry| !entry.value.is_empty())?;
    let path = path::to_native(&entry.value);
    let absolute = if path.is_absolute() {
        resolve_cli_path(host, &entry.value)
    } else {
        let build_path = bin.join(&path);
        if is_file(host, &build_path) {
            build_path
        } else {
            src.join(&path)
        }
    };
    entry.value = path::from_native(&absolute);
    Some(absolute)
}

fn remove_entries(interp: &mut Interp, pattern: &[u8], tracked: &mut BTreeSet<BString>) {
    let mut cx = interp.cx();
    let names: Vec<_> = cx
        .cache()
        .iter()
        .filter(|(name, entry)| entry.ty != CacheType::Static && matches_glob(pattern, name))
        .map(|(name, _)| name.clone())
        .collect();
    for name in &names {
        cx.cache().remove(name);
        tracked.remove(name);
    }
    interp.untrack_variable_reads(names);
}

pub(crate) fn matches_glob(mut pattern: &[u8], mut name: &[u8]) -> bool {
    let mut wildcard = None;
    while let Some((byte, tail)) = name.split_first() {
        match pattern.split_first() {
            Some((b'*', rest)) => {
                wildcard = Some((rest, name));
                pattern = rest;
            }
            Some((matched, rest)) if matched == byte || *matched == b'?' => {
                pattern = rest;
                name = tail;
            }
            _ => {
                let Some((rest, [_, next @ ..])) = wildcard else {
                    return false;
                };
                name = next;
                pattern = rest;
                wildcard = Some((rest, next));
            }
        }
    }
    pattern.iter().all(|byte| *byte == b'*')
}

fn list_cache(host: &dyn Host, interp: &mut Interp, list: &options::ListCache) {
    let mut cx = interp.cx();
    let filter = list
        .pattern
        .as_ref()
        .and_then(|pattern| cx.regex(pattern).ok());
    let mut budget = cx.limits().regex_steps;
    let mut out = BString::from("-- Cache values\n");
    for (name, entry) in cache::sorted(cx.cache()) {
        if matches!(
            entry.ty,
            CacheType::Internal | CacheType::Uninitialized | CacheType::Static
        ) || (!list.advanced && entry.advanced)
            || filter.as_ref().is_some_and(|r| {
                !r.find_at(name, 0, Default::default(), &mut budget)
                    .is_ok_and(|m| m.is_some())
            })
        {
            continue;
        }
        if list.help {
            out.extend_from_slice(b"// ");
            out.extend_from_slice(&entry.help);
            out.push(b'\n');
        }
        out.extend_from_slice(name);
        out.extend_from_slice(format!(":{}=", entry.ty.as_str()).as_bytes());
        out.extend_from_slice(&entry.value);
        out.push(b'\n');
        if list.help {
            out.push(b'\n');
        }
    }
    host.write_out(Stream::Stdout, &out);
}

fn is_preload_file(host: &dyn Host, path: &Path) -> bool {
    host.metadata(path, Follow::Yes).is_ok_and(|meta| {
        meta.kind == FileKind::File
            || (meta.kind == FileKind::Other
                && path.parent().is_some_and(|dir| {
                    dir == Path::new("/dev/fd") || dir == Path::new("/proc/self/fd")
                }))
    })
}

pub(crate) fn is_file(host: &dyn Host, path: &Path) -> bool {
    host.metadata(path, Follow::Yes)
        .is_ok_and(|meta| meta.kind == FileKind::File)
}

pub(crate) fn is_dir(host: &dyn Host, path: &Path) -> bool {
    host.metadata(path, Follow::Yes)
        .is_ok_and(|meta| meta.kind == FileKind::Dir)
}

fn generation_failed(host: &dyn Host) {
    host.write_out(
        Stream::Stderr,
        b"CMake Generate step failed.  Build files cannot be regenerated correctly.\n",
    );
}

fn error(host: &dyn Host, message: String) -> i32 {
    emit_cli_error(host, message);
    1
}
