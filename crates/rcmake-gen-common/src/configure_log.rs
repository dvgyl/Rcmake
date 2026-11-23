//! `CMakeFiles/CMakeConfigureLog.yaml`.

use std::path::Path;

use rcmake_base::BString;
use rcmake_host::{Host, WriteMode};
use rcmake_resolve::ConfigureMetadata;

use crate::{GenError, write_error};

/// A configure log event and its source context.
#[derive(Clone, Copy, Debug)]
pub struct ConfigureEvent<'a> {
    /// Formatted source locations, from the most recent frame to the oldest.
    pub backtrace: &'a [BString],
    /// Pending checks, from the most recent check to the oldest.
    pub checks: &'a [BString],
    /// Event contents.
    pub kind: EventKind<'a>,
}

/// The configure log event kinds supported by this writer.
#[derive(Clone, Copy, Debug)]
pub enum EventKind<'a> {
    /// Text from `message(CONFIGURE_LOG)`.
    Message(&'a [u8]),
    /// Result of building a test project.
    TryCompile(TryCompile<'a>),
    /// Results of building a test project and running its executable.
    TryRun {
        /// Test project and build result.
        compile: TryCompile<'a>,
        /// Execution result.
        run: RunResult<'a>,
    },
}

/// Common fields of `try_compile` and `try_run` events.
#[derive(Clone, Copy, Debug)]
pub struct TryCompile<'a> {
    /// Optional `LOG_DESCRIPTION`.
    pub description: Option<&'a [u8]>,
    /// Test project source directory.
    pub source_dir: &'a [u8],
    /// Test project binary directory.
    pub binary_dir: &'a [u8],
    /// CMake variables passed into the test project, in output order.
    pub cmake_variables: &'a [(BString, BString)],
    /// Build result.
    pub build_result: BuildResult<'a>,
}

/// Result of building a test project.
#[derive(Clone, Copy, Debug)]
pub struct BuildResult<'a> {
    /// CMake variable that stores the result.
    pub variable: &'a [u8],
    /// Whether the variable is stored in the cache.
    pub cached: bool,
    /// Combined standard output and standard error from the build.
    pub stdout: &'a [u8],
    /// Build tool exit code.
    pub exit_code: i32,
}

/// Result of running the executable of a test project.
#[derive(Clone, Copy, Debug)]
pub struct RunResult<'a> {
    /// CMake variable that stores the result.
    pub variable: &'a [u8],
    /// Whether the variable is stored in the cache.
    pub cached: bool,
    /// Captured standard output, or combined output when requested.
    pub stdout: Option<&'a [u8]>,
    /// Standard error when captured separately.
    pub stderr: Option<&'a [u8]>,
    /// Process exit status.
    pub exit_code: Option<RunExitCode<'a>>,
}

/// Numeric exit code or CMake status text such as `FAILED_TO_RUN`.
#[derive(Clone, Copy, Debug)]
pub enum RunExitCode<'a> {
    /// Process exit code.
    Code(i32),
    /// Status reported when execution failed.
    Status(&'a [u8]),
}

/// Render an empty configure log document for metadata without recorded events.
pub fn render(_metadata: &ConfigureMetadata) -> Result<BString, GenError> {
    Ok(render_events(&[]))
}

