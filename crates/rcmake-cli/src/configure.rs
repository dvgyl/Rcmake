use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use rcmake_base::{BString, truth::is_on};
use rcmake_commands::ConfigureRuntime;
use rcmake_diag::{Effect, MessageKind};
use rcmake_eval::{BuiltinFn, CmdError, Cx, Interp, Registry, RunMode, Services};
use rcmake_host::{FileKind, FileTime, Follow, Host, Stream, WriteMode, path};
use rcmake_instrument::Query;
use rcmake_json::Value;
use rcmake_model::{Cache, CacheType, GeneratorKind};

use crate::options::{self, DirectoryArg, Options};
use crate::persona::{
    DiagStyle, Persona, resolve_diag_style, resolve_rich_color, rich_width, stick_diag_style,
};
use crate::profile::ProfilePhase;
use crate::{cache, emit_cli_error, new_interp_with_paths, presets};

mod cache_glob;
mod cache_setup;
mod diagnostics;
mod graphviz;
mod preloads;
mod profiling;
mod restart;
mod speed;
#[cfg(test)]
mod tests;

use cache_glob::CacheGlob;
use graphviz::write_graphviz;
use preloads::Preloads;
use profiling::Profilers;
mod unused_cli;

pub(crate) use diagnostics::{
    MapSourceLookup, RichGenerateHost, initialize_diagnostics, render_rich_cmake_diagnostic,
    warn_legacy_define,
};

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
    list_file: BString,
    preset: presets::Application,
    profiling: Profilers,
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
    let start = host.monotonic();
    let (mut session, mut saved) = ConfigureSession::resolve(argv, opts, host)?;
    for attempt in 0..=1 {
        let selection = restart::selection(&saved);
        let (mut runtime, mut interp, has_cache) =
            session.initialize(saved, cmake_command.clone(), cmake_root.clone())?;
        interp.add_session_data(Arc::clone(&selection));
        let configure_start = session.host.monotonic();
        let time = session.host.now();
        let mut preloads = session.apply_cache_edits(&runtime, &mut interp);
        let mut results = std::mem::take(&mut preloads.results);
        let outcome = (|| {
            if runtime.denied() {
                return Err(Exit::Code(1));
            }
            if session.opts.view {
                session.list_cache(&mut interp);
                return Ok(());
            }
            if preloads.fatal {
                return Err(Exit::Code(preloads.exit_code));
            }
            if let Err(exit) = session.check_source() {
                // `cmake -L` lists the cache even when the configure step fails.
                session.list_cache(&mut interp);
                return Err(exit);
            }
            if preloads.missing {
                return Err(Exit::Code(1));
            }
            let configured = session.configure(
                &mut runtime,
                &mut interp,
                has_cache,
                configure_start,
                preloads,
                &selection,
            )?;
            session.generate(&mut interp, configured, &mut results)
        })();
        let changes = selection.take_changes();
        if !changes.is_empty() && !runtime.denied() {
            let _ = runtime.finish(false);
            if attempt != 0 {
                return Err(Exit::Message(
                    "The platform selection changed again during configure.".into(),
                ));
            }
            saved = restart::reset(&session, &mut interp, changes)?;
            continue;
        }
        let finish = runtime.finish(outcome.is_ok() && !session.opts.view);
        let outcome = match (outcome, finish) {
            (Ok(()), Some(error)) => {
                interp.cx().issue(
                    MessageKind::Error,
                    Effect::Fatal,
                    None,
                    error.to_string().into(),
                );
                Err(Exit::Code(1))
            }
            (outcome, _) => outcome,
        };
        let code = match &outcome {
            Ok(()) => 0,
            Err(Exit::Code(code)) => *code,
            Err(Exit::Message(_)) => 1,
        };
        let profile = session.profiling.write(session.host.as_ref(), start);
        session.write_sarif(&mut interp, results, time, code)?;
        profile?;
        return outcome;
    }
    Err(Exit::Code(1))
}

