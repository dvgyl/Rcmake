//! Windows console backend using safe `winapi_util::console` wrappers (zero `unsafe`).
//!
//! The backend drives the console through virtual terminal (VT) sequences, so the
//! renderer and key decoder are the same on Windows and on POSIX systems:
//! - Output mode adds `ENABLE_PROCESSED_OUTPUT`, `ENABLE_VIRTUAL_TERMINAL_PROCESSING`
//!   (which requires processed output), and `DISABLE_NEWLINE_AUTO_RETURN`.
//! - Input mode removes line input, echo, and processed input (Ctrl+C arrives as
//!   byte `0x03`), and adds `ENABLE_VIRTUAL_TERMINAL_INPUT` (arrow keys arrive as
//!   `ESC [` sequences).
//!
//! Cleanup is pre-armed BEFORE any console mutation:
//! - The original input and output modes are captured before any mode change.
//! - One shared state slot records what must be restored. The panic hook,
//!   `leave_raw`, and `Drop` all take that slot, so the restore runs exactly once.
//!   After the panic hook restores the console, `Drop` does not write the
//!   alternate screen exit sequence again to a console without VT processing.
//! - The one-time chained panic hook restores the console even under release
//!   `panic = "abort"`, and then calls the previously installed hook.
//! - Any partial failure during `enter_raw` rolls back every mutation that
//!   already succeeded.
//!
//! The console has no `VMIN`/`VTIME` timed read. A background thread reads
//! standard input and sends each chunk through a bounded channel.
//! [`Terminal::read`](crate::terminal::Terminal::read)
//! waits for at most one 200 ms tick (the same tick as the POSIX `VTIME=2`
//! setting) and returns `Ok(0)` when no input arrives.
//!
//! A blocked console read cannot be cancelled without `CancelIoEx`, which needs
//! `unsafe` FFI. So the reader thread lives for the whole process, and a session
//! gate controls it instead:
//! - The reader starts a new read only while a raw session is active.
//! - The reader stamps each chunk with the session that was active when the
//!   read completed.
//! - [`Terminal::read`](crate::terminal::Terminal::read) discards a chunk from an
//!   earlier session or from a time without a session, and `enter_raw` discards
//!   all buffered input. Keys from one session never reach the next session.

