//! Deterministic scripted [`Terminal`] implementation for automated tests.

use std::collections::VecDeque;
use std::io;

use super::{Terminal, TerminalError};

/// One scripted input event fed to [`ScriptedTerminal`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ScriptEvent {
    /// Raw bytes returned by a single [`Terminal::read`] call.
    Bytes(Vec<u8>),
    /// Timeout tick (`read` returns `Ok(0)`).
    Tick,
    /// Terminal resize to `(cols, rows)` followed by a timeout tick.
    Resize(u16, u16),
}

/// Deterministic in-memory terminal for unit and integration tests.
#[doc(hidden)]
pub struct ScriptedTerminal {
    cols: u16,
    rows: u16,
    events: VecDeque<ScriptEvent>,
    raw: bool,
    enter_count: usize,
    leave_count: usize,
    fail_enter: bool,
    output: Vec<u8>,
    virtual_grid: VirtualScreen,
}

impl ScriptedTerminal {
    /// Construct a scripted terminal with initial dimensions `(cols, rows)`.
    #[must_use]
    pub fn new(cols: u16, rows: u16) -> Self {
        Self {
            cols,
            rows,
            events: VecDeque::new(),
            raw: false,
            enter_count: 0,
            leave_count: 0,
            fail_enter: false,
            output: Vec::new(),
            virtual_grid: VirtualScreen::new(cols, rows),
        }
    }

    /// Configure [`Terminal::enter_raw`] to fail with [`TerminalError::NotATty`].
    pub fn set_fail_enter(&mut self, fail: bool) {
        self.fail_enter = fail;
    }

    /// Enqueue a chunk of raw input bytes.
    pub fn push_bytes(&mut self, bytes: &[u8]) {
        self.events.push_back(ScriptEvent::Bytes(bytes.to_vec()));
    }

    /// Enqueue a single timeout tick (`Ok(0)`).
    pub fn push_tick(&mut self) {
        self.events.push_back(ScriptEvent::Tick);
    }

    /// Enqueue a terminal resize event.
    pub fn push_resize(&mut self, cols: u16, rows: u16) {
        self.events.push_back(ScriptEvent::Resize(cols, rows));
    }

    /// Return whether raw mode is currently active.
    #[must_use]
    pub const fn is_raw(&self) -> bool {
        self.raw
    }

    /// Number of times [`Terminal::enter_raw`] succeeded.
    #[must_use]
    pub const fn enter_count(&self) -> usize {
        self.enter_count
    }

    /// Number of times [`Terminal::leave_raw`] ran while active.
    #[must_use]
    pub const fn leave_count(&self) -> usize {
        self.leave_count
    }

    /// Raw VT100 bytes written to the terminal across the session.
    #[must_use]
    pub fn output_bytes(&self) -> &[u8] {
        &self.output
    }

    /// Current rendered text of row `row` (0-indexed) as tracked by the built-in VT100 parser.
    #[must_use]
    pub fn row_text(&self, row: u16) -> String {
        self.virtual_grid.row_text(row)
    }

    /// All rendered rows of the current virtual screen.
    #[must_use]
    pub fn screen_lines(&self) -> Vec<String> {
        self.virtual_grid.lines()
    }
}

impl Terminal for ScriptedTerminal {
    fn size(&mut self) -> Option<(u16, u16)> {
        if self.cols == 0 || self.rows == 0 {
            None
        } else {
            Some((self.cols, self.rows))
        }
    }

    fn enter_raw(&mut self) -> Result<(), TerminalError> {
        if self.fail_enter {
            return Err(TerminalError::NotATty);
        }
        if !self.raw {
            self.raw = true;
            self.enter_count = self.enter_count.saturating_add(1);
        }
        Ok(())
    }

    fn leave_raw(&mut self) {
        if self.raw {
            self.raw = false;
            self.leave_count = self.leave_count.saturating_add(1);
        }
    }

    fn read(&mut self, buf: &mut [u8]) -> Result<usize, TerminalError> {
        let Some(event) = self.events.pop_front() else {
            return Err(TerminalError::Io(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "script exhausted",
            )));
        };
        match event {
            ScriptEvent::Tick => Ok(0),
            ScriptEvent::Resize(cols, rows) => {
                self.cols = cols;
                self.rows = rows;
                self.virtual_grid.resize(cols, rows);
                Ok(0)
            }
            ScriptEvent::Bytes(mut bytes) => {
                if bytes.is_empty() || buf.is_empty() {
                    return Ok(0);
                }
                let take = bytes.len().min(buf.len());
                if let (Some(dst), Some(src)) = (buf.get_mut(..take), bytes.get(..take)) {
                    dst.copy_from_slice(src);
                }
                if take < bytes.len() {
                    let remainder = bytes.split_off(take);
                    self.events.push_front(ScriptEvent::Bytes(remainder));
                }
                Ok(take)
            }
        }
    }

    fn write_all(&mut self, bytes: &[u8]) -> Result<(), TerminalError> {
        self.output.extend_from_slice(bytes);
        self.virtual_grid.feed(bytes);
        Ok(())
    }
}

