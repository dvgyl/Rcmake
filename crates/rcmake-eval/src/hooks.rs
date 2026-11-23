//! Evaluator hooks: variable watches (`variable_watch()`), command tracing
//! (`--trace`, `--trace-expand`, `--trace-format`, `--trace-source`,
//! `--trace-redirect`), and deferred calls (`cmake_language(DEFER)`).

use std::path::PathBuf;
use std::sync::Arc;

use rcmake_base::BString;
use rcmake_diag::{Effect, Frame, FrameKind, MessageKind, PolicyStatus};
use rcmake_host::{Stream, WriteMode};
use rcmake_syntax::{ArgKind, Argument, CommandInvocation};

use crate::Interp;
use crate::code::Code;
use crate::exec::cat;

/// Line number that CMake gives to the synthetic command of a variable watch callback
/// (`std::numeric_limits<long>::max()`). [`display_line`] prints it as CMake does.
pub(crate) const WATCH_CALLBACK_LINE: u32 = u32::MAX;

/// Line number text as CMake prints it. The watch callback line is CMake's `long` maximum.
pub(crate) fn display_line(line: u32) -> String {
    if line == WATCH_CALLBACK_LINE {
        if cfg!(windows) {
            "2147483647".to_owned()
        } else {
            "9223372036854775807".to_owned()
        }
    } else {
        line.to_string()
    }
}

/// Kind of variable access that is reported to a variable watch
/// (`cmVariableWatch::AccessType`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WatchAccess {
    /// A defined variable was read.
    Read,
    /// An undefined variable was read.
    UnknownRead,
    /// `if(DEFINED)` (or a similar query) tested an undefined variable.
    UnknownDefined,
    /// The variable was set.
    Modified,
    /// The variable was unset.
    Removed,
}

impl WatchAccess {
    /// Access name as CMake passes it to the callback (for example `MODIFIED_ACCESS`).
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Read => "READ_ACCESS",
            Self::UnknownRead => "UNKNOWN_READ_ACCESS",
            Self::UnknownDefined => "UNKNOWN_DEFINED_ACCESS",
            Self::Modified => "MODIFIED_ACCESS",
            Self::Removed => "REMOVED_ACCESS",
        }
    }
}

/// One registered variable watch.
#[derive(Debug)]
pub(crate) struct Watch {
    id: u64,
    /// Callback command name. An empty name prints the default log message.
    command: BString,
    /// The callback of this watch is running (`InCallback`).
    in_callback: bool,
}

/// Output format of command tracing (`--trace-format`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum TraceFormat {
    /// `--trace-format=human` (default): `<file>(<line>):  <command>(<args> )`.
    #[default]
    Human,
    /// `--trace-format=json-v1`: one JSON object per line.
    JsonV1,
}

/// Command tracing configuration (`--trace` and related options).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TraceConfig {
    /// Output format.
    pub format: TraceFormat,
    /// Expand variable references in non-bracket arguments (`--trace-expand`).
    pub expand: bool,
    /// Trace only commands in files that match one of these paths (`--trace-source`).
    /// An empty list traces all files.
    pub sources: Vec<BString>,
    /// Write trace lines to this file instead of stderr (`--trace-redirect`).
    pub redirect: Option<PathBuf>,
}

/// A call queued by `cmake_language(DEFER CALL)`.
#[derive(Clone, Debug)]
pub(crate) struct DeferredCall {
    /// Call identifier. An empty identifier marks a canceled or started call.
    id: BString,
    /// File that scheduled the call.
    file: Arc<[u8]>,
    /// The command with its raw (unexpanded) arguments.
    cmd: CommandInvocation,
}

/// Append `s` to `out` as a JSON string (JsonCpp escapes: no raw control or non-ASCII
/// bytes).
fn json_string(out: &mut Vec<u8>, s: &[u8]) {
    out.push(b'"');
    for ch in String::from_utf8_lossy(s).chars() {
        match ch {
            '"' => out.extend_from_slice(b"\\\""),
            '\\' => out.extend_from_slice(b"\\\\"),
            '\u{8}' => out.extend_from_slice(b"\\b"),
            '\u{c}' => out.extend_from_slice(b"\\f"),
            '\n' => out.extend_from_slice(b"\\n"),
            '\r' => out.extend_from_slice(b"\\r"),
            '\t' => out.extend_from_slice(b"\\t"),
            c if c.is_ascii() && !c.is_ascii_control() => out.push(c as u8),
            c => {
                let mut units = [0u16; 2];
                for unit in c.encode_utf16(&mut units) {
                    out.extend_from_slice(format!("\\u{unit:04x}").as_bytes());
                }
            }
        }
    }
    out.push(b'"');
}

