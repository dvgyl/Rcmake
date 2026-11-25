use std::path::Path;
use std::time::Duration;

use rcmake_base::{BString, list};
use rcmake_diag::{DiagCategory, Effect, MessageKind};
use rcmake_eval::args::{KeywordParser, Parsed, Slot};
use rcmake_eval::{CmdError, CmdResult, Cx, Flow};
use rcmake_expr::ExpandedArg;
use rcmake_host::{CommandSpec, Input, Output, ProcessResult, RunOutput, RunRequest, Stream};
use rcmake_host::{HostError, path};

use super::{diagnostic, environment, text};

const TIMEOUT_MESSAGE: &str = "Process terminated due to timeout";

#[derive(Clone, Copy)]
enum Fatal {
    None,
    Any,
    Last,
}

pub(super) fn execute_process(cx: &mut Cx<'_>, args: &[ExpandedArg]) -> CmdResult {
    let parsed = parse(cx, args)?;
    let fatal = fatal_option(cx, &parsed)?;
    let echo = echo_option(cx, &parsed)?;
    let timeout = value(&parsed, "TIMEOUT")
        .map(parse_timeout)
        .transpose()?
        .flatten();
    check_encoding(cx, &parsed);

    let pipeline = parsed
        .groups("COMMAND")
        .iter()
        .filter_map(|group| {
            let (program, args) = group.split_first()?;
            Some(CommandSpec {
                program: program.clone(),
                args: args.to_vec(),
            })
        })
        .collect::<Vec<_>>();
    let cwd = value(&parsed, "WORKING_DIRECTORY").map_or_else(
        || cx.host().initial_cwd(),
        |directory| cx.resolve_path(directory),
    );

    let stdout_var = value(&parsed, "OUTPUT_VARIABLE");
    let stderr_var = value(&parsed, "ERROR_VARIABLE");
    let stdout = output(
        &parsed,
        &cwd,
        "OUTPUT_FILE",
        "OUTPUT_QUIET",
        stdout_var.is_some() || parsed.flag("ECHO_OUTPUT_VARIABLE"),
    );
    let mut stderr = output(
        &parsed,
        &cwd,
        "ERROR_FILE",
        "ERROR_QUIET",
        stderr_var.is_some() || parsed.flag("ECHO_ERROR_VARIABLE"),
    );
    let same_variable = stdout_var.is_some() && stdout_var == stderr_var;
    stderr = match (&stdout, &stderr) {
        (Output::File { path: out, .. }, Output::File { path: err, .. }) if out == err => {
            Output::SharedWithStdout
        }
        (_, Output::File { .. }) => stderr,
        (Output::File { .. }, _) if same_variable => Output::Null,
        _ if same_variable => Output::SharedWithStdout,
        _ => stderr,
    };
    let echo_stdout = parsed.flag("ECHO_OUTPUT_VARIABLE") && stdout == Output::Capture;
    let echo_stderr = parsed.flag("ECHO_ERROR_VARIABLE") && stderr == Output::Capture;

    let request = RunRequest {
        pipeline,
        env: environment::child_environment(cx.host(), &parsed)
            .map_err(|error| crate::exit_with_error(cx, 1, &error))?,
        stdin: value(&parsed, "INPUT_FILE").map_or(Input::Inherit, |file| {
            Input::File(cwd.join(path::to_native(file)))
        }),
        stdout,
        stderr,
        cwd,
        timeout,
        tee: echo_stdout || echo_stderr,
    };

    #[cfg(all(feature = "native", any(unix, windows)))]
    let mut fetch = crate::fetch_observe::before_process(cx, &request)?;

    let host = cx.host();
    if let Some(stream) = echo {
        for command in &request.pipeline {
            let mut line = Vec::new();
            for arg in std::iter::once(&command.program).chain(&command.args) {
                if !line.is_empty() {
                    line.push(b' ');
                }
                line.push(b'\'');
                line.extend_from_slice(arg);
                line.push(b'\'');
            }
            line.push(b'\n');
            host.write_out(stream, &line);
        }
    }
    let execute = || {
        host.run(&request, &mut |stream, bytes| {
            let echo = match stream {
                Stream::Stdout => echo_stdout,
                Stream::Stderr => echo_stderr,
            };
            if echo {
                host.write_out(stream, bytes);
            }
        })
    };
    #[cfg(all(feature = "native", any(unix, windows)))]
    let run = fetch.result.take().map_or_else(
        || crate::configure::platform_queries::run(cx, &request).unwrap_or_else(execute),
        Ok,
    );
    #[cfg(not(all(feature = "native", any(unix, windows))))]
    let run = execute();
    let run = run.unwrap_or_else(|error| RunOutput {
        results: vec![ProcessResult::SpawnFailed(error); request.pipeline.len()],
        ..RunOutput::default()
    });

    if let Some(var) = stdout_var {
        cx.set_var(
            var,
            output_value(&run.stdout, parsed.flag("OUTPUT_STRIP_TRAILING_WHITESPACE")),
        );
    }
    if let Some(var) = stderr_var.filter(|_| !same_variable) {
        cx.set_var(
            var,
            output_value(&run.stderr, parsed.flag("ERROR_STRIP_TRAILING_WHITESPACE")),
        );
    }
    let denied = |result: &ProcessResult| matches!(result, ProcessResult::SpawnFailed(error) if rcmake_host::no_process::is_process_denial(error));
    let fatal_denial = match fatal {
        Fatal::None => false,
        Fatal::Last => run.results.last().is_some_and(denied),
        Fatal::Any => run.results.iter().any(denied),
    } && !run.results.contains(&ProcessResult::TimedOut);
    let results: Vec<_> = run.results.iter().map(result_text).collect();
    if let (Some(var), Some(last)) = (value(&parsed, "RESULT_VARIABLE"), results.last()) {
        let result = if run.results.contains(&ProcessResult::TimedOut) {
            TIMEOUT_MESSAGE.into()
        } else {
            last.clone()
        };
        cx.set_var(var, result);
    }
    if let Some(var) = value(&parsed, "RESULTS_VARIABLE") {
        cx.set_var(var, list::join(&results));
    }

    #[cfg(all(feature = "native", any(unix, windows)))]
    crate::fetch_observe::after_process(cx, fetch, &run)?;

    match fatal_error(fatal, &run.results, &results) {
        Some(error) => {
            if fatal_denial {
                cx.stop_for_denied_process();
            } else {
                cx.set_fatal();
            }
            Err(CmdError::Status(error))
        }
        None => Ok(Flow::Normal),
    }
}

