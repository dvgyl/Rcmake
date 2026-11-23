//! CTest dashboard script execution.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use rcmake_base::{BString, Limits, list, path_lex::Lexical};
use rcmake_eval::{
    CmdError, CmdFlags, CmdResult, Cx, Flow, IncludeOptions, Interp, InterpConfig, LogLevel,
    Registry, RunMode,
};
use rcmake_expr::ExpandedArg;
use rcmake_host::{
    CommandSpec, FileKind, Follow, Host, Input, Output, ProcessResult, RunOutput, RunRequest,
    Stream, path,
};

use crate::cli::{Invocation, Script};
use crate::dart::{self, Configuration, Dashboard, Model, Tag};
use crate::{CtestError, CtestOptions, run_tests_with_dashboard};

const SCRIPTABLE: CmdFlags = CmdFlags { scriptable: true };
const DEPTH_KEY: &[u8] = b"RCMAKE_CTEST_SCRIPT_DEPTH";
const MAX_SCRIPT_DEPTH: usize = 32;
const PROCESS_TIMEOUT: Duration = Duration::from_secs(30 * 60);

#[derive(Clone)]
struct Settings {
    ctest: BString,
    verbose: bool,
    extra_verbose: bool,
    quiet: bool,
    depth: usize,
    config: Option<BString>,
    definitions: BTreeMap<BString, BString>,
}

struct Session {
    host: Arc<dyn Host>,
    settings: Settings,
    start: Option<Start>,
}

#[derive(Clone)]
struct Start {
    source: PathBuf,
    build: PathBuf,
    tag: Tag,
}

thread_local! {
    static SESSION: RefCell<Option<Session>> = const { RefCell::new(None) };
}

struct Scope(Option<Session>);

impl Drop for Scope {
    fn drop(&mut self) {
        SESSION.replace(self.0.take());
    }
}

pub(crate) fn run_script(
    host: &Arc<dyn Host>,
    invocation: &Invocation,
    script: &Script,
) -> Result<i32, CtestError> {
    let exe = host
        .current_exe()
        .unwrap_or_else(|_| host.initial_cwd().join("ctest"));
    let ctest = exe.with_file_name(if cfg!(windows) { "ctest.exe" } else { "ctest" });
    let settings = Settings {
        ctest: path::from_native(&ctest),
        verbose: invocation.options.verbose,
        extra_verbose: invocation.output.verbosity == Some(rcmake_presets::TestVerbosity::Extra),
        quiet: invocation.options.quiet,
        depth: host
            .env_get(DEPTH_KEY)
            .and_then(|n| n.to_string().parse().ok())
            .unwrap_or_default(),
        config: invocation.options.config.clone(),
        definitions: invocation
            .definitions
            .iter()
            .filter_map(|definition| {
                let (key, value) = dart::split(definition, b'=')?;
                Some((key.into(), value.into()))
            })
            .collect(),
    };
    execute_script(host, settings, &script.argument, script.new_process)
}

