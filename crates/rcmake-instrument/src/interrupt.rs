//! Catch a user interrupt around an instrumented command.
//!
//! CMake records the overall `cmakeBuild`, `cmakeInstall`, or `ctest` snippet
//! when the user presses Ctrl+C (`SIGINT`). It skips the post-command hook and
//! then ends the process with the same signal, so the exit status does not change.
//!
//! Rcmake does this without a signal handler. A guard blocks `SIGINT` in the
//! calling thread, and one watcher thread accepts the signal with `sigwait`.
//! Threads that the command starts get the blocked mask from the calling thread.
//! Before re-raising `SIGINT` on an interrupt, the flush thread terminates active
//! descendants and any child-created process groups so tests, install scripts, or
//! native build tools do not linger or outlive the interrupted command.
//!
//! The thread that starts the worker thread and only waits for it must call
//! [`shield_current_thread`] after spawning the worker thread. If it does not,
//! the kernel can give the signal to that thread, and the process ends with the
//! default action before the watcher sees it.
//!
//! Only `SIGINT` is caught, as in CMake. Other signals keep their default action.

use std::sync::atomic::{AtomicBool, Ordering};

#[cfg(not(any(unix, windows)))]
use fallback as platform;
#[cfg(unix)]
use native as platform;
#[cfg(windows)]
use windows_native as platform;

static INTERRUPTED: AtomicBool = AtomicBool::new(false);

/// Return `true` once an interrupt signal has been received by the active guard.
///
/// Schedulers and multi-step loops poll this to avoid launching new work or
/// mutating resume checkpoints after `SIGINT` arrives.
#[must_use]
pub fn is_interrupted() -> bool {
    INTERRUPTED.load(Ordering::Acquire)
}

/// The result of a command that ran under [`guard`] or [`guard_and_flush`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[must_use]
pub struct Guarded<T> {
    /// The value that the command returned.
    pub value: T,
    /// The interrupt signal that arrived while the command ran.
    pub signal: Option<i32>,
}

impl<T> Guarded<T> {
    const fn completed(value: T) -> Self {
        Self {
            value,
            signal: None,
        }
    }
}

/// Keep the calling thread from taking an interrupt signal.
///
/// Call this in a thread that has already started the worker thread and then
/// only waits for it. The worker thread keeps the default action until it runs
/// a [`guard`] or [`guard_and_flush`].
pub fn shield_current_thread() {
    platform::shield();
}

/// Run `run` and record an interrupt signal instead of ending the process.
///
/// If `active` is false, `run` runs with the default signal action. A second
/// interrupt ends the process with the default action. When the result has a
/// signal, the caller writes its snippet, skips its hooks, and calls [`raise`].
pub fn guard<T>(active: bool, run: impl FnOnce() -> T) -> Guarded<T> {
    if active {
        platform::guarded(None::<fn(i32)>, run)
    } else {
        Guarded::completed(run())
    }
}

/// Run `run`. On an interrupt, run `flush` on another thread, terminate
/// descendants and child-created process groups, and then end the process with
/// the signal.
///
/// Use this when `run` cannot stop by itself after an interrupt, for example a
/// scheduler that would start the next test. `flush` writes the snippet of the
/// interrupted command. `SIGINT` remains masked across the entire flush so a
/// second `SIGINT` cannot terminate the process before the snippet is written.
/// When the result has a signal, `flush` has run: the caller must not write the
/// snippet again, skips its hooks, and calls [`raise`].
pub fn guard_and_flush<T>(
    active: bool,
    flush: impl FnOnce(i32) + Send,
    run: impl FnOnce() -> T,
) -> Guarded<T> {
    if active {
        platform::guarded(Some(flush), run)
    } else {
        Guarded::completed(run())
    }
}

/// End the process with `signal` and its default action.
///
/// The function returns if the signal does not end the process. Then the caller
/// returns its exit code.
pub fn raise(signal: i32) {
    platform::raise(signal);
}

#[cfg(not(any(unix, windows)))]
mod fallback {
    use super::Guarded;

    pub(super) fn shield() {}

    pub(super) fn raise(_signal: i32) {}

    pub(super) fn guarded<T, F: FnOnce(i32) + Send>(
        _flush: Option<F>,
        run: impl FnOnce() -> T,
    ) -> Guarded<T> {
        Guarded::completed(run())
    }
}

