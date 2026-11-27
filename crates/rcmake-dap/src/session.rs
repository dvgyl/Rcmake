//! Shared session state, the ordered outbound queue, and the request reader.
//!
//! Three threads take part in a session:
//!
//! - The evaluator thread runs the interpreter and the [`crate::adapter::Hook`]. Only
//!   this thread reads the interpreter. At a stop it serves the queued frame queries
//!   ([`Query`]) of the current stop.
//! - The reader thread reads requests. It answers the control requests from [`State`]
//!   and queues the frame queries. It never waits for the evaluator.
//! - The writer thread writes the queued messages in order. A slow client can block only
//!   this thread, and only for the transport write time limit.
//!
//! One terminal transition ([`Shared::terminate`]) ends the session on `disconnect`, the
//! end of the stream, a read or write error, a full outbound queue, or a session error.
//! It wakes the handshake, stop, and exit waiters.

use std::collections::{BTreeSet, HashMap, VecDeque};
use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use rcmake_base::BString;
use rcmake_eval::debug::DebugView;
use rcmake_host::Host;
use rcmake_json::Value;
use rcmake_model::DirId;

use crate::breakpoints::{self, BreakpointManager, source_key};
use crate::budget;
use crate::protocol::{
    Request, RequestError, RequestKind, error_reply, event, object, parse_request, reply,
};
use crate::variables::{self, Registry};
use crate::wire::{FrameReader, encode};

/// Interval at which waiters check the state again (a bound on a lost notification).
pub(crate) const POLL: Duration = Duration::from_millis(100);

/// Most bytes in the outbound queue. A client that does not read ends the session.
pub(crate) const MAX_QUEUED_BYTES: usize = 64 * 1024 * 1024;

/// Most messages in the outbound queue.
pub(crate) const MAX_QUEUED_MESSAGES: usize = 1 << 16;

/// Most queued frame queries of one stop.
pub(crate) const MAX_QUERIES: usize = 4096;

/// Most bytes that the queued frame queries of one stop keep.
pub(crate) const MAX_QUERY_BYTES: usize = 4 * 1024 * 1024;

/// Exception filters in the order of the `initialize` response, with labels.
pub(crate) const EXCEPTION_FILTERS: [(&str, &str); 5] = [
    ("LOG", "Debug log"),
    ("WARNING", "Warning"),
    ("MESSAGE", "Other messages"),
    ("INTERNAL_ERROR", "Internal error"),
    ("FATAL_ERROR", "Fatal error"),
];

/// Receiver of text for the process standard output.
pub type OutputSink = Arc<dyn Fn(&[u8]) + Send + Sync>;

/// One frame of the debugger stack (`cmDebuggerStackFrame`).
#[derive(Clone, Debug)]
pub(crate) struct Frame {
    pub(crate) id: i64,
    pub(crate) file: BString,
    pub(crate) line: u32,
    pub(crate) name: BString,
    pub(crate) args: Vec<BString>,
    pub(crate) directory: DirId,
}

/// Pending step and pause requests (`ClearStepRequests`).
#[derive(Clone, Copy, Debug)]
pub(crate) struct Steps {
    pub(crate) next_from: i64,
    pub(crate) step_in: bool,
    pub(crate) step_out: i64,
    pub(crate) pause: bool,
}

impl Steps {
    pub(crate) const CLEAR: Self = Self {
        next_from: i64::MIN,
        step_in: false,
        step_out: i64::MIN,
        pause: false,
    };
}

/// State of the one debugger thread (`CMake script`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Thread {
    NotStarted,
    Running(i64),
    Exited,
}

/// A frame query that only the evaluator can answer.
#[derive(Clone, Debug)]
pub(crate) enum Query {
    Scopes {
        frame: i64,
    },
    Variables {
        reference: i64,
        start: Option<u64>,
        count: Option<u64>,
    },
    Evaluate {
        expression: BString,
        frame: Option<i64>,
    },
}