fn execute_script(
    host: &Arc<dyn Host>,
    settings: Settings,
    argument: &[u8],
    new_process: bool,
) -> Result<i32, CtestError> {
    if settings.depth >= MAX_SCRIPT_DEPTH {
        return Err(CtestError::Schedule(
            "Dashboard script nesting limit exceeded.".into(),
        ));
    }
    let (file, arg) = dart::split(argument, b',').unwrap_or((argument, b""));
    let cwd = host.initial_cwd();
    let file = absolute(&cwd, file);
    if new_process {
        let mut argument = path::from_native(&file);
        if !arg.is_empty() {
            argument.push(b',');
            argument.extend_from_slice(arg);
        }
        let mut args = vec![BString::from("-S"), argument];
        if settings.quiet {
            args.push("-Q".into());
        } else if settings.extra_verbose {
            args.push("-VV".into());
        } else if settings.verbose {
            args.push("-V".into());
        }
        if let Some(config) = settings.config {
            args.extend(["-C".into(), config]);
        }
        for (key, value) in settings.definitions {
            args.push(format!("-D{key}={value}").into());
        }
        let mut env: BTreeMap<_, _> = host.env_all().into_iter().collect();
        env.insert(DEPTH_KEY.into(), (settings.depth + 1).to_string().into());
        let output = run_process(
            host.as_ref(),
            cwd,
            settings.ctest,
            args,
            env.into_iter().collect(),
            !settings.quiet,
        )
        .map_err(|source| CtestError::Access { path: file, source })?;
        return Ok(process_code(&output).unwrap_or(1));
    }

    if !host
        .metadata(&file, Follow::Yes)
        .is_ok_and(|metadata| metadata.kind == FileKind::File)
    {
        host.write_out(
            Stream::Stderr,
            format!("CMake Error: Cannot find file: {}\n", file.display()).as_bytes(),
        );
        return Ok(1);
    }
    let exe = host.current_exe().unwrap_or_else(|_| cwd.join("cmake"));
    let candidate = exe.with_file_name(if cfg!(windows) { "cmake.exe" } else { "cmake" });
    let cmake_command = if host.metadata(&candidate, Follow::Yes).is_ok()
        || exe.file_stem().is_some_and(|name| name == "ctest")
    {
        candidate
    } else {
        exe
    };
    let cmake_root = host
        .env_get(b"RCMAKE_VENDORED_DATA_ROOT")
        .map(|root| absolute(&cwd, &root))
        .or_else(|| Some(cmake_command.parent()?.parent()?.join("share/cmake-4.5")))
        .unwrap_or_default();
    let mut interp = Interp::new(
        InterpConfig {
            host: Arc::clone(host),
            mode: RunMode::Script,
            cmake_root,
            cmake_command,
            cwd,
            limits: Limits {
                max_recursion: 128,
                ..Limits::default()
            },
            diag: rcmake_diag::DiagOptions::default(),
            defines: Vec::new(),
            script_args: Vec::new(),
        },
        register,
    );
    if settings.quiet || !settings.verbose {
        interp.set_log_level(Some(LogLevel::Notice));
    }
    let mut cx = interp.cx();
    cx.set_var(b"CMAKE_CTEST_COMMAND", settings.ctest.clone());
    cx.set_var(
        b"CTEST_SCRIPT_DIRECTORY",
        path::from_native(file.parent().unwrap_or(&file)),
    );
    cx.set_var(
        b"CTEST_SCRIPT_NAME",
        file.file_name()
            .map_or_else(BString::default, |name| path::from_native(Path::new(name))),
    );
    if !arg.is_empty() {
        cx.set_var(b"CTEST_SCRIPT_ARG", arg.into());
    }
    if let Some(config) = &settings.config {
        cx.set_var(b"CTEST_CONFIGURATION_TYPE", config.clone());
    }
    for (key, value) in &settings.definitions {
        cx.set_var(key, value.clone());
    }
    cx.set_var(b"CTEST_ELAPSED_TIME", "0".into());
    let _scope = Scope(SESSION.replace(Some(Session {
        host: Arc::clone(host),
        settings,
        start: None,
    })));
    let code = interp.run_script(&file).exit_code;
    Ok(if code == 1 && interp.diag_engine().error_occurred() {
        255
    } else {
        code
    })
}

fn register(registry: &mut Registry) {
    rcmake_commands::register_all(registry);
    for (name, command) in [
        ("ctest_start", ctest_start as rcmake_eval::BuiltinFn),
        ("ctest_configure", ctest_configure),
        ("ctest_build", ctest_build),
        ("ctest_test", ctest_test),
        ("ctest_empty_binary_directory", ctest_empty_binary_directory),
        ("ctest_run_script", ctest_run_script),
    ] {
        registry.add(name, command, SCRIPTABLE);
    }
}

fn absolute(cwd: &Path, raw: &[u8]) -> PathBuf {
    let file = path::from_native(&cwd.join(path::to_native(raw)));
    path::to_native(&Lexical::new(&file).normalize())
}

fn error(message: impl Into<BString>) -> CmdError {
    CmdError::Status(message.into())
}

fn prior_start() -> Option<Start> {
    SESSION.with_borrow(|session| session.as_ref()?.start.clone())
}

fn configuration(
    cx: &Cx<'_>,
    build: &Path,
    source: Option<&Path>,
) -> Result<Configuration, CmdError> {
    let host = cx.host();
    let selected = [Some(build), source]
        .into_iter()
        .flatten()
        .find(|dir| {
            ["CTestConfiguration.ini", "DartConfiguration.tcl"]
                .iter()
                .any(|name| host.metadata(&dir.join(name), Follow::Yes).is_ok())
        })
        .unwrap_or(build);
    let mut config =
        Configuration::load(host, selected, &[]).map_err(|err| error(err.to_string()))?;
    for (key, value) in [
        (b"CTEST_SITE".as_slice(), &mut config.site),
        (b"CTEST_BUILD_NAME", &mut config.build_name),
        (b"CTEST_NIGHTLY_START_TIME", &mut config.nightly_start),
    ] {
        if cx.var_defined(key) {
            *value = cx.var_or_empty(key);
        }
    }
    Ok(config)
}

