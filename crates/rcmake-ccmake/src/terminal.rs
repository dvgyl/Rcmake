//! Terminal abstraction, RAII raw-mode guard, and platform backends.

#[cfg(unix)]
pub mod native;
pub mod scripted;

use std::fmt;

/// Error produced by [`Terminal`] operations.
#[derive(Debug)]
pub enum TerminalError {
    /// Standard input or standard output is not attached to an interactive TTY,
    /// or terminal control is unsupported on this platform.
    NotATty,
    /// Underlying I/O or termios error.
    Io(std::io::Error),
}

impl fmt::Display for TerminalError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotATty => f.write_str("not a terminal"),
            Self::Io(err) => write!(f, "{err}"),
        }
    }
}

impl std::error::Error for TerminalError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::NotATty => None,
            Self::Io(err) => Some(err),
        }
    }
}

impl From<std::io::Error> for TerminalError {
    fn from(err: std::io::Error) -> Self {
        Self::Io(err)
    }
}

/// Abstract terminal I/O boundary. The TUI state machine never touches OS devices directly.
pub trait Terminal {
    /// Current terminal size as `(cols, rows)`, or `None` if unavailable.
    fn size(&mut self) -> Option<(u16, u16)>;

    /// Enter raw mode and switch to the alternate screen buffer. Idempotent.
    ///
    /// Implementations must arm restoration before any terminal mutation and roll
    /// back partial initialization on error.
    fn enter_raw(&mut self) -> Result<(), TerminalError>;

    /// Restore original terminal attributes and leave the alternate screen buffer.
    ///
    /// Must be idempotent and infallible.
    fn leave_raw(&mut self);

    /// Read raw key bytes into `buf`. Returning `Ok(0)` indicates a poll timeout tick.
    fn read(&mut self, buf: &mut [u8]) -> Result<usize, TerminalError>;

    /// Write all `bytes` to the terminal output and flush.
    fn write_all(&mut self, bytes: &[u8]) -> Result<(), TerminalError>;
}

/// RAII guard that enters raw mode on construction and restores the terminal on drop.
pub struct RawGuard<'a, T: Terminal> {
    terminal: &'a mut T,
    active: bool,
}

impl<'a, T: Terminal> RawGuard<'a, T> {
    /// Enter raw mode on `terminal` and return an armed guard.
    pub fn enter(terminal: &'a mut T) -> Result<Self, TerminalError> {
        terminal.enter_raw()?;
        Ok(Self {
            terminal,
            active: true,
        })
    }

    /// Borrow the guarded terminal mutably.
    pub fn terminal_mut(&mut self) -> &mut T {
        self.terminal
    }

    /// Explicitly restore the terminal and disarm the guard.
    pub fn leave(mut self) {
        if self.active {
            self.terminal.leave_raw();
            self.active = false;
        }
    }
}

impl<T: Terminal> Drop for RawGuard<'_, T> {
    fn drop(&mut self) {
        if self.active {
            self.terminal.leave_raw();
            self.active = false;
        }
    }
}