/// Seconds since the epoch with 17 significant digits (JsonCpp `double` output).
fn json_time(secs: i64, nanos: u32) -> String {
    let int_digits = secs.unsigned_abs().to_string().len();
    let decimals = 17usize.saturating_sub(int_digits).min(9);
    let frac = u64::from(nanos) / 10u64.pow(9 - u32::try_from(decimals).unwrap_or(9));
    if decimals == 0 {
        format!("{secs}.0")
    } else {
        format!("{secs}.{frac:0decimals$}")
    }
}

/// `true` if `--trace-source=<source>` selects `path` (`PrintCommandTrace`).
fn trace_source_matches(path: &[u8], source: &[u8]) -> bool {
    let name = |p: &[u8]| -> Vec<u8> {
        p.rsplit(|&b| b == b'/' || b == b'\\')
            .next()
            .unwrap_or(p)
            .to_vec()
    };
    path.ends_with(source) && name(path) == name(source)
}

impl Interp {
    // ----------------------------------------------------------------------------------
    // Variable watches.
    // ----------------------------------------------------------------------------------

    /// `cmVariableWatch::AddWatch`: every call adds a new watch.
    pub(crate) fn add_watch(&mut self, var: &[u8], command: &[u8]) {
        self.next_watch_id = self.next_watch_id.saturating_add(1);
        let watch = Watch {
            id: self.next_watch_id,
            command: BString::from(command),
            in_callback: false,
        };
        self.watches
            .entry(BString::from(var))
            .or_default()
            .push(watch);
    }

    /// Remove the watches of `var` whose callback is `command` (all watches of `var` when
    /// `command` is `None`). Returns `true` if a watch was removed.
    pub(crate) fn remove_watch(&mut self, var: &[u8], command: Option<&[u8]>) -> bool {
        let Some(list) = self.watches.get_mut(var) else {
            return false;
        };
        let before = list.len();
        list.retain(|w| command.is_some_and(|c| w.command.as_slice() != c));
        let removed = list.len() != before;
        if list.is_empty() {
            self.watches.remove(var);
        }
        removed
    }

    /// Callback commands of the watches of `var` (an empty name means the default log
    /// message), in registration order.
    pub(crate) fn watch_commands(&self, var: &[u8]) -> Vec<BString> {
        self.watches
            .get(var)
            .map(|list| list.iter().map(|w| w.command.clone()).collect())
            .unwrap_or_default()
    }

    /// `cmVariableWatch::VariableAccessed`: run the callbacks of the watches that exist
    /// now. Watches added by a callback do not run for this access. Returns `true` if
    /// `name` has watches.
    pub(crate) fn fire_watches(
        &mut self,
        name: &[u8],
        access: WatchAccess,
        value: Option<&[u8]>,
    ) -> bool {
        if self.watches.is_empty() {
            return false;
        }
        let Some(list) = self.watches.get(name) else {
            return false;
        };
        let ids: Vec<u64> = list.iter().map(|w| w.id).collect();
        let value = value.map(BString::from);
        for id in ids {
            let Some(command) = self.set_in_callback(name, id, true) else {
                continue;
            };
            self.run_watch_callback(name, access, value.as_ref().map(|v| v.as_slice()), &command);
            let _ = self.set_in_callback(name, id, false);
        }
        true
    }

    /// Set the `InCallback` flag of watch `id`. When `on` is `true`, returns the callback
    /// command, or `None` if the watch is gone or its callback is already running.
    fn set_in_callback(&mut self, name: &[u8], id: u64, on: bool) -> Option<BString> {
        let watch = self
            .watches
            .get_mut(name)?
            .iter_mut()
            .find(|w| w.id == id)?;
        if on && watch.in_callback {
            return None;
        }
        watch.in_callback = on;
        Some(watch.command.clone())
    }

