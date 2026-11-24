use rcmake_eval::{CmdError, Cx};
use rcmake_host::{WriteMode, path};

use super::args::Options;
use super::run::RunResult;
use super::{ProbeDirs, error_text};

pub(super) fn log_probe(
    cx: &Cx<'_>,
    dirs: &ProbeDirs,
    options: &Options<'_>,
    build_var: &[u8],
    exit_code: i32,
    output: &[u8],
    run_result: Option<&RunResult<'_>>,
) -> Result<(), CmdError> {
    use std::fmt::Write as _;

    let directory = path::to_native(&cx.var_or_empty(b"CMAKE_BINARY_DIR")).join("CMakeFiles");
    let kind = if run_result.is_some()
        || cx
            .backtrace()
            .top()
            .and_then(|frame| frame.name.as_deref())
            .is_some_and(|name| name.eq_ignore_ascii_case(b"try_run"))
    {
        "try_run-v1"
    } else {
        "try_compile-v1"
    };
    let cached = !options.has(b"NO_CACHE");
    let mut text = format!("---\nevents:\n  - kind: \"{kind}\"\n    backtrace:");
    let trace = cx.backtrace();
    if trace.is_empty() {
        text.push_str(" []\n");
    } else {
        text.push('\n');
        for frame in trace.frames() {
            let call = format!(
                "{}:{} ({})",
                String::from_utf8_lossy(&frame.file),
                frame.line,
                String::from_utf8_lossy(frame.name.as_deref().unwrap_or_default()),
            );
            let _ = writeln!(text, "      - {}", yaml_string(call.as_bytes()));
        }
    }
    if let Some(description) = options.one(b"LOG_DESCRIPTION") {
        let _ = writeln!(text, "    description: {}", yaml_string(description));
    }
    let _ = writeln!(
        text,
        "    directories:\n      source: {}\n      binary: {}\n    buildResult:\n      variable: {}\n      cached: {cached}\n      exitCode: {exit_code}",
        yaml_string(&path::from_native(&dirs.source)),
        yaml_string(&path::from_native(&dirs.binary)),
        yaml_string(build_var),
    );
    log_output(&mut text, "stdout", output);
    if let Some(run) = run_result {
        let _ = writeln!(
            text,
            "    runResult:\n      variable: {}\n      cached: {cached}\n      exitCode: {}",
            yaml_string(run.variable),
            run.value,
        );
        log_output(&mut text, "stdout", &run.stdout);
        log_output(&mut text, "stderr", &run.stderr);
    }
    text.push_str("...\n");
    cx.host().create_dir_all(&directory).map_err(error_text)?;
    cx.host()
        .write(
            &directory.join("CMakeConfigureLog.yaml"),
            text.as_bytes(),
            WriteMode::Append,
        )
        .map_err(error_text)
}

fn yaml_string(value: &[u8]) -> String {
    use std::fmt::Write as _;

    let mut quoted = String::from("\"");
    for &byte in value {
        match byte {
            b'\\' | b'"' => {
                quoted.push('\\');
                quoted.push(char::from(byte));
            }
            b' '..=b'~' => quoted.push(char::from(byte)),
            _ => {
                let _ = write!(quoted, "\\x{byte:02x}");
            }
        }
    }
    quoted.push('"');
    quoted
}

fn log_output(text: &mut String, key: &str, output: &[u8]) {
    use std::fmt::Write as _;

    let _ = writeln!(
        text,
        "      {key}: |2{}",
        if output.ends_with(b"\n") { "+" } else { "-" }
    );
    for line in output.split_inclusive(|&byte| byte == b'\n') {
        text.push_str("        ");
        for &byte in line.strip_suffix(b"\n").unwrap_or(line) {
            match byte {
                b'\\' => text.push_str("\\\\"),
                b' '..=b'~' => text.push(char::from(byte)),
                _ => {
                    let _ = write!(text, "\\x{byte:02x}");
                }
            }
        }
        text.push('\n');
    }
}
