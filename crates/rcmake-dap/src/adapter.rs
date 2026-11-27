//! The evaluator side of the adapter: [`DebugHook`] events, the frame stack, and the
//! stop rules (`cmDebuggerAdapter::OnBeginFunctionCall`).

use std::sync::Arc;
use std::sync::atomic::Ordering;

use rcmake_base::BString;
use rcmake_eval::debug::{CallInfo, CommandSpan, DebugHook, DebugMessageKind, DebugView};
use rcmake_json::Value;
use rcmake_model::DirId;

use crate::protocol::{event, object};
use crate::session::{KeyCache, Shared, Steps};

/// A [`DebugHook`] connected to one debugger session.
///
/// Dropping the hook does not end the session, so an interpreter of the same invocation
/// (for example, one for a `-C` script) can use the hook and then drop it.
pub(crate) struct Hook {
    shared: Arc<Shared>,
    keys: KeyCache,
}

impl Hook {
    pub(crate) fn new(shared: Arc<Shared>) -> Self {
        Self {
            shared,
            keys: KeyCache::default(),
        }
    }
}

const fn filter_name(kind: DebugMessageKind) -> &'static str {
    match kind {
        DebugMessageKind::FatalError => "FATAL_ERROR",
        DebugMessageKind::InternalError => "INTERNAL_ERROR",
        DebugMessageKind::Message => "MESSAGE",
        DebugMessageKind::Warning => "WARNING",
        DebugMessageKind::Log => "LOG",
    }
}

impl DebugHook for Hook {
    fn file_parsed(&mut self, path: &[u8], spans: &[CommandSpan]) {
        let key = self.keys.get(self.shared.host.as_ref(), path);
        let events = self.shared.lock().breakpoints.source_loaded(&key, spans);
        for body in events {
            self.shared.send(event("breakpoint", body));
        }
    }

    fn begin_parse(&mut self, path: &[u8], directory: DirId) {
        // A file frame has line 0 and the path as its name. It never stops.
        self.shared
            .lock()
            .push_frame(path, 0, path, Vec::new(), directory);
    }

    fn end_parse(&mut self) {
        self.shared.lock().pop_frame();
    }

    fn begin_command(&mut self, view: &DebugView<'_>, call: &CallInfo<'_>) {
        let key = (call.line != 0 && self.shared.has_breakpoints.load(Ordering::Acquire))
            .then(|| self.keys.get(self.shared.host.as_ref(), call.file));
        let mut state = self.shared.lock();
        state.push_frame(
            call.file,
            call.line,
            call.name,
            call.args.to_vec(),
            call.directory,
        );
        // A frame at line 0 has just loaded a file: continue to the first command.
        if call.line == 0 || state.terminal {
            return;
        }
        let hits = key
            .as_ref()
            .map_or_else(Vec::new, |key| state.breakpoints.hits(key, call.line));
        let size = i64::try_from(state.frames.len()).unwrap_or(i64::MAX);
        let mut reason = None;
        if !hits.is_empty() {
            state.steps = Steps::CLEAR;
            reason = Some("breakpoint");
        }
        let steps = state.steps;
        if size <= steps.next_from || steps.step_in || size <= steps.step_out {
            state.steps = Steps::CLEAR;
            reason = Some("step");
        }
        if state.steps.pause {
            state.steps = Steps::CLEAR;
            reason = Some("pause");
        }
        let Some(reason) = reason else {
            return;
        };
        let thread = state.thread_id();
        drop(state);
        let mut body = object([
            ("allThreadsStopped", Value::Bool(true)),
            ("reason", Value::string(reason)),
            ("threadId", Value::Int(thread)),
        ]);
        if !hits.is_empty()
            && let Value::Object(map) = &mut body
        {
            map.insert(
                "hitBreakpointIds".into(),
                Value::Array(hits.into_iter().map(Value::Int).collect()),
            );
        }
        self.shared.stop(view, body);
    }

    fn end_command(&mut self) {
        self.shared.lock().pop_frame();
    }

    fn message(&mut self, view: &DebugView<'_>, kind: DebugMessageKind, text: &[u8]) {
        let filter = filter_name(kind);
        let thread = {
            let mut state = self.shared.lock();
            if state.terminal || !state.exceptions.contains(filter.as_bytes()) {
                return;
            }
            state.exception = Some((BString::from(filter), BString::from(text)));
            state.thread_id()
        };
        let body = object([
            ("allThreadsStopped", Value::Bool(true)),
            ("description", Value::string("Pause on exception")),
            ("reason", Value::string("exception")),
            ("text", Value::string(text)),
            ("threadId", Value::Int(thread)),
        ]);
        self.shared.stop(view, body);
    }
}