/// POSIX `SIGINT` signal number recorded in instrumentation snippets (`interruptSignal`),
/// matching CMake 4.5.1 (`InstrumentationInterruptReal` requires a positive integer).
#[cfg(any(windows, test))]
const WINDOWS_INTERRUPT_SIGNAL: i32 = 2;

/// Windows NTSTATUS exit code for console Ctrl+C / Ctrl+Break termination
/// (`STATUS_CONTROL_C_EXIT`, matching CMake 4.5.1).
#[cfg(any(windows, test))]
const STATUS_CONTROL_C_EXIT: i32 = 0xC000_013A_u32.cast_signed();

/// Parse child PIDs from Windows `Get-CimInstance` / `wmic` stdout, excluding `0` and `own_pid`.
#[cfg(any(windows, test))]
fn parse_windows_child_pids(output: &str, own_pid: u32) -> Vec<u32> {
    let mut pids: Vec<u32> = output
        .split_ascii_whitespace()
        .filter_map(|token| token.parse::<u32>().ok())
        .filter(|&pid| pid > 0 && pid != own_pid)
        .collect();
    pids.sort_unstable();
    pids.dedup();
    pids
}

#[cfg(windows)]
mod windows_native {
    use std::future::Future as _;
    use std::pin::pin;
    use std::process::{Command, Stdio};
    use std::sync::atomic::Ordering;
    use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError, mpsc};
    use std::task::{Poll, Waker};

    use super::{
        Guarded, INTERRUPTED, STATUS_CONTROL_C_EXIT, WINDOWS_INTERRUPT_SIGNAL,
        parse_windows_child_pids,
    };

    struct State {
        active: bool,
        ready: bool,
        signal: Option<i32>,
        spent: bool,
    }

    enum Entry {
        Owner,
        Nested,
        Unguarded,
    }

    struct StopState {
        stopped: bool,
        waker: Option<Waker>,
    }

    struct StopHandle {
        state: Arc<Mutex<StopState>>,
    }

    impl StopHandle {
        fn new() -> (Self, Arc<Mutex<StopState>>) {
            let state = Arc::new(Mutex::new(StopState {
                stopped: false,
                waker: None,
            }));
            (
                Self {
                    state: Arc::clone(&state),
                },
                state,
            )
        }

        fn signal(&self) {
            let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
            state.stopped = true;
            if let Some(waker) = state.waker.take() {
                waker.wake();
            }
        }
    }

    static STATE: Mutex<State> = Mutex::new(State {
        active: false,
        ready: false,
        signal: None,
        spent: false,
    });

    static READY: Condvar = Condvar::new();

    fn state() -> MutexGuard<'static, State> {
        STATE.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn take<F>(slot: &Mutex<Option<F>>) -> Option<F> {
        slot.lock().unwrap_or_else(PoisonError::into_inner).take()
    }

    /// Ensure concurrent guards waiting on [`READY`] are always woken if the owner unwinds.
    struct OwnerGuard {
        active: bool,
    }

    impl OwnerGuard {
        fn disarm(&mut self) {
            self.active = false;
        }
    }

    impl Drop for OwnerGuard {
        fn drop(&mut self) {
            if self.active {
                let mut state = state();
                state.active = false;
                state.ready = false;
                READY.notify_all();
            }
        }
    }

    pub(super) fn shield() {}

    /// Collect direct child PIDs of the current process on Windows without `unsafe`.
    #[allow(
        clippy::disallowed_methods,
        reason = "Windows interrupt teardown enumerates direct child PIDs before std::process::exit bypasses Drop."
    )]
    fn direct_children(own_pid: u32) -> Vec<u32> {
        let ps_script = format!(
            "(Get-CimInstance Win32_Process -Filter 'ParentProcessId={own_pid}').ProcessId"
        );
        if let Ok(output) = Command::new("powershell.exe")
            .args(["-NoProfile", "-NonInteractive", "-Command", &ps_script])
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
            && output.status.success()
        {
            return parse_windows_child_pids(&String::from_utf8_lossy(&output.stdout), own_pid);
        }
        let wmic_filter = format!("(ParentProcessId={own_pid})");
        if let Ok(output) = Command::new("wmic")
            .args(["process", "where", &wmic_filter, "get", "ProcessId"])
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
            && output.status.success()
        {
            return parse_windows_child_pids(&String::from_utf8_lossy(&output.stdout), own_pid);
        }
        Vec::new()
    }

    /// Terminate active child process trees before `std::process::exit` bypasses `Drop`.
    #[allow(
        clippy::disallowed_methods,
        reason = "Windows interrupt teardown terminates child process trees before std::process::exit bypasses Drop."
    )]
    fn terminate_descendants() {
        let own_pid = std::process::id();
        for child_pid in direct_children(own_pid) {
            if child_pid == own_pid || child_pid == 0 {
                continue;
            }
            let _ = Command::new("taskkill.exe")
                .args(["/F", "/T", "/PID", &child_pid.to_string()])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
        }
    }

    #[allow(
        clippy::exit,
        reason = "Windows has no POSIX signal raise; terminate child trees and exit with STATUS_CONTROL_C_EXIT."
    )]
    pub(super) fn raise(_signal: i32) {
        terminate_descendants();
        std::process::exit(STATUS_CONTROL_C_EXIT);
    }

    /// Wait for either a console Ctrl+C / Ctrl+Break event or the owner stop signal.
    ///
    /// Polls console events BEFORE checking `stop.stopped` (biased toward interrupts)
    /// so an already-accepted Tokio console event is never discarded when guarded work
    /// completes concurrently.
    async fn wait_interrupt_or_stop(
        stop_state: &Mutex<StopState>,
        ctrl_c: &mut tokio::signal::windows::CtrlC,
        ctrl_break: &mut tokio::signal::windows::CtrlBreak,
    ) -> bool {
        let mut recv_c = pin!(ctrl_c.recv());
        let mut recv_break = pin!(ctrl_break.recv());
        let mut c_open = true;
        let mut break_open = true;
        std::future::poll_fn(|cx| {
            if c_open {
                match recv_c.as_mut().poll(cx) {
                    Poll::Ready(Some(())) => return Poll::Ready(true),
                    Poll::Ready(None) => c_open = false,
                    Poll::Pending => {}
                }
            }
            if break_open {
                match recv_break.as_mut().poll(cx) {
                    Poll::Ready(Some(())) => return Poll::Ready(true),
                    Poll::Ready(None) => break_open = false,
                    Poll::Pending => {}
                }
            }
            let mut stop = stop_state.lock().unwrap_or_else(PoisonError::into_inner);
            if stop.stopped {
                return Poll::Ready(false);
            }
            stop.waker = Some(cx.waker().clone());
            Poll::Pending
        })
        .await
    }

    pub(super) fn guarded<T, F: FnOnce(i32) + Send>(
        flush: Option<F>,
        run: impl FnOnce() -> T,
    ) -> Guarded<T> {
        let entry = {
            let mut state = state();
            loop {
                if state.spent {
                    break Entry::Unguarded;
                }
                if state.active {
                    if !state.ready {
                        state = READY
                            .wait_while(state, |s| s.active && !s.ready)
                            .unwrap_or_else(PoisonError::into_inner);
                        continue;
                    }
                    break Entry::Nested;
                }
                state.active = true;
                state.ready = false;
                state.signal = None;
                break Entry::Owner;
            }
        };
        match entry {
            Entry::Unguarded => return Guarded::completed(run()),
            Entry::Nested => {
                let value = run();
                let signal = state().signal;
                return Guarded { value, signal };
            }
            Entry::Owner => {}
        }

        let mut owner_guard = OwnerGuard { active: true };
        let flush = Mutex::new(flush);
        let flush = &flush;
        let (stop, stop_state) = StopHandle::new();
        let value = std::thread::scope(|threads| {
            let (ready_tx, ready_rx) = mpsc::sync_channel::<bool>(1);
            let spawned = std::thread::Builder::new()
                .name("rcmake-interrupt".into())
                .spawn_scoped(threads, move || {
                    let Ok(runtime) = tokio::runtime::Builder::new_current_thread().build() else {
                        let _ = ready_tx.send(false);
                        return;
                    };
                    runtime.block_on(async move {
                        let Ok(mut ctrl_c) = tokio::signal::windows::ctrl_c() else {
                            let _ = ready_tx.send(false);
                            return;
                        };
                        let Ok(mut ctrl_break) = tokio::signal::windows::ctrl_break() else {
                            let _ = ready_tx.send(false);
                            return;
                        };
                        let _ = ready_tx.send(true);
                        if !wait_interrupt_or_stop(&stop_state, &mut ctrl_c, &mut ctrl_break).await
                        {
                            return;
                        }
                        INTERRUPTED.store(true, Ordering::SeqCst);
                        {
                            let mut state = state();
                            state.spent = true;
                            state.signal.get_or_insert(WINDOWS_INTERRUPT_SIGNAL);
                        }
                        if let Some(flush) = take(flush) {
                            flush(WINDOWS_INTERRUPT_SIGNAL);
                            raise(WINDOWS_INTERRUPT_SIGNAL);
                        }
                        // Second interrupt terminates immediately while the guarded work unwinds.
                        if wait_interrupt_or_stop(&stop_state, &mut ctrl_c, &mut ctrl_break).await {
                            raise(WINDOWS_INTERRUPT_SIGNAL);
                        }
                    });
                });
            if spawned.is_ok() {
                let _ = ready_rx.recv();
            }
            {
                let mut state = state();
                state.ready = true;
                READY.notify_all();
            }
            let value = run();
            stop.signal();
            value
        });
        // Watcher thread has joined after draining any already-accepted console event.
        let signal = {
            let mut state = state();
            state.active = false;
            state.ready = false;
            READY.notify_all();
            state.signal.take()
        };
        owner_guard.disarm();
        Guarded { value, signal }
    }
}