impl Query {
    /// The request command that the response repeats.
    const fn command(&self) -> &'static str {
        match self {
            Self::Scopes { .. } => "scopes",
            Self::Variables { .. } => "variables",
            Self::Evaluate { .. } => "evaluate",
        }
    }

    /// The bytes that a queued query keeps.
    fn cost(&self) -> usize {
        let text = match self {
            Self::Evaluate { expression, .. } => expression.len(),
            Self::Scopes { .. } | Self::Variables { .. } => 0,
        };
        text.saturating_add(size_of::<Pending>())
    }

    /// The response body when the stop of the query ended before it was served.
    pub(crate) fn cancelled(&self) -> Value {
        match self {
            Self::Scopes { .. } => object([("scopes", Value::Array(Vec::new()))]),
            Self::Variables { .. } => object([("variables", Value::Array(Vec::new()))]),
            Self::Evaluate { .. } => object([
                ("result", Value::string("")),
                ("variablesReference", Value::Int(0)),
            ]),
        }
    }
}

/// A queued query of the current stop and the `seq` of its request.
pub(crate) struct Pending {
    seq: i64,
    query: Query,
}

impl Pending {
    fn response(&self, body: Value) -> Value {
        reply(self.seq, self.query.command(), body)
    }
}

/// Session state shared by the evaluator, reader, and writer threads.
pub(crate) struct State {
    pub(crate) terminal: bool,
    pub(crate) configured: bool,
    /// Resume signals (`ContinueSem`).
    pub(crate) permits: u64,
    pub(crate) steps: Steps,
    pub(crate) breakpoints: BreakpointManager,
    pub(crate) exceptions: BTreeSet<BString>,
    /// The exception of the last exception stop: filter id and text.
    pub(crate) exception: Option<(BString, BString)>,
    pub(crate) frames: Vec<Frame>,
    pub(crate) next_frame: i64,
    /// The current stop. The reader clears it when it accepts a resume or ends the
    /// session, and the evaluator then resumes.
    pub(crate) stopped: Option<u64>,
    pub(crate) next_stop: u64,
    pub(crate) queries: VecDeque<Pending>,
    /// Sum of [`Query::cost`] for `queries`.
    pub(crate) query_bytes: usize,
    pub(crate) thread: Thread,
    pub(crate) variable_types: bool,
    pub(crate) registry: Registry,
}

impl State {
    fn new() -> Self {
        Self {
            terminal: false,
            configured: false,
            permits: 0,
            steps: Steps::CLEAR,
            breakpoints: BreakpointManager::default(),
            exceptions: ["FATAL_ERROR", "INTERNAL_ERROR"]
                .into_iter()
                .map(BString::from)
                .collect(),
            exception: None,
            frames: Vec::new(),
            next_frame: 1,
            stopped: None,
            next_stop: 1,
            queries: VecDeque::new(),
            query_bytes: 0,
            thread: Thread::NotStarted,
            variable_types: false,
            registry: Registry::default(),
        }
    }

    /// Push a frame and return its id.
    pub(crate) fn push_frame(
        &mut self,
        file: &[u8],
        line: u32,
        name: &[u8],
        args: Vec<BString>,
        directory: DirId,
    ) {
        let id = self.next_frame;
        self.next_frame = self.next_frame.saturating_add(1);
        self.frames.push(Frame {
            id,
            file: file.into(),
            line,
            name: name.into(),
            args,
            directory,
        });
    }

    /// Pop the innermost frame and release its variable references.
    pub(crate) fn pop_frame(&mut self) {
        if let Some(frame) = self.frames.pop() {
            self.registry.release_frame(frame.id);
        }
    }

    pub(crate) fn frame(&self, id: i64) -> Option<&Frame> {
        self.frames.iter().rev().find(|frame| frame.id == id)
    }

    fn stack_size(&self) -> i64 {
        i64::try_from(self.frames.len()).unwrap_or(i64::MAX)
    }

    pub(crate) fn thread_id(&self) -> i64 {
        match self.thread {
            Thread::Running(id) => id,
            Thread::NotStarted | Thread::Exited => 1,
        }
    }
}

