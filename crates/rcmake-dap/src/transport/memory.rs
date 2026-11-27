//! In-process transport for tests: two bounded byte pipes.

use std::collections::VecDeque;
use std::io::{self, Read, Write};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use super::{Connection, Listener};
use crate::wire::{FrameError, FrameReader, encode};

#[derive(Default)]
struct PipeState {
    data: VecDeque<u8>,
    closed: bool,
}

/// One direction of a byte stream with a capacity.
struct Pipe {
    state: Mutex<PipeState>,
    cv: Condvar,
    capacity: usize,
    write_timeout: Duration,
}

impl Pipe {
    fn new(capacity: usize, write_timeout: Duration) -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::default(),
            cv: Condvar::new(),
            capacity: capacity.max(1),
            write_timeout,
        })
    }

    fn lock(&self) -> MutexGuard<'_, PipeState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn close(&self) {
        self.lock().closed = true;
        self.cv.notify_all();
    }

    /// Read some bytes. With a `limit`, return a time-out error when no data arrives.
    fn read(&self, buf: &mut [u8], limit: Option<Duration>) -> io::Result<usize> {
        let mut state = self.lock();
        let mut waited = Duration::ZERO;
        let step = Duration::from_millis(50);
        while state.data.is_empty() && !state.closed {
            if limit.is_some_and(|limit| waited >= limit) {
                return Err(io::ErrorKind::TimedOut.into());
            }
            state = self
                .cv
                .wait_timeout(state, step)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
            waited = waited.saturating_add(step);
        }
        let n = buf.len().min(state.data.len());
        for (slot, byte) in buf.iter_mut().zip(state.data.drain(..n)) {
            *slot = byte;
        }
        self.cv.notify_all();
        Ok(n)
    }

    fn write(&self, buf: &[u8]) -> io::Result<usize> {
        let mut state = self.lock();
        loop {
            if state.closed {
                return Err(io::ErrorKind::BrokenPipe.into());
            }
            if state.data.len() < self.capacity {
                break;
            }
            let (next, timeout) = self
                .cv
                .wait_timeout(state, self.write_timeout)
                .unwrap_or_else(PoisonError::into_inner);
            state = next;
            if timeout.timed_out() && state.data.len() >= self.capacity {
                return Err(io::ErrorKind::TimedOut.into());
            }
        }
        let n = buf.len().min(self.capacity - state.data.len());
        state.data.extend(buf.iter().take(n));
        self.cv.notify_all();
        Ok(n)
    }
}

struct PipeReader(Arc<Pipe>, Option<Duration>);

impl Read for PipeReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.0.read(buf, self.1)
    }
}

struct PipeWriter(Arc<Pipe>);

impl Write for PipeWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Settings of a memory transport.
#[derive(Clone, Copy, Debug)]
pub struct MemoryOptions {
    /// Bytes that the pipe to the client holds before a write blocks.
    pub client_buffer: usize,
    /// Time limit for one blocked write to the client.
    pub write_timeout: Duration,
    /// Time limit for one read of the client (`None`: no limit).
    pub client_read_timeout: Option<Duration>,
}

impl Default for MemoryOptions {
    fn default() -> Self {
        Self {
            client_buffer: 64 * 1024 * 1024,
            write_timeout: Duration::from_secs(5),
            client_read_timeout: Some(Duration::from_secs(10)),
        }
    }
}

/// Server side of a memory transport.
pub struct MemoryListener {
    to_server: Arc<Pipe>,
    to_client: Arc<Pipe>,
}

impl Listener for MemoryListener {
    fn accept(self: Box<Self>) -> io::Result<Box<dyn Connection>> {
        Ok(Box::new(MemoryConnection {
            to_server: self.to_server,
            to_client: self.to_client,
        }))
    }
}

struct MemoryConnection {
    to_server: Arc<Pipe>,
    to_client: Arc<Pipe>,
}

impl Connection for MemoryConnection {
    fn reader(&self) -> io::Result<Box<dyn Read + Send>> {
        Ok(Box::new(PipeReader(Arc::clone(&self.to_server), None)))
    }

    fn writer(&self) -> io::Result<Box<dyn Write + Send>> {
        Ok(Box::new(PipeWriter(Arc::clone(&self.to_client))))
    }

    fn shutdown(&self) {
        self.to_server.close();
        self.to_client.close();
    }
}

/// Client side of a memory transport.
pub struct MemoryClient {
    to_server: Arc<Pipe>,
    to_client: Arc<Pipe>,
    reader: FrameReader<PipeReader>,
}

impl MemoryClient {
    /// Send one framed message body.
    ///
    /// # Errors
    ///
    /// Return an error if the server closed the connection.
    pub fn send(&self, body: &[u8]) -> io::Result<()> {
        self.send_raw(&encode(body))
    }

    /// Send bytes without framing.
    ///
    /// # Errors
    ///
    /// Return an error if the server closed the connection.
    pub fn send_raw(&self, mut bytes: &[u8]) -> io::Result<()> {
        while !bytes.is_empty() {
            let n = self.to_server.write(bytes)?;
            bytes = bytes.get(n..).unwrap_or_default();
        }
        Ok(())
    }

    /// Receive the next message body, or `None` at the end of the stream.
    ///
    /// # Errors
    ///
    /// Return a [`FrameError`] for a malformed message.
    pub fn recv(&mut self) -> Result<Option<Vec<u8>>, FrameError> {
        self.reader.read_frame()
    }

    /// Close the direction to the server (the server reads the end of the stream).
    pub fn close_write(&self) {
        self.to_server.close();
    }

    /// Close both directions.
    pub fn close(&self) {
        self.to_server.close();
        self.to_client.close();
    }
}

/// Make a connected memory transport.
#[must_use]
pub fn pair(options: MemoryOptions) -> (MemoryListener, MemoryClient) {
    let to_server = Pipe::new(64 * 1024 * 1024, options.write_timeout);
    let to_client = Pipe::new(options.client_buffer, options.write_timeout);
    let client = MemoryClient {
        to_server: Arc::clone(&to_server),
        to_client: Arc::clone(&to_client),
        reader: FrameReader::new(PipeReader(
            Arc::clone(&to_client),
            options.client_read_timeout,
        )),
    };
    (
        MemoryListener {
            to_server,
            to_client,
        },
        client,
    )
}
