//! Deterministic in-memory [`Host`] implementation for fast, reproducible unit tests.
//!
//! Full in-memory symlink resolution, permissions, mtime advancing, and scripted
//! subprocess support are completed in Wave M0-b alongside [`crate::conformance`].

use core::time::Duration;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use jiff::tz::TimeZone;
use rcmake_base::BString;

use crate::error::{HostError, HostResult};
use crate::{
    CopyOptions, DirEntry, FileTime, Follow, Host, Metadata, ReadRange, RunOutput, RunRequest,
    Stream, SystemInfo, WriteMode,
};

/// An ordered stdio output event recorded by [`PureHost`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IoEvent {
    /// Target stream.
    pub stream: Stream,
    /// Bytes written in this event.
    pub bytes: BString,
}

#[derive(Debug)]
struct PureState {
    env: BTreeMap<BString, BString>,
    events: Vec<IoEvent>,
    cwd: PathBuf,
    exe: PathBuf,
    now: FileTime,
    monotonic: Duration,
}

/// In-memory deterministic [`Host`] with recorded stdio event log and virtual clock.
#[derive(Debug)]
pub struct PureHost {
    state: Mutex<PureState>,
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
                cwd,
                exe,
                now: FileTime {
                    secs: 1_700_000_000,
                    nanos: 0,
                },
                monotonic: Duration::ZERO,
            }),
        }
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
    fn metadata(&self, p: &Path, _follow: Follow) -> HostResult<Metadata> {
        if !p.is_absolute() {
            return Err(HostError::not_absolute(p.to_path_buf()));
        }
        Err(HostError::unsupported(
            "PureHost::metadata scheduled for M0-b",
        ))
    }

    fn read(&self, p: &Path, _range: ReadRange) -> HostResult<Vec<u8>> {
        if !p.is_absolute() {
            return Err(HostError::not_absolute(p.to_path_buf()));
        }
        Err(HostError::unsupported("PureHost::read scheduled for M0-b"))
    }

    fn write(&self, p: &Path, _data: &[u8], _mode: WriteMode) -> HostResult<()> {
        if !p.is_absolute() {
            return Err(HostError::not_absolute(p.to_path_buf()));
        }
        Err(HostError::unsupported("PureHost::write scheduled for M0-b"))
    }

    fn read_dir(&self, p: &Path) -> HostResult<Vec<DirEntry>> {
        if !p.is_absolute() {
            return Err(HostError::not_absolute(p.to_path_buf()));
        }
        Err(HostError::unsupported(
            "PureHost::read_dir scheduled for M0-b",
        ))
    }

    fn create_dir_all(&self, p: &Path) -> HostResult<()> {
        if !p.is_absolute() {
            return Err(HostError::not_absolute(p.to_path_buf()));
        }
        Err(HostError::unsupported(
            "PureHost::create_dir_all scheduled for M0-b",
        ))
    }

    fn remove_file(&self, p: &Path) -> HostResult<()> {
        if !p.is_absolute() {
            return Err(HostError::not_absolute(p.to_path_buf()));
        }
        Err(HostError::unsupported(
            "PureHost::remove_file scheduled for M0-b",
        ))
    }

    fn remove_dir(&self, p: &Path) -> HostResult<()> {
        if !p.is_absolute() {
            return Err(HostError::not_absolute(p.to_path_buf()));
        }
        Err(HostError::unsupported(
            "PureHost::remove_dir scheduled for M0-b",
        ))
    }

    fn remove_dir_all(&self, p: &Path) -> HostResult<()> {
        if !p.is_absolute() {
            return Err(HostError::not_absolute(p.to_path_buf()));
        }
        Err(HostError::unsupported(
            "PureHost::remove_dir_all scheduled for M0-b",
        ))
    }

    fn rename(&self, from: &Path, to: &Path) -> HostResult<()> {
        if !from.is_absolute() {
            return Err(HostError::not_absolute(from.to_path_buf()));
        }
        if !to.is_absolute() {
            return Err(HostError::not_absolute(to.to_path_buf()));
        }
        Err(HostError::unsupported(
            "PureHost::rename scheduled for M0-b",
        ))
    }

    fn copy_file(&self, from: &Path, to: &Path, _opts: CopyOptions) -> HostResult<()> {
        if !from.is_absolute() {
            return Err(HostError::not_absolute(from.to_path_buf()));
        }
        if !to.is_absolute() {
            return Err(HostError::not_absolute(to.to_path_buf()));
        }
        Err(HostError::unsupported(
            "PureHost::copy_file scheduled for M0-b",
        ))
    }

    fn symlink(&self, _target: &Path, link: &Path) -> HostResult<()> {
        if !link.is_absolute() {
            return Err(HostError::not_absolute(link.to_path_buf()));
        }
        Err(HostError::unsupported(
            "PureHost::symlink scheduled for M0-b",
        ))
    }

    fn hard_link(&self, src: &Path, dst: &Path) -> HostResult<()> {
        if !src.is_absolute() {
            return Err(HostError::not_absolute(src.to_path_buf()));
        }
        if !dst.is_absolute() {
            return Err(HostError::not_absolute(dst.to_path_buf()));
        }
        Err(HostError::unsupported(
            "PureHost::hard_link scheduled for M0-b",
        ))
    }

    fn read_link(&self, p: &Path) -> HostResult<PathBuf> {
        if !p.is_absolute() {
            return Err(HostError::not_absolute(p.to_path_buf()));
        }
        Err(HostError::unsupported(
            "PureHost::read_link scheduled for M0-b",
        ))
    }

    fn canonicalize(&self, p: &Path) -> HostResult<PathBuf> {
        if !p.is_absolute() {
            return Err(HostError::not_absolute(p.to_path_buf()));
        }
        Err(HostError::unsupported(
            "PureHost::canonicalize scheduled for M0-b",
        ))
    }

    fn set_permissions(&self, p: &Path, _mode: u32) -> HostResult<()> {
        if !p.is_absolute() {
            return Err(HostError::not_absolute(p.to_path_buf()));
        }
        Err(HostError::unsupported(
            "PureHost::set_permissions scheduled for M0-b",
        ))
    }

    fn set_times(
        &self,
        p: &Path,
        _atime: Option<FileTime>,
        _mtime: Option<FileTime>,
    ) -> HostResult<()> {
        if !p.is_absolute() {
            return Err(HostError::not_absolute(p.to_path_buf()));
        }
        Err(HostError::unsupported(
            "PureHost::set_times scheduled for M0-b",
        ))
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
        _req: &RunRequest,
        _on_output: &mut dyn FnMut(Stream, &[u8]),
    ) -> HostResult<RunOutput> {
        Err(HostError::unsupported("PureHost::run scheduled for M0-b"))
    }

    fn write_out(&self, stream: Stream, bytes: &[u8]) {
        if let Ok(mut guard) = self.state.lock() {
            guard.events.push(IoEvent {
                stream,
                bytes: BString::from(bytes),
            });
        }
    }

    fn read_stdin(&self, _limit: u64) -> HostResult<Vec<u8>> {
        Ok(Vec::new())
    }

    fn is_terminal(&self, _stream: Stream) -> bool {
        false
    }

    fn now(&self) -> FileTime {
        self.state.lock().map(|g| g.now).unwrap_or_default()
    }

    fn monotonic(&self) -> Duration {
        self.state
            .lock()
            .map(|g| g.monotonic)
            .unwrap_or(Duration::ZERO)
    }

    fn sleep(&self, duration: Duration) {
        if let Ok(mut guard) = self.state.lock() {
            guard.monotonic += duration;
            let total_nanos = u64::from(guard.now.nanos) + u64::from(duration.subsec_nanos());
            guard.now.secs = guard
                .now
                .secs
                .saturating_add(i64::try_from(duration.as_secs()).unwrap_or(0))
                .saturating_add(i64::try_from(total_nanos / 1_000_000_000).unwrap_or(0));
            guard.now.nanos = u32::try_from(total_nanos % 1_000_000_000).unwrap_or(0);
        }
    }

    fn time_zone(&self) -> TimeZone {
        TimeZone::UTC
    }

    fn current_exe(&self) -> HostResult<PathBuf> {
        self.state
            .lock()
            .map(|g| g.exe.clone())
            .map_err(|_| HostError::unsupported("PureHost lock poisoned"))
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
