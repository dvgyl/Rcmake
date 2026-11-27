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

use crate::breakpoints::{BreakpointManager, source_key};
use crate::protocol::{
    Request, RequestError, RequestKind, error_response, event, object, parse_request, response,
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

/// A queued query tagged with the stop that it belongs to.
pub(crate) struct Pending {
    pub(crate) stop: u64,
    pub(crate) request: Request,
    pub(crate) query: Query,
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
    pub(crate) stopped: Option<u64>,
    pub(crate) next_stop: u64,
    pub(crate) queries: VecDeque<Pending>,
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
        let bytes = rcmake_json::write_compact(&message);
        if state.queue.len() >= MAX_QUEUED_MESSAGES
            || state.bytes.saturating_add(bytes.len()) > MAX_QUEUED_BYTES
        {
            state.closed = true;
            self.cv.notify_all();
            return false;
        }
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
    fn discard(&self) {
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

    /// The terminal transition: clear breakpoints, exception filters, and step requests,
    /// resume a stopped evaluator, and wake every waiter. A second call does nothing.
    pub(crate) fn terminate(&self) {
        let mut state = self.lock();
        if state.terminal {
            return;
        }
        state.terminal = true;
        state.breakpoints.clear();
        state.exceptions.clear();
        state.steps = Steps::CLEAR;
        drop(state);
        self.has_breakpoints.store(false, Ordering::Release);
        self.ended.store(true, Ordering::Release);
        self.notify();
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
    /// this stop until a resume request or the end of the session.
    pub(crate) fn stop(&self, view: &DebugView<'_>, body: Value) {
        let stop = {
            let mut state = self.lock();
            if state.terminal {
                return;
            }
            let stop = state.next_stop;
            state.next_stop = state.next_stop.saturating_add(1);
            // The stopped state is set before the event is published.
            state.stopped = Some(stop);
            stop
        };
        self.send(event("stopped", body));
        loop {
            let mut state = self.lock();
            let pending = loop {
                if let Some(pending) = state.queries.pop_front() {
                    break Some(pending);
                }
                if state.permits > 0 || state.terminal {
                    if !state.terminal {
                        state.permits -= 1;
                    }
                    break None;
                }
                state = self.wait(state);
            };
            let Some(pending) = pending else {
                state.stopped = None;
                let stale: Vec<Pending> = state.queries.drain(..).collect();
                drop(state);
                for pending in stale {
                    self.send(response(&pending.request, pending.query.cancelled()));
                }
                return;
            };
            drop(state);
            let body = if pending.stop == stop {
                self.serve(view, &pending.query)
            } else {
                pending.query.cancelled()
            };
            self.send(response(&pending.request, body));
        }
    }

    /// Answer a frame query on the evaluator thread.
    fn serve(&self, view: &DebugView<'_>, query: &Query) -> Value {
        match query {
            Query::Scopes { frame } => {
                let mut state = self.lock();
                let Some(found) = state.frame(*frame).cloned() else {
                    return query.cancelled();
                };
                let Some(reference) = state.registry.scope(found.id) else {
                    return query.cancelled();
                };
                drop(state);
                object([(
                    "scopes",
                    Value::Array(vec![object([
                        ("expensive", Value::Bool(false)),
                        ("name", Value::string("Locals")),
                        ("presentationHint", Value::string("locals")),
                        ("source", crate::protocol::source(&found.file)),
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
                match directory.and_then(|dir| view.definition(dir, expression)) {
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
        let reply = |body: Value| self.send(response(&request, body));
        let empty = Value::object;
        match &request.kind {
            RequestKind::Initialize {
                supports_variable_type,
            } => {
                let filters = {
                    let mut state = self.lock();
                    state.variable_types = *supports_variable_type;
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
                reply(object([
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
                let key = source_key(self.host.as_ref(), path);
                let result = {
                    let mut state = self.lock();
                    let result = state.breakpoints.set(&key, lines);
                    self.has_breakpoints
                        .store(!state.breakpoints.is_empty(), Ordering::Release);
                    result
                };
                match result {
                    Ok(body) => reply(body),
                    Err(message) => self.send(error_response(&request, message)),
                }
            }
            RequestKind::SetExceptionBreakpoints { filters } => {
                self.lock().exceptions = filters.iter().cloned().collect();
                reply(empty());
            }
            RequestKind::ExceptionInfo => {
                let exception = self.lock().exception.take();
                reply(match exception {
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
                self.lock().configured = true;
                self.notify();
                reply(empty());
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
                reply(object([("threads", Value::Array(threads))]));
            }
            RequestKind::StackTrace { thread, format } => {
                let body = {
                    let state = self.lock();
                    (state.thread == Thread::Running(*thread))
                        .then(|| variables::stack_trace(&state.frames, *format))
                };
                match body {
                    Some(body) => reply(body),
                    None => self.send(error_response(
                        &request,
                        &format!("Unknown threadId '{thread}'"),
                    )),
                }
            }
            RequestKind::Scopes { frame } => self.query(&request, Query::Scopes { frame: *frame }),
            RequestKind::Variables {
                reference,
                start,
                count,
            } => self.query(
                &request,
                Query::Variables {
                    reference: *reference,
                    start: *start,
                    count: *count,
                },
            ),
            RequestKind::Evaluate { expression, frame } => self.query(
                &request,
                Query::Evaluate {
                    expression: expression.clone(),
                    frame: *frame,
                },
            ),
            RequestKind::Pause => {
                self.lock().steps.pause = true;
                reply(empty());
            }
            RequestKind::Continue
            | RequestKind::Next
            | RequestKind::StepIn
            | RequestKind::StepOut => {
                {
                    let mut state = self.lock();
                    let size = state.stack_size();
                    match request.kind {
                        RequestKind::Next => state.steps.next_from = size,
                        RequestKind::StepIn => state.steps.step_in = true,
                        RequestKind::StepOut => state.steps.step_out = size.saturating_sub(1),
                        _ => {}
                    }
                    state.permits = state.permits.saturating_add(1);
                }
                self.notify();
                reply(empty());
            }
            RequestKind::Launch => reply(empty()),
            RequestKind::Disconnect => {
                // Queue the response before the terminal transition, so that the
                // shutdown that the transition wakes still sends it.
                reply(empty());
                self.terminate();
                return false;
            }
        }
        true
    }

    /// Queue a frame query for the evaluator if it is stopped. Otherwise answer it as
    /// cancelled.
    fn query(&self, request: &Request, query: Query) {
        let mut state = self.lock();
        if let Some(stop) = state.stopped
            && !state.terminal
            && state.queries.len() < MAX_QUERIES
        {
            state.queries.push_back(Pending {
                stop,
                request: request.clone(),
                query,
            });
            drop(state);
            self.notify();
            return;
        }
        drop(state);
        self.send(response(request, query.cancelled()));
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