use std::io::{self, ErrorKind, Read};
use std::sync::mpsc::{Receiver, RecvTimeoutError, SyncSender, TryRecvError};
use std::sync::{Condvar, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

/// Console input mode flag: `ReadConsole` handles Ctrl+C as a signal.
const ENABLE_PROCESSED_INPUT: u32 = 0x0001;
/// Console input mode flag: `ReadConsole` returns only after Enter.
const ENABLE_LINE_INPUT: u32 = 0x0002;
/// Console input mode flag: the console echoes typed characters.
const ENABLE_ECHO_INPUT: u32 = 0x0004;
/// Console input mode flag: keys arrive as VT input sequences.
const ENABLE_VIRTUAL_TERMINAL_INPUT: u32 = 0x0200;
/// Console output mode flag: the console handles control characters.
/// `ENABLE_VIRTUAL_TERMINAL_PROCESSING` requires this flag.
const ENABLE_PROCESSED_OUTPUT: u32 = 0x0001;
/// Console output mode flag: the console interprets VT output sequences.
const ENABLE_VIRTUAL_TERMINAL_PROCESSING: u32 = 0x0004;
/// Console output mode flag: VT-style delayed wrap at the last column.
const DISABLE_NEWLINE_AUTO_RETURN: u32 = 0x0008;

/// Poll tick for `Terminal::read`. Matches the POSIX backend `VTIME=2` (200 ms).
const READ_TICK: Duration = Duration::from_millis(200);
/// Maximum number of input chunks that wait in the channel before the reader blocks.
const CHANNEL_CAPACITY: usize = 16;
/// Size of one read from standard input.
const READ_CHUNK_BYTES: usize = 1024;
/// Wait time after an empty read from a console that is still open.
///
/// A console read returns zero bytes when the user types only Ctrl+Z, because the
/// Rust standard library removes a trailing Ctrl+Z. The console is still open, so
/// the reader waits a short time and reads again.
const EMPTY_READ_BACKOFF: Duration = Duration::from_millis(10);

/// Return the console input mode for raw VT key input.
fn raw_input_mode(original: u32) -> u32 {
    (original & !(ENABLE_LINE_INPUT | ENABLE_ECHO_INPUT | ENABLE_PROCESSED_INPUT))
        | ENABLE_VIRTUAL_TERMINAL_INPUT
}

/// Return the console output mode for VT rendering.
fn vt_output_mode(original: u32) -> u32 {
    original
        | ENABLE_PROCESSED_OUTPUT
        | ENABLE_VIRTUAL_TERMINAL_PROCESSING
        | DISABLE_NEWLINE_AUTO_RETURN
}

/// Convert an inclusive console window rectangle to `(cols, rows)`.
///
/// Returns `None` for an empty, inverted, or too large rectangle.
fn window_cells(left: i16, top: i16, right: i16, bottom: i16) -> Option<(u16, u16)> {
    let cols = i32::from(right) - i32::from(left) + 1;
    let rows = i32::from(bottom) - i32::from(top) + 1;
    let cols = u16::try_from(cols).ok().filter(|&c| c > 0)?;
    let rows = u16::try_from(rows).ok().filter(|&r| r > 0)?;
    Some((cols, rows))
}

/// State behind [`SessionGate`].
#[derive(Clone, Copy, Debug)]
struct GateState {
    /// Identifier of the most recent raw session.
    generation: u64,
    /// `true` while that raw session is active.
    active: bool,
}

/// Raw-session gate shared by the terminal and the background reader.
#[derive(Debug)]
struct SessionGate {
    state: Mutex<GateState>,
    changed: Condvar,
}

impl SessionGate {
    const fn new() -> Self {
        Self {
            state: Mutex::new(GateState {
                generation: 0,
                active: false,
            }),
            changed: Condvar::new(),
        }
    }

    fn lock(&self) -> MutexGuard<'_, GateState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Start a new raw session and wake the reader.
    fn begin(&self) {
        {
            let mut state = self.lock();
            state.generation = state.generation.wrapping_add(1);
            state.active = true;
        }
        self.changed.notify_all();
    }

    /// End the active raw session. The reader starts no new read after this.
    fn end(&self) {
        self.lock().active = false;
    }

    /// Identifier of the active raw session, or `None` without a session.
    fn current(&self) -> Option<u64> {
        let state = self.lock();
        state.active.then_some(state.generation)
    }

    /// Block until a raw session is active.
    fn wait_active(&self) {
        let guard = self.lock();
        let _active = self
            .changed
            .wait_while(guard, |state| !state.active)
            .unwrap_or_else(PoisonError::into_inner);
    }
}

/// One message from the background input reader.
#[derive(Debug)]
enum ReaderEvent {
    /// Bytes read from the input stream.
    Data {
        /// Raw session that was active when the read completed.
        session: Option<u64>,
        /// The bytes.
        bytes: Vec<u8>,
    },
    /// The input stream is closed. The error is `None` at end of input.
    Closed(Option<io::Error>),
}

/// Read `reader` until end of input or error and send each chunk to `tx`.
///
/// Each read starts only while `gate` has an active session. After an empty read,
/// `is_open` tells if the input is still open: if yes, the loop waits
/// [`EMPTY_READ_BACKOFF`] and reads again; if no, the loop reports end of input.
/// The loop also stops when the receiver is dropped.
fn pump_input<R: Read, F: Fn() -> bool>(
    mut reader: R,
    tx: &SyncSender<ReaderEvent>,
    gate: &SessionGate,
    is_open: F,
) {
    let mut buf = vec![0u8; READ_CHUNK_BYTES];
    loop {
        gate.wait_active();
        let event = match reader.read(&mut buf) {
            Ok(0) => {
                if is_open() {
                    std::thread::sleep(EMPTY_READ_BACKOFF);
                    continue;
                }
                ReaderEvent::Closed(None)
            }
            Ok(n) => ReaderEvent::Data {
                session: gate.current(),
                bytes: buf.get(..n).unwrap_or_default().to_vec(),
            },
            Err(err) if err.kind() == ErrorKind::Interrupted => continue,
            Err(err) => ReaderEvent::Closed(Some(err)),
        };
        let closed = matches!(event, ReaderEvent::Closed(_));
        if tx.send(event).is_err() || closed {
            return;
        }
    }
}

/// Start a named background thread that reads `reader` into a bounded channel.
fn spawn_reader<R, F>(
    reader: R,
    gate: &'static SessionGate,
    is_open: F,
) -> io::Result<Receiver<ReaderEvent>>
where
    R: Read + Send + 'static,
    F: Fn() -> bool + Send + 'static,
{
    let (tx, rx) = std::sync::mpsc::sync_channel(CHANNEL_CAPACITY);
    let _detached = std::thread::Builder::new()
        .name("rcmake-ccmake-input".to_owned())
        .spawn(move || pump_input(reader, &tx, gate, is_open))?;
    Ok(rx)
}

/// Timed input: the receiving half of the background reader plus any bytes that
/// did not fit in the caller buffer.
#[derive(Debug)]
struct TimedInput {
    rx: Receiver<ReaderEvent>,
    gate: &'static SessionGate,
    pending: Vec<u8>,
    closed: bool,
    tick: Duration,
}

impl TimedInput {
    fn new(rx: Receiver<ReaderEvent>, gate: &'static SessionGate, tick: Duration) -> Self {
        Self {
            rx,
            gate,
            pending: Vec::new(),
            closed: false,
            tick,
        }
    }

    /// Discard all buffered input: the pending bytes and every queued chunk.
    fn discard_buffered(&mut self) {
        self.pending.clear();
        loop {
            match self.rx.try_recv() {
                Ok(ReaderEvent::Data { .. }) => {}
                Ok(ReaderEvent::Closed(_)) | Err(TryRecvError::Disconnected) => {
                    self.closed = true;
                    return;
                }
                Err(TryRecvError::Empty) => return,
            }
        }
    }

    /// Copy available input of the active session into `buf`.
    ///
    /// Waits at most one tick. Returns `Ok(0)` on a tick without input, and on a
    /// chunk from an earlier session (the chunk is discarded). After the input
    /// closes, each call waits one tick and returns `Ok(0)`, so the event loop
    /// keeps its cadence without a busy loop.
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        if self.pending.is_empty() {
            if self.closed {
                std::thread::sleep(self.tick);
                return Ok(0);
            }
            match self.rx.recv_timeout(self.tick) {
                Ok(ReaderEvent::Data { session, bytes }) => {
                    if session.is_none() || session != self.gate.current() {
                        return Ok(0);
                    }
                    self.pending = bytes;
                }
                Ok(ReaderEvent::Closed(None)) | Err(RecvTimeoutError::Disconnected) => {
                    self.closed = true;
                    return Ok(0);
                }
                Ok(ReaderEvent::Closed(Some(err))) => {
                    self.closed = true;
                    return Err(err);
                }
                Err(RecvTimeoutError::Timeout) => return Ok(0),
            }
        }
        let count = buf.len().min(self.pending.len());
        for (dst, src) in buf.iter_mut().zip(self.pending.drain(..count)) {
            *dst = src;
        }
        Ok(count)
    }
}