    /// `cmVariableWatchCommandVariableAccessed`.
    fn run_watch_callback(
        &mut self,
        name: &[u8],
        access: WatchAccess,
        value: Option<&[u8]>,
        command: &BString,
    ) {
        let value = value.unwrap_or_default();
        if command.is_empty() {
            let body = cat(&[
                b"Variable \"",
                name,
                b"\" was accessed using ",
                access.as_str().as_bytes(),
                b" with value \"",
                value,
                b"\".",
            ]);
            self.issue_here(MessageKind::Log, Effect::None, None, body, false);
            return;
        }
        let current = self
            .lookup_var(b"CMAKE_CURRENT_LIST_FILE")
            .cloned()
            .unwrap_or_default();
        let mut values = vec![
            BString::from(name),
            BString::from(access.as_str()),
            BString::from(value),
            current,
            self.listfile_stack(),
        ];
        match self.check_cmp0219(command, &values) {
            PolicyStatus::New => {
                for v in &mut values {
                    *v = crate::util::replace_all(v, b"\\", b"\\\\").into();
                }
            }
            PolicyStatus::Warn => self.issue_cmp0219_warning(command, &values),
            PolicyStatus::Old => {}
        }
        let args = values
            .into_iter()
            .map(|text| Argument {
                kind: ArgKind::Quoted,
                text,
                line: WATCH_CALLBACK_LINE,
                span: 0..0,
            })
            .collect();
        let file = self
            .bt
            .top()
            .map_or_else(|| Arc::from(&b""[..]), |f| Arc::clone(&f.file));
        let code = synthetic_code(command, args, WATCH_CALLBACK_LINE, file);
        if self.exec_command(&code, 0).is_err() {
            self.error_no_location(cat(&[
                b"Error in cmake code at\nUnknown:0:\nA command failed during the invocation of callback \"",
                command,
                b"\".",
            ]));
        }
    }

    /// `LISTFILE_STACK`: the listfile of each file and function call scope, outermost
    /// first.
    pub(crate) fn listfile_stack(&self) -> BString {
        let mut out = Vec::new();
        for (i, file) in self.listfiles.iter().enumerate() {
            if i > 0 {
                out.push(b';');
            }
            out.extend_from_slice(file);
        }
        BString::from(out)
    }

    /// Define a normal variable and notify its watches (`cmMakefile::AddDefinition`).
    pub(crate) fn define(&mut self, name: &[u8], value: BString) {
        if self.watches.contains_key(name) {
            self.vars.set(name, value.clone());
            self.fire_watches(name, WatchAccess::Modified, Some(&value));
        } else {
            self.vars.set(name, value);
        }
    }

    /// Undefine a normal variable and notify its watches (`RemoveDefinition`).
    pub(crate) fn undefine(&mut self, name: &[u8]) {
        self.vars.unset(name);
        self.fire_watches(name, WatchAccess::Removed, None);
    }

    /// Read a variable and notify its watches (`cmMakefile::GetDefinition`).
    pub(crate) fn read_var(&mut self, name: &[u8]) -> Option<BString> {
        let value = self.lookup_var(name).cloned();
        if !self.watches.contains_key(name) {
            return value;
        }
        let access = if value.is_some() {
            WatchAccess::Read
        } else {
            WatchAccess::UnknownRead
        };
        self.fire_watches(name, access, value.as_ref().map(|v| v.as_slice()));
        self.lookup_var(name).cloned()
    }

    // ----------------------------------------------------------------------------------
    // Tracing.
    // ----------------------------------------------------------------------------------

    /// Install or remove the trace configuration. With `--trace-redirect`, truncate the
    /// output file. With JSON output, write the format version line.
    pub(crate) fn install_trace(&mut self, config: Option<TraceConfig>) {
        self.trace = config;
        let Some(trace) = &self.trace else { return };
        if let Some(path) = &trace.redirect {
            let _ = self.config.host.write(path, b"", WriteMode::Truncate);
        }
        if trace.format == TraceFormat::JsonV1 {
            self.write_trace_line(b"{\"version\":{\"major\":1,\"minor\":2}}");
        }
    }

    fn write_trace_line(&self, line: &[u8]) {
        let Some(trace) = &self.trace else { return };
        let mut bytes = line.to_vec();
        bytes.push(b'\n');
        match &trace.redirect {
            Some(path) => {
                let _ = self.config.host.write(path, &bytes, WriteMode::Append);
            }
            None => self.config.host.write_out(Stream::Stderr, &bytes),
        }
    }