fn ctest_start(cx: &mut Cx<'_>, args: &[ExpandedArg]) -> CmdResult {
    let mut append = false;
    let mut quiet = false;
    let mut track = None;
    let mut positional = Vec::new();
    let mut args = args.iter().map(|arg| &arg.value);
    while let Some(arg) = args.next() {
        match arg.as_slice() {
            b"APPEND" => append = true,
            b"QUIET" => quiet = true,
            b"TRACK" | b"GROUP" => {
                track = Some(
                    args.next()
                        .filter(|value| {
                            !matches!(value.as_slice(), b"APPEND" | b"QUIET" | b"TRACK" | b"GROUP")
                        })
                        .ok_or_else(|| error(format!("{arg} argument missing group name")))?
                        .to_string(),
                );
            }
            _ => positional.push(arg),
        }
    }
    let model = positional.first().and_then(|model| Model::parse(model));
    let paths = positional
        .get(usize::from(model.is_some())..)
        .unwrap_or_default();
    if model.is_none() && !append {
        return Err(error(
            "no test model specified and APPEND not specified.  Specify either a test model or the APPEND argument",
        ));
    }
    if paths.len() > 2 {
        return Err(error("Too many arguments"));
    }
    let previous = prior_start();
    let source = directory(
        cx,
        paths.first().copied(),
        b"CTEST_SOURCE_DIRECTORY",
        previous.as_ref().map(|start| start.source.as_path()),
    )?;
    let build = directory(
        cx,
        paths.get(1).copied(),
        b"CTEST_BINARY_DIRECTORY",
        previous.as_ref().map(|start| start.build.as_path()),
    )?;
    if !cx
        .host()
        .metadata(&source, Follow::Yes)
        .is_ok_and(|meta| meta.kind == FileKind::Dir)
    {
        return Err(error(format!(
            "given source path \n\n  {}\n\nwhich is not an existing directory.",
            source.display()
        )));
    }
    for directory in [&build, &source] {
        let config = directory.join("CTestConfig.cmake");
        if cx.host().metadata(&config, Follow::Yes).is_ok() {
            cx.include_file(&config, IncludeOptions::default())?;
            break;
        }
    }
    let config = configuration(cx, &build, Some(&source))?;
    let host = cx.host();
    let log = !quiet && verbose();
    if log {
        let name = if config.build_name.is_empty() {
            "(empty)".into()
        } else {
            config.build_name.to_string()
        };
        let model = model.map_or("with to-be-determined model".into(), |model| {
            format!("with model {}", model.name())
        });
        let group = track
            .as_ref()
            .map_or_else(String::new, |track| format!("   Group: {track}\n"));
        let message = format!(
            "Run dashboard {model}\n   Source directory: {}\n   Build directory: {}\n{group}   Site: {}\n   Build name: {name}\n",
            source.display(),
            build.display(),
            config.site
        );
        host.write_out(Stream::Stdout, message.as_bytes());
    }
    let tag = if append {
        let mut tag =
            Tag::read_with_model(host, &build, model).map_err(|err| error(err.to_string()))?;
        if model.is_some_and(|model| model != tag.model) {
            host.write_out(
                Stream::Stderr,
                b"Model given in TAG does not match model given in ctest_start()\n",
            );
        }
        if let Some(track) = track.filter(|track| !track.is_empty() && *track != tag.track) {
            host.write_out(
                Stream::Stderr,
                b"Group given in TAG does not match group given in ctest_start()\n",
            );
            tag.track = track;
        }
        if log {
            host.write_out(
                Stream::Stdout,
                format!("  Use existing tag: {} - {}\n", tag.stamp(), tag.track).as_bytes(),
            );
        }
        tag
    } else {
        Tag::create(
            host,
            &build,
            host.now(),
            model.unwrap_or(Model::Experimental),
            track.as_deref(),
            &config.nightly_start,
        )
        .map_err(|err| error(err.to_string()))?
    };
    if log {
        host.write_out(
            Stream::Stdout,
            format!("   Use {} tag: {}\n", tag.track, tag.stamp()).as_bytes(),
        );
    }
    SESSION.with_borrow_mut(|session| {
        if let Some(session) = session {
            session.start = Some(Start { source, build, tag });
        }
    });
    Ok(Flow::Normal)
}

fn directory(
    cx: &Cx<'_>,
    arg: Option<&BString>,
    var: &[u8],
    prior: Option<&Path>,
) -> Result<PathBuf, CmdError> {
    let value = arg.cloned().unwrap_or_else(|| cx.var_or_empty(var));
    if !value.is_empty() {
        return Ok(absolute(&cx.host().initial_cwd(), &value));
    }
    prior
        .map(Path::to_path_buf)
        .ok_or_else(|| error(format!("requires {}", String::from_utf8_lossy(var))))
}

fn verbose() -> bool {
    SESSION.with_borrow(|session| {
        session
            .as_ref()
            .is_some_and(|session| session.settings.verbose && !session.settings.quiet)
    })
}

fn extra_verbose() -> bool {
    SESSION.with_borrow(|session| {
        session
            .as_ref()
            .is_some_and(|session| session.settings.extra_verbose && !session.settings.quiet)
    })
}

struct Arguments<'a>(BTreeMap<&'a [u8], Vec<&'a BString>>);

