//! Write configure log events from the evaluator.

use std::path::Path;

use rcmake_base::BString;
use rcmake_diag::FrameKind;
use rcmake_host::WriteMode;

use crate::{Cx, Interp, RunMode, hooks::json_string, util::cat};

mod find_package;

/// Fields recorded for a configure-time search.
pub struct FindEvent<'a> {
    /// The search mode, such as `program` or `file`.
    pub mode: &'a str,
    /// Name of the variable that receives the result.
    pub variable: &'a [u8],
    /// Search documentation string.
    pub description: &'a [u8],
    /// Framework search order.
    pub framework: &'a [u8],
    /// Application bundle search order.
    pub app_bundle: &'a [u8],
    /// Search paths that were enabled.
    pub settings: &'a [(&'a str, bool)],
    /// Names in search order.
    pub names: &'a [BString],
    /// Candidate directories in search order.
    pub candidates: &'a [BString],
    /// Failed paths in search order.
    pub attempted: &'a [BString],
    /// Path returned by the search.
    pub found: Option<&'a [u8]>,
}

/// Fields recorded for a `try_compile()` or `try_run()` call.
pub struct TryCompileEvent<'a> {
    /// `try_compile-v1` or `try_run-v1`.
    pub kind: &'a str,
    /// `LOG_DESCRIPTION` text.
    pub description: Option<&'a [u8]>,
    /// Source directory of the test project.
    pub source_dir: &'a [u8],
    /// Binary directory of the test project.
    pub binary_dir: &'a [u8],
    /// Variables that the call passes to the test project, sorted by name.
    pub variables: &'a [(BString, BString)],
    /// Build result of the test project.
    pub build: TryBuildOutcome<'a>,
    /// Run result of a `try_run()` call whose build succeeded or failed.
    pub run: Option<TryRunOutcome<'a>>,
}

/// Result variable, cache flag, captured output, and exit code of one step.
pub struct TryBuildOutcome<'a> {
    /// Result variable name.
    pub variable: &'a [u8],
    /// `true` when the result is stored in the cache.
    pub cached: bool,
    /// Captured build output.
    pub stdout: &'a [u8],
    /// Build tool exit code.
    pub exit_code: i32,
}

/// Run step of a `try_run()` event.
pub struct TryRunOutcome<'a> {
    /// Result variable name.
    pub variable: &'a [u8],
    /// `true` when the result is stored in the cache.
    pub cached: bool,
    /// Standard output, or the merged output.
    pub stdout: Option<&'a [u8]>,
    /// Standard error when it is captured separately.
    pub stderr: Option<&'a [u8]>,
    /// Exit code text. It is absent when the test executable did not run.
    pub exit_code: Option<&'a [u8]>,
}

impl Cx<'_> {
    /// Add a `try_compile-v1` or `try_run-v1` event to the configure log.
    pub fn log_configure_try_compile(&mut self, event: TryCompileEvent<'_>) {
        if self.mode() != RunMode::Configure {
            return;
        }
        self.interp.start_configure_event(event.kind.as_bytes());
        self.interp.write_configure_checks();
        let out = &mut self.interp.configure_log;
        if let Some(description) = event.description {
            yaml_value(out, b"    description", description);
        }
        out.extend_from_slice(b"    directories:\n");
        yaml_value(out, b"      source", event.source_dir);
        yaml_value(out, b"      binary", event.binary_dir);
        if !event.variables.is_empty() {
            out.extend_from_slice(b"    cmakeVariables:\n");
            for (name, value) in event.variables {
                let mut key = b"      ".to_vec();
                if name
                    .iter()
                    .all(|&byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
                {
                    key.extend_from_slice(name);
                } else {
                    json_string(&mut key, name);
                }
                yaml_value(out, &key, value);
            }
        }
        out.extend_from_slice(b"    buildResult:\n");
        yaml_value(out, b"      variable", event.build.variable);
        yaml_bool(out, b"      cached", event.build.cached);
        yaml_block(out, b"      stdout", event.build.stdout);
        out.extend_from_slice(format!("      exitCode: {}\n", event.build.exit_code).as_bytes());
        if let Some(run) = event.run {
            out.extend_from_slice(b"    runResult:\n");
            yaml_value(out, b"      variable", run.variable);
            yaml_bool(out, b"      cached", run.cached);
            if let Some(stdout) = run.stdout {
                yaml_block(out, b"      stdout", stdout);
            }
            if let Some(stderr) = run.stderr {
                yaml_block(out, b"      stderr", stderr);
            }
            if let Some(code) = run.exit_code {
                out.extend_from_slice(b"      exitCode: ");
                out.extend_from_slice(code);
                out.push(b'\n');
            }
        }
    }

    /// Add a find event to the configure log.
    pub fn log_configure_find(&mut self, event: FindEvent<'_>) {
        if self.mode() != RunMode::Configure {
            return;
        }
        let context = [
            (
                "CMAKE_CURRENT_SOURCE_DIR",
                self.var_or_empty(b"CMAKE_CURRENT_SOURCE_DIR"),
            ),
            (
                "CMAKE_CURRENT_BINARY_DIR",
                self.var_or_empty(b"CMAKE_CURRENT_BINARY_DIR"),
            ),
            ("CMAKE_PREFIX_PATH", self.var_or_empty(b"CMAKE_PREFIX_PATH")),
        ];
        self.interp.start_configure_event(b"find-v1");
        let out = &mut self.interp.configure_log;
        yaml_value(out, b"    mode", event.mode.as_bytes());
        yaml_value(out, b"    variable", event.variable);
        yaml_value(out, b"    description", event.description);
        out.extend_from_slice(b"    settings:\n");
        yaml_value(out, b"      SearchFramework", event.framework);
        yaml_value(out, b"      SearchAppBundle", event.app_bundle);
        for (name, enabled) in event.settings {
            out.extend_from_slice(b"      ");
            out.extend_from_slice(name.as_bytes());
            out.extend_from_slice(if *enabled { b": true\n" } else { b": false\n" });
        }
        yaml_list(out, b"names", event.names);
        yaml_list(out, b"candidate_directories", event.candidates);
        yaml_list(out, b"searched_directories", event.attempted);
        if let Some(found) = event.found {
            yaml_value(out, b"    found", found);
        } else {
            out.extend_from_slice(b"    found: false\n");
        }
        out.extend_from_slice(b"    search_context:\n");
        for (key, value) in context {
            let key = [b"      ".as_slice(), key.as_bytes()].concat();
            yaml_value(out, &key, &value);
        }
    }
}