#[derive(Default)]
struct OutState {
    queue: VecDeque<Vec<u8>>,
    bytes: usize,
    next_seq: i64,
    closed: bool,
}

/// Ordered outbound message queue with a byte and message bound.
#[derive(Default)]
pub(crate) struct Outbox {
    state: Mutex<OutState>,
    cv: Condvar,
}

impl Outbox {
    fn lock(&self) -> MutexGuard<'_, OutState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Give `message` the next `seq` and queue it. Return `false` if the queue is closed
    /// or full.
    ///
    /// The exact size of the JSON text is checked against the remaining byte budget
    /// before serialization, so a message that does not fit is never serialized.
    fn push(&self, message: Value) -> bool {
        let mut message = message;
        let mut state = self.lock();
        if state.closed {
            return false;
        }
        state.next_seq = state.next_seq.saturating_add(1);
        if let Value::Object(map) = &mut message {
            map.insert("seq".into(), Value::Int(state.next_seq));
        }
        if state.queue.len() >= MAX_QUEUED_MESSAGES
            || state.bytes.saturating_add(budget::value_len(&message)) > MAX_QUEUED_BYTES
        {
            state.closed = true;
            self.cv.notify_all();
            return false;
        }
        let bytes = rcmake_json::write_compact(&message);
        state.bytes = state.bytes.saturating_add(bytes.len());
        state.queue.push_back(bytes.into());
        self.cv.notify_all();
        true
    }

    /// Wait for the next message. Return `None` when the queue is closed and empty.
    fn pop(&self) -> Option<Vec<u8>> {
        let mut state = self.lock();
        loop {
            if let Some(bytes) = state.queue.pop_front() {
                state.bytes = state.bytes.saturating_sub(bytes.len());
                return Some(bytes);
            }
            if state.closed {
                return None;
            }
            state = self
                .cv
                .wait_timeout(state, POLL)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
    }

    /// Accept no more messages. The writer sends the queued ones and stops.
    pub(crate) fn close(&self) {
        self.lock().closed = true;
        self.cv.notify_all();
    }

    /// Drop the queued messages.
    pub(crate) fn discard(&self) {
        let mut state = self.lock();
        state.queue.clear();
        state.bytes = 0;
        state.closed = true;
        self.cv.notify_all();
    }
}

/// Everything that the session threads share.
pub(crate) struct Shared {
    state: Mutex<State>,
    cv: Condvar,
    pub(crate) out: Outbox,
    pub(crate) host: Arc<dyn Host>,
    pub(crate) stdout: OutputSink,
    pub(crate) log: Option<OutputSink>,
    /// Some source has breakpoints (a fast check for each command).
    pub(crate) has_breakpoints: AtomicBool,
    /// The session has ended (a fast check for each command).
    pub(crate) ended: AtomicBool,
}

impl Shared {
    pub(crate) fn new(host: Arc<dyn Host>, stdout: OutputSink, log: Option<OutputSink>) -> Self {
        Self {
            state: Mutex::new(State::new()),
            cv: Condvar::new(),
            out: Outbox::default(),
            host,
            stdout,
            log,
            has_breakpoints: AtomicBool::new(false),
            ended: AtomicBool::new(false),
        }
    }