impl<'a> Arguments<'a> {
    fn parse(args: &'a [ExpandedArg], keys: &[&[u8]], flags: &[&[u8]]) -> Result<Self, CmdError> {
        let common = [b"BUILD".as_slice(), b"RETURN_VALUE", b"CAPTURE_CMAKE_ERROR"];
        let takes_value = |key: &[u8]| common.contains(&key) || keys.contains(&key);
        let is_flag = |key: &[u8]| key == b"QUIET" || flags.contains(&key);
        let mut args = args.iter().map(|arg| &arg.value).peekable();
        let mut parsed = BTreeMap::new();
        while let Some(key) = args.next() {
            let key = key.as_slice();
            let mut values = Vec::new();
            if takes_value(key) {
                while let Some(value) = args.next_if(|value| !takes_value(value) && !is_flag(value))
                {
                    values.push(value);
                    if !matches!(key, b"INCLUDE_LABEL" | b"EXCLUDE_LABEL") {
                        break;
                    }
                }
                if values.is_empty() {
                    return Err(error(format!(
                        "requires a value for {}",
                        String::from_utf8_lossy(key)
                    )));
                }
            } else if !is_flag(key) {
                return Err(error(format!(
                    "called with unknown argument \"{}\"",
                    String::from_utf8_lossy(key)
                )));
            }
            if parsed.insert(key, values).is_some() {
                return Err(error(format!(
                    "called with duplicate argument \"{}\"",
                    String::from_utf8_lossy(key)
                )));
            }
        }
        Ok(Self(parsed))
    }

    fn value(&self, key: &[u8]) -> Option<&'a BString> {
        self.0.get(key)?.first().copied()
    }

    fn labels(&self, key: &[u8]) -> Vec<BString> {
        self.0
            .get(key)
            .into_iter()
            .flatten()
            .copied()
            .filter(|value| !value.is_empty())
            .cloned()
            .collect()
    }

    fn flag(&self, key: &[u8]) -> bool {
        self.0.contains_key(key)
    }

    fn set(&self, cx: &mut Cx<'_>, key: &[u8], value: impl Into<BString>) {
        if let Some(var) = self.value(key).filter(|name| !name.is_empty()) {
            cx.set_var(var, value.into());
        }
    }
}

struct HandlerResult {
    code: i32,
    capture_error: bool,
}

fn with_capture(
    cx: &mut Cx<'_>,
    args: &[ExpandedArg],
    keys: &[&[u8]],
    flags: &[&[u8]],
    run: impl FnOnce(&mut Cx<'_>, &Arguments<'_>) -> Result<HandlerResult, CmdError>,
) -> CmdResult {
    let result = Arguments::parse(args, keys, flags).and_then(|args| {
        let result = run(cx, &args)?;
        args.set(cx, b"RETURN_VALUE", result.code.to_string());
        args.set(
            cx,
            b"CAPTURE_CMAKE_ERROR",
            if result.capture_error { "-1" } else { "0" },
        );
        if result.capture_error && !args.flag(b"CAPTURE_CMAKE_ERROR") {
            return Err(error("could not complete the dashboard command"));
        }
        Ok(Flow::Normal)
    });
    if let Err(CmdError::Status(message)) = &result
        && let Some([_, var]) = args.windows(2).find(|pair| {
            pair.first()
                .is_some_and(|arg| arg.value == "CAPTURE_CMAKE_ERROR")
        })
    {
        cx.host()
            .write_out(Stream::Stderr, format!("{message}\n").as_bytes());
        cx.set_var(&var.value, "-1".into());
        return Ok(Flow::Normal);
    }
    result
}

fn ctest_configure(cx: &mut Cx<'_>, args: &[ExpandedArg]) -> CmdResult {
    with_capture(
        cx,
        args,
        &[b"SOURCE", b"OPTIONS"],
        &[b"APPEND"],
        |cx, args| {
            let previous = prior_start();
            let build = directory(
                cx,
                args.value(b"BUILD"),
                b"CTEST_BINARY_DIRECTORY",
                previous.as_ref().map(|start| start.build.as_path()),
            )?;
            let custom = cx.var_or_empty(b"CTEST_CONFIGURE_COMMAND");
            let command = if custom.is_empty() {
                let source = directory(
                    cx,
                    args.value(b"SOURCE"),
                    b"CTEST_SOURCE_DIRECTORY",
                    previous.as_ref().map(|start| start.source.as_path()),
                )?;
                if !cx
                    .host()
                    .metadata(&source.join("CMakeLists.txt"), Follow::Yes)
                    .is_ok_and(|meta| meta.kind == FileKind::File)
                {
                    return Err(error(
                        "requires a source directory that contains CMakeLists.txt",
                    ));
                }
                let generator = cx.var_or_empty(b"CTEST_CMAKE_GENERATOR");
                if generator.is_empty() {
                    return Err(error(
                        "requires CTEST_CMAKE_GENERATOR or CTEST_CONFIGURE_COMMAND",
                    ));
                }
                let mut command = vec![
                    cx.var_or_empty(b"CMAKE_COMMAND"),
                    [b"-S", path::from_native(&source).as_slice()]
                        .concat()
                        .into(),
                    [b"-B", path::from_native(&build).as_slice()]
                        .concat()
                        .into(),
                    [b"-G", generator.as_slice()].concat().into(),
                ];
                if let Some(options) = args.value(b"OPTIONS") {
                    command.extend(list::split(options, list::Empty::Drop));
                }
                for (option, var) in [
                    ("-A", b"CTEST_CMAKE_GENERATOR_PLATFORM".as_slice()),
                    ("-T", b"CTEST_CMAKE_GENERATOR_TOOLSET"),
                ] {
                    let value = cx.var_or_empty(var);
                    if !value.is_empty() {
                        command.extend([option.into(), value]);
                    }
                }
                command
            } else {
                split_command(cx, &custom)?
            };
            cx.host()
                .create_dir_all(&build)
                .map_err(|err| error(err.to_string()))?;
            if verbose() && !args.flag(b"QUIET") {
                cx.host().write_out(Stream::Stdout, b"Configure project\n");
            }
            let output = handler_process(cx, args, build, command)?;
            let code = process_code(&output).unwrap_or(-1);
            Ok(HandlerResult {
                code,
                capture_error: code != 0,
            })
        },
    )
}

