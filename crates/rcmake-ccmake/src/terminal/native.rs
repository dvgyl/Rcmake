//! POSIX terminal backend using safe `rustix::termios` (zero `unsafe`).
//!
//! Cleanup is pre-armed BEFORE any terminal mutation:
//! - Original `Termios` is captured prior to changing mode.
//! - A one-time chained panic hook restores both the alternate screen and saved
//!   `Termios` even under release `panic = "abort"` (before delegating to the
//!   previously installed hook).
//! - Any partial failure during `enter_raw` rolls back every mutation that
//!   already succeeded.

use std::io::{self, ErrorKind, Read, Write};
use std::sync::{Mutex, Once};

use rustix::termios::{
    InputModes, LocalModes, OptionalActions, SpecialCodeIndex, Termios, isatty, tcgetattr,
    tcgetwinsize, tcsetattr,
};

use super::{Terminal, TerminalError};

const ENTER_ALT_SCREEN: &[u8] = b"\x1b[?1049h\x1b[?25l";
const LEAVE_ALT_SCREEN: &[u8] = b"\x1b[?25h\x1b[0m\x1b[?1049l";

struct ActiveTerminalState {
    saved: Termios,
    alt_screen: bool,
}

static ACTIVE_STATE: Mutex<Option<ActiveTerminalState>> = Mutex::new(None);
static PANIC_HOOK_ONCE: Once = Once::new();

fn install_emergency_restore_hook() {
    PANIC_HOOK_ONCE.call_once(|| {
        let prev = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            restore_emergency_terminal();
            prev(info);
        }));
    });
}

/// Best-effort restoration of any active native raw terminal state.
///
/// Safe to invoke from panic hooks or cleanup paths; never panics.
pub fn restore_emergency_terminal() {
    let state = ACTIVE_STATE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .take();
    let Some(state) = state else {
        return;
    };
    if state.alt_screen {
        let mut out = io::stdout();
        let _ = out.write_all(LEAVE_ALT_SCREEN);
        let _ = out.flush();
    }
    let stdin = io::stdin();
    let _ = tcsetattr(&stdin, OptionalActions::Now, &state.saved);
}

/// Native POSIX terminal attached to standard input (fd 0) and standard output (fd 1).
pub struct NativeTerminal {
    saved: Option<Termios>,
    alt_screen: bool,
    active: bool,
}

impl NativeTerminal {
    /// Verify that stdin and stdout are interactive terminals without mutating
    /// terminal attributes.
    pub fn open() -> Result<Self, TerminalError> {
        let stdin = io::stdin();
        let stdout = io::stdout();
        if !isatty(&stdin) || !isatty(&stdout) {
            return Err(TerminalError::NotATty);
        }
        // Probe tcgetattr early so non-termios devices fail cleanly before any output.
        tcgetattr(&stdin).map_err(|err| TerminalError::Io(io::Error::from(err)))?;
        Ok(Self {
            saved: None,
            alt_screen: false,
            active: false,
        })
    }
}

impl Terminal for NativeTerminal {
    fn size(&mut self) -> Option<(u16, u16)> {
        let ws = tcgetwinsize(io::stdout())
            .or_else(|_| tcgetwinsize(io::stdin()))
            .ok()?;
        if ws.ws_col == 0 || ws.ws_row == 0 {
            None
        } else {
            Some((ws.ws_col, ws.ws_row))
        }
    }

    fn enter_raw(&mut self) -> Result<(), TerminalError> {
        if self.active {
            return Ok(());
        }
        let stdin = io::stdin();
        let stdout = io::stdout();
        if !isatty(&stdin) || !isatty(&stdout) {
            return Err(TerminalError::NotATty);
        }

        let saved = tcgetattr(&stdin).map_err(|err| TerminalError::Io(io::Error::from(err)))?;

        install_emergency_restore_hook();
        {
            let mut slot = ACTIVE_STATE
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            *slot = Some(ActiveTerminalState {
                saved: saved.clone(),
                alt_screen: false,
            });
        }
        self.saved = Some(saved.clone());

        // Raw mode with VMIN=0, VTIME=2 (200 ms poll ticks).
        let mut raw = saved;
        raw.local_modes
            .remove(LocalModes::ICANON | LocalModes::ECHO | LocalModes::ISIG);
        raw.input_modes.remove(InputModes::IXON | InputModes::ICRNL);
        raw.special_codes[SpecialCodeIndex::VMIN] = 0;
        raw.special_codes[SpecialCodeIndex::VTIME] = 2;

        if let Err(err) = tcsetattr(&stdin, OptionalActions::Now, &raw) {
            self.saved = None;
            let mut slot = ACTIVE_STATE
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            *slot = None;
            return Err(TerminalError::Io(io::Error::from(err)));
        }

        // Arm the alternate screen before its enter sequence, so a partial write rolls back.
        self.active = true;
        self.alt_screen = true;
        {
            let mut slot = ACTIVE_STATE
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some(active) = slot.as_mut() {
                active.alt_screen = true;
            }
        }

        let mut out = io::stdout();
        if let Err(err) = out.write_all(ENTER_ALT_SCREEN).and_then(|()| out.flush()) {
            self.leave_raw();
            return Err(TerminalError::Io(err));
        }

        Ok(())
    }

    fn leave_raw(&mut self) {
        if !self.active && self.saved.is_none() && !self.alt_screen {
            return;
        }
        if self.alt_screen {
            let mut out = io::stdout();
            let _ = out.write_all(LEAVE_ALT_SCREEN);
            let _ = out.flush();
            self.alt_screen = false;
        }
        if let Some(saved) = self.saved.take() {
            let stdin = io::stdin();
            let _ = tcsetattr(&stdin, OptionalActions::Now, &saved);
        }
        {
            let mut slot = ACTIVE_STATE
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            *slot = None;
        }
        self.active = false;
    }

    fn read(&mut self, buf: &mut [u8]) -> Result<usize, TerminalError> {
        let mut stdin = io::stdin();
        match stdin.read(buf) {
            Ok(n) => Ok(n),
            Err(err) if err.kind() == ErrorKind::Interrupted => Ok(0),
            Err(err) => Err(TerminalError::Io(err)),
        }
    }

    fn write_all(&mut self, bytes: &[u8]) -> Result<(), TerminalError> {
        let mut out = io::stdout();
        out.write_all(bytes)?;
        out.flush()?;
        Ok(())
    }
}

impl Drop for NativeTerminal {
    fn drop(&mut self) {
        self.leave_raw();
    }
}