    pub(crate) fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn wait<'a>(&self, state: MutexGuard<'a, State>) -> MutexGuard<'a, State> {
        self.cv
            .wait_timeout(state, POLL)
            .unwrap_or_else(PoisonError::into_inner)
            .0
    }

    pub(crate) fn notify(&self) {
        self.cv.notify_all();
    }

    /// Queue a message. A full queue ends the session. Never call this with the state
    /// lock held.
    pub(crate) fn send(&self, message: Value) {
        if !self.out.push(message) {
            self.terminate();
        }
    }

    /// Queue a message while the state lock is held, so that the message keeps its
    /// order relative to the state change. A full queue ends the session.
    fn send_locked(&self, state: &mut State, message: Value) {
        if !self.out.push(message) {
            self.end(state);
        }
    }

    /// Retire the current stop: the evaluator resumes, and each queued query gets its
    /// cancelled response in request order. Return `false` if the outbound queue is
    /// closed or full.
    fn retire_stop(&self, state: &mut State) -> bool {
        state.stopped = None;
        state.query_bytes = 0;
        let mut queued = true;
        for pending in std::mem::take(&mut state.queries) {
            queued &= self.out.push(pending.response(pending.query.cancelled()));
        }
        queued
    }

    /// The terminal transition: clear breakpoints, exception filters, and step requests,
    /// retire the current stop, and wake every waiter. A second call does nothing.
    fn end(&self, state: &mut State) {
        if state.terminal {
            return;
        }
        state.terminal = true;
        state.breakpoints.clear();
        state.exceptions.clear();
        state.steps = Steps::CLEAR;
        let _ = self.retire_stop(state);
        self.has_breakpoints.store(false, Ordering::Release);
        self.ended.store(true, Ordering::Release);
        self.notify();
    }

    /// The terminal transition ([`Self::end`]) from a thread without the state lock.
    pub(crate) fn terminate(&self) {
        let mut state = self.lock();
        self.end(&mut state);
    }

    /// Queue the last response of the session, then make the terminal transition. The
    /// shutdown that the transition wakes still sends the response.
    fn finish_with(&self, message: Value) {
        let mut state = self.lock();
        if state.stopped.is_some() && !self.retire_stop(&mut state) {
            self.end(&mut state);
            return;
        }
        self.send_locked(&mut state, message);
        self.end(&mut state);
    }

    /// Wait until `configurationDone` or the end of the session. Return `false` if the
    /// time limit passes first.
    pub(crate) fn wait_configured(&self, limit: Option<Duration>) -> bool {
        let mut waited = Duration::ZERO;
        let mut state = self.lock();
        while !state.configured && !state.terminal {
            if limit.is_some_and(|limit| waited >= limit) {
                return false;
            }
            state = self.wait(state);
            waited = waited.saturating_add(POLL);
        }
        true
    }

    /// Wait until the session ends. Return `false` if the time limit passes first.
    pub(crate) fn wait_terminal(&self, limit: Option<Duration>) -> bool {
        let mut waited = Duration::ZERO;
        let mut state = self.lock();
        while !state.terminal {
            if limit.is_some_and(|limit| waited >= limit) {
                return false;
            }
            state = self.wait(state);
            waited = waited.saturating_add(POLL);
        }
        true
    }

    /// Stop the evaluator: send a `stopped` event with `body`, then serve the queries of
    /// this stop until the reader retires the stop or the session ends.
    pub(crate) fn stop(&self, view: &DebugView<'_>, body: Value) {
        let mut state = self.lock();
        if state.terminal {
            return;
        }
        let stop = state.next_stop;
        state.next_stop = state.next_stop.saturating_add(1);
        state.stopped = Some(stop);
        self.send_locked(&mut state, event("stopped", body));
        loop {
            if state.stopped != Some(stop) {
                return;
            }
            if state.permits > 0 {
                state.permits -= 1;
                if !self.retire_stop(&mut state) {
                    self.end(&mut state);
                }
                return;
            }
            let Some(pending) = state.queries.pop_front() else {
                state = self.wait(state);
                continue;
            };
            state.query_bytes = state.query_bytes.saturating_sub(pending.query.cost());
            drop(state);
            let body = self.serve(view, &pending.query);
            state = self.lock();
            let body = if state.stopped == Some(stop) {
                body
            } else {
                pending.query.cancelled()
            };
            self.send_locked(&mut state, pending.response(body));
        }
    }

    /// Answer a frame query on the evaluator thread.
    fn serve(&self, view: &DebugView<'_>, query: &Query) -> Value {
        match query {
            Query::Scopes { frame } => {
                let mut state = self.lock();
                // Copy only the source path, not the argument values of the frame.
                let Some((id, file)) = state
                    .frame(*frame)
                    .map(|found| (found.id, found.file.clone()))
                else {
                    return query.cancelled();
                };
                let Some(reference) = state.registry.scope(id) else {
                    return query.cancelled();
                };
                drop(state);
                object([(
                    "scopes",
                    Value::Array(vec![object([
                        ("expensive", Value::Bool(false)),
                        ("name", Value::string("Locals")),
                        ("presentationHint", Value::string("locals")),
                        ("source", crate::protocol::source(&file)),
                        ("variablesReference", Value::Int(reference)),
                    ])]),
                )])
            }
            Query::Variables {
                reference,
                start,
                count,
            } => variables::respond(self, view, *reference, *start, *count),
            Query::Evaluate { expression, frame } => {
                let directory = frame.and_then(|id| {
                    let state = self.lock();
                    state.frame(id).map(|frame| frame.directory)
                });
                match directory
                    .and_then(|dir| view.definition(dir, expression))
                    .filter(|value| budget::bytes_len(value, budget::MAX_RESPONSE_BYTES).is_some())
                {
                    Some(value) => object([
                        ("result", Value::string(value)),
                        ("type", Value::string("string")),
                        ("variablesReference", Value::Int(0)),
                    ]),
                    None => query.cancelled(),
                }
            }
        }
    }

    /// Print a session error and end the session (`Session->onError`).
    fn session_error(&self, text: &str) {
        (self.stdout)(format!("[CMake Debugger] DAP session error: {text}\n").as_bytes());
        self.terminate();
    }

    /// Handle one request body. Return `false` when the reader must stop.
    pub(crate) fn handle(&self, body: &[u8]) -> bool {
        let request = match parse_request(body) {
            Ok(request) => request,
            Err(RequestError::NotRequest) => return true,
            Err(error) => {
                if let Some(text) = error.session_error() {
                    self.session_error(&text);
                }
                return false;
            }
        };
        let Request { seq, kind, .. } = request;
        let command = kind.command();
        let answer = |body: Value| self.send(reply(seq, command, body));
        let error = |message: &str| self.send(error_reply(seq, command, message));
        let empty = Value::object;
        match kind {
            RequestKind::Initialize {
                supports_variable_type,
            } => {
                let filters = {
                    let mut state = self.lock();
                    state.variable_types = supports_variable_type;
                    EXCEPTION_FILTERS
                        .iter()
                        .map(|(filter, label)| {
                            object([
                                (
                                    "default",
                                    Value::Bool(state.exceptions.contains(filter.as_bytes())),
                                ),
                                ("filter", Value::string(*filter)),
                                ("label", Value::string(*label)),
                            ])
                        })
                        .collect()
                };
                answer(object([
                    (
                        "cmakeVersion",
                        object([
                            ("full", Value::string(rcmake_base::CMAKE_VERSION_STR)),
                            (
                                "major",
                                Value::Int(i64::from(rcmake_base::CMAKE_VERSION_MAJOR)),
                            ),
                            (
                                "minor",
                                Value::Int(i64::from(rcmake_base::CMAKE_VERSION_MINOR)),
                            ),
                            (
                                "patch",
                                Value::Int(i64::from(rcmake_base::CMAKE_VERSION_PATCH)),
                            ),
                        ]),
                    ),
                    ("exceptionBreakpointFilters", Value::Array(filters)),
                    ("supportsConfigurationDoneRequest", Value::Bool(true)),
                    ("supportsExceptionInfoRequest", Value::Bool(true)),
                    ("supportsValueFormattingOptions", Value::Bool(true)),
                ]));
                // The `initialized` event follows the `initialize` response.
                self.send(event("initialized", empty()));
            }
            RequestKind::SetBreakpoints { path, lines } => {
                let result = breakpoints::check_request(&path, &lines).and_then(|()| {
                    let key = source_key(self.host.as_ref(), &path);
                    let mut state = self.lock();
                    let result = state.breakpoints.set(&key, &lines);
                    self.has_breakpoints
                        .store(!state.breakpoints.is_empty(), Ordering::Release);
                    result
                });
                match result {
                    Ok(body) => answer(body),
                    Err(message) => error(message),
                }
            }
            RequestKind::SetExceptionBreakpoints { filters } => {
                self.lock().exceptions = filters.into_iter().collect();
                answer(empty());
            }
            RequestKind::ExceptionInfo => {
                let exception = self.lock().exception.take();
                answer(match exception {
                    Some((id, description)) => object([
                        ("breakMode", Value::string("always")),
                        ("description", Value::string(description)),
                        ("exceptionId", Value::string(id)),
                    ]),
                    None => object([
                        ("breakMode", Value::string("")),
                        ("exceptionId", Value::string("")),
                    ]),
                });
            }
            RequestKind::ConfigurationDone => {
                let mut state = self.lock();
                self.send_locked(&mut state, reply(seq, command, empty()));
                state.configured = true;
                drop(state);
                self.notify();
            }
            RequestKind::Threads => {
                let thread = self.lock().thread;
                let threads = match thread {
                    Thread::Running(id) => vec![object([
                        ("id", Value::Int(id)),
                        ("name", Value::string("CMake script")),
                    ])],
                    Thread::NotStarted | Thread::Exited => Vec::new(),
                };
                answer(object([("threads", Value::Array(threads))]));
            }
            RequestKind::StackTrace {
                thread,
                format,
                start,
                levels,
            } => {
                let body = {
                    let state = self.lock();
                    (state.thread == Thread::Running(thread)).then(|| {
                        variables::stack_trace(
                            &state.frames,
                            format,
                            variables::Page::new(start, levels),
                            budget::MAX_RESPONSE_BYTES,
                        )
                    })
                };
                match body {
                    Some(body) => answer(body),
                    None => error(&format!("Unknown threadId '{thread}'")),
                }
            }
            RequestKind::Scopes { frame } => self.query(seq, Query::Scopes { frame }),
            RequestKind::Variables {
                reference,
                start,
                count,
            } => self.query(
                seq,
                Query::Variables {
                    reference,
                    start,
                    count,
                },
            ),
            RequestKind::Evaluate { expression, frame } => {
                self.query(seq, Query::Evaluate { expression, frame });
            }
            RequestKind::Pause => {
                self.lock().steps.pause = true;
                answer(empty());
            }
            RequestKind::Continue
            | RequestKind::Next
            | RequestKind::StepIn
            | RequestKind::StepOut => {
                let mut state = self.lock();
                let size = state.stack_size();
                match kind {
                    RequestKind::Next => state.steps.next_from = size,
                    RequestKind::StepIn => state.steps.step_in = true,
                    RequestKind::StepOut => state.steps.step_out = size.saturating_sub(1),
                    _ => {}
                }
                if state.stopped.is_none() {
                    state.permits = state.permits.saturating_add(1);
                } else if !self.retire_stop(&mut state) {
                    self.end(&mut state);
                }
                self.send_locked(&mut state, reply(seq, command, empty()));
                drop(state);
                self.notify();
            }
            RequestKind::Launch => answer(empty()),
            RequestKind::Disconnect => {
                self.finish_with(reply(seq, command, empty()));
                return false;
            }
        }
        true
    }

    /// Queue a frame query for the evaluator if it is stopped and the queue has room.
    /// Otherwise answer it as cancelled.
    fn query(&self, seq: i64, query: Query) {
        let pending = Pending { seq, query };
        let cost = pending.query.cost();
        let mut state = self.lock();
        if state.stopped.is_some()
            && state.queries.len() < MAX_QUERIES
            && state.query_bytes.saturating_add(cost) <= MAX_QUERY_BYTES
        {
            state.query_bytes = state.query_bytes.saturating_add(cost);
            state.queries.push_back(pending);
            drop(state);
            self.notify();
            return;
        }
        let message = pending.response(pending.query.cancelled());
        self.send_locked(&mut state, message);
    }
}