fn ctest_build(cx: &mut Cx<'_>, args: &[ExpandedArg]) -> CmdResult {
    with_capture(
        cx,
        args,
        &[
            b"TARGET",
            b"CONFIGURATION",
            b"FLAGS",
            b"NUMBER_ERRORS",
            b"NUMBER_WARNINGS",
        ],
        &[b"APPEND"],
        |cx, args| {
            let previous = prior_start();
            let build = directory(
                cx,
                args.value(b"BUILD"),
                b"CTEST_BINARY_DIRECTORY",
                previous.as_ref().map(|start| start.build.as_path()),
            )?;
            let custom = cx.var_or_empty(b"CTEST_BUILD_COMMAND");
            let command = if custom.is_empty() {
                let config = args
                    .value(b"CONFIGURATION")
                    .cloned()
                    .unwrap_or_else(|| cx.var_or_empty(b"CTEST_BUILD_CONFIGURATION"));
                let config = if config.is_empty() {
                    "Release".into()
                } else {
                    config
                };
                let mut command = vec![
                    cx.var_or_empty(b"CMAKE_COMMAND"),
                    "--build".into(),
                    ".".into(),
                    "--config".into(),
                    config,
                ];
                let target = args
                    .value(b"TARGET")
                    .cloned()
                    .unwrap_or_else(|| cx.var_or_empty(b"CTEST_BUILD_TARGET"));
                if !target.is_empty() {
                    command.extend(["--target".into(), target]);
                }
                let flags = args
                    .value(b"FLAGS")
                    .cloned()
                    .unwrap_or_else(|| cx.var_or_empty(b"CTEST_BUILD_FLAGS"));
                if !flags.is_empty() {
                    command.push("--".into());
                    command.extend(split_command(cx, &flags)?);
                }
                command
            } else {
                split_command(cx, &custom)?
            };
            let log = verbose() && !args.flag(b"QUIET");
            if log {
                cx.host().write_out(Stream::Stdout, b"Build project\n");
            }
            let output = handler_process(cx, args, build, command)?;
            let mut warnings = 0;
            let mut errors = 0;
            for line in output
                .stdout
                .split(|&byte| byte == b'\n')
                .chain(output.stderr.split(|&byte| byte == b'\n'))
            {
                let text = String::from_utf8_lossy(line).to_ascii_lowercase();
                errors += usize::from(text.contains("error:") || text.contains(" error "));
                warnings += usize::from(text.contains("warning:") || text.contains(" warning "));
            }
            let code = process_code(&output).unwrap_or(-1);
            args.set(cx, b"NUMBER_ERRORS", errors.to_string());
            let warnings = warnings + usize::from(code != 0);
            args.set(cx, b"NUMBER_WARNINGS", warnings.to_string());
            if log {
                cx.host().write_out(
                    Stream::Stdout,
                    format!("   {errors} Compiler errors\n   {warnings} Compiler warnings\n")
                        .as_bytes(),
                );
            }
            Ok(HandlerResult {
                code,
                capture_error: code != 0 || errors > 0,
            })
        },
    )
}

fn split_command(cx: &mut Cx<'_>, command: &[u8]) -> Result<Vec<BString>, CmdError> {
    const VAR: &[u8] = b"_RCMAKE_CTEST_COMMAND_LINE";
    let previous = cx.var_normal_only(VAR);
    cx.set_var(VAR, command.into());
    let result = cx.eval_code(b"separate_arguments(_RCMAKE_CTEST_COMMAND_LINE NATIVE_COMMAND \"${_RCMAKE_CTEST_COMMAND_LINE}\")");
    let value = cx.var_or_empty(VAR);
    if let Some(previous) = previous {
        cx.set_var(VAR, previous);
    } else {
        cx.unset_var(VAR);
    }
    result?;
    Ok(list::split(&value, list::Empty::Keep))
}

fn handler_process(
    cx: &Cx<'_>,
    args: &Arguments<'_>,
    build: PathBuf,
    command: Vec<BString>,
) -> Result<RunOutput, CmdError> {
    let mut command = command.into_iter();
    let program = command
        .next()
        .filter(|program| !program.is_empty())
        .ok_or_else(|| error("has an empty command"))?;
    let host = cx.host();
    run_process(
        host,
        build,
        program,
        command.collect(),
        host.env_all(),
        extra_verbose() && !args.flag(b"QUIET"),
    )
    .map_err(|err| error(err.to_string()))
}

