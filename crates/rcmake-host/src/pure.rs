//! Deterministic in-memory [`Host`] implementation for fast, reproducible unit tests.
//!
mod fs;

use core::time::Duration;
use std::collections::{BTreeMap, VecDeque};
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};

use jiff::tz::TimeZone;
use rcmake_base::BString;

use self::fs::FileSystem;
use crate::error::{HostError, HostResult};
use crate::{
    CopyOptions, DirEntry, FileTime, Follow, Host, Metadata, ReadRange, RunOutput, RunRequest,
    Stream, SystemInfo, WriteMode, validate_run,
};

/// An ordered stdio output event recorded by [`PureHost`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IoEvent {
    /// Target stream.
    pub stream: Stream,
    /// Bytes written in this event.
    pub bytes: BString,
}

type CommandHandler = dyn Fn(&PureHost, &RunRequest, &mut dyn FnMut(Stream, &[u8])) -> HostResult<RunOutput>
    + Send
    + Sync;

struct PureState {
    env: BTreeMap<BString, BString>,
    events: Vec<IoEvent>,
    cwd: PathBuf,
    exe: PathBuf,
    fs: FileSystem,
    stdin: VecDeque<u8>,
    handler: Option<Arc<CommandHandler>>,
}

/// In-memory deterministic [`Host`] with recorded stdio event log and virtual clock.
pub struct PureHost {
    state: Mutex<PureState>,
}

impl fmt::Debug for PureHost {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PureHost").finish_non_exhaustive()
    }
}

impl Default for PureHost {
    fn default() -> Self {
        Self::new(
            PathBuf::from("/work"),
            PathBuf::from("/usr/local/bin/cmake"),
        )
    }
}

impl PureHost {
    /// Create a new [`PureHost`] rooted at `cwd` with executable path `exe`.
    #[must_use]
    pub fn new(cwd: PathBuf, exe: PathBuf) -> Self {
        Self {
            state: Mutex::new(PureState {
                env: BTreeMap::new(),
                events: Vec::new(),
                fs: FileSystem::new(&cwd),
                cwd,
                exe,
                stdin: VecDeque::new(),
                handler: None,
            }),
        }
    }

    fn lock(&self) -> HostResult<MutexGuard<'_, PureState>> {
        self.state
            .lock()
            .map_err(|_| HostError::unsupported("PureHost lock poisoned"))
    }

    /// Replace the command handler. The handler receives the full request and an output
    /// callback that forwards chunks only when the request enables tee output.
    ///
    /// The handler supplies the process results and can use the host to simulate I/O.
    pub fn set_command_handler(
        &self,
        handler: impl Fn(&Self, &RunRequest, &mut dyn FnMut(Stream, &[u8])) -> HostResult<RunOutput>
        + Send
        + Sync
        + 'static,
    ) -> HostResult<()> {
        self.lock()?.handler = Some(Arc::new(handler));
        Ok(())
    }

    /// Replace the bytes available to subsequent calls to [`Host::read_stdin`].
    pub fn seed_stdin(&self, bytes: &[u8]) -> HostResult<()> {
        self.lock()?.stdin = bytes.iter().copied().collect();
        Ok(())
    }

    /// Create any missing parent directories, then write the file.
    pub fn seed_file(&self, path: &Path, bytes: &[u8]) -> HostResult<()> {
        crate::require_absolute(path)?;
        let mut state = self.lock()?;
        if let Some(parent) = path.parent() {
            state.fs.create_dir_all(parent)?;
        }
        state.fs.write(path, bytes, WriteMode::Truncate)
    }

    /// Return a snapshot of all recorded stdio events in order.
    #[must_use]
    pub fn events(&self) -> Vec<IoEvent> {
        self.state
            .lock()
            .map(|s| s.events.clone())
            .unwrap_or_default()
    }

    /// Concatenate all bytes written to `stream`.
    #[must_use]
    pub fn stream_bytes(&self, stream: Stream) -> BString {
        let mut out = Vec::new();
        if let Ok(guard) = self.state.lock() {
            for ev in &guard.events {
                if ev.stream == stream {
                    out.extend_from_slice(ev.bytes.as_slice());
                }
            }
        }
        BString::from(out)
    }
}

impl Host for PureHost {
    fn metadata(&self, p: &Path, follow: Follow) -> HostResult<Metadata> {
        self.lock()?.fs.metadata(p, follow)
    }

    fn read(&self, p: &Path, range: ReadRange) -> HostResult<Vec<u8>> {
        self.lock()?.fs.read(p, range)
    }

    fn write(&self, p: &Path, data: &[u8], mode: WriteMode) -> HostResult<()> {
        self.lock()?.fs.write(p, data, mode)
    }