fn parse(cx: &mut Cx<'_>, args: &[ExpandedArg]) -> Result<Parsed, CmdError> {
    if args.is_empty() {
        return Err(CmdError::Status(
            "called with incorrect number of arguments".into(),
        ));
    }
    let mut parser = KeywordParser::new().keyword("COMMAND", Slot::Groups);
    for key in [
        "WORKING_DIRECTORY",
        "TIMEOUT",
        "RESULT_VARIABLE",
        "RESULTS_VARIABLE",
        "OUTPUT_VARIABLE",
        "ERROR_VARIABLE",
        "INPUT_FILE",
        "OUTPUT_FILE",
        "ERROR_FILE",
        "COMMAND_ECHO",
        "COMMAND_ERROR_IS_FATAL",
        "ENCODING",
    ] {
        parser = parser.keyword(key, Slot::Single);
    }
    for key in [
        "OUTPUT_QUIET",
        "ERROR_QUIET",
        "OUTPUT_STRIP_TRAILING_WHITESPACE",
        "ERROR_STRIP_TRAILING_WHITESPACE",
        "ECHO_OUTPUT_VARIABLE",
        "ECHO_ERROR_VARIABLE",
    ] {
        parser = parser.keyword(key, Slot::Flag);
    }
    let parsed = parser
        .keyword("ENVIRONMENT", Slot::Multi)
        .keyword("ENVIRONMENT_MODIFICATION", Slot::Multi)
        .parse(args);
    if let Some(arg) = parsed.unparsed().first() {
        return Err(CmdError::Status(text(&[
            b"given unknown argument \"",
            arg,
            b"\".",
        ])));
    }
    if let Some(error) = parsed.report_error_text() {
        return Err(diagnostic(cx, error));
    }
    if !parsed.seen("COMMAND") {
        return Err(CmdError::Status("called with no COMMAND argument.".into()));
    }
    if parsed.groups("COMMAND").iter().any(Vec::is_empty) {
        return Err(CmdError::Status(
            "given COMMAND argument with no value.".into(),
        ));
    }
    Ok(parsed)
}

fn value<'a>(parsed: &'a Parsed, key: &str) -> Option<&'a [u8]> {
    parsed
        .single(key)
        .map(|value| value.as_slice())
        .filter(|value| !value.is_empty())
}

fn output(parsed: &Parsed, cwd: &Path, file: &str, quiet: &str, capture: bool) -> Output {
    if let Some(file) = value(parsed, file) {
        Output::File {
            path: cwd.join(path::to_native(file)),
            append: false,
        }
    } else if parsed.flag(quiet) {
        Output::Null
    } else if capture {
        Output::Capture
    } else {
        Output::Inherit
    }
}