impl<'a> ConfigureSession<'a> {
    fn resolve(
        argv: &'a [BString],
        mut opts: Options,
        host: Arc<dyn Host>,
    ) -> Result<(Self, Cache), Exit> {
        let (mut src, mut bin) = resolve_directories(
            host.as_ref(),
            &opts.directories,
            opts.extensions.bare_profile_arg.as_deref(),
        );
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
        let mut saved = cache_setup::load(host.as_ref(), bin.as_deref().unwrap_or(&cwd))?;
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
                    saved = cache_setup::load(host.as_ref(), &dir)?;
                }
                bin = Some(dir);
            }
        }
        if !had_src && !had_bin && opts.preset.is_none() {
            options::warn(
                host.as_ref(),
                "No source or binary directory provided.  Both will be assumed to be the\n  same as the current working directory, but note that this warning will\n  become a fatal error in future CMake releases.",
            );
        }
        let bin = bin.unwrap_or(cwd);
        if opts.fresh && !opts.view {
            cache_setup::clear(host.as_ref(), &bin).map_err(|err| {
                Exit::Message(format!(
                    "Could not remove the cache in {}: {err}",
                    bin.display()
                ))
            })?;
            saved = Cache::default();
        }
        if !opts.view {
            cache_setup::apply_environment_identity(host.as_ref(), &bin, &mut saved)?;
            cache_setup::refresh_toolchain(
                host.as_ref(),
                &src,
                &bin,
                &opts.cache_edits,
                &mut saved,
            )?;
        }
        let list_file =
            cache_setup::select_list_file(host.as_ref(), opts.project_file.as_ref(), &saved)?;
        let profiling = Profilers::new(host.as_ref(), &opts, &bin);
        Ok((
            Self {
                argv,
                host,
                opts,
                src,
                bin,
                list_file,
                preset,
                profiling,
            },
            saved,
        ))
    }

    fn initialize(
        &self,
        saved: Cache,
        cmake_command: PathBuf,
        cmake_root: PathBuf,
    ) -> Result<(ConfigureRuntime, Interp, bool), Exit> {
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
        let (mut speed, persistent) = speed::settings(host, &self.opts, self.profiling.active());
        let style = resolve_diag_style(self.opts.persona, self.opts.extensions.diagnostics, host);
        if self.opts.persona == Persona::Native {
            stick_diag_style(host, style);
        }
        if speed.incremental
            && (saved.contains(b"CMAKE_TOOLCHAIN_FILE")
                || host.env_get(b"CMAKE_TOOLCHAIN_FILE").is_some()
                || [&self.src, &self.bin]
                    .into_iter()
                    .any(|dir| is_file(host, &dir.join("PreLoad.cmake"))))
        {
            speed.incremental = false;
        }
        // Rich formatting annotates live diagnostics during emission. Replay keeps its
        // normal eligibility checks for regions that issue diagnostics.
        let hermetic_active = speed.hermetic != rcmake_eval::HermeticMode::Off;
        let services = Services {
            speed,
            ..Services::default()
        };
        let runtime = ConfigureRuntime::new(
            Arc::clone(&self.host),
            &self.src,
            &self.bin,
            &services,
            persistent,
        );
        let mut interp = new_interp_with_paths(
            &self.opts,
            RunMode::Configure,
            runtime.host(),
            cmake_command,
            cmake_root,
            self.argv,
            services,
        );
        self.prepare_presentation(&mut interp, style);
        if hermetic_active {
            runtime.attach(&mut interp);
        }
        *interp.cx().cache() = saved;
        interp.cx().project().generator = generator_kind;
        cache_setup::initialize_standard_cache(
            &mut interp.cx(),
            host,
            generator,
            &generator_spec,
            &self.src,
            &self.bin,
            &self.list_file,
        );
        interp.cx().project().generator_spec = generator_spec;
        interp.set_list_file_name(self.list_file.clone());
        interp.set_project_injection(true);
        set_directory_vars(&mut interp, &self.src, &self.bin);
        Ok((runtime, interp, has_cache))
    }

    fn prepare_presentation(&self, interp: &mut Interp, style: DiagStyle) {
        self.profiling.attach(interp);
        if style == DiagStyle::Rich {
            let rich = rich_options(self.host.as_ref());
            interp.add_session_data(rich);
            interp.set_rich_diagnostics(rich);
        }
    }

    fn check_source(&self) -> Result<(), Exit> {
        let host = self.host.as_ref();
        let name = String::from_utf8_lossy(&self.list_file);
        if is_file(host, &self.src.join(name.as_ref())) {
            return Ok(());
        }
        let problem = if is_dir(host, &self.src) {
            format!("does not appear to contain {name}")
        } else {
            "does not exist".into()
        };
        let hint = if self.opts.persona == crate::persona::Persona::Native {
            "Specify --help for usage."
        } else {
            "Specify --help for usage, or press the help button on the CMake GUI."
        };
        let profile_hint =
            if self.opts.extensions.bare_profile_arg.as_deref() == Some(self.src.as_path()) {
                "\nHint: pass a profile output directory with --profile=<dir>."
            } else {
                ""
            };
        Err(Exit::Message(format!(
            "The source directory \"{}\" {problem}.\n{hint}{profile_hint}",
            self.src.display()
        )))
    }

    fn configure(
        &self,
        runtime: &mut rcmake_commands::ConfigureRuntime,
        interp: &mut Interp,
        has_cache: bool,
        start: Duration,
        preloads: Preloads,
        selection: &rcmake_commands::ConfigureSelection,
    ) -> Result<Configured, Exit> {
        let host = self.host.as_ref();
        // The system module loads the toolchain when project() starts platform setup.
        cache_setup::apply_environment_defaults(host, interp.cx().cache(), has_cache);
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
        let result = self.profiling.measure(host, ProfilePhase::Eval, || {
            self.profiling.measure_listfile(
                host,
                &self.src.join(path::to_native(&self.list_file)),
                || runtime.run(interp, &[]),
            )
        });
        if selection.changed() {
            return Err(Exit::Code(1));
        }
        let codegen_error = result.exit_code == 0
            && rcmake_resolve::validate_codegen_name(interp.cx().project(), host).is_err();
        if codegen_error && self.show_status() {
            options::out(host, "-- Configuring incomplete, errors occurred!\n");
        }
        let exit_code = if codegen_error {
            1
        } else if result.exit_code == 0 {
            preloads.exit_code
        } else {
            result.exit_code
        };
        let directories = interp.cx().project().dirs().count();
        cache::insert_internal(
            interp.cx().cache(),
            b"CMAKE_NUMBER_OF_MAKEFILES",
            directories.to_string().into(),
            "number of local generators",
        );
        let cache_result = cache::save(host, &self.bin, interp.cx().cache());
        if let Err(error) = cache_result {
            emit_cli_error(host, format!("Could not write CMakeCache.txt: {error}"));
            if exit_code == 0 {
                return Err(Exit::Code(1));
            }
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
                    &crate::CommandExit::Code(exit_code),
                )
                .map_err(Exit::message)?;
        }
        if result.exit_code == 0 && !codegen_error && self.show_status() {
            let elapsed = host.monotonic().saturating_sub(start);
            options::out(
                host,
                &format!("-- Configuring done ({:.1}s)\n", elapsed.as_secs_f64()),
            );
        }
        if exit_code != 0 {
            if let Err(error) = rcmake_gen_common::fileapi::write_error_replies(
                interp.cx().project(),
                host,
                &self.bin,
            ) {
                emit_cli_error(host, error.to_string());
            }
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
        if let Some(message) = diagnostics::unsupported_config_variable(interp) {
            host.write_out(Stream::Stderr, message.as_bytes());
            generation_failed(host);
            return Err(Exit::Code(1));
        }
        let start = host.monotonic();
        let generate_step = crate::InstrumentationStep::start(host, sample);
        let result = self.profiling.measure(host, ProfilePhase::Generate, || {
            crate::generate::run(interp, host, &self.bin, &queries)
        });
        use crate::generate::Outcome;
        let outcome = match result {
            Ok(outcome) => outcome,
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
        if !matches!(outcome, Outcome::Unsupported) && show_status {
            options::out(
                host,
                &format!("-- Generating done ({:.1}s)\n", elapsed.as_secs_f64()),
            );
        }
        let generated = matches!(outcome, Outcome::Complete);
        if unused_cli::report(host, interp, results) || matches!(outcome, Outcome::DeferredError) {
            generation_failed(host);
            return Err(Exit::Code(1));
        }
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
            arguments: self.argv,
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

fn resolve_directories(
    host: &dyn Host,
    args: &[DirectoryArg],
    bare_profile_arg: Option<&Path>,
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
            let profile_hint = if bare_profile_arg == Some(path) {
                "\n\n  Hint: pass a profile output directory with --profile=<dir>."
            } else {
                ""
            };
            options::warn(
                host,
                &format!(
                    "Ignoring extra path from command line:\n\n   \"{}{suffix}\"{profile_hint}",
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

/// Remove the non-`STATIC` cache entries that the `-U` glob `pattern` matches.
pub(crate) fn remove_cache_glob(cache: &mut Cache, pattern: &[u8]) {
    let pattern = CacheGlob::new(pattern);
    let names: Vec<_> = cache
        .iter()
        .filter(|(name, entry)| entry.ty != CacheType::Static && pattern.matches(name))
        .map(|(name, _)| name.clone())
        .collect();
    for name in &names {
        cache.remove(name);
    }
}

/// Remove the non-`STATIC` cache entries that `pattern` matches and return
/// their names.
fn remove_entries(
    interp: &mut Interp,
    pattern: &CacheGlob,
    tracked: &mut BTreeSet<BString>,
) -> Vec<BString> {
    let mut cx = interp.cx();
    let names: Vec<_> = cx
        .cache()
        .iter()
        .filter(|(name, entry)| entry.ty != CacheType::Static && pattern.matches(name))
        .map(|(name, _)| name.clone())
        .collect();
    for name in &names {
        cx.cache().remove(name);
        tracked.remove(name);
    }
    interp.untrack_variable_reads(names.iter().cloned());
    names
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

/// Rich diagnostic settings for standard error on `host`.
pub(crate) fn rich_options(host: &dyn Host) -> rcmake_diag::rich::RichOptions {
    rcmake_diag::rich::RichOptions {
        color: resolve_rich_color(host, host.is_terminal(Stream::Stderr), DiagStyle::Rich),
        terminal_width: rich_width(host),
        ..rcmake_diag::rich::RichOptions::default()
    }
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

#[cfg(test)]
#[test]
fn failed_configure_publishes_a_file_api_error_index() -> rcmake_host::HostResult<()> {
    use rcmake_host::{PureHost, WriteMode};
    use std::sync::Arc;

    let work = Path::new(if cfg!(windows) { "C:/work" } else { "/work" });
    let host = Arc::new(PureHost::new(work.into(), work.join("bin/cmake")));
    host.seed_file(
        &work.join("CMakeLists.txt"),
        br#"
cmake_minimum_required(VERSION 4.5)
project(Failure NONE)
message(FATAL_ERROR "Cannot configure this project.")
"#,
    )?;
    let root = work.join("data");
    host.seed_file(&root.join("Modules/CMakeDetermineSystem.cmake"), b"")?;
    host.env_set(
        b"RCMAKE_VENDORED_DATA_ROOT",
        Some(&rcmake_host::path::from_native_cmake(&root)),
    );
    let api = work.join("build/.cmake/api/v1");
    let query = api.join("query");
    host.create_dir_all(&query)?;
    host.write(&query.join("configureLog-v1"), b"", WriteMode::CreateNew)?;

    let argv = ["cmake", "-S", ".", "-B", "build", "-G", "Unix Makefiles"].map(BString::from);
    assert_eq!(crate::run(&argv, host.clone()), 1);
    assert!(
        host.read_dir(&api.join("reply"))?
            .iter()
            .any(|entry| entry.name.starts_with(b"error-"))
    );
    Ok(())
}
