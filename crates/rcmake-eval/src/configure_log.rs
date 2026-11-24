//! Write configure log events from the evaluator.

use std::path::Path;

use rcmake_diag::FrameKind;
use rcmake_host::WriteMode;

use crate::{Interp, RunMode, exec::cat, hooks::json_string};

impl Interp {
    pub(crate) fn log_configure_message(&mut self, message: &[u8]) {
        if self.config.mode != RunMode::Configure {
            return;
        }
        let out = &mut self.configure_log;
        out.extend_from_slice(b"  -\n    kind: \"message-v1\"\n    backtrace:\n");
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
