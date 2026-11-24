//! Write configure log events from the evaluator.

use std::path::Path;

use rcmake_base::BString;
use rcmake_diag::FrameKind;
use rcmake_host::WriteMode;

use crate::{Cx, Interp, RunMode, hooks::json_string, util::cat};

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

impl Cx<'_> {
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
        let out = &mut self.configure_log;
        if !self.checks.is_empty() {
            out.extend_from_slice(b"    checks:\n");
            for check in self.checks.iter().rev() {
                yaml_item(out, check);
            }
        }
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