/// Render one configure run as a YAML document.
#[must_use]
pub fn render_events(events: &[ConfigureEvent<'_>]) -> BString {
    let mut out = BString::from("\n---\nevents:");
    if events.is_empty() {
        out.extend_from_slice(b" []");
    }
    out.push(b'\n');
    for event in events {
        render_event(&mut out, event);
    }
    out.extend_from_slice(b"...\n");
    out
}

/// Append all events from one configure run. An empty run leaves the log untouched.
pub fn write_configure_log(
    events: &[ConfigureEvent<'_>],
    build_dir: &Path,
    host: &dyn Host,
) -> Result<(), GenError> {
    if events.is_empty() {
        return Ok(());
    }
    let dir = build_dir.join("CMakeFiles");
    host.create_dir_all(&dir)
        .map_err(|err| write_error(&dir, err))?;
    let path = dir.join("CMakeConfigureLog.yaml");
    host.write(&path, &render_events(events), WriteMode::Append)
        .map_err(|err| write_error(&path, err))
}

/// Append an event as a complete one-event configure log document.
pub fn append_event(
    event: &ConfigureEvent<'_>,
    build_dir: &Path,
    host: &dyn Host,
) -> Result<(), GenError> {
    write_configure_log(std::slice::from_ref(event), build_dir, host)
}

fn render_event(out: &mut BString, event: &ConfigureEvent<'_>) {
    let name = match event.kind {
        EventKind::Message(_) => "message-v1",
        EventKind::TryCompile(_) => "try_compile-v1",
        EventKind::TryRun { .. } => "try_run-v1",
    };
    out.extend_from_slice(b"  -\n");
    scalar(out, b"    kind: ", name.as_bytes());
    sequence(out, b"    backtrace:", event.backtrace);
    if !event.checks.is_empty() {
        sequence(out, b"    checks:", event.checks);
    }
    match event.kind {
        EventKind::Message(message) => text_block(out, b"    message: ", b"      ", message),
        EventKind::TryCompile(compile) => render_compile(out, &compile),
        EventKind::TryRun { compile, run } => {
            render_compile(out, &compile);
            out.extend_from_slice(b"    runResult:\n");
            result_header(out, run.variable, run.cached);
            if let Some(stdout) = run.stdout {
                text_block(out, b"      stdout: ", b"        ", stdout);
            }
            if let Some(stderr) = run.stderr {
                text_block(out, b"      stderr: ", b"        ", stderr);
            }
            match run.exit_code {
                Some(RunExitCode::Code(code)) => exit_code(out, code),
                Some(RunExitCode::Status(status)) => scalar(out, b"      exitCode: ", status),
                None => {}
            }
        }
    }
}

fn render_compile(out: &mut BString, compile: &TryCompile<'_>) {
    if let Some(description) = compile.description {
        scalar(out, b"    description: ", description);
    }
    out.extend_from_slice(b"    directories:\n");
    scalar(out, b"      source: ", compile.source_dir);
    scalar(out, b"      binary: ", compile.binary_dir);
    if !compile.cmake_variables.is_empty() {
        out.extend_from_slice(b"    cmakeVariables:\n");
        for (key, value) in compile.cmake_variables {
            out.extend_from_slice(b"      ");
            if key.starts_with(b"CMAKE_")
                && key
                    .iter()
                    .all(|&byte| byte.is_ascii_alphanumeric() || byte == b'_')
            {
                out.extend_from_slice(key);
            } else {
                yaml_string(out, key);
            }
            scalar(out, b": ", value);
        }
    }
    out.extend_from_slice(b"    buildResult:\n");
    let build = &compile.build_result;
    result_header(out, build.variable, build.cached);
    text_block(out, b"      stdout: ", b"        ", build.stdout);
    exit_code(out, build.exit_code);
}

fn result_header(out: &mut BString, variable: &[u8], cached: bool) {
    scalar(out, b"      variable: ", variable);
    out.extend_from_slice(if cached {
        b"      cached: true\n"
    } else {
        b"      cached: false\n"
    });
}

fn exit_code(out: &mut BString, code: i32) {
    out.extend_from_slice(format!("      exitCode: {code}\n").as_bytes());
}

fn sequence(out: &mut BString, label: &[u8], items: &[BString]) {
    out.extend_from_slice(label);
    if items.is_empty() {
        out.extend_from_slice(b" []");
    }
    out.push(b'\n');
    for item in items {
        scalar(out, b"      - ", item);
    }
}

fn scalar(out: &mut BString, label: &[u8], value: &[u8]) {
    out.extend_from_slice(label);
    yaml_string(out, value);
    out.push(b'\n');
}

fn yaml_string(out: &mut BString, value: &[u8]) {
    out.push(b'"');
    for chunk in value.utf8_chunks() {
        for ch in chunk.valid().chars() {
            match ch {
                '"' => out.extend_from_slice(br#"\""#),
                '\\' => out.extend_from_slice(br"\\"),
                '\n' => out.extend_from_slice(br"\n"),
                '\r' => out.extend_from_slice(br"\r"),
                '\t' => out.extend_from_slice(br"\t"),
                ch if yaml_plain_char(ch) => push_char(out, ch),
                ch => out.extend_from_slice(format!("\\u{:04x}", u32::from(ch)).as_bytes()),
            }
        }
        for &byte in chunk.invalid() {
            hex_byte(out, byte);
        }
    }
    out.push(b'"');
}

fn text_block(out: &mut BString, label: &[u8], indent: &[u8], value: &[u8]) {
    out.extend_from_slice(label);
    out.push(b'|');
    if value
        .split(|&byte| byte == b'\n')
        .find(|line| !line.is_empty())
        .is_some_and(|line| line.starts_with(b" "))
    {
        out.push(b'2');
    }
    out.push(b'\n');
    let value = value.strip_suffix(b"\n").unwrap_or(value);
    if value.is_empty() {
        return;
    }
    for line in value.split(|&byte| byte == b'\n') {
        out.extend_from_slice(indent);
        for chunk in line.utf8_chunks() {
            for ch in chunk.valid().chars() {
                if ch == '\\' {
                    out.extend_from_slice(br"\\");
                } else if yaml_plain_char(ch) || ch == '\t' {
                    push_char(out, ch);
                } else {
                    for &byte in ch.encode_utf8(&mut [0; 4]).as_bytes() {
                        hex_byte(out, byte);
                    }
                }
            }
            for &byte in chunk.invalid() {
                hex_byte(out, byte);
            }
        }
        out.push(b'\n');
    }
}

fn yaml_plain_char(ch: char) -> bool {
    matches!(ch, '\u{20}'..='\u{7e}' | '\u{a0}'..='\u{d7ff}' | '\u{e000}'..='\u{fffd}' | '\u{10000}'..='\u{10ffff}')
        && !matches!(ch, '\u{2028}' | '\u{2029}' | '\u{feff}')
}

fn hex_byte(out: &mut BString, byte: u8) {
    out.extend_from_slice(format!("\\x{byte:02x}").as_bytes());
}

fn push_char(out: &mut BString, ch: char) {
    out.extend_from_slice(ch.encode_utf8(&mut [0; 4]).as_bytes());
}
