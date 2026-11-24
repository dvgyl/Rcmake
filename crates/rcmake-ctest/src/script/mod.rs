use std::cell::RefCell;
use std::collections::BTreeMap;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use rcmake_base::{BString, Limits, list, path_lex::Lexical};
use rcmake_eval::{
    CmdError, CmdFlags, CmdResult, Cx, Flow, Interp, InterpConfig, LogLevel, Registry, RunMode,
};
use rcmake_expr::ExpandedArg;
use rcmake_host::{
    CommandSpec, FileKind, Follow, Host, Input, Output, ProcessResult, RunOutput, RunRequest,
    Stream, path,
};

use crate::CtestError;
use crate::cli::{Invocation, Script};
use crate::dart::{self, Configuration, Tag};

mod build;
mod configure;
mod start;
mod test;

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
    include: Option<BString>,
    exclude: Option<BString>,
    selection: crate::selection::SelectionOptions,
    interactive_debugging: Option<bool>,
    no_tests_action: Option<rcmake_presets::NoTestsAction>,
    include_labels: Vec<BString>,
    exclude_labels: Vec<BString>,
    definitions: BTreeMap<BString, BString>,
}

struct Session {
    host: Arc<dyn Host>,
    settings: Settings,
    start: Option<Start>,
    failed: bool,
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
        include: invocation.options.include.clone(),
        exclude: invocation.options.exclude.clone(),
        selection: invocation.selection.clone(),
        interactive_debugging: invocation.execution.interactive_debugging,
        no_tests_action: invocation.execution.no_tests_action,
        include_labels: invocation.options.include_labels.clone(),
        exclude_labels: invocation.options.exclude_labels.clone(),
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
    let cmake_command = cmake_executable(host.as_ref());
    let (_, cmake_root) = rcmake_host::discover_cmake_root(host.as_ref(), None);
    let mut interp = Interp::new(
        InterpConfig {
            host: Arc::clone(host),
            mode: RunMode::Script,
            cmake_root: cmake_root.unwrap_or_default(),
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
        failed: false,
    })));
    let code = interp.run_script(&file).exit_code;
    if let Some(code) = interp.requested_exit_code() {
        // Match the exit status from a CTest script that runs in a child process.
        #[cfg(unix)]
        let code = code & 0xff;
        return Ok(code);
    }
    let failed =
        SESSION.with_borrow(|session| session.as_ref().is_some_and(|session| session.failed));
    if code == 0 && failed {
        return Ok(255);
    }
    if code == 1 && (interp.diag_engine().error_occurred() || interp.fatal_occurred()) {
        return Ok(255);
    }
    Ok(code)
}

pub(crate) fn cmake_executable(host: &dyn Host) -> PathBuf {
    let exe = host
        .current_exe()
        .unwrap_or_else(|_| host.initial_cwd().join("cmake"));
    let executable = host.canonicalize(&exe).unwrap_or_else(|_| exe.clone());
    let cmake_name = if cfg!(windows) { "cmake.exe" } else { "cmake" };
    [
        exe.with_file_name(cmake_name),
        executable.with_file_name(cmake_name),
    ]
    .into_iter()
    .find(|candidate| host.metadata(candidate, Follow::Yes).is_ok())
    .unwrap_or(executable)
}

fn register(registry: &mut Registry) {
    rcmake_commands::register_all(registry);
    for (name, command) in [
        ("ctest_start", start::ctest_start as rcmake_eval::BuiltinFn),
        ("ctest_configure", configure::ctest_configure),
        ("ctest_build", build::ctest_build),
        ("ctest_test", test::ctest_test),
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
        (b"CTEST_CHANGE_ID", &mut config.change_id),
        (b"CTEST_TEST_COVERAGE_TOOL", &mut config.coverage_tool),
    ] {
        if cx.var_defined(key) {
            *value = cx.var_or_empty(key);
        }
    }
    Ok(config)
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
                if values.is_empty() && matches!(key, b"INCLUDE_LABEL" | b"EXCLUDE_LABEL") {
                    return Err(error(format!(
                        "Error after keyword \"{}\":\n  missing required value",
                        BString::from(key)
                    )));
                }
                if values.is_empty() && key != b"PARALLEL_LEVEL" {
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
                    "called with more than one value for {}",
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
            SESSION.with_borrow_mut(|session| {
                if let Some(session) = session {
                    session.failed = true;
                }
            });
        }
        Ok(Flow::Normal)
    });
    if let Err(CmdError::Status(message)) = &result {
        if let Some([_, var]) = args.windows(2).find(|pair| {
            pair.first()
                .is_some_and(|arg| arg.value == "CAPTURE_CMAKE_ERROR")
        }) {
            cx.host()
                .write_out(Stream::Stderr, format!("{message}\n").as_bytes());
            cx.set_var(&var.value, "-1".into());
            return Ok(Flow::Normal);
        }
        if message.starts_with(b"Error after keyword ") {
            cx.issue(
                rcmake_diag::MessageKind::Error,
                rcmake_diag::Effect::Fatal,
                None,
                message.clone(),
            );
            return Err(CmdError::Reported);
        }
    }
    result
}

pub(crate) fn split_command(cx: &mut Cx<'_>, command: &[u8]) -> Result<Vec<BString>, CmdError> {
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

pub(crate) fn run_process(
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

fn ctest_empty_binary_directory(cx: &mut Cx<'_>, args: &[ExpandedArg]) -> CmdResult {
    let [dir] = args else {
        return Err(error("requires exactly one directory"));
    };
    if dir.value.is_empty() {
        return Err(error("requires a binary directory"));
    }
    let host = cx.host();
    let dir = absolute(&host.initial_cwd(), &dir.value);
    let (Some(parent), Some(name)) = (dir.parent(), dir.file_name()) else {
        return Err(error("requires a non-root directory"));
    };
    let dir = match host.canonicalize(parent) {
        Ok(parent) => parent.join(name),
        Err(err) if err.kind == ErrorKind::NotFound => return Ok(Flow::Normal),
        Err(err) => return Err(error(err.to_string())),
    };
    let kind = match host.metadata(&dir, Follow::No) {
        Ok(metadata) => metadata.kind,
        Err(err) if err.kind == ErrorKind::NotFound => return Ok(Flow::Normal),
        Err(err) => return Err(error(err.to_string())),
    };
    if kind != FileKind::Dir {
        return Err(error("requires a directory as the final path component"));
    }
    if !host
        .metadata(&dir.join("CMakeCache.txt"), Follow::Yes)
        .is_ok_and(|meta| meta.kind == FileKind::File)
    {
        return Err(error("requires a directory that contains CMakeCache.txt"));
    }
    host.remove_dir_all(&dir)
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
mod tests;