#[cfg(unix)]
mod native {
    use std::collections::BTreeSet;
    use std::sync::atomic::Ordering;
    use std::sync::{Mutex, MutexGuard, OnceLock, PoisonError, mpsc};

    use nix::sys::signal::{SigSet, SigmaskHow, Signal, kill, killpg};
    use nix::unistd::{Pid, getpgid, getpgrp};

    use super::{Guarded, INTERRUPTED};

    /// The guard that owns the interrupt state.
    struct Scope {
        /// The first interrupt signal.
        signal: Option<i32>,
        /// The channel to the thread that writes the snippet of an interrupted command.
        flush: Option<mpsc::Sender<i32>>,
    }

    struct State {
        scope: Option<Scope>,
        /// The watcher accepted a signal. Later signals get the default action.
        spent: bool,
    }

    /// What the watcher does after it accepts a signal.
    enum Next {
        /// Send the signal to the flush thread of the scope.
        Flush(mpsc::Sender<i32>, i32),
        /// The scope recorded the signal. Its owner continues.
        Recorded,
        /// No scope is active. End the process with the default action.
        Default(i32),
        /// `sigwait` failed. Later signals get the default action.
        Failed,
    }

    /// How a guard takes part in the interrupt state.
    enum Entry {
        /// This guard owns the scope.
        Owner,
        /// An outer guard owns the scope.
        Nested,
        /// The signal cannot be caught.
        Unguarded,
    }

