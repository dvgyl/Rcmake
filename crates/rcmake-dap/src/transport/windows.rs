//! Windows named-pipe transport (`cmDebuggerPipeConnection_WIN32`, ADR-050).
//!
//! CMake 4.5.1 makes one local, duplex, byte-mode pipe instance with
//! `FILE_FLAG_FIRST_PIPE_INSTANCE`, `PIPE_REJECT_REMOTE_CLIENTS`, 16 KiB buffers, and
//! the default security descriptor. [`WindowsEndpoint`] makes the same pipe through
//! the safe `tokio::net::windows::named_pipe` wrapper, so this crate keeps
//! `#![forbid(unsafe_code)]`.
//!
//! The session uses blocking [`std::io::Read`] and [`std::io::Write`] halves on its own
//! threads. One
//! transport thread runs a current-thread Tokio runtime that owns the pipe. Bounded
//! channels connect the two sides:
//!
//! - The inbound pump reads the pipe and sends chunks to the session reader.
//! - The outbound pump writes each request of the session writer and then acknowledges
//!   it. A write that does not complete in [`WRITE_TIMEOUT`] fails.
//!
//! The pumps are independent tasks, so a silent client never blocks a write, and a
//! blocked write never blocks a read. [`crate::transport::Connection::shutdown`] stops
//! the pumps, closes
//! the pipe, and ends the transport thread. A blocked session read then returns the
//! end of the stream.
//!
//! The pipe-name check ([`validate_pipe_name`]) is platform independent, so its tests
//! also run on Unix hosts.

#[cfg(windows)]
pub use native::{WRITE_TIMEOUT, WindowsEndpoint};

/// Prefix of a local pipe name. Windows compares pipe names without case.
const PREFIX: &[u8; 9] = br"\\.\pipe\";

/// Longest pipe name that `CreateNamedPipeW` accepts, in UTF-16 code units.
pub const MAX_NAME_UNITS: usize = 256;

/// Why a pipe name is not a local Windows pipe name.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PipeNameError {
    /// The name has a NUL code unit. The Win32 call would cut the name at that unit.
    Nul,
    /// The name has more than [`MAX_NAME_UNITS`] code units.
    TooLong,
    /// The name does not start with `\\.\pipe\`.
    MissingPrefix,
    /// The name has nothing after `\\.\pipe\`.
    EmptyName,
}

/// Check that `units` (a UTF-16 name) is a local pipe name: `\\.\pipe\<name>`.
///
/// The prefix comparison ignores ASCII case. A `/` is equal to a `\` in the prefix,
/// because Win32 path conversion changes `/` to `\` in `\\.\` names. The check does
/// not change the name.
///
/// # Errors
///
/// Return the first [`PipeNameError`] of the name.
pub fn validate_pipe_name(units: &[u16]) -> Result<(), PipeNameError> {
    if units.contains(&0) {
        return Err(PipeNameError::Nul);
    }
    if units.len() > MAX_NAME_UNITS {
        return Err(PipeNameError::TooLong);
    }
    let prefix_matches = units.len() >= PREFIX.len()
        && units.iter().zip(PREFIX).all(|(&unit, &expected)| {
            let unit = if unit == u16::from(b'/') {
                u16::from(b'\\')
            } else {
                unit
            };
            u8::try_from(unit).is_ok_and(|byte| byte.eq_ignore_ascii_case(&expected))
        });
    if !prefix_matches {
        return Err(PipeNameError::MissingPrefix);
    }
    if units.len() == PREFIX.len() {
        return Err(PipeNameError::EmptyName);
    }
    Ok(())
}

#[cfg(windows)]
mod native {
    use std::io::{self, Read, Write};
    use std::os::windows::ffi::OsStrExt;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
    use std::thread::JoinHandle;
    use std::time::Duration;

    use tokio::net::windows::named_pipe::{NamedPipeServer, PipeMode, ServerOptions};
    use tokio::runtime::Runtime;
    use tokio::sync::{mpsc, oneshot};

    use super::validate_pipe_name;
    use crate::transport::{Connection, Listener, error_text};