/// Minimal VT100 screen emulator used by [`ScriptedTerminal`] to inspect rendered rows.
struct VirtualScreen {
    cols: usize,
    rows: usize,
    cells: Vec<char>,
    cur_row: usize,
    cur_col: usize,
    state: VtState,
    params: Vec<u8>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum VtState {
    Ground,
    Esc,
    Csi,
}

impl VirtualScreen {
    fn new(cols: u16, rows: u16) -> Self {
        let cols = usize::from(cols);
        let rows = usize::from(rows);
        Self {
            cols,
            rows,
            cells: vec![' '; cols.saturating_mul(rows)],
            cur_row: 0,
            cur_col: 0,
            state: VtState::Ground,
            params: Vec::new(),
        }
    }

    fn resize(&mut self, cols: u16, rows: u16) {
        self.cols = usize::from(cols);
        self.rows = usize::from(rows);
        self.cells = vec![' '; self.cols.saturating_mul(self.rows)];
        self.cur_row = 0;
        self.cur_col = 0;
    }

    fn feed(&mut self, bytes: &[u8]) {
        for &byte in bytes {
            match self.state {
                VtState::Ground => match byte {
                    0x1b => {
                        self.state = VtState::Esc;
                        self.params.clear();
                    }
                    b'\r' => self.cur_col = 0,
                    b'\n' => {
                        if self.cur_row + 1 < self.rows {
                            self.cur_row += 1;
                        }
                    }
                    0x20..=0x7e => self.put_char(char::from(byte)),
                    _ => {}
                },
                VtState::Esc => {
                    if byte == b'[' {
                        self.state = VtState::Csi;
                        self.params.clear();
                    } else {
                        self.state = VtState::Ground;
                    }
                }
                VtState::Csi => {
                    if (0x40..=0x7e).contains(&byte) {
                        self.exec_csi(byte);
                        self.state = VtState::Ground;
                        self.params.clear();
                    } else if self.params.len() < 32 {
                        self.params.push(byte);
                    }
                }
            }
        }
    }

    fn exec_csi(&mut self, final_byte: u8) {
        match final_byte {
            b'H' | b'f' => {
                let text = std::str::from_utf8(&self.params).unwrap_or("");
                let mut parts = text.split(';');
                let row = parts
                    .next()
                    .and_then(|s| s.parse::<usize>().ok())
                    .unwrap_or(1)
                    .saturating_sub(1);
                let col = parts
                    .next()
                    .and_then(|s| s.parse::<usize>().ok())
                    .unwrap_or(1)
                    .saturating_sub(1);
                self.cur_row = row.min(self.rows.saturating_sub(1));
                self.cur_col = col.min(self.cols.saturating_sub(1));
            }
            b'J' => {
                if self.params.as_slice() == b"2" {
                    self.cells.fill(' ');
                }
            }
            b'K' => {
                let start = self
                    .cur_row
                    .saturating_mul(self.cols)
                    .saturating_add(self.cur_col);
                let end = self.cur_row.saturating_add(1).saturating_mul(self.cols);
                if let Some(slice) = self.cells.get_mut(start..end) {
                    slice.fill(' ');
                }
            }
            _ => {}
        }
    }

    fn put_char(&mut self, ch: char) {
        if self.cur_row < self.rows && self.cur_col < self.cols {
            let idx = self
                .cur_row
                .saturating_mul(self.cols)
                .saturating_add(self.cur_col);
            if let Some(cell) = self.cells.get_mut(idx) {
                *cell = ch;
            }
            self.cur_col = self.cur_col.saturating_add(1);
        }
    }

    fn row_text(&self, row: u16) -> String {
        let row = usize::from(row);
        if row >= self.rows {
            return String::new();
        }
        let start = row.saturating_mul(self.cols);
        let end = start.saturating_add(self.cols);
        self.cells
            .get(start..end)
            .map(|slice| slice.iter().collect())
            .unwrap_or_default()
    }

    fn lines(&self) -> Vec<String> {
        (0..self.rows)
            .map(|r| self.row_text(u16::try_from(r).unwrap_or(u16::MAX)))
            .collect()
    }
}