    static STATE: Mutex<State> = Mutex::new(State {
        scope: None,
        spent: false,
    });

    static WATCHER: OnceLock<bool> = OnceLock::new();

    fn state() -> MutexGuard<'static, State> {
        STATE.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn take<F>(slot: &Mutex<Option<F>>) -> Option<F> {
        slot.lock().unwrap_or_else(PoisonError::into_inner).take()
    }

    fn interrupts() -> SigSet {
        let mut set = SigSet::empty();
        set.add(Signal::SIGINT);
        set
    }

    pub(super) fn shield() {
        let _ = interrupts().thread_block();
    }

    pub(super) fn raise(signal: i32) {
        let Ok(signal) = Signal::try_from(signal) else {
            return;
        };
        let mut set = SigSet::empty();
        set.add(signal);
        let _ = set.thread_unblock();
        let _ = nix::sys::signal::raise(signal);
    }

    /// Collect direct child PIDs of `parent` from the host OS.
    #[expect(
        clippy::disallowed_methods,
        reason = "Interrupt teardown inspects OS process tree before re-raising SIGINT."
    )]
    fn direct_children(parent: i32) -> Vec<i32> {
        #[cfg(target_os = "linux")]
        {
            let mut children = Vec::new();
            let task_dir = format!("/proc/{parent}/task");
            if let Ok(entries) = std::fs::read_dir(&task_dir) {
                for entry in entries.flatten() {
                    let path = entry.path().join("children");
                    if let Ok(text) = std::fs::read_to_string(path) {
                        for token in text.split_ascii_whitespace() {
                            if let Ok(pid) = token.parse::<i32>()
                                && pid > 0
                            {
                                children.push(pid);
                            }
                        }
                    }
                }
            }
            children
        }
        #[cfg(not(target_os = "linux"))]
        {
            let output = std::process::Command::new("/usr/bin/pgrep")
                .arg("-P")
                .arg(parent.to_string())
                .stdin(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .output();
            let Ok(output) = output else {
                return Vec::new();
            };
            let Ok(text) = std::str::from_utf8(&output.stdout) else {
                return Vec::new();
            };
            text.split_ascii_whitespace()
                .filter_map(|token| token.parse::<i32>().ok().filter(|&pid| pid > 0))
                .collect()
        }
    }

    /// Snapshot descendant PIDs and ONLY child-created process groups (groups
    /// whose leader PID is itself in the descendant set) so we never `killpg`
    /// a foreign process group such as the parent shell.
    fn collect_descendants() -> (Vec<Pid>, Vec<Pid>) {
        let Ok(root) = i32::try_from(std::process::id()) else {
            return (Vec::new(), Vec::new());
        };
        let own_pgrp = getpgrp();
        let mut descendants = BTreeSet::new();
        let mut queue = vec![root];
        while let Some(parent) = queue.pop() {
            for child in direct_children(parent) {
                if child == root || !descendants.insert(child) {
                    continue;
                }
                queue.push(child);
            }
        }
        let mut child_groups = BTreeSet::new();
        let mut pids = Vec::with_capacity(descendants.len());
        for &child in &descendants {
            let pid = Pid::from_raw(child);
            if let Ok(pgid) = getpgid(Some(pid))
                && pgid != own_pgrp
                && descendants.contains(&pgid.as_raw())
            {
                child_groups.insert(pgid.as_raw());
            }
            pids.push(pid);
        }
        let groups = child_groups.into_iter().map(Pid::from_raw).collect();
        (groups, pids)
    }

    fn terminate_descendants(groups: &[Pid], pids: &[Pid]) {
        for &group in groups {
            let _ = killpg(group, Signal::SIGKILL);
        }
        for &pid in pids {
            let _ = kill(pid, Signal::SIGKILL);
        }
    }

    /// Start the watcher one time. The caller has blocked the interrupt signals,
    /// so the watcher starts with them blocked.
    fn watcher_running() -> bool {
        *WATCHER.get_or_init(|| {
            std::thread::Builder::new()
                .name("rcmake-interrupt".into())
                .spawn(watch)
                .is_ok()
        })
    }

    fn watch() {
        let set = interrupts();
        let _ = set.thread_block();
        let accepted = set.wait().map(|signal| signal as i32);
        if accepted.is_ok() {
            INTERRUPTED.store(true, Ordering::SeqCst);
        }
        let next = {
            let mut state = state();
            state.spent = true;
            match (accepted, state.scope.as_mut()) {
                (Ok(signal), Some(scope)) => {
                    scope.signal.get_or_insert(signal);
                    match scope.flush.take() {
                        Some(flush) => Next::Flush(flush, signal),
                        None => Next::Recorded,
                    }
                }
                (Ok(signal), None) => Next::Default(signal),
                (Err(_), _) => Next::Failed,
            }
        };
        match next {
            Next::Flush(flush, signal) => {
                // Keep SIGINT masked on this watcher thread while the flush thread
                // writes the interrupted snippet; the flush thread unblocks SIGINT
                // and calls `raise(signal)` right after flushing completes.
                let _ = flush.send(signal);
            }
            Next::Default(signal) => {
                let _ = set.thread_unblock();
                raise(signal);
            }
            Next::Recorded | Next::Failed => {
                // Like `SA_RESETHAND`: a later interrupt goes to this thread and ends the process.
                let _ = set.thread_unblock();
            }
        }
        loop {
            std::thread::park();
        }
    }

    pub(super) fn guarded<T, F: FnOnce(i32) + Send>(
        flush: Option<F>,
        run: impl FnOnce() -> T,
    ) -> Guarded<T> {
        let Ok(previous) = interrupts().thread_swap_mask(SigmaskHow::SIG_BLOCK) else {
            return Guarded::completed(run());
        };
        if !watcher_running() {
            let _ = previous.thread_set_mask();
            return Guarded::completed(run());
        }
        let result = scoped(flush, run);
        let _ = previous.thread_set_mask();
        result
    }

    fn scoped<T, F: FnOnce(i32) + Send>(flush: Option<F>, run: impl FnOnce() -> T) -> Guarded<T> {
        let has_flush = flush.is_some();
        let flush = Mutex::new(flush);
        let flush = &flush;
        std::thread::scope(|threads| {
            let (sender, receiver) = mpsc::channel::<i32>();
            let flusher = has_flush
                && std::thread::Builder::new()
                    .name("rcmake-interrupt-flush".into())
                    .spawn_scoped(threads, move || {
                        if let Ok(signal) = receiver.recv()
                            && let Some(flush) = take(flush)
                        {
                            flush(signal);
                            // Collect child-created process groups while they are still
                            // alive, then signal them right before re-raising SIGINT so
                            // CTest/install schedulers cannot checkpoint or launch work.
                            let (groups, pids) = collect_descendants();
                            terminate_descendants(&groups, &pids);
                            raise(signal);
                        }
                    })
                    .is_ok();
            let mut sender = flusher.then_some(sender);
            let entry = {
                let mut state = state();
                if state.spent {
                    Entry::Unguarded
                } else if state.scope.is_some() {
                    Entry::Nested
                } else {
                    state.scope = Some(Scope {
                        signal: None,
                        flush: sender.take(),
                    });
                    Entry::Owner
                }
            };
            // A dropped sender stops a flush thread that does not own the scope.
            drop(sender);
            let value = run();
            let signal = match entry {
                Entry::Unguarded => None,
                Entry::Nested => state().scope.as_ref().and_then(|scope| scope.signal),
                // Dropping the scope also drops an unused sender, so the flush
                // thread stops before the thread scope joins it.
                Entry::Owner => state().scope.take().and_then(|scope| scope.signal),
            };
            if let Some(signal) = signal
                && !flusher
                && matches!(entry, Entry::Owner)
                && let Some(flush) = take(flush)
            {
                flush(signal);
            }
            Guarded { value, signal }
        })
    }
}