    /// Time limit for one write to the client, as in the Unix transport. A client
    /// that does not read for this time ends the session.
    pub const WRITE_TIMEOUT: Duration = Duration::from_secs(30);

    /// Size of the pipe input and output buffers, as in CMake (`1024 * 16`).
    const PIPE_BUFFER_BYTES: u32 = 16 * 1024;

    /// Bytes that one inbound read takes from the pipe.
    const CHUNK_BYTES: usize = 16 * 1024;

    /// Inbound chunks that wait for the session reader before the pump stops reading.
    const INBOUND_CHUNKS: usize = 4;

    /// Time for the I/O driver to complete the cancelled pipe operations after the
    /// pumps stop. The pipe handle closes when these operations complete.
    const DRAIN_TIME: Duration = Duration::from_millis(50);

    /// Time that shutdown waits for the transport thread.
    const JOIN_LIMIT: Duration = Duration::from_secs(5);

    /// `ERROR_INVALID_NAME`: the error that `CreateNamedPipeW` gives for a name that
    /// is not a pipe name.
    const ERROR_INVALID_NAME: i32 = 123;

    type Chunk = io::Result<Vec<u8>>;
    type WriteRequest = (Vec<u8>, std::sync::mpsc::SyncSender<io::Result<()>>);

    /// A named pipe that waits for one local client.
    pub struct WindowsEndpoint {
        runtime: Runtime,
        server: NamedPipeServer,
        name: PathBuf,
    }

    impl WindowsEndpoint {
        /// Make the pipe `name` with the CMake 4.5.1 settings: first instance only, one
        /// instance, byte mode, local clients only, and 16 KiB buffers.
        ///
        /// # Errors
        ///
        /// Return the CMake reason text: `Internal Error with <name>: <error>`. A name
        /// that [`validate_pipe_name`] rejects gives the `ERROR_INVALID_NAME` text.
        pub fn bind(name: &Path) -> Result<Self, String> {
            let fail = |error: &io::Error| {
                format!(
                    "Internal Error with {}: {}",
                    name.display(),
                    error_text(error)
                )
            };
            let units: Vec<u16> = name.as_os_str().encode_wide().collect();
            if validate_pipe_name(&units).is_err() {
                return Err(fail(&io::Error::from_raw_os_error(ERROR_INVALID_NAME)));
            }
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_io()
                .enable_time()
                .build()
                .map_err(|error| fail(&error))?;
            let server = {
                let _context = runtime.enter();
                ServerOptions::new()
                    .first_pipe_instance(true)
                    .max_instances(1)
                    .reject_remote_clients(true)
                    .pipe_mode(PipeMode::Byte)
                    .in_buffer_size(PIPE_BUFFER_BYTES)
                    .out_buffer_size(PIPE_BUFFER_BYTES)
                    .create(name.as_os_str())
            }
            .map_err(|error| fail(&error))?;
            Ok(Self {
                runtime,
                server,
                name: name.to_path_buf(),
            })
        }

        /// Name of the pipe.
        #[must_use]
        pub fn name(&self) -> &Path {
            &self.name
        }
    }

    impl Listener for WindowsEndpoint {
        fn accept(self: Box<Self>) -> io::Result<Box<dyn Connection>> {
            let Self {
                runtime, server, ..
            } = *self;
            runtime.block_on(server.connect())?;
            let (inbound_tx, inbound_rx) = mpsc::channel(INBOUND_CHUNKS);
            let (outbound_tx, outbound_rx) = mpsc::channel(1);
            let (stop_tx, stop_rx) = oneshot::channel();
            let thread = std::thread::Builder::new()
                .name("rcmake-dap-pipe".into())
                .spawn(move || {
                    runtime.block_on(serve(server, inbound_tx, outbound_rx, stop_rx));
                })?;
            Ok(Box::new(WindowsConnection {
                inbound: Mutex::new(Some(inbound_rx)),
                outbound: Mutex::new(Some(outbound_tx)),
                stop: Mutex::new(Some(stop_tx)),
                thread: Mutex::new(Some(thread)),
            }))
        }
    }