    /// `cmMakefile::PrintCommandTrace` for `cmd` in `file`. `missing` adds one frame
    /// for end commands, which are not on the call stack (`CommandMissingFromStack`).
    pub(crate) fn trace_command(&mut self, cmd: &CommandInvocation, file: &[u8], missing: bool) {
        let Some(trace) = &self.trace else { return };
        if !trace.sources.is_empty() && !trace.sources.iter().any(|s| trace_source_matches(file, s))
        {
            return;
        }
        let (format, expand) = (trace.format, trace.expand);
        let mut args: Vec<BString> = Vec::with_capacity(cmd.args.len());
        for arg in &cmd.args {
            if expand && arg.kind != ArgKind::Bracket {
                let expand_string = self.frontend.expand_string;
                self.trace_expanding = true;
                let value = expand_string(&arg.text, self).unwrap_or_else(|_| arg.text.clone());
                self.trace_expanding = false;
                args.push(value);
            } else {
                args.push(arg.text.clone());
            }
        }
        let defer_id = self.top_defer_id.clone();
        let mut out = Vec::new();
        match format {
            TraceFormat::Human => {
                out.extend_from_slice(file);
                out.extend_from_slice(format!("({}):", display_line(cmd.line)).as_bytes());
                if let Some(id) = &defer_id {
                    out.extend_from_slice(b"DEFERRED:");
                    out.extend_from_slice(id);
                    out.push(b':');
                }
                out.extend_from_slice(b"  ");
                out.extend_from_slice(&cmd.name);
                out.push(b'(');
                for arg in &args {
                    out.extend_from_slice(arg);
                    out.push(b' ');
                }
                out.push(b')');
            }
            TraceFormat::JsonV1 => {
                let frame = u64::from(self.depth) + u64::from(missing);
                out.extend_from_slice(b"{\"args\":[");
                for (i, arg) in args.iter().enumerate() {
                    if i > 0 {
                        out.push(b',');
                    }
                    json_string(&mut out, arg);
                }
                out.extend_from_slice(b"],\"cmd\":");
                json_string(&mut out, &cmd.name);
                if let Some(id) = &defer_id {
                    out.extend_from_slice(b",\"defer\":");
                    json_string(&mut out, id);
                }
                out.extend_from_slice(b",\"file\":");
                json_string(&mut out, file);
                let now = self.config.host.now();
                out.extend_from_slice(
                    format!(
                        ",\"frame\":{frame},\"global_frame\":{frame},\"line\":{}",
                        display_line(cmd.line)
                    )
                    .as_bytes(),
                );
                if cmd.close_line != cmd.line && cmd.close_line != 0 {
                    out.extend_from_slice(
                        format!(",\"line_end\":{}", display_line(cmd.close_line)).as_bytes(),
                    );
                }
                out.extend_from_slice(
                    format!(",\"time\":{}}}", json_time(now.secs, now.nanos)).as_bytes(),
                );
            }
        }
        self.write_trace_line(&out);
    }

    // ----------------------------------------------------------------------------------
    // Deferred calls.
    // ----------------------------------------------------------------------------------

    /// `cmGlobalGenerator::NewDeferId`: `__0`, `__1`, and so on.
    pub(crate) fn new_defer_id(&mut self) -> BString {
        let id = format!("__{}", self.next_defer_id);
        self.next_defer_id = self.next_defer_id.saturating_add(1);
        BString::from(id)
    }

    /// `cmMakefile::DeferCall`: queue `command` with raw `args`. The call keeps the file
    /// and line of the current command. Returns `false` when no deferred-call queue is
    /// active.
    pub(crate) fn queue_deferred(
        &mut self,
        id: BString,
        command: &[u8],
        args: &[Argument],
    ) -> bool {
        let line = self.cur_line;
        let file = self
            .bt
            .top()
            .map_or_else(|| Arc::from(&b""[..]), |f| Arc::clone(&f.file));
        let Some(queue) = self.dir_frame_mut().defer.as_mut() else {
            return false;
        };
        let args = args
            .iter()
            .map(|a| Argument {
                line,
                span: 0..0,
                ..a.clone()
            })
            .collect();
        queue.push(DeferredCall {
            id,
            file,
            cmd: synthetic_invocation(command, args, line),
        });
        true
    }

