//! CMake 4.5.1 Debug Adapter Protocol (`--debugger`) server for `rcmake`.
//!
//! [`DebugServer::start`] waits for one client on a [`transport::Listener`], answers the
//! configuration requests, and returns after `configurationDone`, as
//! `cmDebuggerAdapter` does. [`DebugServer::hook`] gives a [`DebugHook`] for each
//! interpreter of the invocation. [`DebugServer::finish`] reports the exit code and
//! waits for the client to disconnect.
//!
//! The client is untrusted: message framing, JSON depth, breakpoint counts, variable
//! responses, and the outbound queue all have limits, and no client action can block
//! the interpreter outside of a stop.

#![forbid(unsafe_code)]

use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

use rcmake_eval::debug::DebugHook;
use rcmake_host::Host;
use rcmake_json::Value;

mod adapter;
pub mod breakpoints;
mod budget;
pub mod protocol;
mod session;
pub mod transport;
mod variables;
pub mod wire;

pub use breakpoints::source_key;
pub use session::OutputSink;

use protocol::{event, object};
use session::{Shared, Thread};
use transport::{Connection, Listener};

/// Whether this platform has a native debugger pipe transport.
///
/// Windows named pipes need a vetted safe wrapper crate (a future ADR). Until then the
/// Windows build reports `"debugger": false` and rejects the `--debugger*` options.
pub const TRANSPORT_SUPPORTED: bool = cfg!(unix);

/// Time that shutdown waits for the writer and reader threads.
const SHUTDOWN_LIMIT: Duration = Duration::from_secs(5);

/// Settings of a debugger session.
#[derive(Clone)]
pub struct Config {
    /// Host for source path lookups.
    pub host: Arc<dyn Host>,
    /// Receiver of the standard output lines (`Waiting for debugger client to
    /// connect...`, session errors).
    pub stdout: OutputSink,
    /// Receiver of the DAP traffic log (`--debugger-dap-log`).
    pub log: Option<OutputSink>,
    /// Longest wait for `configurationDone`. CMake waits without a limit (`None`).
    pub handshake_limit: Option<Duration>,
    /// Longest wait for the client to disconnect after the exit events. CMake waits
    /// without a limit (`None`).
    pub exit_limit: Option<Duration>,
}

/// One debugger session of a top-level `cmake` invocation.
pub struct DebugServer {
    shared: Arc<Shared>,
    connection: Option<Box<dyn Connection>>,
    threads: Vec<JoinHandle<()>>,
    exit_limit: Option<Duration>,
}

impl DebugServer {
    /// Print `Waiting for debugger client to connect...`, accept the client, and serve
    /// it until `configurationDone`. Then start the debugger thread.
    ///
    /// If no client connects, this prints the accept error and returns an ended
    /// session. If the client closes the connection before `configurationDone`, this
    /// returns an ended session. In both cases the run continues without a debugger.
    #[must_use]
    pub fn start(listener: Box<dyn Listener>, config: Config) -> Self {
        let shared = Arc::new(Shared::new(
            config.host,
            Arc::clone(&config.stdout),
            config.log,
        ));
        let mut server = Self {
            shared: Arc::clone(&shared),
            connection: None,
            threads: Vec::new(),
            exit_limit: config.exit_limit,
        };
        (config.stdout)(b"Waiting for debugger client to connect...\n");
        let connection = match listener.accept() {
            Ok(connection) => connection,
            Err(error) => {
                let text = format!(
                    "Error: Failed to accept the debugger client connection.\n{}\n",
                    transport::error_text(&error)
                );
                (config.stdout)(text.as_bytes());
                shared.terminate();
                return server;
            }
        };
        (config.stdout)(b"Debugger client connected.\n");
        let (Ok(reader), Ok(writer)) = (connection.reader(), connection.writer()) else {
            shared.terminate();
            return server;
        };
        server.connection = Some(connection);
        let writer_shared = Arc::clone(&shared);
        let reader_shared = Arc::clone(&shared);
        for spawned in [
            std::thread::Builder::new()
                .name("rcmake-dap-writer".into())
                .spawn(move || session::run_writer(&writer_shared, writer)),
            std::thread::Builder::new()
                .name("rcmake-dap-reader".into())
                .spawn(move || session::run_reader(&reader_shared, reader)),
        ] {
            match spawned {
                Ok(handle) => server.threads.push(handle),
                Err(_) => shared.terminate(),
            }
        }
        if !shared.wait_configured(config.handshake_limit) {
            shared.terminate();
        }
        let active = {
            let mut state = shared.lock();
            state.thread = Thread::Running(1);
            !state.terminal
        };
        if active {
            shared.send(event(
                "thread",
                object([
                    ("reason", Value::string("started")),
                    ("threadId", Value::Int(1)),
                ]),
            ));
        }
        server
    }

    /// A hook that reports the events of one interpreter to this session.
    #[must_use]
    pub fn hook(&self) -> Box<dyn DebugHook> {
        Box::new(adapter::Hook::new(Arc::clone(&self.shared)))
    }

    /// Return whether the client is still connected and has not disconnected.
    #[must_use]
    pub fn is_active(&self) -> bool {
        !self.shared.lock().terminal
    }

    /// Report `exit_code` (`thread exited`, `exited`, `terminated`), wait until the
    /// client disconnects, and close the connection (`ReportExitCode`).
    pub fn finish(mut self, exit_code: i32) {
        let (thread, active) = {
            let mut state = self.shared.lock();
            let thread = state.thread_id();
            state.thread = Thread::Exited;
            (thread, !state.terminal)
        };
        if active {
            self.shared.send(event(
                "thread",
                object([
                    ("reason", Value::string("exited")),
                    ("threadId", Value::Int(thread)),
                ]),
            ));
            self.shared.send(event(
                "exited",
                object([("exitCode", Value::Int(i64::from(exit_code)))]),
            ));
            self.shared.send(event("terminated", Value::object()));
        }
        if !self.shared.wait_terminal(self.exit_limit) {
            self.shared.terminate();
        }
        self.shutdown();
    }

    /// Send the queued messages, close the connection, and stop the session threads.
    fn shutdown(&mut self) {
        self.shared.terminate();
        self.shared.out.close();
        let writer_done = self
            .threads
            .first()
            .is_none_or(|writer| wait_finished(writer, SHUTDOWN_LIMIT));
        if !writer_done {
            self.shared.out.discard();
        }
        if let Some(connection) = self.connection.take() {
            connection.shutdown();
        }
        for handle in self.threads.drain(..) {
            if wait_finished(&handle, SHUTDOWN_LIMIT) {
                let _ = handle.join();
            }
        }
    }
}

impl Drop for DebugServer {
    fn drop(&mut self) {
        if self.connection.is_some() || !self.threads.is_empty() {
            self.shutdown();
        }
    }
}

/// Wait up to `limit` for a thread to end.
fn wait_finished(handle: &JoinHandle<()>, limit: Duration) -> bool {
    let step = Duration::from_millis(5);
    let mut waited = Duration::ZERO;
    while !handle.is_finished() {
        if waited >= limit {
            return false;
        }
        std::thread::sleep(step);
        waited = waited.saturating_add(step);
    }
    true
}