    /// Run the two pumps until a stop signal, then close the pipe.
    async fn serve(
        server: NamedPipeServer,
        inbound: mpsc::Sender<Chunk>,
        outbound: mpsc::Receiver<WriteRequest>,
        stop: oneshot::Receiver<()>,
    ) {
        let pipe = Arc::new(server);
        let pending = Arc::new(AtomicBool::new(false));
        let reading = tokio::spawn(pump_in(Arc::clone(&pipe), inbound));
        let writing = tokio::spawn(pump_out(Arc::clone(&pipe), outbound, Arc::clone(&pending)));
        // A send and a dropped sender both stop the transport.
        let _ = stop.await;
        reading.abort();
        writing.abort();
        let _ = reading.await;
        let _ = writing.await;
        // The pipe cancels a pending read when it closes, but not a pending write. A
        // pending write would keep the pipe handle open, so disconnect the client
        // first. Do this only for a pending write, because a disconnect also discards
        // the bytes that the client did not read yet.
        if pending.load(Ordering::Acquire) {
            disconnect(&pipe);
        }
        drop(pipe);
        // The cancelled operations complete on the I/O driver. Run the driver for a
        // short time so the pipe handle closes before the runtime ends.
        tokio::time::sleep(DRAIN_TIME).await;
    }

    /// Send the client bytes to the session reader until the end of the stream.
    async fn pump_in(pipe: Arc<NamedPipeServer>, inbound: mpsc::Sender<Chunk>) {
        let mut buffer = vec![0_u8; CHUNK_BYTES];
        loop {
            let chunk = match read_some(&pipe, &mut buffer).await {
                Ok(0) => return,
                Ok(n) => Ok(buffer.get(..n).unwrap_or_default().to_vec()),
                Err(error) if is_closed(&error) => return,
                Err(error) => Err(error),
            };
            let failed = chunk.is_err();
            if inbound.send(chunk).await.is_err() || failed {
                return;
            }
        }
    }

    /// Write each request of the session writer and acknowledge it. `pending` is
    /// `true` while a write can be incomplete.
    async fn pump_out(
        pipe: Arc<NamedPipeServer>,
        mut outbound: mpsc::Receiver<WriteRequest>,
        pending: Arc<AtomicBool>,
    ) {
        while let Some((bytes, done)) = outbound.recv().await {
            pending.store(true, Ordering::Release);
            let result = tokio::time::timeout(WRITE_TIMEOUT, write_all(&pipe, &bytes))
                .await
                .unwrap_or_else(|_| Err(io::ErrorKind::TimedOut.into()));
            let failed = result.is_err();
            if failed {
                // The session ends, and the incomplete write must not keep the pipe
                // handle open.
                disconnect(&pipe);
            }
            pending.store(false, Ordering::Release);
            let _ = done.try_send(result);
            if failed {
                return;
            }
        }
    }

    /// Disconnect the client, so that the pending pipe operations fail. The error is
    /// ignored: a client that already closed the pipe gives `NotConnected` or
    /// `BrokenPipe`, and the pipe closes when the transport ends in all cases.
    fn disconnect(pipe: &NamedPipeServer) {
        let _ = pipe.disconnect();
    }

    async fn read_some(pipe: &NamedPipeServer, buffer: &mut [u8]) -> io::Result<usize> {
        loop {
            pipe.readable().await?;
            match pipe.try_read(buffer) {
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
                result => return result,
            }
        }
    }

    /// Write all of `bytes` and wait until the pipe has the last byte.
    async fn write_all(pipe: &NamedPipeServer, mut bytes: &[u8]) -> io::Result<()> {
        while !bytes.is_empty() {
            pipe.writable().await?;
            match pipe.try_write(bytes) {
                Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
                Ok(n) => bytes = bytes.get(n..).unwrap_or_default(),
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
                Err(error) => return Err(error),
            }
        }
        // The pipe has one overlapped write at a time. A new write returns
        // `WouldBlock` until the last write completes, so an empty write that the
        // pipe accepts shows that the client has room for all of the bytes. Thus the
        // time limit also covers a client that does not read.
        loop {
            pipe.writable().await?;
            match pipe.try_write(&[]) {
                Ok(_) => return Ok(()),
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
                Err(error) => return Err(error),
            }
        }
    }