#[cfg(windows)]
pub use console::{NativeTerminal, restore_emergency_terminal};

#[cfg(windows)]
mod console {
    use std::io::{self, IsTerminal as _, Write};
    use std::sync::{Mutex, Once, PoisonError};

    use winapi_util::console::{mode, screen_buffer_info, set_mode};

    use super::{READ_TICK, SessionGate, TimedInput, raw_input_mode, spawn_reader, vt_output_mode};
    use crate::terminal::{Terminal, TerminalError};

    const ENTER_ALT_SCREEN: &[u8] = b"\x1b[?1049h\x1b[?25l";
    const LEAVE_ALT_SCREEN: &[u8] = b"\x1b[?25h\x1b[0m\x1b[?1049l";

    /// Original console modes captured before any mutation.
    #[derive(Clone, Copy, Debug)]
    struct SavedModes {
        input: u32,
        output: u32,
    }

    /// What the next restore must undo. `None` means that nothing is left to restore.
    struct ActiveConsoleState {
        saved: SavedModes,
        alt_screen: bool,
    }

    static ACTIVE_STATE: Mutex<Option<ActiveConsoleState>> = Mutex::new(None);
    static PANIC_HOOK_ONCE: Once = Once::new();
    /// Raw-session gate for the process-wide standard input reader.
    static SESSION: SessionGate = SessionGate::new();
    /// Process-wide standard input reader. One thread serves every terminal value,
    /// so two readers never compete for console input.
    static STDIN_INPUT: Mutex<Option<TimedInput>> = Mutex::new(None);