/// A reader that copies the received bytes to the DAP log (`dap::spy`).
struct LogReader {
    inner: Box<dyn Read + Send>,
    log: Option<OutputSink>,
}

impl Read for LogReader {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.inner.read(buf)?;
        if let (Some(log), Some(bytes)) = (&self.log, buf.get(..n))
            && n > 0
        {
            let mut line = b"\n->".to_vec();
            line.extend_from_slice(bytes);
            log(&line);
        }
        Ok(n)
    }
}

/// The reader thread: handle requests until the session ends.
pub(crate) fn run_reader(shared: &Shared, reader: Box<dyn Read + Send>) {
    let mut frames = FrameReader::new(LogReader {
        inner: reader,
        log: shared.log.clone(),
    });
    loop {
        match frames.read_frame() {
            Ok(Some(body)) => {
                if !shared.handle(&body) {
                    return;
                }
            }
            Ok(None) | Err(_) => {
                shared.terminate();
                return;
            }
        }
        if shared.ended.load(Ordering::Acquire) {
            return;
        }
    }
}

/// The writer thread: write queued messages in order until the queue closes.
pub(crate) fn run_writer(shared: &Shared, mut writer: Box<dyn Write + Send>) {
    while let Some(body) = shared.out.pop() {
        if let Some(log) = &shared.log {
            let header = format!("\n<-Content-Length: {}\r\n\r\n", body.len());
            log(header.as_bytes());
            let mut line = b"\n<-".to_vec();
            line.extend_from_slice(&body);
            log(&line);
        }
        if writer
            .write_all(&encode(&body))
            .and_then(|()| writer.flush())
            .is_err()
        {
            shared.out.discard();
            shared.terminate();
            return;
        }
    }
}