fn run_process(
    host: &dyn Host,
    cwd: PathBuf,
    program: BString,
    args: Vec<BString>,
    env: Vec<(BString, BString)>,
    tee: bool,
) -> rcmake_host::HostResult<RunOutput> {
    host.run(
        &RunRequest {
            pipeline: vec![CommandSpec { program, args }],
            cwd,
            env,
            stdin: Input::Null,
            stdout: Output::Capture,
            stderr: Output::Capture,
            timeout: Some(PROCESS_TIMEOUT),
            tee,
        },
        &mut |stream, bytes| host.write_out(stream, bytes),
    )
}

fn process_code(output: &RunOutput) -> Option<i32> {
    match output.results.last()? {
        ProcessResult::Exited(code) => Some(*code),
        _ => None,
    }
}

fn ctest_test(cx: &mut Cx<'_>, args: &[ExpandedArg]) -> CmdResult {
    with_capture(
        cx,
        args,
        &[
            b"INCLUDE",
            b"EXCLUDE",
            b"INCLUDE_LABEL",
            b"EXCLUDE_LABEL",
            b"PARALLEL_LEVEL",
            b"OUTPUT_JUNIT",
            b"REPEAT",
            b"TEST_LOAD",
        ],
        &[b"STOP_ON_FAILURE", b"APPEND"],
        |cx, args| {
            let previous = prior_start();
            let build = directory(
                cx,
                args.value(b"BUILD"),
                b"CTEST_BINARY_DIRECTORY",
                previous.as_ref().map(|start| start.build.as_path()),
            )?;
            let (host, settings) = SESSION
                .with_borrow(|session| {
                    session
                        .as_ref()
                        .map(|session| (Arc::clone(&session.host), session.settings.clone()))
                })
                .ok_or_else(|| error("requires a dashboard script session"))?;
            let config = [
                b"CTEST_CONFIGURATION_TYPE".as_slice(),
                b"CTEST_BUILD_CONFIGURATION",
            ]
            .into_iter()
            .map(|var| cx.var_or_empty(var))
            .find(|value| !value.is_empty())
            .or(settings.config);
            let timeout = crate::execution::parse_timeout(&cx.var_or_empty(b"CTEST_TEST_TIMEOUT"));
            let parallel = args
                .value(b"PARALLEL_LEVEL")
                .cloned()
                .or_else(|| host.env_get(b"CTEST_PARALLEL_LEVEL"));
            let jobs = match parallel.as_ref().map(|value| value.as_slice()) {
                None => 1,
                Some(b"") => host.system().cpu_count.max(2) as usize,
                Some(value) => {
                    let jobs = parse_number(value).ok_or_else(|| {
                        error("requires a non-negative integer for PARALLEL_LEVEL")
                    })?;
                    if jobs == 0 { usize::MAX } else { jobs }
                }
            };
            let test_load = args
                .value(b"TEST_LOAD")
                .cloned()
                .unwrap_or_else(|| cx.var_or_empty(b"CTEST_TEST_LOAD"));
            let mut invocation = Invocation {
                options: CtestOptions {
                    test_dir: build.clone(),
                    config,
                    include: args
                        .value(b"INCLUDE")
                        .cloned()
                        .filter(|value| !value.is_empty()),
                    exclude: args
                        .value(b"EXCLUDE")
                        .cloned()
                        .filter(|value| !value.is_empty()),
                    include_labels: args.labels(b"INCLUDE_LABEL"),
                    exclude_labels: args.labels(b"EXCLUDE_LABEL"),
                    jobs,
                    quiet: !verbose(),
                    verbose: extra_verbose(),
                    stop_on_failure: args.flag(b"STOP_ON_FAILURE"),
                    timeout,
                    ..CtestOptions::default()
                },
                test_load: parse_number(&test_load).map(|load| load as u64),
                ..Invocation::default()
            };
            invocation.output.output_junit_file = args.value(b"OUTPUT_JUNIT").cloned();
            if let Some(repeat) = args.value(b"REPEAT") {
                let mut repeat = repeat.to_ascii_lowercase();
                for byte in &mut repeat {
                    if *byte == b'_' {
                        *byte = b'-';
                    }
                }
                invocation.execution.repeat =
                    Some(crate::cli::parse_repeat(b"--repeat", &repeat).map_err(error)?);
            }
            let configuration = configuration(
                cx,
                &build,
                previous.as_ref().map(|start| start.source.as_path()),
            )?;
            let tag = match previous.filter(|start| start.build == build) {
                Some(start) => start.tag,
                None => Tag::read(host.as_ref(), &build).map_err(|err| error(err.to_string()))?,
            };
            let dashboard = Dashboard {
                directory: build,
                tag,
                configuration,
                show_project: !args.flag(b"QUIET"),
            };
            let run =
                run_tests_with_dashboard(Arc::clone(&host), &invocation, true, Some(dashboard))
                    .map_err(|err| error(err.to_string()))?;
            for message in &run.not_run {
                host.write_out(Stream::Stderr, message);
                host.write_out(Stream::Stderr, b"\n");
            }
            Ok(HandlerResult {
                code: if run.outcome.exit_code == 0 { 0 } else { -1 },
                capture_error: !run.not_run.is_empty(),
            })
        },
    )
}