fn check_encoding(cx: &mut Cx<'_>, parsed: &Parsed) {
    // The Unix host passes through the input bytes for each encoding.
    if let Some(encoding) = parsed.single("ENCODING")
        && !matches!(
            encoding.as_slice(),
            b"NONE" | b"AUTO" | b"ANSI" | b"OEM" | b"UTF8" | b"UTF-8"
        )
    {
        cx.issue(
            MessageKind::AuthorWarning,
            Effect::None,
            Some(DiagCategory::Author),
            text(&[
                b"ENCODING option given unknown value \"",
                encoding,
                b"\".  Ignoring.",
            ]),
        );
    }
}

fn echo_option(cx: &mut Cx<'_>, parsed: &Parsed) -> Result<Option<Stream>, CmdError> {
    let explicit = value(parsed, "COMMAND_ECHO");
    let default = cx.var(b"CMAKE_EXECUTE_PROCESS_COMMAND_ECHO");
    match explicit
        .or(default.as_ref().map(|value| value.as_slice()))
        .unwrap_or(b"NONE")
    {
        b"STDOUT" => Ok(Some(Stream::Stdout)),
        b"STDERR" => Ok(Some(Stream::Stderr)),
        b"NONE" | b"" => Ok(None),
        invalid => {
            let message = if explicit.is_some() {
                text(&[
                    b" called with '",
                    invalid,
                    b"' expected STDERR|STDOUT|NONE for COMMAND_ECHO.",
                ])
            } else {
                text(&[
                    b" CMAKE_EXECUTE_PROCESS_COMMAND_ECHO set to '",
                    invalid,
                    b"' expected STDERR|STDOUT|NONE",
                ])
            };
            Err(diagnostic(cx, message))
        }
    }
}

fn fatal_option(cx: &mut Cx<'_>, parsed: &Parsed) -> Result<Fatal, CmdError> {
    let explicit = value(parsed, "COMMAND_ERROR_IS_FATAL");
    let default = if explicit.is_none()
        && value(parsed, "RESULT_VARIABLE").is_none()
        && value(parsed, "RESULTS_VARIABLE").is_none()
    {
        cx.var(b"CMAKE_EXECUTE_PROCESS_COMMAND_ERROR_IS_FATAL")
    } else {
        None
    };
    match explicit
        .or(default.as_ref().map(|value| value.as_slice()))
        .unwrap_or_default()
    {
        b"ANY" => Ok(Fatal::Any),
        b"LAST" => Ok(Fatal::Last),
        b"NONE" | b"" => Ok(Fatal::None),
        _ if explicit.is_some() => Err(CmdError::Status(
            "COMMAND_ERROR_IS_FATAL option can be ANY, LAST or NONE".into(),
        )),
        invalid => Err(CmdError::Status(text(&[
            b"Using CMAKE_EXECUTE_PROCESS_COMMAND_ERROR_IS_FATAL with invalid value \"",
            invalid,
            b"\".  This variable can be ANY, LAST or NONE",
        ]))),
    }
}

fn parse_timeout(bytes: &[u8]) -> Result<Option<Duration>, CmdError> {
    parse_seconds(bytes)
        .map(|seconds| Duration::try_from_secs_f64(seconds).ok())
        .ok_or_else(|| {
            CmdError::Status("called with TIMEOUT value that could not be parsed.".into())
        })
}

fn parse_seconds(bytes: &[u8]) -> Option<f64> {
    let mut bytes = bytes.iter().copied().peekable();
    while bytes.next_if(|byte| c_whitespace(*byte)).is_some() {}
    let mut number = Vec::new();
    number.extend(bytes.next_if(|byte| matches!(byte, b'+' | b'-')));
    let sign_len = number.len();
    while let Some(digit) = bytes.next_if(u8::is_ascii_digit) {
        number.push(digit);
    }
    let whole_digits = number.len() - sign_len;
    let mut fraction_digits = 0;
    if let Some(dot) = bytes.next_if_eq(&b'.') {
        number.push(dot);
        while let Some(digit) = bytes.next_if(u8::is_ascii_digit) {
            number.push(digit);
            fraction_digits += 1;
        }
    }
    if whole_digits + fraction_digits == 0 {
        let prefix: Vec<_> = bytes.take(3).collect();
        return (prefix.eq_ignore_ascii_case(b"inf") || prefix.eq_ignore_ascii_case(b"nan"))
            .then_some(f64::INFINITY);
    }
    let mantissa = number.len();
    if let Some(exponent) = bytes.next_if(|byte| matches!(byte, b'e' | b'E')) {
        number.push(exponent);
        number.extend(bytes.next_if(|byte| matches!(byte, b'+' | b'-')));
        let exponent_start = number.len();
        while let Some(digit) = bytes.next_if(u8::is_ascii_digit) {
            number.push(digit);
        }
        if number.len() == exponent_start {
            number.truncate(mantissa);
        }
    }
    std::str::from_utf8(&number).ok()?.parse().ok()
}