/// Cache of [`source_key`] values for the evaluator thread.
#[derive(Default)]
pub(crate) struct KeyCache {
    keys: HashMap<BString, BString>,
}

impl KeyCache {
    /// Most cached keys.
    const LIMIT: usize = 1 << 16;

    pub(crate) fn get(&mut self, host: &dyn Host, path: &[u8]) -> BString {
        if let Some(key) = self.keys.get(path) {
            return key.clone();
        }
        let key = source_key(host, path);
        if self.keys.len() < Self::LIMIT {
            self.keys.insert(path.into(), key.clone());
        }
        key
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    fn shared() -> Shared {
        let host = rcmake_host::PureHost::new(PathBuf::from("/w"), PathBuf::from("/bin/cmake"));
        Shared::new(Arc::new(host), Arc::new(|_: &[u8]| {}), None)
    }

    fn queued(shared: &Shared) -> Vec<String> {
        shared
            .out
            .lock()
            .queue
            .iter()
            .map(|bytes| String::from_utf8(bytes.clone()).unwrap())
            .collect()
    }

    fn request(seq: i64, command: &str, arguments: &str) -> Vec<u8> {
        format!(r#"{{"seq":{seq},"type":"request","command":"{command}","arguments":{arguments}}}"#)
            .into_bytes()
    }

    #[test]
    fn queued_query_bytes_are_bounded() {
        let shared = shared();
        shared.lock().stopped = Some(1);
        let expression = BString::from(vec![b'x'; MAX_QUERY_BYTES / 4]);
        for seq in 0..8 {
            shared.query(
                seq,
                Query::Evaluate {
                    expression: expression.clone(),
                    frame: None,
                },
            );
        }
        let state = shared.lock();
        assert_eq!(state.queries.len(), 3);
        assert!(state.query_bytes <= MAX_QUERY_BYTES);
        drop(state);
        let out = queued(&shared);
        assert_eq!(out.len(), 5);
        assert!(out.iter().all(|message| {
            message.contains(r#""command":"evaluate""#)
                && message.contains(r#""body":{"result":"","variablesReference":0}"#)
                && !message.contains("xxxx")
        }));
    }

    #[test]
    fn resume_retires_the_stop_and_answers_its_queries_in_order() {
        let shared = shared();
        shared.lock().stopped = Some(1);
        assert!(shared.handle(&request(1, "scopes", r#"{"frameId":1}"#)));
        assert!(shared.handle(&request(2, "evaluate", r#"{"expression":"A"}"#)));
        assert!(shared.handle(&request(3, "continue", r#"{"threadId":1}"#)));
        assert!(shared.handle(&request(4, "variables", r#"{"variablesReference":1}"#)));
        let state = shared.lock();
        assert_eq!(state.stopped, None);
        assert_eq!(state.permits, 0);
        assert!(state.queries.is_empty());
        assert_eq!(state.query_bytes, 0);
        drop(state);
        let order: Vec<(String, String)> = queued(&shared)
            .iter()
            .map(|message| {
                let value = rcmake_json::parse(message.as_bytes(), &Default::default()).unwrap();
                let body = value.get("body").map(rcmake_json::write_compact);
                (
                    value
                        .get("command")
                        .and_then(Value::as_str)
                        .unwrap()
                        .to_owned(),
                    String::from_utf8(body.unwrap_or_default().to_vec()).unwrap(),
                )
            })
            .collect();
        assert_eq!(
            order,
            [
                ("scopes".into(), r#"{"scopes":[]}"#.into()),
                (
                    "evaluate".into(),
                    r#"{"result":"","variablesReference":0}"#.into()
                ),
                ("continue".into(), "{}".into()),
                ("variables".into(), r#"{"variables":[]}"#.into()),
            ]
        );
    }

    #[test]
    fn a_resume_while_running_is_kept_for_the_next_stop() {
        let shared = shared();
        assert!(shared.handle(&request(1, "next", r#"{"threadId":1}"#)));
        assert_eq!(shared.lock().permits, 1);
    }

    #[test]
    fn disconnect_answers_queries_then_itself_then_ends() {
        let shared = shared();
        shared.lock().stopped = Some(1);
        assert!(shared.handle(&request(1, "scopes", r#"{"frameId":1}"#)));
        assert!(!shared.handle(&request(2, "disconnect", "{}")));
        let state = shared.lock();
        assert!(state.terminal);
        assert_eq!(state.stopped, None);
        drop(state);
        let out = queued(&shared);
        assert_eq!(out.len(), 2);
        assert!(out[0].contains(r#""command":"scopes""#));
        assert!(out[1].contains(r#""command":"disconnect""#));
    }

    #[test]
    fn end_of_stream_cancels_the_queued_queries() {
        let shared = shared();
        shared.lock().stopped = Some(1);
        assert!(shared.handle(&request(1, "scopes", r#"{"frameId":1}"#)));
        shared.terminate();
        assert_eq!(shared.lock().stopped, None);
        let out = queued(&shared);
        assert_eq!(out.len(), 1);
        assert!(out[0].contains(r#""body":{"scopes":[]}"#));
    }

    #[test]
    fn outbox_checks_the_escaped_size_before_it_serializes() {
        // 40 control bytes are 240 bytes of JSON text.
        let message = || event("output", object([("output", Value::string([1_u8; 40]))]));
        let mut numbered = message();
        if let Value::Object(map) = &mut numbered {
            map.insert("seq".into(), Value::Int(1));
        }
        let exact = rcmake_json::write_compact(&numbered).len();
        assert!(exact > 240);
        let shared = shared();
        shared.out.lock().bytes = MAX_QUEUED_BYTES - exact;
        assert!(shared.out.push(message()));
        assert_eq!(shared.out.lock().bytes, MAX_QUEUED_BYTES);
        let shared = self::shared();
        // Room for the 40 raw bytes and more, but not for their escapes.
        shared.out.lock().bytes = MAX_QUEUED_BYTES - (exact - 1);
        assert!(!shared.out.push(message()));
        let out = shared.out.lock();
        assert!(out.closed && out.queue.is_empty());
    }

    #[test]
    fn configuration_done_response_is_queued_before_the_release() {
        let shared = shared();
        assert!(shared.handle(&request(1, "configurationDone", "{}")));
        assert!(shared.lock().configured);
        let out = queued(&shared);
        assert_eq!(out.len(), 1);
        assert!(out[0].contains(r#""command":"configurationDone""#));
    }
}