fn parse_number(value: &[u8]) -> Option<usize> {
    std::str::from_utf8(value).ok()?.trim().parse().ok()
}

fn ctest_empty_binary_directory(cx: &mut Cx<'_>, args: &[ExpandedArg]) -> CmdResult {
    let [dir] = args else {
        return Err(error("requires exactly one directory"));
    };
    if dir.value.is_empty() {
        return Err(error("requires a binary directory"));
    }
    let host = cx.host();
    let dir = absolute(&host.initial_cwd(), &dir.value);
    let canonical = match host.canonicalize(&dir) {
        Ok(path) => path,
        Err(err) if err.kind == ErrorKind::NotFound => return Ok(Flow::Normal),
        Err(err) => return Err(error(err.to_string())),
    };
    if canonical != dir || canonical.parent().is_none() {
        return Err(error(
            "requires a non-root directory without symbolic links",
        ));
    }
    if !host
        .metadata(&dir.join("CMakeCache.txt"), Follow::Yes)
        .is_ok_and(|meta| meta.kind == FileKind::File)
    {
        return Err(error("requires a directory that contains CMakeCache.txt"));
    }
    host.remove_dir_all(&canonical)
        .map_err(|err| error(err.to_string()))?;
    Ok(Flow::Normal)
}

fn ctest_run_script(cx: &mut Cx<'_>, args: &[ExpandedArg]) -> CmdResult {
    let mut scripts = Vec::new();
    let mut new_process = false;
    let mut return_value = None;
    let mut args = args.iter().map(|arg| &arg.value);
    while let Some(arg) = args.next() {
        match arg.as_slice() {
            b"NEW_PROCESS" => new_process = true,
            b"RETURN_VALUE" => {
                return_value = Some(
                    args.next()
                        .ok_or_else(|| error("requires a variable for RETURN_VALUE"))?,
                )
            }
            _ => scripts.push(arg),
        }
    }
    if scripts.is_empty() {
        return Err(error("requires at least one script"));
    }
    let (host, mut settings) = SESSION
        .with_borrow(|session| {
            session
                .as_ref()
                .map(|session| (Arc::clone(&session.host), session.settings.clone()))
        })
        .ok_or_else(|| error("requires a dashboard script session"))?;
    let mut code = 0;
    settings.depth += usize::from(!new_process);
    for script in scripts {
        code = execute_script(&host, settings.clone(), script, new_process)
            .map_err(|err| error(err.to_string()))?;
    }
    if let Some(var) = return_value {
        cx.set_var(var, code.to_string().into());
    }
    Ok(Flow::Normal)
}

#[cfg(test)]
mod tests {
    use std::error::Error;

    use rcmake_host::{PureHost, ReadRange};

    use super::*;

    fn invoke(host: Arc<PureHost>, args: &[&str]) -> i32 {
        let argv = std::iter::once("ctest")
            .chain(args.iter().copied())
            .map(BString::from)
            .collect::<Vec<_>>();
        crate::main(&argv, host)
    }

