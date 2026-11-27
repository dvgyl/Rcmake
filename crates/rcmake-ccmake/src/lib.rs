//! Clean-room interactive terminal cache editor (`ccmake`) for `rcmake`.
//!
//! All terminal I/O passes through [`Terminal`], and all CMake cache loading,
//! configure, generate, and filesystem queries pass through [`CacheDriver`].
//! This keeps the entire TUI state machine deterministic and testable without
//! touching OS devices or `std::fs` directly.

#![forbid(unsafe_code)]

pub mod app;
pub mod cli;
pub mod keys;
pub mod layout;
pub mod log_view;
pub mod model;
pub mod screen;
pub mod terminal;
pub mod widgets;

use std::collections::BTreeSet;
use std::fmt;
use std::io::Write;

use rcmake_base::BString;
use rcmake_model::CacheType;

use crate::app::{App, Effect};
use crate::keys::KeyDecoder;
use crate::layout::{ERR_INIT_FAILED, ERR_LOAD_CACHE, ERR_WINDOW_TOO_SMALL, MIN_HEIGHT, MIN_WIDTH};
use crate::screen::{Renderer, Screen};
use crate::terminal::{RawGuard, Terminal};

/// Single directory entry returned by [`CacheDriver::list_dir`] for Tab path completion.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DirItem {
    /// Entry filename (single path component).
    pub name: BString,
    /// `true` when the entry is a directory (or symlink resolving to a directory).
    pub is_dir: bool,
}

/// Snapshot of a single cache variable presented by [`CacheDriver::load`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EntrySnapshot {
    /// Cache variable name.
    pub name: BString,
    /// Cache entry type (`BOOL`, `STRING`, `PATH`, `FILEPATH`, `INTERNAL`, `STATIC`, `UNINITIALIZED`).
    pub ty: CacheType,
    /// Current entry byte value.
    pub value: BString,
    /// Entry help string (`HELPSTRING`).
    pub help: BString,
    /// Whether the entry is marked `ADVANCED`.
    pub advanced: bool,
    /// Optional enumerated choice list from the `STRINGS` cache entry property.
    pub strings: Vec<BString>,
}

/// Complete cache snapshot returned by [`CacheDriver::load`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CacheSnapshot {
    /// All cache entries currently present in the cache.
    pub entries: Vec<EntrySnapshot>,
}

/// User edit queued in the TUI for the next configure step.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EntryEdit {
    /// Update or insert a cache entry value.
    Set {
        /// Cache variable name.
        name: BString,
        /// Cache entry type.
        ty: CacheType,
        /// Updated value.
        value: BString,
    },
    /// Remove a cache entry from the cache.
    Remove {
        /// Cache variable name to delete.
        name: BString,
    },
}

/// One line of captured configure or generate output.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LogLine {
    /// `true` if emitted on stderr or as a CMake warning/error diagnostic.
    pub is_error: bool,
    /// Raw line bytes without trailing newline.
    pub text: BString,
}

/// Outcome of a single [`CacheDriver::configure`] or [`CacheDriver::generate`] run.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RunReport {
    /// Process-style exit code (`0` on success).
    pub exit_code: i32,
    /// Captured stdout and stderr output lines in emission order.
    pub lines: Vec<LogLine>,
    /// `true` if any non-status output (error, warning, or stderr output) occurred.
    pub has_non_status: bool,
    /// Optional explicit set of newly created cache entry names.
    pub new_entries: BTreeSet<BString>,
}

/// Error returned by [`CacheDriver::load`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DriverError {
    /// Human-readable error description.
    pub message: String,
}

impl DriverError {
    /// Create a new driver error with the given description.
    #[must_use]
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for DriverError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for DriverError {}

/// Backend interface that loads the cache, runs configure/generate in-process,
/// and lists directories for path completion.
pub trait CacheDriver {
    /// Load the current cache entries.
    fn load(&mut self) -> Result<CacheSnapshot, DriverError>;

    /// Run configure with the queued edits, writing `CMakeCache.txt` without
    /// generating build files.
    fn configure(&mut self, edits: &[EntryEdit]) -> RunReport;

    /// Run generation on the retained configured state without re-running configure.
    fn generate(&mut self) -> RunReport;

    /// Notify the driver that the user edited or deleted a cache entry,
    /// invalidating any retained configured state.
    fn invalidate(&mut self) {}