impl Interp {
    pub(crate) fn log_configure_message(&mut self, message: &[u8]) {
        if self.config.mode != RunMode::Configure {
            return;
        }
        self.start_configure_event(b"message-v1");
        self.write_configure_checks();
        let out = &mut self.configure_log;
        out.extend_from_slice(b"    message: |\n");
        let message = message.strip_suffix(b"\n").unwrap_or(message);
        if !message.is_empty() {
            for line in message.split(|&byte| byte == b'\n') {
                out.extend_from_slice(b"      ");
                out.extend_from_slice(line);
                out.push(b'\n');
            }
        }
    }

    /// Active `message(CHECK_START)` checks, innermost first.
    fn write_configure_checks(&mut self) {
        if !self.checks.is_empty() {
            self.configure_log.extend_from_slice(b"    checks:\n");
            for check in self.checks.iter().rev() {
                yaml_item(&mut self.configure_log, check);
            }
        }
    }

    fn start_configure_event(&mut self, kind: &[u8]) {
        let out = &mut self.configure_log;
        out.extend_from_slice(b"  -\n");
        yaml_value(out, b"    kind", kind);
        out.extend_from_slice(b"    backtrace:\n");
        for frame in self
            .bt
            .frames()
            .filter(|frame| frame.kind != FrameKind::ModuleInclude)
        {
            let file = self
                .render
                .relative_base
                .as_ref()
                .and_then(|base| frame.file.strip_prefix(base.as_slice())?.strip_prefix(b"/"))
                .unwrap_or(&frame.file);
            let line = if frame.kind == FrameKind::Deferred {
                "DEFERRED".to_owned()
            } else {
                frame.line.to_string()
            };
            let mut location = cat(&[file, b":", line.as_bytes()]);
            if let Some(name) = &frame.name {
                location.extend_from_slice(b" (");
                location.extend_from_slice(name);
                location.push(b')');
            }
            yaml_item(out, &location);
        }
    }

    pub(crate) fn write_configure_log(&mut self, build_dir: &Path) {
        let events = core::mem::take(&mut self.configure_log);
        if events.is_empty() {
            return;
        }
        let dir = build_dir.join("CMakeFiles");
        let document = cat(&[b"\n---\nevents:\n", &events, b"...\n"]);
        if let Err(error) = self.config.host.create_dir_all(&dir).and_then(|()| {
            self.config.host.write(
                &dir.join("CMakeConfigureLog.yaml"),
                &document,
                WriteMode::Append,
            )
        }) {
            self.error_no_location(format!("Could not write the configure log: {error}").into());
        }
    }
}

fn yaml_item(out: &mut Vec<u8>, item: &[u8]) {
    out.extend_from_slice(b"      - ");
    json_string(out, item);
    out.push(b'\n');
}

fn yaml_value(out: &mut Vec<u8>, key: &[u8], value: &[u8]) {
    out.extend_from_slice(key);
    out.extend_from_slice(b": ");
    json_string(out, value);
    out.push(b'\n');
}

fn yaml_list(out: &mut Vec<u8>, name: &[u8], values: &[BString]) {
    if !values.is_empty() {
        out.extend_from_slice(b"    ");
        out.extend_from_slice(name);
        out.extend_from_slice(b":\n");
        for value in values {
            yaml_item(out, value);
        }
    }
}

fn yaml_bool(out: &mut Vec<u8>, key: &[u8], value: bool) {
    out.extend_from_slice(key);
    out.extend_from_slice(if value { b": true\n" } else { b": false\n" });
}

/// Literal block scalar (`key: |`). Backslashes and control characters other than tab
/// are escaped.
fn yaml_block(out: &mut Vec<u8>, key: &[u8], text: &[u8]) {
    out.extend_from_slice(key);
    out.extend_from_slice(b": |\n");
    let text = text.strip_suffix(b"\n").unwrap_or(text);
    if text.is_empty() {
        return;
    }
    for line in text.split(|&byte| byte == b'\n') {
        out.extend_from_slice(b"        ");
        for &byte in line.strip_suffix(b"\r").unwrap_or(line) {
            if byte == b'\\' {
                out.extend_from_slice(b"\\\\");
            } else if byte < b' ' && byte != b'\t' {
                out.extend_from_slice(format!("\\x{byte:02x}").as_bytes());
            } else {
                out.push(byte);
            }
        }
        out.push(b'\n');
    }
}
