//! Real OS [`Host`] implementation backed by `std::fs`, `std::process`, and `rustix`.
//!
//! Full filesystem, symlink, timestamp, permission, and pipeline process-group support
//! is completed in Wave M0-b alongside [`crate::conformance`].

#![allow(clippy::disallowed_methods)]

use core::time::Duration;
use std::collections::BTreeMap;
use std::io::{IsTerminal, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::RwLock;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use jiff::tz::TimeZone;
use rcmake_base::BString;

use crate::error::{HostError, HostResult};
use crate::{
    CopyOptions, DirEntry, FileTime, Follow, Host, Metadata, ReadRange, RunOutput, RunRequest,
    Stream, SystemInfo, WriteMode,
};

/// Production [`Host`] backed by the operating system with an edition-2024-safe
/// interior-mutable environment overlay.
#[derive(Debug)]
pub struct NativeHost {
    env: RwLock<BTreeMap<BString, BString>>,
    initial_cwd: PathBuf,
    start: Instant,
}

impl NativeHost {
    /// Capture the current process environment and working directory into a new [`NativeHost`].
    pub fn new() -> HostResult<Self> {
        let mut map = BTreeMap::new();
        for (k, v) in std::env::vars_os() {
            map.insert(
                crate::path::from_native(Path::new(&k)),
                crate::path::from_native(Path::new(&v)),
            );
        }
        let initial_cwd = std::env::current_dir().map_err(|e| HostError::from_io(e, None))?;
        Ok(Self {
            env: RwLock::new(map),
            initial_cwd,
            start: Instant::now(),
        })
    }
}

fn require_abs(p: &Path) -> HostResult<()> {
    if p.is_absolute() {
        Ok(())
    } else {
        Err(HostError::not_absolute(p.to_path_buf()))
    }
}

impl Host for NativeHost {
    fn metadata(&self, p: &Path, _follow: Follow) -> HostResult<Metadata> {
        require_abs(p)?;
        Err(HostError::unsupported(
            "NativeHost::metadata scheduled for M0-b",
        ))
    }

    fn read(&self, p: &Path, _range: ReadRange) -> HostResult<Vec<u8>> {
        require_abs(p)?;
        std::fs::read(p).map_err(|e| HostError::from_io(e, Some(p.to_path_buf())))
    }

    fn write(&self, p: &Path, data: &[u8], _mode: WriteMode) -> HostResult<()> {
        require_abs(p)?;
        std::fs::write(p, data).map_err(|e| HostError::from_io(e, Some(p.to_path_buf())))
    }

    fn read_dir(&self, p: &Path) -> HostResult<Vec<DirEntry>> {
        require_abs(p)?;
        Err(HostError::unsupported(
            "NativeHost::read_dir scheduled for M0-b",
        ))
    }

    fn create_dir_all(&self, p: &Path) -> HostResult<()> {
        require_abs(p)?;
        std::fs::create_dir_all(p).map_err(|e| HostError::from_io(e, Some(p.to_path_buf())))
    }

    fn remove_file(&self, p: &Path) -> HostResult<()> {
        require_abs(p)?;
        std::fs::remove_file(p).map_err(|e| HostError::from_io(e, Some(p.to_path_buf())))
    }

    fn remove_dir(&self, p: &Path) -> HostResult<()> {
        require_abs(p)?;
        std::fs::remove_dir(p).map_err(|e| HostError::from_io(e, Some(p.to_path_buf())))
    }

    fn remove_dir_all(&self, p: &Path) -> HostResult<()> {
        require_abs(p)?;
        std::fs::remove_dir_all(p).map_err(|e| HostError::from_io(e, Some(p.to_path_buf())))
    }

    fn rename(&self, from: &Path, to: &Path) -> HostResult<()> {
        require_abs(from)?;
        require_abs(to)?;
        std::fs::rename(from, to).map_err(|e| HostError::from_io(e, Some(from.to_path_buf())))
    }

    fn copy_file(&self, from: &Path, to: &Path, _opts: CopyOptions) -> HostResult<()> {
        require_abs(from)?;
        require_abs(to)?;
        std::fs::copy(from, to)
            .map(|_| ())
            .map_err(|e| HostError::from_io(e, Some(from.to_path_buf())))
    }

    fn symlink(&self, _target: &Path, link: &Path) -> HostResult<()> {
        require_abs(link)?;
        Err(HostError::unsupported(
            "NativeHost::symlink scheduled for M0-b",
        ))
    }

    fn hard_link(&self, src: &Path, dst: &Path) -> HostResult<()> {
        require_abs(src)?;
        require_abs(dst)?;
        std::fs::hard_link(src, dst).map_err(|e| HostError::from_io(e, Some(dst.to_path_buf())))
    }

    fn read_link(&self, p: &Path) -> HostResult<PathBuf> {
        require_abs(p)?;
        std::fs::read_link(p).map_err(|e| HostError::from_io(e, Some(p.to_path_buf())))
    }

    fn canonicalize(&self, p: &Path) -> HostResult<PathBuf> {
        require_abs(p)?;
        std::fs::canonicalize(p).map_err(|e| HostError::from_io(e, Some(p.to_path_buf())))
    }

    fn set_permissions(&self, p: &Path, _mode: u32) -> HostResult<()> {
        require_abs(p)?;
        Err(HostError::unsupported(
            "NativeHost::set_permissions scheduled for M0-b",
        ))
    }

    fn set_times(
        &self,
        p: &Path,
        _atime: Option<FileTime>,
        _mtime: Option<FileTime>,
    ) -> HostResult<()> {
        require_abs(p)?;
        Err(HostError::unsupported(
            "NativeHost::set_times scheduled for M0-b",
        ))
    }

    fn env_get(&self, key: &[u8]) -> Option<BString> {
        let guard = self.env.read().ok()?;
        guard.get(key).cloned()
    }

    fn env_set(&self, key: &[u8], value: Option<&[u8]>) {
        if let Ok(mut guard) = self.env.write() {
            match value {
                Some(v) => {
                    guard.insert(BString::from(key), BString::from(v));
                }
                None => {
                    guard.remove(key);
                }
            }
        }
    }

    fn env_all(&self) -> Vec<(BString, BString)> {
        self.env
            .read()
            .map(|g| g.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
            .unwrap_or_default()
    }

    fn run(
        &self,
        _req: &RunRequest,
        _on_output: &mut dyn FnMut(Stream, &[u8]),
    ) -> HostResult<RunOutput> {
        Err(HostError::unsupported("NativeHost::run scheduled for M0-b"))
    }

    fn write_out(&self, stream: Stream, bytes: &[u8]) {
        match stream {
            Stream::Stdout => {
                let mut out = std::io::stdout().lock();
                let _ = out.write_all(bytes);
                let _ = out.flush();
            }
            Stream::Stderr => {
                let mut err = std::io::stderr().lock();
                let _ = err.write_all(bytes);
                let _ = err.flush();
            }
        }
    }

    fn read_stdin(&self, limit: u64) -> HostResult<Vec<u8>> {
        let mut buf = Vec::new();
        std::io::stdin()
            .take(limit)
            .read_to_end(&mut buf)
            .map_err(|e| HostError::from_io(e, None))?;
        Ok(buf)
    }

    fn is_terminal(&self, stream: Stream) -> bool {
        match stream {
            Stream::Stdout => std::io::stdout().is_terminal(),
            Stream::Stderr => std::io::stderr().is_terminal(),
        }
    }

    fn now(&self) -> FileTime {
        match SystemTime::now().duration_since(UNIX_EPOCH) {
            Ok(d) => FileTime {
                secs: i64::try_from(d.as_secs()).unwrap_or(i64::MAX),
                nanos: d.subsec_nanos(),
            },
            Err(_) => FileTime::default(),
        }
    }

    fn monotonic(&self) -> Duration {
        self.start.elapsed()
    }

    fn sleep(&self, duration: Duration) {
        std::thread::sleep(duration);
    }

    fn time_zone(&self) -> TimeZone {
        TimeZone::system()
    }

    fn current_exe(&self) -> HostResult<PathBuf> {
        std::env::current_exe().map_err(|e| HostError::from_io(e, None))
    }

    fn initial_cwd(&self) -> PathBuf {
        self.initial_cwd.clone()
    }

    fn system(&self) -> SystemInfo {
        let u = rustix::system::uname();
        let cpus = std::thread::available_parallelism()
            .map(|n| u32::try_from(n.get()).unwrap_or(1))
            .unwrap_or(1);
        SystemInfo {
            sysname: BString::from(u.sysname().to_bytes()),
            release: BString::from(u.release().to_bytes()),
            version: BString::from(u.version().to_bytes()),
            machine: BString::from(u.machine().to_bytes()),
            hostname: BString::from(u.nodename().to_bytes()),
            cpu_count: cpus,
            pid: std::process::id(),
        }
    }
}
