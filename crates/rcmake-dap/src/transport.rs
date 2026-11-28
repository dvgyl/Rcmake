//! Byte-stream transports between the adapter and one debugger client.
//!
//! The transport is a process-level side channel that the CLI opens. It is not part of
//! the `rcmake_host::Host` contract. The `unix` module (an `AF_UNIX` socket) and the
//! `windows` module (a named pipe, ADR-050) are the only operating-system boundaries of
//! this crate. Tests use [`memory`].

use std::io::{self, Read, Write};

pub mod memory;
#[cfg(unix)]
pub mod unix;
// The pipe-name check of `windows` has no Win32 calls, so its tests also run on Unix.
#[cfg(any(windows, test))]
pub mod windows;

/// One accepted client connection.
pub trait Connection: Send + Sync {
    /// A reader for the bytes from the client.
    ///
    /// # Errors
    ///
    /// Return an error if the transport can not make a reader.
    fn reader(&self) -> io::Result<Box<dyn Read + Send>>;

    /// A writer for the bytes to the client. A write that can not complete in a limited
    /// time returns an error.
    ///
    /// # Errors
    ///
    /// Return an error if the transport can not make a writer.
    fn writer(&self) -> io::Result<Box<dyn Write + Send>>;

    /// Close both directions. A blocked read or write returns.
    fn shutdown(&self);
}

/// A bound endpoint that waits for one client.
pub trait Listener: Send {
    /// Wait for the client and release the endpoint.
    ///
    /// # Errors
    ///
    /// Return an error if no client connects.
    fn accept(self: Box<Self>) -> io::Result<Box<dyn Connection>>;
}

/// Text of an operating-system error as `strerror` gives it (without the
/// ` (os error <n>)` suffix that Rust adds).
#[must_use]
pub fn error_text(error: &io::Error) -> String {
    let text = error.to_string();
    match text.rfind(" (os error ") {
        Some(index) if text.ends_with(')') => text.get(..index).unwrap_or(&text).to_owned(),
        _ => text,
    }
}