    /// `cmMakefile::DeferCancelCall`.
    pub(crate) fn cancel_deferred(&mut self, id: &[u8]) -> bool {
        let Some(queue) = self.dir_frame_mut().defer.as_mut() else {
            return false;
        };
        for call in queue.iter_mut().filter(|c| c.id.as_slice() == id) {
            call.id = BString::default();
        }
        true
    }

    /// `cmMakefile::DeferGetCallIds`.
    pub(crate) fn deferred_ids(&self) -> Option<Vec<BString>> {
        self.dir_frame().defer.as_ref().map(|queue| {
            queue
                .iter()
                .filter(|c| !c.id.is_empty())
                .map(|c| c.id.clone())
                .collect()
        })
    }

    /// `cmMakefile::DeferGetCall`: `[command, args...]`, or an empty list for an unknown
    /// identifier. `None` when no deferred-call queue is active.
    pub(crate) fn deferred_call(&self, id: &[u8]) -> Option<Vec<BString>> {
        let queue = self.dir_frame().defer.as_ref()?;
        let Some(call) = queue
            .iter()
            .find(|c| !id.is_empty() && c.id.as_slice() == id)
        else {
            return Some(Vec::new());
        };
        let mut out = vec![call.cmd.name.clone()];
        out.extend(call.cmd.args.iter().map(|a| a.text.clone()));
        Some(out)
    }

    /// Run the queued deferred calls at the end of the directory listfile `dir_file`
    /// (`RunListFile`). A deferred call can queue more calls.
    pub(crate) fn run_deferred(&mut self, dir_file: &[u8]) {
        let saved_bt = self.bt.clone();
        self.bt = self.bt.push(Frame {
            file: Arc::from(dir_file),
            line: 0,
            name: None,
            kind: FrameKind::Deferred,
        });
        let mut i = 0;
        while !self.fatal && self.exit_override.is_none() {
            let Some(call) = self
                .dir_frame_mut()
                .defer
                .as_mut()
                .and_then(|q| q.get_mut(i))
            else {
                break;
            };
            i += 1;
            if call.id.is_empty() {
                continue;
            }
            let id = core::mem::take(&mut call.id);
            let code = Code::new(Arc::from(vec![call.cmd.clone()]), Arc::clone(&call.file));
            self.exec_deferred(&code, id);
        }
        self.bt = saved_bt;
    }
}

/// A command invocation that is not from a parsed listfile.
fn synthetic_invocation(name: &[u8], args: Vec<Argument>, line: u32) -> CommandInvocation {
    CommandInvocation {
        name: BString::from(name),
        lower: rcmake_base::fold_command_name(name),
        line,
        close_line: line,
        span: 0..0,
        args,
    }
}

/// One-command [`Code`] for a synthetic invocation in `file`.
fn synthetic_code(name: &[u8], args: Vec<Argument>, line: u32, file: Arc<[u8]>) -> Code {
    Code::new(
        Arc::from(vec![synthetic_invocation(name, args, line)]),
        file,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_escapes_match_jsoncpp() {
        let mut out = Vec::new();
        json_string(&mut out, "a\"b\\c\n\u{1}é".as_bytes());
        assert_eq!(out, b"\"a\\\"b\\\\c\\n\\u0001\\u00e9\"".to_vec());
    }

    #[test]
    fn json_time_has_17_significant_digits() {
        assert_eq!(json_time(1_700_000_000, 123_456_789), "1700000000.1234567");
        assert_eq!(json_time(5, 500_000_000), "5.500000000");
    }

    #[test]
    fn trace_source_needs_same_file_name() {
        assert!(trace_source_matches(
            b"/a/b/CMakeLists.txt",
            b"b/CMakeLists.txt"
        ));
        assert!(trace_source_matches(
            b"/a/b/CMakeLists.txt",
            b"CMakeLists.txt"
        ));
        assert!(!trace_source_matches(
            b"/a/b/xCMakeLists.txt",
            b"CMakeLists.txt"
        ));
    }

    #[test]
    fn watch_callback_line_prints_like_cmake_long_max() {
        assert_eq!(display_line(7), "7");
        assert_ne!(
            display_line(WATCH_CALLBACK_LINE),
            WATCH_CALLBACK_LINE.to_string()
        );
    }
}