    /// Return whether `error` means that the client closed the pipe.
    fn is_closed(error: &io::Error) -> bool {
        matches!(
            error.kind(),
            io::ErrorKind::BrokenPipe | io::ErrorKind::NotConnected
        )
    }

    fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
        mutex.lock().unwrap_or_else(PoisonError::into_inner)
    }

    struct WindowsConnection {
        inbound: Mutex<Option<mpsc::Receiver<Chunk>>>,
        outbound: Mutex<Option<mpsc::Sender<WriteRequest>>>,
        stop: Mutex<Option<oneshot::Sender<()>>>,
        thread: Mutex<Option<JoinHandle<()>>>,
    }

    impl Connection for WindowsConnection {
        fn reader(&self) -> io::Result<Box<dyn Read + Send>> {
            let inbound = lock(&self.inbound)
                .take()
                .ok_or_else(|| io::Error::other("the debugger pipe reader is in use"))?;
            Ok(Box::new(PipeReader {
                inbound,
                chunk: Vec::new(),
                offset: 0,
            }))
        }

        fn writer(&self) -> io::Result<Box<dyn Write + Send>> {
            let outbound = lock(&self.outbound)
                .clone()
                .ok_or_else(|| io::Error::from(io::ErrorKind::BrokenPipe))?;
            Ok(Box::new(PipeWriter { outbound }))
        }

        fn shutdown(&self) {
            if let Some(stop) = lock(&self.stop).take() {
                let _ = stop.send(());
            }
            lock(&self.outbound).take();
            if let Some(thread) = lock(&self.thread).take() {
                join_within(thread, JOIN_LIMIT);
            }
        }
    }

    impl Drop for WindowsConnection {
        fn drop(&mut self) {
            self.shutdown();
        }
    }

    /// Wait up to `limit` for `thread` to end. A thread that does not end stays
    /// detached.
    fn join_within(thread: JoinHandle<()>, limit: Duration) {
        let step = Duration::from_millis(5);
        let mut waited = Duration::ZERO;
        while !thread.is_finished() {
            if waited >= limit {
                return;
            }
            std::thread::sleep(step);
            waited = waited.saturating_add(step);
        }
        let _ = thread.join();
    }

    /// Blocking reader of the inbound chunks. The end of the chunks is the end of the
    /// stream.
    struct PipeReader {
        inbound: mpsc::Receiver<Chunk>,
        chunk: Vec<u8>,
        offset: usize,
    }

    impl Read for PipeReader {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            if buf.is_empty() {
                return Ok(0);
            }
            while self.offset >= self.chunk.len() {
                match self.inbound.blocking_recv() {
                    Some(Ok(chunk)) => {
                        self.chunk = chunk;
                        self.offset = 0;
                    }
                    Some(Err(error)) => return Err(error),
                    None => return Ok(0),
                }
            }
            let rest = self.chunk.get(self.offset..).unwrap_or_default();
            let n = rest.len().min(buf.len());
            for (slot, byte) in buf.iter_mut().zip(rest) {
                *slot = *byte;
            }
            self.offset = self.offset.saturating_add(n);
            Ok(n)
        }
    }

    /// Blocking writer. Each write returns after the pipe has all of its bytes, or
    /// after an error.
    struct PipeWriter {
        outbound: mpsc::Sender<WriteRequest>,
    }

    impl Write for PipeWriter {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            if buf.is_empty() {
                return Ok(0);
            }
            let (done, wait) = std::sync::mpsc::sync_channel(1);
            self.outbound
                .blocking_send((buf.to_vec(), done))
                .map_err(|_| io::Error::from(io::ErrorKind::BrokenPipe))?;
            // A stopped pump drops the acknowledgement sender.
            wait.recv()
                .unwrap_or_else(|_| Err(io::ErrorKind::BrokenPipe.into()))?;
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{MAX_NAME_UNITS, PipeNameError, validate_pipe_name};

    fn check(name: &str) -> Result<(), PipeNameError> {
        let units: Vec<u16> = name.encode_utf16().collect();
        validate_pipe_name(&units)
    }

    #[test]
    fn local_pipe_names_pass() {
        assert_eq!(check(r"\\.\pipe\cmake-debugger-pipe-1234"), Ok(()));
        assert_eq!(check(r"\\.\PIPE\x"), Ok(()));
        assert_eq!(check(r"\\.\Pipe\LOCAL\nested"), Ok(()));
        assert_eq!(check("//./pipe/x"), Ok(()));
        assert_eq!(check("\\\\.\\pipe\\d\u{e9}bogueur"), Ok(()));
    }

    #[test]
    fn other_names_fail() {
        assert_eq!(check(""), Err(PipeNameError::MissingPrefix));
        assert_eq!(check("/tmp/cmake.sock"), Err(PipeNameError::MissingPrefix));
        assert_eq!(check(r"\\server\pipe\x"), Err(PipeNameError::MissingPrefix));
        assert_eq!(check(r"\\?\pipe\x"), Err(PipeNameError::MissingPrefix));
        assert_eq!(check(r"\\.\pipex"), Err(PipeNameError::MissingPrefix));
        assert_eq!(check(r"C:\pipe\x"), Err(PipeNameError::MissingPrefix));
        assert_eq!(check(r"\\.\pipe\"), Err(PipeNameError::EmptyName));
    }

    #[test]
    fn a_non_ascii_unit_in_the_prefix_fails() {
        // U+0130 (capital I with dot) folds to `i` in some locales. The check folds
        // only ASCII.
        assert_eq!(
            check("\\\\.\\p\u{130}pe\\x"),
            Err(PipeNameError::MissingPrefix)
        );
        // U+0170 has the low byte of `p`. The check does not cut a unit to a byte.
        let mut units: Vec<u16> = r"\\.\pipe\x".encode_utf16().collect();
        if let Some(unit) = units.get_mut(4) {
            *unit = u16::from(b'p') | 0x0100;
        }
        assert_eq!(
            validate_pipe_name(&units),
            Err(PipeNameError::MissingPrefix)
        );
    }

    #[test]
    fn nul_units_fail_before_the_win32_call_cuts_the_name() {
        assert_eq!(check("\\\\.\\pipe\\a\0b"), Err(PipeNameError::Nul));
        assert_eq!(check("\\\\.\\pipe\\\0"), Err(PipeNameError::Nul));
    }

    #[test]
    fn the_length_limit_counts_utf16_units() {
        let prefix = r"\\.\pipe\";
        let longest = format!("{prefix}{}", "a".repeat(MAX_NAME_UNITS - prefix.len()));
        assert_eq!(check(&longest), Ok(()));
        let too_long = format!("{longest}a");
        assert_eq!(check(&too_long), Err(PipeNameError::TooLong));
        // U+1F600 is two UTF-16 units.
        let pairs = MAX_NAME_UNITS.saturating_sub(prefix.len()) / 2 + 1;
        let surrogates = format!("{prefix}{}", "\u{1f600}".repeat(pairs));
        assert_eq!(check(&surrogates), Err(PipeNameError::TooLong));
    }
}

#[cfg(all(test, windows))]
#[allow(clippy::unwrap_used, reason = "a fixture failure fails the test")]
mod windows_tests {
    use std::io::{self, Read, Write};
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    use tokio::net::windows::named_pipe::{ClientOptions, NamedPipeClient};
    use tokio::runtime::Runtime;

    use super::WindowsEndpoint;
    use crate::transport::Listener;
    use crate::wire::encode;
    use crate::{Config, DebugServer};

    const LIMIT: Duration = Duration::from_secs(20);

    fn unique_name() -> PathBuf {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        PathBuf::from(format!(
            r"\\.\pipe\rcmake-dap-test-{}-{n}",
            std::process::id()
        ))
    }

    fn client_runtime() -> Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_io()
            .enable_time()
            .build()
            .unwrap()
    }

    fn open(runtime: &Runtime, name: &Path) -> NamedPipeClient {
        let _context = runtime.enter();
        ClientOptions::new().open(name.as_os_str()).unwrap()
    }

    fn send(runtime: &Runtime, client: &NamedPipeClient, mut bytes: &[u8]) {
        runtime.block_on(async {
            while !bytes.is_empty() {
                client.writable().await.unwrap();
                match client.try_write(bytes) {
                    Ok(n) => bytes = bytes.get(n..).unwrap_or_default(),
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
                    Err(error) => panic!("client write failed: {error}"),
                }
            }
        });
    }

    fn request(
        runtime: &Runtime,
        client: &NamedPipeClient,
        seq: u32,
        command: &str,
        arguments: &str,
    ) {
        let body = format!(
            r#"{{"seq":{seq},"type":"request","command":"{command}","arguments":{arguments}}}"#
        );
        send(runtime, client, &encode(body.as_bytes()));
    }

    /// Read the next bytes, or `None` at the end of the stream.
    fn read_chunk(runtime: &Runtime, client: &NamedPipeClient) -> Option<Vec<u8>> {
        runtime.block_on(async {
            tokio::time::timeout(LIMIT, async {
                let mut buffer = [0_u8; 4096];
                loop {
                    client.readable().await.unwrap();
                    match client.try_read(&mut buffer) {
                        Ok(0) => return None,
                        Ok(n) => return Some(buffer.get(..n).unwrap().to_vec()),
                        Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
                        Err(error) if error.kind() == io::ErrorKind::BrokenPipe => return None,
                        Err(error) => panic!("client read failed: {error}"),
                    }
                }
            })
            .await
            .unwrap()
        })
    }

    /// Read until `received` has all `needles`.
    fn receive_until(
        runtime: &Runtime,
        client: &NamedPipeClient,
        received: &mut Vec<u8>,
        needles: &[&str],
    ) {
        while !needles
            .iter()
            .all(|needle| String::from_utf8_lossy(received).contains(needle))
        {
            let chunk = read_chunk(runtime, client).unwrap_or_else(|| {
                panic!("the stream ended before {needles:?}");
            });
            received.extend_from_slice(&chunk);
        }
    }

    /// Read to the end of the stream and return the bytes.
    fn receive_to_end(runtime: &Runtime, client: &NamedPipeClient) -> Vec<u8> {
        let mut received = Vec::new();
        while let Some(chunk) = read_chunk(runtime, client) {
            received.extend_from_slice(&chunk);
        }
        received
    }

    /// Close the client and run its I/O driver, so that the client handle closes.
    fn close(runtime: &Runtime, client: NamedPipeClient) {
        drop(client);
        // `sleep` needs the runtime context, so make it in the `async` block.
        runtime.block_on(async { tokio::time::sleep(Duration::from_millis(50)).await });
    }

    /// Join `thread`. Fail the test if it does not end in [`LIMIT`].
    fn join<T>(thread: std::thread::JoinHandle<T>) -> T {
        let step = Duration::from_millis(10);
        let mut waited = Duration::ZERO;
        while !thread.is_finished() {
            assert!(waited < LIMIT, "the thread did not end");
            std::thread::sleep(step);
            waited += step;
        }
        thread.join().unwrap()
    }

    #[test]
    fn a_second_endpoint_with_the_same_name_fails() {
        let name = unique_name();
        let first = WindowsEndpoint::bind(&name).unwrap();
        let Err(reason) = WindowsEndpoint::bind(&name) else {
            panic!("the second pipe instance was made");
        };
        assert!(
            reason.starts_with(&format!("Internal Error with {}: ", name.display())),
            "{reason}"
        );
        drop(first);
    }

    #[test]
    fn a_name_outside_the_pipe_namespace_fails() {
        let Err(reason) = WindowsEndpoint::bind(Path::new(r"C:\Temp\cmake.sock")) else {
            panic!("a file name was accepted");
        };
        assert!(reason.starts_with(r"Internal Error with C:\Temp\cmake.sock: "));
        assert!(!reason.contains("(os error"), "{reason}");
    }

    #[test]
    fn writes_pass_while_a_read_waits_and_shutdown_ends_the_read() {
        let name = unique_name();
        let endpoint = WindowsEndpoint::bind(&name).unwrap();
        let runtime = client_runtime();
        let client = open(&runtime, &name);
        let connection = Box::new(endpoint).accept().unwrap();
        let mut reader = connection.reader().unwrap();
        let mut writer = connection.writer().unwrap();
        let blocked = std::thread::spawn(move || {
            let mut buffer = [0_u8; 16];
            reader.read(&mut buffer)
        });
        // The client sends nothing, so the read waits. A write must still complete.
        writer.write_all(b"hello").unwrap();
        let mut received = Vec::new();
        receive_until(&runtime, &client, &mut received, &["hello"]);
        assert!(!blocked.is_finished());
        connection.shutdown();
        assert_eq!(join(blocked).unwrap(), 0);
        assert!(writer.write_all(b"late").is_err());
        assert_eq!(receive_to_end(&runtime, &client), b"");
    }

    #[test]
    fn a_large_write_completes_when_the_client_reads() {
        let name = unique_name();
        let endpoint = WindowsEndpoint::bind(&name).unwrap();
        let runtime = client_runtime();
        let client = open(&runtime, &name);
        let connection = Box::new(endpoint).accept().unwrap();
        let mut writer = connection.writer().unwrap();
        // Four times the pipe buffer, so the write waits for the client.
        let payload = vec![b'x'; 64 * 1024];
        let expected = payload.len();
        let writing = std::thread::spawn(move || writer.write_all(&payload));
        let mut received = Vec::new();
        while received.len() < expected {
            received.extend_from_slice(&read_chunk(&runtime, &client).unwrap());
        }
        join(writing).unwrap();
        assert_eq!(received.len(), expected);
        connection.shutdown();
    }

    #[test]
    fn the_client_end_of_stream_ends_the_session_read() {
        let name = unique_name();
        let endpoint = WindowsEndpoint::bind(&name).unwrap();
        let runtime = client_runtime();
        let client = open(&runtime, &name);
        let connection = Box::new(endpoint).accept().unwrap();
        let mut reader = connection.reader().unwrap();
        let reading = std::thread::spawn(move || {
            let mut received = Vec::new();
            reader.read_to_end(&mut received).map(|_| received)
        });
        send(&runtime, &client, b"abc");
        close(&runtime, client);
        assert_eq!(join(reading).unwrap(), b"abc");
        connection.shutdown();
    }

    #[test]
    fn dap_initialize_and_disconnect_over_a_named_pipe() {
        let name = unique_name();
        let endpoint = WindowsEndpoint::bind(&name).unwrap();
        let runtime = client_runtime();
        let client = open(&runtime, &name);
        let server = std::thread::spawn(move || {
            let host = rcmake_host::PureHost::new(
                PathBuf::from(r"C:\w"),
                PathBuf::from(r"C:\bin\cmake.exe"),
            );
            let config = Config {
                host: Arc::new(host),
                stdout: Arc::new(|_: &[u8]| {}),
                log: None,
                handshake_limit: Some(LIMIT),
                exit_limit: Some(LIMIT),
            };
            let server = DebugServer::start(Box::new(endpoint), config);
            let active = server.is_active();
            server.finish(0);
            active
        });
        let mut received = Vec::new();
        request(&runtime, &client, 1, "initialize", r#"{"adapterID":"t"}"#);
        receive_until(
            &runtime,
            &client,
            &mut received,
            &[r#""command":"initialize""#, r#""event":"initialized""#],
        );
        assert!(String::from_utf8_lossy(&received).contains(r#""success":true"#));
        request(&runtime, &client, 2, "configurationDone", "{}");
        receive_until(
            &runtime,
            &client,
            &mut received,
            &[
                r#""command":"configurationDone""#,
                r#""event":"terminated""#,
            ],
        );
        assert!(String::from_utf8_lossy(&received).contains(r#""exitCode":0"#));
        request(&runtime, &client, 3, "disconnect", "{}");
        let rest = receive_to_end(&runtime, &client);
        assert!(String::from_utf8_lossy(&rest).contains(r#""command":"disconnect""#));
        assert!(join(server));
    }
}