    fn install_emergency_restore_hook() {
        PANIC_HOOK_ONCE.call_once(|| {
            let prev = std::panic::take_hook();
            std::panic::set_hook(Box::new(move |info| {
                restore_emergency_terminal();
                prev(info);
            }));
        });
    }

    /// `true` while standard input is still an open console.
    fn stdin_is_open_console() -> bool {
        mode(io::stdin()).is_ok()
    }

    /// Discard all input that the reader buffered before this point.
    fn discard_buffered_input() {
        let mut slot = STDIN_INPUT.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(input) = slot.as_mut() {
            input.discard_buffered();
        }
    }

    /// Best-effort restoration of any active native raw console state.
    ///
    /// Takes the shared state slot, so only the first caller restores (panic hook,
    /// `leave_raw`, or `Drop`). Safe to invoke from panic hooks or cleanup paths;
    /// never panics.
    pub fn restore_emergency_terminal() {
        SESSION.end();
        let state = ACTIVE_STATE
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        let Some(state) = state else {
            return;
        };
        // Leave the alternate screen while VT processing is still on.
        if state.alt_screen {
            let mut out = io::stdout();
            let _ = out.write_all(LEAVE_ALT_SCREEN);
            let _ = out.flush();
        }
        let _ = set_mode(io::stdin(), state.saved.input);
        let _ = set_mode(io::stdout(), state.saved.output);
    }

    /// Native Windows console attached to standard input and standard output.
    pub struct NativeTerminal {
        active: bool,
    }