#[cfg(all(test, any(unix, windows)))]
mod tests {
    use super::{
        STATUS_CONTROL_C_EXIT, WINDOWS_INTERRUPT_SIGNAL, guard, guard_and_flush, is_interrupted,
        parse_windows_child_pids,
    };

    #[test]
    fn inactive_guards_run_the_command_without_a_signal() {
        assert!(!is_interrupted());
        let result = guard(false, || 7);
        assert_eq!((result.value, result.signal), (7, None));
        let result = guard_and_flush(false, |_| unreachable!("no interrupt"), || "done");
        assert_eq!((result.value, result.signal), ("done", None));
    }

    #[test]
    fn an_active_guard_without_an_interrupt_returns_the_value() {
        let result = guard_and_flush(true, |_| unreachable!("no interrupt"), || 3);
        assert_eq!((result.value, result.signal), (3, None));
        let nested = guard(true, || guard(true, || 4));
        assert_eq!((nested.value.value, nested.value.signal), (4, None));
        assert_eq!(nested.signal, None);
    }

    #[test]
    fn windows_interrupt_signal_is_positive_sigint_and_child_pid_parser_excludes_self() {
        const {
            assert!(WINDOWS_INTERRUPT_SIGNAL == 2);
            assert!(WINDOWS_INTERRUPT_SIGNAL > 0);
            assert!(STATUS_CONTROL_C_EXIT == 0xC000_013A_u32.cast_signed());
        }
        assert_eq!(
            parse_windows_child_pids(
                "ProcessId\r\n0\r\n100\r\n4242\r\n200\r\n100\r\nnoise\n",
                4242
            ),
            vec![100, 200]
        );
        assert!(parse_windows_child_pids("", 4242).is_empty());
    }
}