    /// List entries inside `dir` for Tab path completion (never uses `std::fs`).
    fn list_dir(&mut self, _dir: &[u8]) -> Vec<DirItem> {
        Vec::new()
    }
}

/// Run the interactive `ccmake` TUI loop on `terminal` backed by `driver`.
///
/// Any fatal startup diagnostic (terminal initialization failure, window smaller
/// than 65 × 6, or cache load failure) is written to `stderr` after restoring the
/// terminal to its original mode, and exit code `1` is returned.
pub fn run<T: Terminal, D: CacheDriver>(
    terminal: &mut T,
    driver: &mut D,
    stderr: &mut dyn Write,
) -> i32 {
    let mut guard = match RawGuard::enter(terminal) {
        Ok(guard) => guard,
        Err(_) => {
            let _ = stderr.write_all(ERR_INIT_FAILED.as_bytes());
            let _ = stderr.flush();
            return 1;
        }
    };

    let Some((mut cols, mut rows)) = guard.terminal_mut().size() else {
        guard.leave();
        let _ = stderr.write_all(ERR_INIT_FAILED.as_bytes());
        let _ = stderr.flush();
        return 1;
    };

    if cols < MIN_WIDTH || rows < MIN_HEIGHT {
        guard.leave();
        let _ = stderr.write_all(ERR_WINDOW_TOO_SMALL.as_bytes());
        let _ = stderr.flush();
        return 1;
    }

    let snapshot = match driver.load() {
        Ok(snapshot) => snapshot,
        Err(_) => {
            guard.leave();
            let _ = stderr.write_all(ERR_LOAD_CACHE.as_bytes());
            let _ = stderr.flush();
            return 1;
        }
    };

    let mut app = App::new(snapshot);
    let mut renderer = Renderer::new();
    let mut decoder = KeyDecoder::new();
    let mut read_buf = [0u8; 64];

    if draw_screen(guard.terminal_mut(), &mut renderer, &app, cols, rows, None).is_err() {
        guard.leave();
        return 1;
    }

    let exit_code = 'event_loop: loop {
        let count = match guard.terminal_mut().read(&mut read_buf) {
            Ok(n) => n,
            Err(_) => break 'event_loop 1,
        };

        if count == 0 {
            let mut needs_redraw = false;
            if let Some(key) = decoder.flush_timeout() {
                match dispatch_key(
                    &mut app,
                    driver,
                    guard.terminal_mut(),
                    &mut renderer,
                    key,
                    cols,
                    rows,
                ) {
                    LoopAction::Continue(redraw) => needs_redraw |= redraw,
                    LoopAction::Exit(code) => break 'event_loop code,
                }
            }
            if let Some((new_cols, new_rows)) = guard.terminal_mut().size()
                && (new_cols != cols || new_rows != rows)
            {
                cols = new_cols;
                rows = new_rows;
                renderer.invalidate();
                needs_redraw = true;
            }
            if needs_redraw
                && draw_screen(guard.terminal_mut(), &mut renderer, &app, cols, rows, None).is_err()
            {
                break 'event_loop 1;
            }
            continue;
        }

        let Some(chunk) = read_buf.get(..count) else {
            continue;
        };
        for &byte in chunk {
            if let Some(key) = decoder.push(byte) {
                match dispatch_key(
                    &mut app,
                    driver,
                    guard.terminal_mut(),
                    &mut renderer,
                    key,
                    cols,
                    rows,
                ) {
                    LoopAction::Continue(true) => {
                        if draw_screen(guard.terminal_mut(), &mut renderer, &app, cols, rows, None)
                            .is_err()
                        {
                            break 'event_loop 1;
                        }
                    }
                    LoopAction::Continue(false) => {}
                    LoopAction::Exit(code) => break 'event_loop code,
                }
            }
        }
    };

    guard.leave();
    exit_code
}

enum LoopAction {
    Continue(bool),
    Exit(i32),
}

fn dispatch_key<T: Terminal, D: CacheDriver>(
    app: &mut App,
    driver: &mut D,
    terminal: &mut T,
    renderer: &mut Renderer,
    key: crate::keys::Key,
    cols: u16,
    rows: u16,
) -> LoopAction {
    match app.handle(key, cols, rows) {
        Effect::None => LoopAction::Continue(false),
        Effect::Redraw => LoopAction::Continue(true),
        Effect::InvalidateAndRedraw => {
            driver.invalidate();
            LoopAction::Continue(true)
        }
        Effect::CompletePath { dir } => {
            let items = driver.list_dir(&dir);
            app.apply_path_completion(items);
            LoopAction::Continue(true)
        }
        Effect::Configure => {
            if draw_screen(terminal, renderer, app, cols, rows, Some("Configuring...")).is_err() {
                return LoopAction::Exit(1);
            }
            let edits = app.take_pending_edits();
            let report = driver.configure(&edits);
            let reloaded = driver.load().ok();
            app.finish_configure(report, reloaded);
            renderer.invalidate();
            LoopAction::Continue(true)
        }
        Effect::Generate => {
            if draw_screen(terminal, renderer, app, cols, rows, Some("Generating...")).is_err() {
                return LoopAction::Exit(1);
            }
            let report = driver.generate();
            if report.exit_code == 0 && !report.has_non_status {
                return LoopAction::Exit(0);
            }
            let succeeded = report.exit_code == 0;
            app.finish_generate(report);
            if succeeded {
                // Generation succeeded with warnings; show the output screen so the
                // user can inspect it, then exit cleanly when dismissed or immediately.
                renderer.invalidate();
                return LoopAction::Continue(true);
            }
            renderer.invalidate();
            LoopAction::Continue(true)
        }
        Effect::Quit(code) => LoopAction::Exit(code),
    }
}

fn draw_screen<T: Terminal>(
    terminal: &mut T,
    renderer: &mut Renderer,
    app: &App,
    cols: u16,
    rows: u16,
    busy_message: Option<&str>,
) -> Result<(), crate::terminal::TerminalError> {
    let mut screen = Screen::new(cols, rows);
    app.render(&mut screen, busy_message);
    let bytes = renderer.render(&screen);
    if !bytes.is_empty() {
        terminal.write_all(&bytes)?;
    }
    Ok(())
}