    fn read_dir(&self, p: &Path) -> HostResult<Vec<DirEntry>> {
        self.lock()?.fs.read_dir(p)
    }

    fn create_dir_all(&self, p: &Path) -> HostResult<()> {
        self.lock()?.fs.create_dir_all(p)
    }

    fn remove_file(&self, p: &Path) -> HostResult<()> {
        self.lock()?.fs.remove_file(p)
    }

    fn remove_dir(&self, p: &Path) -> HostResult<()> {
        self.lock()?.fs.remove_dir(p)
    }

    fn remove_dir_all(&self, p: &Path) -> HostResult<()> {
        self.lock()?.fs.remove_dir_all(p)
    }

    fn rename(&self, from: &Path, to: &Path) -> HostResult<()> {
        self.lock()?.fs.rename(from, to)
    }

    fn copy_file(&self, from: &Path, to: &Path, opts: CopyOptions) -> HostResult<()> {
        self.lock()?.fs.copy_file(from, to, opts)
    }

    fn symlink(&self, target: &Path, link: &Path) -> HostResult<()> {
        self.lock()?.fs.symlink(target, link)
    }

    fn hard_link(&self, src: &Path, dst: &Path) -> HostResult<()> {
        self.lock()?.fs.hard_link(src, dst)
    }

    fn read_link(&self, p: &Path) -> HostResult<PathBuf> {
        self.lock()?.fs.read_link(p)
    }

    fn canonicalize(&self, p: &Path) -> HostResult<PathBuf> {
        self.lock()?.fs.canonicalize(p)
    }

    fn set_permissions(&self, p: &Path, mode: u32) -> HostResult<()> {
        self.lock()?.fs.set_permissions(p, mode)
    }

    fn set_times(
        &self,
        p: &Path,
        atime: Option<FileTime>,
        mtime: Option<FileTime>,
    ) -> HostResult<()> {
        self.lock()?.fs.set_times(p, atime, mtime)
    }

    fn env_get(&self, key: &[u8]) -> Option<BString> {
        self.state.lock().ok()?.env.get(key).cloned()
    }

    fn env_set(&self, key: &[u8], value: Option<&[u8]>) {
        if let Ok(mut guard) = self.state.lock() {
            match value {
                Some(v) => {
                    guard.env.insert(BString::from(key), BString::from(v));
                }
                None => {
                    guard.env.remove(key);
                }
            }
        }
    }

    fn env_all(&self) -> Vec<(BString, BString)> {
        self.state
            .lock()
            .map(|g| g.env.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
            .unwrap_or_default()
    }

    fn run(
        &self,
        req: &RunRequest,
        on_output: &mut dyn FnMut(Stream, &[u8]),
    ) -> HostResult<RunOutput> {
        validate_run(req)?;
        let handler = self
            .lock()?
            .handler
            .clone()
            .ok_or_else(|| HostError::unsupported("No command handler is registered"))?;
        handler(self, req, &mut |stream, bytes| {
            if req.tee {
                on_output(stream, bytes);
            }
        })
    }

    fn write_out(&self, stream: Stream, bytes: &[u8]) {
        if let Ok(mut guard) = self.state.lock() {
            guard.events.push(IoEvent {
                stream,
                bytes: BString::from(bytes),
            });
        }
    }

    fn read_stdin(&self, limit: u64) -> HostResult<Vec<u8>> {
        let mut state = self.lock()?;
        let count = usize::try_from(limit)
            .unwrap_or(usize::MAX)
            .min(state.stdin.len());
        Ok(state.stdin.drain(..count).collect())
    }

    fn is_terminal(&self, _stream: Stream) -> bool {
        false
    }

    fn now(&self) -> FileTime {
        self.state.lock().map(|g| g.fs.now).unwrap_or_default()
    }

    fn monotonic(&self) -> Duration {
        self.state
            .lock()
            .map(|g| g.fs.monotonic)
            .unwrap_or(Duration::ZERO)
    }

    fn sleep(&self, duration: Duration) {
        if let Ok(mut guard) = self.state.lock() {
            guard.fs.advance(duration);
        }
    }

    fn time_zone(&self) -> TimeZone {
        TimeZone::UTC
    }

    fn current_exe(&self) -> HostResult<PathBuf> {
        Ok(self.lock()?.exe.clone())
    }

    fn initial_cwd(&self) -> PathBuf {
        self.state
            .lock()
            .map(|g| g.cwd.clone())
            .unwrap_or_else(|_| PathBuf::from("/work"))
    }

    fn system(&self) -> SystemInfo {
        SystemInfo {
            sysname: BString::from("PureOS"),
            release: BString::from("1.0.0"),
            version: BString::from("PureHost"),
            machine: BString::from("arm64"),
            hostname: BString::from("purehost"),
            cpu_count: 4,
            pid: 1000,
        }
    }
}