    #[test]
    fn runs_dashboard_handlers_and_sets_script_variables() -> Result<(), Box<dyn Error>> {
        let host = Arc::new(PureHost::default());
        host.seed_file(Path::new("/work/src/CMakeLists.txt"), b"project(test NONE)")?;
        host.seed_file(
            Path::new("/work/build/CTestTestfile.cmake"),
            br#"
            add_test(wanted runner)
            add_test(excluded unexpected)
            set_tests_properties(wanted PROPERTIES LABELS fast)
        "#,
        )?;
        host.seed_file(Path::new("/work/nested.cmake"), b"cmake_language(EXIT 7)")?;
        host.seed_file(Path::new("/work/dashboard.cmake"), br#"
            if(NOT CTEST_SCRIPT_NAME STREQUAL "dashboard.cmake" OR NOT CTEST_SCRIPT_ARG STREQUAL "a,b")
              message(FATAL_ERROR "Wrong script variables")
            endif()
            set(CTEST_SITE "test-site")
            set(CTEST_BUILD_NAME "test-build")
            set(CTEST_CMAKE_GENERATOR "Ninja")
            set(CTEST_SOURCE_DIRECTORY "/work/src")
            set(CTEST_BINARY_DIRECTORY "/work/build")
            set(CMAKE_COMMAND fake-cmake)
            ctest_start(Experimental TRACK "Test track")
            ctest_configure(OPTIONS "-DONE=1;-DTWO=2" RETURN_VALUE configure CAPTURE_CMAKE_ERROR captured)
            if(NOT configure EQUAL 0 OR NOT captured EQUAL 0)
              message(FATAL_ERROR "Configure failed")
            endif()
            ctest_build(TARGET sample CONFIGURATION Debug FLAGS "-v 'two words'" NUMBER_WARNINGS warnings NUMBER_ERRORS errors RETURN_VALUE build)
            if(NOT build EQUAL 0 OR NOT warnings EQUAL 1 OR NOT errors EQUAL 0)
              message(FATAL_ERROR "Wrong build result")
            endif()
            ctest_test(INCLUDE_LABEL "^fast$" PARALLEL_LEVEL 2 STOP_ON_FAILURE OUTPUT_JUNIT junit.xml RETURN_VALUE test CAPTURE_CMAKE_ERROR captured)
            if(NOT test EQUAL -1 OR NOT captured EQUAL 0)
              message(FATAL_ERROR "Wrong test result")
            endif()
            ctest_run_script(nested.cmake RETURN_VALUE child)
            if(NOT child EQUAL 7)
              message(FATAL_ERROR "Wrong script exit code")
            endif()
            ctest_configure(SOURCE /work/missing CAPTURE_CMAKE_ERROR captured)
            if(NOT captured EQUAL -1)
              message(FATAL_ERROR "Missing configure error")
            endif()
        "#)?;
        host.set_command_handler(|_, request, _| {
            assert_eq!(request.cwd, Path::new("/work/build"));
            let command = &request.pipeline[0];
            let (code, text): (_, &[u8]) = if command.program == "runner" {
                (12, b"test output")
            } else {
                assert_eq!(command.program, "fake-cmake");
                if command.args[0] == "--build" {
                    assert_eq!(
                        command.args,
                        [
                            "--build",
                            ".",
                            "--config",
                            "Debug",
                            "--target",
                            "sample",
                            "--",
                            "-v",
                            "two words"
                        ]
                    );
                    (0, b"compiler: warning: sample\n")
                } else {
                    assert_eq!(
                        command.args,
                        [
                            "-S/work/src",
                            "-B/work/build",
                            "-GNinja",
                            "-DONE=1",
                            "-DTWO=2"
                        ]
                    );
                    (0, b"")
                }
            };
            Ok(RunOutput {
                results: vec![ProcessResult::Exited(code)],
                stdout: text.to_vec(),
                stderr: Vec::new(),
            })
        })?;
        assert_eq!(
            invoke(host.clone(), &["-S", "dashboard.cmake,a,b"]),
            0,
            "{}",
            host.stream_bytes(Stream::Stderr)
        );
        let dir = Path::new("/work/build");
        let tag = Tag::read(host.as_ref(), dir)?;
        assert_eq!(tag.track, "Test track");
        let xml = host.read(
            &dir.join("Testing").join(tag.stamp()).join("Test.xml"),
            ReadRange::All { limit: 16_384 },
        )?;
        let xml = String::from_utf8(xml)?;
        assert!(xml.contains("<Site BuildName=\"test-build\""));
        assert!(xml.contains("Name=\"test-site\""));
        assert!(xml.contains("<Test Status=\"failed\">"));
        assert!(xml.contains("<Value>12</Value>"));
        assert!(!xml.contains("excluded"));
        Ok(())
    }

    #[test]
    fn starts_a_child_ctest_for_sp() -> Result<(), Box<dyn Error>> {
        let host = Arc::new(PureHost::default());
        host.set_command_handler(|_, request, _| {
            assert_eq!(
                request.pipeline[0].args,
                ["-S", "/work/dashboard.cmake,argument", "-V"]
            );
            assert!(
                request
                    .env
                    .iter()
                    .any(|(key, value)| key.as_slice() == DEPTH_KEY && value == "1")
            );
            Ok(RunOutput {
                results: vec![ProcessResult::Exited(3)],
                ..RunOutput::default()
            })
        })?;
        assert_eq!(invoke(host, &["-V", "-SP", "dashboard.cmake,argument"]), 3);
        Ok(())
    }

    #[test]
    fn empties_only_build_directories_with_cmake_cache() -> Result<(), Box<dyn Error>> {
        let host = Arc::new(PureHost::default());
        host.seed_file(Path::new("/work/build/CMakeCache.txt"), b"")?;
        host.seed_file(Path::new("/work/source/CMakeLists.txt"), b"")?;
        host.seed_file(
            Path::new("/work/empty.cmake"),
            b"ctest_empty_binary_directory(/work/build)",
        )?;
        host.seed_file(
            Path::new("/work/invalid.cmake"),
            b"ctest_empty_binary_directory(/work/source)",
        )?;
        assert_eq!(invoke(host.clone(), &["-S", "empty.cmake"]), 0);
        assert!(
            host.metadata(Path::new("/work/build"), Follow::Yes)
                .is_err()
        );
        assert_eq!(invoke(host.clone(), &["-S", "invalid.cmake"]), 255);
        assert!(
            host.metadata(Path::new("/work/source/CMakeLists.txt"), Follow::Yes)
                .is_ok()
        );
        Ok(())
    }
}