    impl NativeTerminal {
        /// Verify that stdin and stdout are interactive consoles without mutating
        /// console modes.
        ///
        /// A pipe that only looks like a terminal (for example an MSYS2 pty) has no
        /// console mode, so this probe rejects it before any output.
        pub fn open() -> Result<Self, TerminalError> {
            if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
                return Err(TerminalError::NotATty);
            }
            mode(io::stdin())?;
            mode(io::stdout())?;
            Ok(Self { active: false })
        }
    }

    impl Terminal for NativeTerminal {
        fn size(&mut self) -> Option<(u16, u16)> {
            let rect = screen_buffer_info(io::stdout()).ok()?.window_rect();
            super::window_cells(rect.left, rect.top, rect.right, rect.bottom)
        }

        fn enter_raw(&mut self) -> Result<(), TerminalError> {
            if self.active {
                return Ok(());
            }
            if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
                return Err(TerminalError::NotATty);
            }

            let saved = SavedModes {
                input: mode(io::stdin())?,
                output: mode(io::stdout())?,
            };

            install_emergency_restore_hook();
            *ACTIVE_STATE.lock().unwrap_or_else(PoisonError::into_inner) =
                Some(ActiveConsoleState {
                    saved,
                    alt_screen: false,
                });

            // Output first: the alternate screen sequence needs VT processing.
            if let Err(err) = set_mode(io::stdout(), vt_output_mode(saved.output)) {
                restore_emergency_terminal();
                return Err(TerminalError::Io(err));
            }
            if let Err(err) = set_mode(io::stdin(), raw_input_mode(saved.input)) {
                restore_emergency_terminal();
                return Err(TerminalError::Io(err));
            }

            // Keys typed before this session never reach it.
            discard_buffered_input();
            SESSION.begin();
            self.active = true;

            // Arm the alternate screen before its enter sequence, so a partial write rolls back.
            if let Some(active) = ACTIVE_STATE
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .as_mut()
            {
                active.alt_screen = true;
            }

            let mut out = io::stdout();
            if let Err(err) = out.write_all(ENTER_ALT_SCREEN).and_then(|()| out.flush()) {
                self.leave_raw();
                return Err(TerminalError::Io(err));
            }

            Ok(())
        }

        fn leave_raw(&mut self) {
            restore_emergency_terminal();
            self.active = false;
        }

        fn read(&mut self, buf: &mut [u8]) -> Result<usize, TerminalError> {
            let mut slot = STDIN_INPUT.lock().unwrap_or_else(PoisonError::into_inner);
            // Start the reader on the first read. The session gate keeps it from
            // starting a console read outside a raw session: a console read that
            // starts in line mode keeps line editing until Enter.
            if slot.is_none() {
                let rx = spawn_reader(io::stdin(), &SESSION, stdin_is_open_console)?;
                *slot = Some(TimedInput::new(rx, &SESSION, READ_TICK));
            }
            match slot.as_mut() {
                Some(input) => Ok(input.read(buf)?),
                None => Ok(0),
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
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::collections::VecDeque;
    use std::sync::mpsc::sync_channel;

    use super::*;

    /// Scripted reader: returns each queued result once, then zero bytes.
    struct ScriptedReader(VecDeque<io::Result<Vec<u8>>>);

    impl Read for ScriptedReader {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            match self.0.pop_front() {
                Some(Ok(bytes)) => {
                    let n = buf.len().min(bytes.len());
                    for (dst, src) in buf.iter_mut().zip(bytes) {
                        *dst = src;
                    }
                    Ok(n)
                }
                Some(Err(err)) => Err(err),
                None => Ok(0),
            }
        }
    }

    fn scripted(items: Vec<io::Result<Vec<u8>>>) -> ScriptedReader {
        ScriptedReader(items.into())
    }

    /// Endless reader: every read returns one byte.
    struct Endless;

    impl Read for Endless {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            match buf.first_mut() {
                Some(first) => {
                    *first = b'z';
                    Ok(1)
                }
                None => Ok(0),
            }
        }
    }

    const TEST_TICK: Duration = Duration::from_millis(20);

    /// A gate with no session yet. Leaked, because the reader needs `'static`.
    fn idle_gate() -> &'static SessionGate {
        Box::leak(Box::new(SessionGate::new()))
    }

    /// A gate with an active session.
    fn active_gate() -> &'static SessionGate {
        let gate = idle_gate();
        gate.begin();
        gate
    }

    fn data(gate: &SessionGate, bytes: &[u8]) -> ReaderEvent {
        ReaderEvent::Data {
            session: gate.current(),
            bytes: bytes.to_vec(),
        }
    }

    fn never_open() -> bool {
        false
    }

    #[test]
    fn read_tick_matches_posix_vtime() {
        // POSIX `VTIME=2` is 2 deciseconds.
        assert_eq!(READ_TICK, Duration::from_millis(200));
    }

    #[test]
    fn raw_input_mode_clears_cooked_flags_and_enables_vt_input() {
        // 0x01F7 is a usual cooked console input mode (processed, line, and echo
        // input, plus insert, quick edit, and other bits that must stay set).
        let original = 0x01F7;
        let raw = raw_input_mode(original);
        assert_eq!(raw & ENABLE_LINE_INPUT, 0);
        assert_eq!(raw & ENABLE_ECHO_INPUT, 0);
        assert_eq!(raw & ENABLE_PROCESSED_INPUT, 0);
        assert_eq!(
            raw & ENABLE_VIRTUAL_TERMINAL_INPUT,
            ENABLE_VIRTUAL_TERMINAL_INPUT
        );
        // All other bits stay unchanged.
        assert_eq!(raw & !0x0207, original & !0x0207);
        assert_eq!(raw, 0x03F0);
    }

    #[test]
    fn vt_output_mode_adds_processed_output_vt_processing_and_delayed_wrap() {
        // 0x0002 is ENABLE_WRAP_AT_EOL_OUTPUT.
        assert_eq!(vt_output_mode(0x0002), 0x000F);
        assert_eq!(vt_output_mode(0x0003), 0x000F);
        assert_eq!(vt_output_mode(0x000F), 0x000F);
        // VT processing never appears without processed output.
        assert_eq!(vt_output_mode(0), 0x000D);
        assert_eq!(vt_output_mode(0) & ENABLE_PROCESSED_OUTPUT, 0x0001);
    }

    #[test]
    fn window_cells_uses_inclusive_rectangle() {
        assert_eq!(window_cells(0, 0, 79, 24), Some((80, 25)));
        // A scrolled window: only the visible rectangle counts, not the buffer.
        assert_eq!(window_cells(0, 9000, 119, 9029), Some((120, 30)));
        assert_eq!(window_cells(5, 5, 5, 5), Some((1, 1)));
    }

    #[test]
    fn window_cells_rejects_empty_and_inverted_rectangles() {
        assert_eq!(window_cells(0, 0, -1, 24), None);
        assert_eq!(window_cells(0, 10, 79, 8), None);
        // 65,536 cells do not fit in `u16`.
        assert_eq!(window_cells(i16::MIN, 0, i16::MAX, 0), None);
        assert_eq!(
            window_cells(i16::MIN, 0, i16::MAX - 1, 0),
            Some((u16::MAX, 1))
        );
    }

    #[test]
    fn session_gate_tracks_generations() {
        let gate = idle_gate();
        assert_eq!(gate.current(), None);
        gate.begin();
        let first = gate.current();
        assert!(first.is_some());
        gate.end();
        assert_eq!(gate.current(), None);
        gate.begin();
        let second = gate.current();
        assert!(second.is_some());
        assert_ne!(first, second);
    }

    #[test]
    fn timed_read_returns_zero_on_tick_without_input() {
        let (_tx, rx) = sync_channel::<ReaderEvent>(1);
        let mut input = TimedInput::new(rx, active_gate(), TEST_TICK);
        let mut buf = [0u8; 8];
        assert_eq!(input.read(&mut buf).unwrap(), 0);
        assert!(!input.closed);
    }

    #[test]
    fn timed_read_splits_chunks_larger_than_caller_buffer() {
        let gate = active_gate();
        let (tx, rx) = sync_channel(4);
        tx.send(data(gate, b"\x1b[A\x1b[B")).unwrap();
        let mut input = TimedInput::new(rx, gate, TEST_TICK);
        let mut buf = [0u8; 4];
        assert_eq!(input.read(&mut buf).unwrap(), 4);
        assert_eq!(&buf, b"\x1b[A\x1b");
        assert_eq!(input.read(&mut buf).unwrap(), 2);
        assert_eq!(&buf[..2], b"[B");
        // Nothing more: one tick, then zero.
        assert_eq!(input.read(&mut buf).unwrap(), 0);
    }

    #[test]
    fn timed_read_discards_chunks_from_other_sessions() {
        let gate = active_gate();
        let (tx, rx) = sync_channel(4);
        // Read while no session was active.
        tx.send(ReaderEvent::Data {
            session: None,
            bytes: b"a".to_vec(),
        })
        .unwrap();
        // Read during the first session, delivered after a new session started.
        tx.send(data(gate, b"b")).unwrap();
        gate.end();
        gate.begin();
        tx.send(data(gate, b"c")).unwrap();
        let mut input = TimedInput::new(rx, gate, TEST_TICK);
        let mut buf = [0u8; 8];
        assert_eq!(input.read(&mut buf).unwrap(), 0);
        assert_eq!(input.read(&mut buf).unwrap(), 0);
        assert_eq!(input.read(&mut buf).unwrap(), 1);
        assert_eq!(buf[0], b'c');
    }

    #[test]
    fn timed_read_discards_chunks_after_session_end() {
        let gate = active_gate();
        let (tx, rx) = sync_channel(4);
        tx.send(data(gate, b"q")).unwrap();
        gate.end();
        let mut input = TimedInput::new(rx, gate, TEST_TICK);
        let mut buf = [0u8; 8];
        assert_eq!(input.read(&mut buf).unwrap(), 0);
    }

    #[test]
    fn discard_buffered_drops_pending_and_queued_input() {
        let gate = active_gate();
        let (tx, rx) = sync_channel(4);
        tx.send(data(gate, b"abc")).unwrap();
        let mut input = TimedInput::new(rx, gate, TEST_TICK);
        let mut buf = [0u8; 1];
        assert_eq!(input.read(&mut buf).unwrap(), 1);
        assert_eq!(input.pending, b"bc");
        tx.send(data(gate, b"def")).unwrap();
        input.discard_buffered();
        assert!(input.pending.is_empty());
        assert!(!input.closed);
        assert_eq!(input.read(&mut buf).unwrap(), 0);
    }

    #[test]
    fn discard_buffered_keeps_end_of_input() {
        let (tx, rx) = sync_channel(4);
        tx.send(ReaderEvent::Closed(None)).unwrap();
        let mut input = TimedInput::new(rx, active_gate(), TEST_TICK);
        input.discard_buffered();
        assert!(input.closed);
    }

    #[test]
    fn timed_read_reports_end_of_input_as_zero_without_error() {
        let gate = active_gate();
        let (tx, rx) = sync_channel(4);
        tx.send(data(gate, b"q")).unwrap();
        tx.send(ReaderEvent::Closed(None)).unwrap();
        drop(tx);
        let mut input = TimedInput::new(rx, gate, TEST_TICK);
        let mut buf = [0u8; 8];
        assert_eq!(input.read(&mut buf).unwrap(), 1);
        assert_eq!(buf[0], b'q');
        assert_eq!(input.read(&mut buf).unwrap(), 0);
        assert!(input.closed);
        // Later reads keep returning zero.
        assert_eq!(input.read(&mut buf).unwrap(), 0);
    }

    #[test]
    fn timed_read_treats_disconnected_reader_as_end_of_input() {
        let (tx, rx) = sync_channel::<ReaderEvent>(1);
        drop(tx);
        let mut input = TimedInput::new(rx, active_gate(), TEST_TICK);
        let mut buf = [0u8; 8];
        assert_eq!(input.read(&mut buf).unwrap(), 0);
        assert!(input.closed);
    }

    #[test]
    fn timed_read_surfaces_reader_error_once() {
        let (tx, rx) = sync_channel(1);
        tx.send(ReaderEvent::Closed(Some(io::Error::from(
            ErrorKind::BrokenPipe,
        ))))
        .unwrap();
        let mut input = TimedInput::new(rx, active_gate(), TEST_TICK);
        let mut buf = [0u8; 8];
        let err = input.read(&mut buf).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::BrokenPipe);
        assert_eq!(input.read(&mut buf).unwrap(), 0);
    }

    #[test]
    fn timed_read_with_empty_buffer_does_not_consume_input() {
        let gate = active_gate();
        let (tx, rx) = sync_channel(1);
        tx.send(data(gate, b"x")).unwrap();
        let mut input = TimedInput::new(rx, gate, TEST_TICK);
        assert_eq!(input.read(&mut []).unwrap(), 0);
        let mut buf = [0u8; 1];
        assert_eq!(input.read(&mut buf).unwrap(), 1);
        assert_eq!(buf[0], b'x');
    }

    #[test]
    fn pump_forwards_data_skips_interrupts_and_closes_on_eof() {
        let gate = active_gate();
        let (tx, rx) = sync_channel(8);
        let reader = scripted(vec![
            Ok(b"ab".to_vec()),
            Err(io::Error::from(ErrorKind::Interrupted)),
            Ok(b"c".to_vec()),
        ]);
        pump_input(reader, &tx, gate, never_open);
        drop(tx);
        let events: Vec<_> = rx.iter().collect();
        assert_eq!(events.len(), 3, "{events:?}");
        let session = gate.current();
        assert!(
            matches!(&events[0], ReaderEvent::Data { session: s, bytes } if *s == session && bytes == b"ab")
        );
        assert!(matches!(&events[1], ReaderEvent::Data { bytes, .. } if bytes == b"c"));
        assert!(matches!(&events[2], ReaderEvent::Closed(None)));
    }

    #[test]
    fn pump_keeps_reading_after_empty_reads_from_an_open_console() {
        // A lone Ctrl+Z on a console reads as zero bytes. The console is still
        // open, so later keys (for example `q`) must still arrive.
        let gate = active_gate();
        let (tx, rx) = sync_channel(8);
        let mut items: Vec<io::Result<Vec<u8>>> = (0..20).map(|_| Ok(Vec::new())).collect();
        items.push(Ok(b"q".to_vec()));
        let probes = Cell::new(0u32);
        let is_open = || {
            probes.set(probes.get() + 1);
            // Open for the 20 scripted empty reads, closed after the queue ends.
            probes.get() <= 20
        };
        pump_input(scripted(items), &tx, gate, is_open);
        drop(tx);
        let events: Vec<_> = rx.iter().collect();
        assert_eq!(events.len(), 2, "{events:?}");
        assert!(matches!(&events[0], ReaderEvent::Data { bytes, .. } if bytes == b"q"));
        assert!(matches!(&events[1], ReaderEvent::Closed(None)));
        assert_eq!(probes.get(), 21);
    }

    #[test]
    fn pump_reports_end_of_input_when_console_is_closed() {
        let gate = active_gate();
        let (tx, rx) = sync_channel(8);
        pump_input(
            scripted(vec![Ok(Vec::new()), Ok(b"never".to_vec())]),
            &tx,
            gate,
            never_open,
        );
        drop(tx);
        let events: Vec<_> = rx.iter().collect();
        assert_eq!(events.len(), 1, "{events:?}");
        assert!(matches!(&events[0], ReaderEvent::Closed(None)));
    }

    #[test]
    fn pump_reports_read_error_and_stops() {
        let gate = active_gate();
        let (tx, rx) = sync_channel(8);
        let reader = scripted(vec![
            Err(io::Error::from(ErrorKind::PermissionDenied)),
            Ok(b"never".to_vec()),
        ]);
        pump_input(reader, &tx, gate, never_open);
        drop(tx);
        let events: Vec<_> = rx.iter().collect();
        assert_eq!(events.len(), 1);
        assert!(
            matches!(&events[0], ReaderEvent::Closed(Some(e)) if e.kind() == ErrorKind::PermissionDenied)
        );
    }

    #[test]
    fn pump_stops_when_receiver_is_dropped() {
        let (tx, rx) = sync_channel(1);
        drop(rx);
        // The pump must return after the first failed send.
        pump_input(Endless, &tx, active_gate(), never_open);
    }

    #[test]
    fn spawned_reader_waits_for_an_active_session() {
        let gate = idle_gate();
        let rx = spawn_reader(scripted(vec![Ok(b"\r".to_vec())]), gate, never_open).unwrap();
        // No session: the reader must not read.
        assert!(matches!(
            rx.recv_timeout(Duration::from_millis(50)),
            Err(RecvTimeoutError::Timeout)
        ));
        gate.begin();
        let mut input = TimedInput::new(rx, gate, Duration::from_secs(5));
        let mut buf = [0u8; 8];
        assert_eq!(input.read(&mut buf).unwrap(), 1);
        assert_eq!(buf[0], b'\r');
        assert_eq!(input.read(&mut buf).unwrap(), 0);
        assert!(input.closed);
    }
}
