use std::path::Path;
use std::time::Duration;

use rcmake_base::{BString, list};
use rcmake_diag::{PolicyId, PolicyStatus};
use rcmake_eval::args::{KeywordParser, Parsed, Slot};
use rcmake_eval::{CmdError, CmdResult, Cx, Flow};
use rcmake_expr::ExpandedArg;
use rcmake_host::{
    CommandSpec, FileKind, Follow, Host, HostError, Input, Output, ProcessResult, RunOutput,
    RunRequest, Stream, path,
};

use super::{cat, environment};

const TIMEOUT_MESSAGE: &str = "Process terminated due to timeout";

#[derive(Clone, Copy)]
enum Fatal {
    None,
    Any,
    Last,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ProcessEncoding {
    None,
    Auto,
    Ansi,
    Oem,
    Utf8,
}

pub(super) fn execute_process(cx: &mut Cx<'_>, args: &[ExpandedArg]) -> CmdResult {
    let parsed = parse(cx, args)?;
    let fatal = fatal_option(cx, &parsed)?;
    let echo = echo_option(cx, &parsed)?;
    let timeout = value(&parsed, "TIMEOUT")
        .map(parse_timeout)
        .transpose()?
        .flatten();
    let encoding = check_encoding(cx, &parsed);

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
        default_error_mode: false,
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
        let mut exec_request = request.clone();
        if cfg!(windows) {
            for command in &mut exec_request.pipeline {
                if let Some(adjusted) =
                    windows_extensionless_program(host, &exec_request.cwd, &command.program)
                {
                    command.program = adjusted;
                }
            }
        }
        host.run(&exec_request, &mut |stream, bytes| {
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
        let decoded = decode_process_output(cx.host(), &run.stdout, encoding);
        cx.set_var(
            var,
            output_value(&decoded, parsed.flag("OUTPUT_STRIP_TRAILING_WHITESPACE")),
        );
    }
    if let Some(var) = stderr_var.filter(|_| !same_variable) {
        let decoded = decode_process_output(cx.host(), &run.stderr, encoding);
        cx.set_var(
            var,
            output_value(&decoded, parsed.flag("ERROR_STRIP_TRAILING_WHITESPACE")),
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
        return Err(CmdError::Status(cat(&[
            b"given unknown argument \"",
            arg,
            b"\".",
        ])));
    }
    if let Some(error) = parsed.report_error_text() {
        return Err(crate::command_error(cx, error));
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

fn check_encoding(cx: &mut Cx<'_>, parsed: &Parsed) -> ProcessEncoding {
    let default = if cx.policy(PolicyId::new(176)) == PolicyStatus::New {
        ProcessEncoding::Utf8
    } else {
        ProcessEncoding::Auto
    };
    let Some(encoding) = parsed.single("ENCODING") else {
        return default;
    };
    match encoding.as_slice() {
        b"NONE" => ProcessEncoding::None,
        b"AUTO" => ProcessEncoding::Auto,
        b"ANSI" => ProcessEncoding::Ansi,
        b"OEM" => ProcessEncoding::Oem,
        b"UTF8" | b"UTF-8" => ProcessEncoding::Utf8,
        _ => {
            crate::author_warning(
                cx,
                cat(&[
                    b"ENCODING option given unknown value \"",
                    encoding,
                    b"\".  Ignoring.",
                ]),
            );
            default
        }
    }
}

fn windows_extensionless_program(host: &dyn Host, cwd: &Path, program: &[u8]) -> Option<BString> {
    let sep_pos = program
        .iter()
        .rposition(|&byte| byte == b'/' || byte == b'\\')?;
    let name = program.get(sep_pos + 1..)?;
    if name.is_empty() || name.contains(&b'.') {
        return None;
    }
    let full = cwd.join(path::to_native(program));
    host.metadata(&full, Follow::Yes)
        .is_ok_and(|meta| meta.kind == FileKind::File)
        .then(|| [program, b"."].concat().into())
}

fn decode_process_output<'a>(
    host: &dyn Host,
    bytes: &'a [u8],
    encoding: ProcessEncoding,
) -> std::borrow::Cow<'a, [u8]> {
    if !cfg!(windows)
        || matches!(encoding, ProcessEncoding::None | ProcessEncoding::Utf8)
        || bytes.iter().all(u8::is_ascii)
    {
        return std::borrow::Cow::Borrowed(bytes);
    }
    let codepage = resolve_windows_codepage(host, encoding);
    if codepage == 65001 {
        return std::borrow::Cow::Borrowed(bytes);
    }
    if let Some(decoded) = decode_codepage_pure(bytes, codepage) {
        return std::borrow::Cow::Owned(decoded);
    }
    if let Some(decoded) = decode_codepage_via_powershell(host, bytes, codepage) {
        return std::borrow::Cow::Owned(decoded);
    }
    std::borrow::Cow::Borrowed(bytes)
}

fn resolve_windows_codepage(host: &dyn Host, encoding: ProcessEncoding) -> u32 {
    match encoding {
        ProcessEncoding::Auto => query_console_codepage(host).unwrap_or(1252),
        ProcessEncoding::Oem => query_windows_codepage(host, "OEMCodePage").unwrap_or(437),
        ProcessEncoding::Ansi => query_windows_codepage(host, "ANSICodePage").unwrap_or(1252),
        ProcessEncoding::None | ProcessEncoding::Utf8 => 65001,
    }
}

fn query_console_codepage(host: &dyn Host) -> Option<u32> {
    let output = host
        .run_bounded(
            &RunRequest {
                pipeline: vec![CommandSpec {
                    program: "cmd".into(),
                    args: vec!["/d".into(), "/c".into(), "chcp".into()],
                }],
                cwd: host.initial_cwd(),
                env: host.env_all(),
                stdin: Input::Null,
                stdout: Output::Capture,
                stderr: Output::Null,
                timeout: Some(Duration::from_secs(5)),
                tee: false,
                default_error_mode: false,
            },
            4096,
        )
        .ok()?;
    if output.results != [ProcessResult::Exited(0)] {
        return None;
    }
    parse_chcp_output(&output.stdout)
}

fn parse_chcp_output(bytes: &[u8]) -> Option<u32> {
    let end = bytes.iter().rposition(u8::is_ascii_digit)? + 1;
    let start = bytes
        .get(..end)?
        .iter()
        .rposition(|byte| !byte.is_ascii_digit())
        .map_or(0, |pos| pos + 1);
    std::str::from_utf8(bytes.get(start..end)?)
        .ok()?
        .parse()
        .ok()
}

fn query_windows_codepage(host: &dyn Host, property: &str) -> Option<u32> {
    let script = format!("[System.Globalization.CultureInfo]::CurrentCulture.TextInfo.{property}");
    let output = host
        .run_bounded(
            &RunRequest {
                pipeline: vec![CommandSpec {
                    program: "powershell.exe".into(),
                    args: vec![
                        "-NoProfile".into(),
                        "-NonInteractive".into(),
                        "-Command".into(),
                        script.into(),
                    ],
                }],
                cwd: host.initial_cwd(),
                env: host.env_all(),
                stdin: Input::Null,
                stdout: Output::Capture,
                stderr: Output::Null,
                timeout: Some(Duration::from_secs(10)),
                tee: false,
                default_error_mode: false,
            },
            4096,
        )
        .ok()?;
    if output.results != [ProcessResult::Exited(0)] {
        return None;
    }
    parse_chcp_output(&output.stdout)
}

fn decode_codepage_via_powershell(host: &dyn Host, bytes: &[u8], codepage: u32) -> Option<Vec<u8>> {
    if !host.supports_process_execution() {
        return None;
    }
    let mut env = host.env_all();
    env.retain(|(key, _)| !key.eq_ignore_ascii_case(b"RCMAKE_CP"));
    env.push(("RCMAKE_CP".into(), codepage.to_string().into()));
    let script = "\
$enc = [System.Text.Encoding]::GetEncoding([int]$env:RCMAKE_CP); \
$ms = New-Object System.IO.MemoryStream; \
[Console]::OpenStandardInput().CopyTo($ms); \
$utf8 = New-Object System.Text.UTF8Encoding($false); \
$out = $utf8.GetBytes($enc.GetString($ms.ToArray())); \
[Console]::OpenStandardOutput().Write($out, 0, $out.Length)";
    let output = host
        .run_bounded(
            &RunRequest {
                pipeline: vec![CommandSpec {
                    program: "powershell.exe".into(),
                    args: vec![
                        "-NoProfile".into(),
                        "-NonInteractive".into(),
                        "-Command".into(),
                        script.into(),
                    ],
                }],
                cwd: host.initial_cwd(),
                env,
                stdin: Input::Bytes(bytes.into()),
                stdout: Output::Capture,
                stderr: Output::Null,
                timeout: Some(Duration::from_secs(15)),
                tee: false,
                default_error_mode: false,
            },
            bytes.len().saturating_mul(4).max(64 * 1024),
        )
        .ok()?;
    (output.results == [ProcessResult::Exited(0)]).then_some(output.stdout)
}

fn decode_codepage_pure(bytes: &[u8], codepage: u32) -> Option<Vec<u8>> {
    let table: &[char; 128] = match codepage {
        437 => &CP437_HIGH,
        850 => &CP850_HIGH,
        863 => &CP863_HIGH,
        1252 => &CP1252_HIGH,
        _ => return None,
    };
    let mut out = String::with_capacity(bytes.len());
    for &byte in bytes {
        if byte < 0x80 {
            out.push(char::from(byte));
        } else if let Some(&ch) = table.get(usize::from(byte - 0x80)) {
            out.push(ch);
        }
    }
    Some(out.into_bytes())
}

const CP437_HIGH: [char; 128] = [
    '\u{00C7}', '\u{00FC}', '\u{00E9}', '\u{00E2}', '\u{00E4}', '\u{00E0}', '\u{00E5}', '\u{00E7}',
    '\u{00EA}', '\u{00EB}', '\u{00E8}', '\u{00EF}', '\u{00EE}', '\u{00EC}', '\u{00C4}', '\u{00C5}',
    '\u{00C9}', '\u{00E6}', '\u{00C6}', '\u{00F4}', '\u{00F6}', '\u{00F2}', '\u{00FB}', '\u{00F9}',
    '\u{00FF}', '\u{00D6}', '\u{00DC}', '\u{00A2}', '\u{00A3}', '\u{00A5}', '\u{20A7}', '\u{0192}',
    '\u{00E1}', '\u{00ED}', '\u{00F3}', '\u{00FA}', '\u{00F1}', '\u{00D1}', '\u{00AA}', '\u{00BA}',
    '\u{00BF}', '\u{2310}', '\u{00AC}', '\u{00BD}', '\u{00BC}', '\u{00A1}', '\u{00AB}', '\u{00BB}',
    '\u{2591}', '\u{2592}', '\u{2593}', '\u{2502}', '\u{2524}', '\u{2561}', '\u{2562}', '\u{2556}',
    '\u{2555}', '\u{2563}', '\u{2551}', '\u{2557}', '\u{255D}', '\u{255C}', '\u{255B}', '\u{2510}',
    '\u{2514}', '\u{2534}', '\u{252C}', '\u{251C}', '\u{2500}', '\u{253C}', '\u{255E}', '\u{255F}',
    '\u{255A}', '\u{2554}', '\u{2569}', '\u{2566}', '\u{2560}', '\u{2550}', '\u{256C}', '\u{2567}',
    '\u{2568}', '\u{2564}', '\u{2565}', '\u{2559}', '\u{2558}', '\u{2552}', '\u{2553}', '\u{256B}',
    '\u{256A}', '\u{2518}', '\u{250C}', '\u{2588}', '\u{2584}', '\u{258C}', '\u{2590}', '\u{2580}',
    '\u{03B1}', '\u{00DF}', '\u{0393}', '\u{03C0}', '\u{03A3}', '\u{03C3}', '\u{00B5}', '\u{03C4}',
    '\u{03A6}', '\u{0398}', '\u{03A9}', '\u{03B4}', '\u{221E}', '\u{03C6}', '\u{03B5}', '\u{2229}',
    '\u{2261}', '\u{00B1}', '\u{2265}', '\u{2264}', '\u{2320}', '\u{2321}', '\u{00F7}', '\u{2248}',
    '\u{00B0}', '\u{2219}', '\u{00B7}', '\u{221A}', '\u{207F}', '\u{00B2}', '\u{25A0}', '\u{00A0}',
];

const CP850_HIGH: [char; 128] = [
    '\u{00C7}', '\u{00FC}', '\u{00E9}', '\u{00E2}', '\u{00E4}', '\u{00E0}', '\u{00E5}', '\u{00E7}',
    '\u{00EA}', '\u{00EB}', '\u{00E8}', '\u{00EF}', '\u{00EE}', '\u{00EC}', '\u{00C4}', '\u{00C5}',
    '\u{00C9}', '\u{00E6}', '\u{00C6}', '\u{00F4}', '\u{00F6}', '\u{00F2}', '\u{00FB}', '\u{00F9}',
    '\u{00FF}', '\u{00D6}', '\u{00DC}', '\u{00F8}', '\u{00A3}', '\u{00D8}', '\u{00D7}', '\u{0192}',
    '\u{00E1}', '\u{00ED}', '\u{00F3}', '\u{00FA}', '\u{00F1}', '\u{00D1}', '\u{00AA}', '\u{00BA}',
    '\u{00BF}', '\u{00AE}', '\u{00AC}', '\u{00BD}', '\u{00BC}', '\u{00A1}', '\u{00AB}', '\u{00BB}',
    '\u{2591}', '\u{2592}', '\u{2593}', '\u{2502}', '\u{2524}', '\u{00C1}', '\u{00C2}', '\u{00C0}',
    '\u{00A9}', '\u{2563}', '\u{2551}', '\u{2557}', '\u{255D}', '\u{00A2}', '\u{00A5}', '\u{2510}',
    '\u{2514}', '\u{2534}', '\u{252C}', '\u{251C}', '\u{2500}', '\u{253C}', '\u{00E3}', '\u{00C3}',
    '\u{255A}', '\u{2554}', '\u{2569}', '\u{2566}', '\u{2560}', '\u{2550}', '\u{256C}', '\u{00A4}',
    '\u{00F0}', '\u{00D0}', '\u{00CA}', '\u{00CB}', '\u{00C8}', '\u{0131}', '\u{00CD}', '\u{00CE}',
    '\u{00CF}', '\u{2518}', '\u{250C}', '\u{2588}', '\u{2584}', '\u{00A6}', '\u{00CC}', '\u{2580}',
    '\u{00D3}', '\u{00DF}', '\u{00D4}', '\u{00D2}', '\u{00F5}', '\u{00D5}', '\u{00B5}', '\u{00FE}',
    '\u{00DE}', '\u{00DA}', '\u{00DB}', '\u{00D9}', '\u{00FD}', '\u{00DD}', '\u{00AF}', '\u{00B4}',
    '\u{00AD}', '\u{00B1}', '\u{2017}', '\u{00BE}', '\u{00B6}', '\u{00A7}', '\u{00F7}', '\u{00B8}',
    '\u{00B0}', '\u{00A8}', '\u{00B7}', '\u{00B9}', '\u{00B3}', '\u{00B2}', '\u{25A0}', '\u{00A0}',
];

const CP863_HIGH: [char; 128] = [
    '\u{00C7}', '\u{00FC}', '\u{00E9}', '\u{00E2}', '\u{00C2}', '\u{00E0}', '\u{00B6}', '\u{00E7}',
    '\u{00EA}', '\u{00EB}', '\u{00E8}', '\u{00EF}', '\u{00EE}', '\u{2017}', '\u{00C0}', '\u{00A7}',
    '\u{00C9}', '\u{00C8}', '\u{00CA}', '\u{00F4}', '\u{00CB}', '\u{00CF}', '\u{00FB}', '\u{00F9}',
    '\u{00A4}', '\u{00D4}', '\u{00DC}', '\u{00A2}', '\u{00A3}', '\u{00D9}', '\u{00DB}', '\u{0192}',
    '\u{00A6}', '\u{00B4}', '\u{00F3}', '\u{00FA}', '\u{00A8}', '\u{00B8}', '\u{00B3}', '\u{00AF}',
    '\u{00CE}', '\u{2310}', '\u{00AC}', '\u{00BD}', '\u{00BC}', '\u{00BE}', '\u{00AB}', '\u{00BB}',
    '\u{2591}', '\u{2592}', '\u{2593}', '\u{2502}', '\u{2524}', '\u{2561}', '\u{2562}', '\u{2556}',
    '\u{2555}', '\u{2563}', '\u{2551}', '\u{2557}', '\u{255D}', '\u{255C}', '\u{255B}', '\u{2510}',
    '\u{2514}', '\u{2534}', '\u{252C}', '\u{251C}', '\u{2500}', '\u{253C}', '\u{255E}', '\u{255F}',
    '\u{255A}', '\u{2554}', '\u{2569}', '\u{2566}', '\u{2560}', '\u{2550}', '\u{256C}', '\u{2567}',
    '\u{2568}', '\u{2564}', '\u{2565}', '\u{2559}', '\u{2558}', '\u{2552}', '\u{2553}', '\u{256B}',
    '\u{256A}', '\u{2518}', '\u{250C}', '\u{2588}', '\u{2584}', '\u{258C}', '\u{2590}', '\u{2580}',
    '\u{03B1}', '\u{00DF}', '\u{0393}', '\u{03C0}', '\u{03A3}', '\u{03C3}', '\u{00B5}', '\u{03C4}',
    '\u{03A6}', '\u{0398}', '\u{03A9}', '\u{03B4}', '\u{221E}', '\u{03C6}', '\u{03B5}', '\u{2229}',
    '\u{2261}', '\u{00B1}', '\u{2265}', '\u{2264}', '\u{2320}', '\u{2321}', '\u{00F7}', '\u{2248}',
    '\u{00B0}', '\u{2219}', '\u{00B7}', '\u{221A}', '\u{207F}', '\u{00B2}', '\u{25A0}', '\u{00A0}',
];

const CP1252_HIGH: [char; 128] = [
    '\u{20AC}', '\u{0081}', '\u{201A}', '\u{0192}', '\u{201E}', '\u{2026}', '\u{2020}', '\u{2021}',
    '\u{02C6}', '\u{2030}', '\u{0160}', '\u{2039}', '\u{0152}', '\u{008D}', '\u{017D}', '\u{008F}',
    '\u{0090}', '\u{2018}', '\u{2019}', '\u{201C}', '\u{201D}', '\u{2022}', '\u{2013}', '\u{2014}',
    '\u{02DC}', '\u{2122}', '\u{0161}', '\u{203A}', '\u{0153}', '\u{009D}', '\u{017E}', '\u{0178}',
    '\u{00A0}', '\u{00A1}', '\u{00A2}', '\u{00A3}', '\u{00A4}', '\u{00A5}', '\u{00A6}', '\u{00A7}',
    '\u{00A8}', '\u{00A9}', '\u{00AA}', '\u{00AB}', '\u{00AC}', '\u{00AD}', '\u{00AE}', '\u{00AF}',
    '\u{00B0}', '\u{00B1}', '\u{00B2}', '\u{00B3}', '\u{00B4}', '\u{00B5}', '\u{00B6}', '\u{00B7}',
    '\u{00B8}', '\u{00B9}', '\u{00BA}', '\u{00BB}', '\u{00BC}', '\u{00BD}', '\u{00BE}', '\u{00BF}',
    '\u{00C0}', '\u{00C1}', '\u{00C2}', '\u{00C3}', '\u{00C4}', '\u{00C5}', '\u{00C6}', '\u{00C7}',
    '\u{00C8}', '\u{00C9}', '\u{00CA}', '\u{00CB}', '\u{00CC}', '\u{00CD}', '\u{00CE}', '\u{00CF}',
    '\u{00D0}', '\u{00D1}', '\u{00D2}', '\u{00D3}', '\u{00D4}', '\u{00D5}', '\u{00D6}', '\u{00D7}',
    '\u{00D8}', '\u{00D9}', '\u{00DA}', '\u{00DB}', '\u{00DC}', '\u{00DD}', '\u{00DE}', '\u{00DF}',
    '\u{00E0}', '\u{00E1}', '\u{00E2}', '\u{00E3}', '\u{00E4}', '\u{00E5}', '\u{00E6}', '\u{00E7}',
    '\u{00E8}', '\u{00E9}', '\u{00EA}', '\u{00EB}', '\u{00EC}', '\u{00ED}', '\u{00EE}', '\u{00EF}',
    '\u{00F0}', '\u{00F1}', '\u{00F2}', '\u{00F3}', '\u{00F4}', '\u{00F5}', '\u{00F6}', '\u{00F7}',
    '\u{00F8}', '\u{00F9}', '\u{00FA}', '\u{00FB}', '\u{00FC}', '\u{00FD}', '\u{00FE}', '\u{00FF}',
];

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
                cat(&[
                    b" called with '",
                    invalid,
                    b"' expected STDERR|STDOUT|NONE for COMMAND_ECHO.",
                ])
            } else {
                cat(&[
                    b" CMAKE_EXECUTE_PROCESS_COMMAND_ECHO set to '",
                    invalid,
                    b"' expected STDERR|STDOUT|NONE",
                ])
            };
            Err(crate::command_error(cx, message))
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
        invalid => Err(CmdError::Status(cat(&[
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
    result_text_for(result, cfg!(windows))
}

fn result_text_for(result: &ProcessResult, windows: bool) -> BString {
    match result {
        ProcessResult::Exited(code) if windows => {
            windows_exception_text(*code).map_or_else(|| code.to_string().into(), BString::from)
        }
        ProcessResult::Exited(code) => code.to_string().into(),
        ProcessResult::TimedOut => TIMEOUT_MESSAGE.into(),
        ProcessResult::Signaled(signal) => signal_text(*signal).into(),
        ProcessResult::SpawnFailed(error) => spawn_error(error).into(),
    }
}

fn windows_exception_text(code: i32) -> Option<String> {
    let code = code.cast_unsigned();
    if (code & 0xF000_0000) != 0xC000_0000 {
        return None;
    }
    let known = match code {
        0xC000_013A => Some("User interrupt"),
        0xC000_008D => Some("Floating-point exception (denormal operand)"),
        0xC000_008E => Some("Divide-by-zero"),
        0xC000_008F => Some("Floating-point exception (inexact result)"),
        0xC000_0090 => Some("Invalid floating-point operation"),
        0xC000_0091 => Some("Floating-point overflow"),
        0xC000_0092 => Some("Floating-point stack check failed"),
        0xC000_0093 => Some("Floating-point underflow"),
        0xC000_02B4 => Some("Floating-point exception (multiple faults)"),
        0xC000_02B5 => Some("Floating-point exception (multiple traps)"),
        0xC000_0094 => Some("Integer divide-by-zero"),
        0xC000_0095 => Some("Integer overflow"),
        0xC000_0005 => Some("Access violation"),
        0xC000_0006 => Some("In-page error"),
        0xC000_0008 => Some("Invalid handle"),
        0xC000_0025 => Some("Noncontinuable exception"),
        0xC000_0026 => Some("Invalid disposition"),
        0xC000_008C => Some("Array bounds exceeded"),
        0xC000_00FD => Some("Stack overflow"),
        0xC000_001D => Some("Illegal instruction"),
        0xC000_0096 => Some("Privileged instruction"),
        _ => None,
    };
    Some(known.map_or_else(|| format!("Exit code 0x{code:x}"), str::to_owned))
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
    fatal_error_for(fatal, results, messages, cfg!(windows))
}

fn fatal_error_for(
    fatal: Fatal,
    results: &[ProcessResult],
    messages: &[BString],
    windows: bool,
) -> Option<BString> {
    if matches!(fatal, Fatal::None) {
        return None;
    }
    if results.contains(&ProcessResult::TimedOut) {
        return Some(TIMEOUT_MESSAGE.into());
    }
    let normal_exit = |result: &ProcessResult| match result {
        ProcessResult::Exited(code) => !(windows && windows_exception_text(*code).is_some()),
        _ => false,
    };
    match fatal {
        Fatal::None => None,
        Fatal::Last => match results.last()? {
            ProcessResult::Exited(0) => None,
            result if normal_exit(result) => Some("last command failed".into()),
            _ => Some(cat(&[b"Abnormal exit: ", messages.last()?])),
        },
        Fatal::Any => {
            let mut failures = Vec::new();
            for (index, (result, message)) in results.iter().zip(messages).enumerate() {
                let label: &[u8] = match result {
                    ProcessResult::Exited(0) => continue,
                    result if normal_exit(result) => b"Child return code: ",
                    _ => b"Abnormal exit with child return code: ",
                };
                failures.push(cat(&[
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

#[cfg(test)]
mod unit_tests {
    use std::path::Path;

    use rcmake_host::{HostResult, ProcessResult, PureHost};

    use super::*;

    #[test]
    fn windows_abnormal_exit_codes_are_classified_cleanly() {
        let crash = ProcessResult::Exited(0xC000_0005_u32.cast_signed());
        let msg = result_text_for(&crash, true);
        assert_eq!(msg, "Access violation");
        assert_eq!(
            fatal_error_for(
                Fatal::Last,
                std::slice::from_ref(&crash),
                std::slice::from_ref(&msg),
                true
            ),
            Some(BString::from("Abnormal exit: Access violation"))
        );
        assert_eq!(
            fatal_error_for(Fatal::Any, &[crash], &[msg], true),
            Some(BString::from(
                "failed command indexes:\n  1: \"Abnormal exit with child return code: Access violation\"\n"
            ))
        );

        let normal_neg = ProcessResult::Exited(-1);
        let neg_msg = result_text_for(&normal_neg, true);
        assert_eq!(neg_msg, "-1");
        assert_eq!(
            fatal_error_for(Fatal::Last, &[normal_neg], &[neg_msg], true),
            Some(BString::from("last command failed"))
        );
    }

    #[test]
    fn decodes_cp437_french_showincludes_and_parses_chcp() {
        assert_eq!(parse_chcp_output(b"Active code page: 437\r\n"), Some(437));
        assert_eq!(
            parse_chcp_output(b"Aktive Codepage: 54936.\r\n"),
            Some(54936)
        );
        assert_eq!(
            decode_codepage_pure(b"Remarque\xff: inclusion du fichier\xff:  ", 437),
            Some("Remarque\u{00a0}: inclusion du fichier\u{00a0}:  ".into())
        );
    }

    #[test]
    fn resolves_windows_extensionless_executable_when_file_exists() -> HostResult<()> {
        let cwd = Path::new("/work");
        let host = PureHost::new(cwd.to_path_buf(), cwd.join("cmake"));
        host.seed_file(&cwd.join("exe"), b"MZ")?;
        host.seed_file(&cwd.join("exe.exe"), b"MZ")?;
        assert_eq!(
            windows_extensionless_program(&host, cwd, b"/work/exe"),
            Some(BString::from("/work/exe."))
        );
        assert_eq!(
            windows_extensionless_program(&host, cwd, b"/work/exe.exe"),
            None
        );
        assert_eq!(
            windows_extensionless_program(&host, cwd, b"/work/missing"),
            None
        );
        Ok(())
    }

    #[test]
    fn windows_encoding_queries_distinct_codepages_and_disables_autorun() -> HostResult<()> {
        let host = PureHost::default();
        host.set_command_handler(|_, request, _| {
            let Some(cmd) = request.pipeline.first() else {
                return Ok(RunOutput::default());
            };
            if cmd.program == "cmd" {
                assert_eq!(cmd.args, vec!["/d", "/c", "chcp"]);
                return Ok(RunOutput {
                    results: vec![ProcessResult::Exited(0)],
                    stdout: b"Active code page: 65001\r\n".to_vec(),
                    ..RunOutput::default()
                });
            }
            if cmd.program == "powershell.exe" {
                let script = cmd.args.last().map(|v| v.as_slice()).unwrap_or_default();
                let out = if script.ends_with(b"OEMCodePage") {
                    b"437\r\n".to_vec()
                } else if script.ends_with(b"ANSICodePage") {
                    b"1252\r\n".to_vec()
                } else {
                    Vec::new()
                };
                return Ok(RunOutput {
                    results: vec![ProcessResult::Exited(0)],
                    stdout: out,
                    ..RunOutput::default()
                });
            }
            Ok(RunOutput::default())
        })?;
        assert_eq!(
            resolve_windows_codepage(&host, ProcessEncoding::Auto),
            65001
        );
        assert_eq!(resolve_windows_codepage(&host, ProcessEncoding::Oem), 437);
        assert_eq!(resolve_windows_codepage(&host, ProcessEncoding::Ansi), 1252);
        Ok(())
    }

    #[test]
    fn formats_windows_ntstatus_exceptions_matching_kwsys() {
        assert_eq!(
            result_text_for(&ProcessResult::Exited(0xC000_0094_u32.cast_signed()), true),
            "Integer divide-by-zero"
        );
        assert_eq!(
            result_text_for(&ProcessResult::Exited(0xC000_008E_u32.cast_signed()), true),
            "Divide-by-zero"
        );
        assert_eq!(
            result_text_for(&ProcessResult::Exited(0xC000_0005_u32.cast_signed()), true),
            "Access violation"
        );
        assert_eq!(
            result_text_for(&ProcessResult::Exited(0xC000_0409_u32.cast_signed()), true),
            "Exit code 0xc0000409"
        );
        // 0x80000003 is not in the 0xC0000000 exception mask in kwsys ProcessWin32.c.
        let breakpoint = 0x8000_0003_u32.cast_signed();
        assert_eq!(
            result_text_for(&ProcessResult::Exited(breakpoint), true),
            breakpoint.to_string()
        );
    }
}