fn output_value(bytes: &[u8], strip: bool) -> BString {
    let mut chars = bytes.iter().copied().peekable();
    let mut result = Vec::with_capacity(bytes.len());
    while let Some(byte) = chars.next() {
        if byte != 0 && !(byte == b'\r' && chars.peek() == Some(&b'\n')) {
            result.push(byte);
        }
    }
    if strip {
        result.truncate(
            result
                .iter()
                .rposition(|byte| !c_whitespace(*byte))
                .map_or(0, |i| i + 1),
        );
    }
    result.into()
}

fn c_whitespace(byte: u8) -> bool {
    byte.is_ascii_whitespace() || byte == 0x0b
}

fn result_text(result: &ProcessResult) -> BString {
    match result {
        ProcessResult::Exited(code) => code.to_string().into(),
        ProcessResult::TimedOut => TIMEOUT_MESSAGE.into(),
        ProcessResult::Signaled(signal) => signal_text(*signal).into(),
        ProcessResult::SpawnFailed(error) => spawn_error(error).into(),
    }
}

fn spawn_error(error: &HostError) -> String {
    let mut message = error.os_message.clone();
    if error.errno.is_some()
        && let Some(first) = message.get_mut(..1)
    {
        first.make_ascii_lowercase();
    }
    message
}

fn signal_text(signal: i32) -> &'static str {
    // BSD and Linux use different numbers for these POSIX signals.
    if cfg!(any(
        target_vendor = "apple",
        target_os = "freebsd",
        target_os = "openbsd",
        target_os = "netbsd",
        target_os = "dragonfly"
    )) {
        match signal {
            7 => return "SIGEMT",
            10 => return "Bus error",
            12 => return "SIGSYS",
            16 => return "SIGURG",
            17 => return "SIGSTOP",
            18 => return "SIGTSTP",
            19 => return "SIGCONT",
            23 => return "SIGIO",
            29 => return "SIGINFO",
            30 => return "SIGUSR1",
            31 => return "SIGUSR2",
            _ => {}
        }
    } else {
        match signal {
            7 => return "Bus error",
            10 => return "SIGUSR1",
            12 => return "SIGUSR2",
            16 => return "SIGSTKFLT",
            17 => return "SIGCHLD",
            18 => return "SIGCONT",
            19 => return "SIGSTOP",
            20 => return "SIGTSTP",
            21 => return "SIGTTIN",
            22 => return "SIGTTOU",
            23 => return "SIGURG",
            29 => return "SIGIO",
            30 => return "SIGPWR",
            31 => return "SIGSYS",
            _ => {}
        }
    }
    match signal {
        1 => "SIGHUP",
        2 => "User interrupt",
        3 => "SIGQUIT",
        4 => "Illegal instruction",
        5 => "SIGTRAP",
        6 => "Subprocess aborted",
        8 => "Floating-point exception",
        9 => "Subprocess killed",
        11 => "Segmentation fault",
        13 => "SIGPIPE",
        14 => "SIGALRM",
        15 => "Subprocess terminated",
        20 => "SIGCHLD",
        21 => "SIGTTIN",
        22 => "SIGTTOU",
        24 => "SIGXCPU",
        25 => "SIGXFSZ",
        26 => "SIGVTALRM",
        27 => "SIGPROF",
        28 => "SIGWINCH",
        _ => "Unknown signal",
    }
}

fn fatal_error(fatal: Fatal, results: &[ProcessResult], messages: &[BString]) -> Option<BString> {
    if matches!(fatal, Fatal::None) {
        return None;
    }
    if results.contains(&ProcessResult::TimedOut) {
        return Some(TIMEOUT_MESSAGE.into());
    }
    match fatal {
        Fatal::None => None,
        Fatal::Last => match results.last()? {
            ProcessResult::Exited(0) => None,
            ProcessResult::Exited(_) => Some("last command failed".into()),
            _ => Some(text(&[b"Abnormal exit: ", messages.last()?])),
        },
        Fatal::Any => {
            let mut failures = Vec::new();
            for (index, (result, message)) in results.iter().zip(messages).enumerate() {
                let label: &[u8] = match result {
                    ProcessResult::Exited(0) => continue,
                    ProcessResult::Exited(_) => b"Child return code: ",
                    _ => b"Abnormal exit with child return code: ",
                };
                failures.push(text(&[
                    b"  ",
                    (index + 1).to_string().as_bytes(),
                    b": \"",
                    label,
                    message,
                    b"\"",
                ]));
            }
            if failures.is_empty() {
                return None;
            }
            let mut error = b"failed command indexes:\n".to_vec();
            for failure in failures {
                error.extend_from_slice(&failure);
                error.push(b'\n');
            }
            Some(error.into())
        }
    }
}
