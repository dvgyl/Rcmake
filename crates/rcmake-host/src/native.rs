//! Real OS [`Host`] implementation backed by `std::fs`, `std::process`, and `rustix`.
//!
#![allow(clippy::disallowed_methods)]

use core::time::Duration;
use std::collections::BTreeMap;
use std::fs::{self, File, FileType, OpenOptions};
use std::io::{self, ErrorKind, IsTerminal, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::RwLock;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use jiff::tz::TimeZone;
use rcmake_base::BString;

use crate::error::{HostError, HostResult, SyntheticError};
use crate::{
    CopyOptions, DirEntry, FILE_CHUNK_SIZE, FileKind, FileLockGuard, FileTime, Follow,
    GracefulTimeout, Host, Metadata, ReadRange, RunOutput, RunRequest, Stream, SystemInfo,
    WriteMode, require_absolute, validate_run,
};

#[cfg(unix)]
mod cwd;
#[cfg(unix)]
#[path = "native/unix.rs"]
mod platform;
#[cfg(windows)]
#[path = "native/windows.rs"]
mod platform;
mod process;

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
                env_key(&crate::path::from_native(Path::new(&k))),
                crate::path::from_native(Path::new(&v)),
            );
        }
        let physical_cwd = std::env::current_dir().map_err(|e| HostError::from_io(e, None))?;
        let pwd = map
            .get(b"PWD".as_slice())
            .map(|path| crate::path::to_native(path));
        let initial_cwd = platform::logical_cwd(&physical_cwd, pwd.as_deref());
        Ok(Self {
            env: RwLock::new(map),
            initial_cwd,
            start: Instant::now(),
        })
    }

    fn set_entry_times(
        &self,
        p: &Path,
        atime: Option<FileTime>,
        mtime: Option<FileTime>,
        follow: Follow,
    ) -> HostResult<()> {
        require_absolute(p)?;
        if atime
            .into_iter()
            .chain(mtime)
            .any(|time| time.nanos >= 1_000_000_000)
        {
            return Err(SyntheticError::InvalidInput.at(Some(p)));
        }
        if atime.is_none() && mtime.is_none() {
            return self.metadata(p, follow).map(|_| ());
        }
        io_at(platform::set_times(p, atime, mtime, follow), p)
    }
}

fn env_key(key: &[u8]) -> BString {
    let mut name = BString::from(key);
    if cfg!(windows) {
        name.make_ascii_uppercase();
    }
    name
}

fn io_at<T>(result: io::Result<T>, path: &Path) -> HostResult<T> {
    result.map_err(|err| HostError::from_io(err, Some(path.to_path_buf())))
}

fn file_kind(kind: FileType) -> FileKind {
    if kind.is_file() {
        FileKind::File
    } else if kind.is_dir() {
        FileKind::Dir
    } else if kind.is_symlink() {
        FileKind::Symlink
    } else {
        FileKind::Other
    }
}

fn open_output(path: &Path, mode: WriteMode) -> HostResult<File> {
    let mut options = OpenOptions::new();
    options.write(true);
    match mode {
        WriteMode::Truncate => options.create(true).truncate(true),
        WriteMode::Append => options.create(true).append(true),
        WriteMode::CreateNew => options.create_new(true),
    };
    io_at(options.open(path), path)
}

/// Interval between two lock attempts while [`Host::lock_file`] waits with a timeout.
const LOCK_POLL_INTERVAL: Duration = Duration::from_millis(20);

/// Exclusive lock held by [`NativeHost::lock_file`]. Drop releases the lock.
#[derive(Debug)]
struct NativeFileLock {
    file: File,
    path: PathBuf,
}

impl FileLockGuard for NativeFileLock {
    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for NativeFileLock {
    fn drop(&mut self) {
        // Closing the file also releases the lock. The explicit unlock releases it
        // at once, also when another handle to the same open file exists.
        let _ = self.file.unlock();
    }
}

#[cfg(any(test, not(any(target_os = "macos", target_os = "linux"))))]
fn rename_noreplace_with_link(from: &Path, to: &Path) -> HostResult<()> {
    const ERROR_INVALID_FUNCTION: i32 = 1;
    const ERROR_NOT_SUPPORTED: i32 = 50;
    match fs::hard_link(from, to) {
        Ok(()) => io_at(fs::remove_file(from), from),
        Err(err)
            if matches!(
                err.kind(),
                ErrorKind::Unsupported | ErrorKind::CrossesDevices
            ) || cfg!(windows)
                && matches!(
                    err.raw_os_error(),
                    Some(ERROR_INVALID_FUNCTION | ERROR_NOT_SUPPORTED)
                ) =>
        {
            Err(HostError::unsupported(
                "hard links are not supported for these paths",
            ))
        }
        Err(err) if err.kind() == ErrorKind::AlreadyExists => io_at(Err(err), to),
        Err(err) => io_at(Err(err), from),
    }
}

impl Host for NativeHost {
    fn metadata(&self, p: &Path, follow: Follow) -> HostResult<Metadata> {
        require_absolute(p)?;
        platform::metadata(p, follow)
    }

    fn open_read(&self, p: &Path) -> HostResult<Box<dyn crate::ReadSeek + '_>> {
        require_absolute(p)?;
        let file = io_at(platform::open_input(p), p)?;
        if !io_at(file.metadata(), p)?.is_file() {
            return Err(HostError::unsupported(
                "Streaming reads require a regular file.",
            ));
        }
        Ok(Box::new(file))
    }

    fn scratch_file(&self) -> HostResult<Box<dyn crate::ScratchFile + '_>> {
        let file = tempfile::tempfile().map_err(|error| HostError::from_io(error, None))?;
        Ok(Box::new(file))
    }

    fn scratch_file_in(&self, dir: &Path) -> HostResult<Box<dyn crate::ScratchFile + '_>> {
        require_absolute(dir)?;
        let file = io_at(tempfile::tempfile_in(dir), dir)?;
        Ok(Box::new(file))
    }

    fn write_from(
        &self,
        p: &Path,
        mode: WriteMode,
        input: &mut dyn Read,
        len: u64,
    ) -> HostResult<()> {
        require_absolute(p)?;
        let mut file = open_output(p, mode)?;
        let result = io_at(crate::stream::copy_exact(input, &mut file, len), p);
        drop(file);
        if result.is_err() && mode == WriteMode::CreateNew {
            let _ = self.remove_file(p);
        }
        result
    }

    fn read(&self, p: &Path, range: ReadRange) -> HostResult<Vec<u8>> {
        require_absolute(p)?;
        let mut file = io_at(platform::open_input(p), p)?;
        let metadata = io_at(file.metadata(), p)?;
        if metadata.is_dir() {
            return Err(SyntheticError::IsADirectory.at(Some(p)));
        }
        let mut bytes = Vec::new();
        match range {
            ReadRange::All { limit } => {
                if metadata.is_file() && metadata.len() > limit {
                    return Err(SyntheticError::FileTooLarge.at(Some(p)));
                }
                let mut reader = (&mut file).take(limit);
                io_at(reader.read_to_end(&mut bytes), p)?;
                if reader.limit() == 0 {
                    match file.read_exact(&mut [0]) {
                        Ok(()) => return Err(SyntheticError::FileTooLarge.at(Some(p))),
                        Err(err) if err.kind() == ErrorKind::UnexpectedEof => {}
                        Err(err) => return io_at(Err(err), p),
                    }
                }
            }
            ReadRange::Slice { offset, len } => {
                if offset != 0 {
                    if metadata.is_file() && i64::try_from(offset).is_err() {
                        return Ok(bytes);
                    }
                    io_at(file.seek(SeekFrom::Start(offset)), p)?;
                }
                io_at(
                    file.take(len.unwrap_or(u64::MAX)).read_to_end(&mut bytes),
                    p,
                )?;
            }
        }
        Ok(bytes)
    }

    fn read_chunks(&self, p: &Path, consume: &mut dyn FnMut(&[u8])) -> HostResult<()> {
        require_absolute(p)?;
        let mut file = io_at(platform::open_input(p), p)?;
        let mut buffer = vec![0; FILE_CHUNK_SIZE];
        loop {
            match file.read(&mut buffer) {
                Ok(0) => return Ok(()),
                Ok(len) => {
                    let bytes = buffer.get(..len).ok_or_else(|| {
                        HostError::from_io(
                            io::Error::other("Invalid file read length"),
                            Some(p.to_path_buf()),
                        )
                    })?;
                    consume(bytes);
                }
                Err(err) if err.kind() == ErrorKind::Interrupted => continue,
                Err(err) => return io_at(Err(err), p),
            }
        }
    }

    fn write(&self, p: &Path, data: &[u8], mode: WriteMode) -> HostResult<()> {
        require_absolute(p)?;
        io_at(open_output(p, mode)?.write_all(data), p)
    }

    fn read_dir(&self, p: &Path) -> HostResult<Vec<DirEntry>> {
        require_absolute(p)?;
        io_at(fs::read_dir(p), p)?
            .map(|entry| {
                let entry = io_at(entry, p)?;
                Ok(DirEntry {
                    name: crate::path::from_native(Path::new(&entry.file_name())),
                    kind: file_kind(io_at(entry.file_type(), &entry.path())?),
                })
            })
            .collect()
    }

    fn create_dir_all(&self, p: &Path) -> HostResult<()> {
        require_absolute(p)?;
        // `fs::create_dir_all` fails for a missing parent when the path ends in `/.`.
        let clean: PathBuf = p.components().collect();
        io_at(fs::create_dir_all(clean), p)
    }

    fn remove_file(&self, p: &Path) -> HostResult<()> {
        require_absolute(p)?;
        // macOS reports EPERM when unlinking a directory.
        if io_at(fs::symlink_metadata(p), p)?.is_dir() {
            return Err(SyntheticError::IsADirectory.at(Some(p)));
        }
        io_at(platform::remove_file(p), p)
    }

    fn remove_dir(&self, p: &Path) -> HostResult<()> {
        let path = crate::path::directory_removal_path(p, require_absolute, cfg!(windows))?;
        if !io_at(fs::symlink_metadata(&path), p)?.is_dir() {
            return Err(SyntheticError::NotADirectory.at(Some(p)));
        }
        io_at(fs::remove_dir(path), p)
    }

    fn remove_dir_all(&self, p: &Path) -> HostResult<()> {
        let path = crate::path::directory_removal_path(p, require_absolute, cfg!(windows))?;
        io_at(platform::remove_dir_all(&path), p)
    }

    fn rename(&self, from: &Path, to: &Path) -> HostResult<()> {
        require_absolute(from)?;
        require_absolute(to)?;
        io_at(fs::rename(from, to), from)
    }

    fn rename_noreplace(&self, from: &Path, to: &Path) -> HostResult<()> {
        require_absolute(from)?;
        require_absolute(to)?;
        platform::rename_noreplace(from, to)
    }

    fn lock_file(
        &self,
        path: &Path,
        timeout: Option<Duration>,
    ) -> HostResult<Box<dyn FileLockGuard>> {
        require_absolute(path)?;
        let file = io_at(
            OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .open(path),
            path,
        )?;
        match timeout {
            None => io_at(file.lock(), path)?,
            Some(limit) => {
                let deadline = Instant::now().checked_add(limit);
                loop {
                    match file.try_lock() {
                        Ok(()) => break,
                        Err(fs::TryLockError::Error(err)) => return io_at(Err(err), path),
                        Err(fs::TryLockError::WouldBlock) => {}
                    }
                    let now = Instant::now();
                    let remaining = match deadline {
                        Some(deadline) if now < deadline => deadline.duration_since(now),
                        Some(_) => {
                            return Err(HostError::timed_out(
                                "Timeout reached",
                                Some(path.to_path_buf()),
                            ));
                        }
                        None => LOCK_POLL_INTERVAL,
                    };
                    std::thread::sleep(remaining.min(LOCK_POLL_INTERVAL));
                }
            }
        }
        Ok(Box::new(NativeFileLock {
            file,
            path: path.to_path_buf(),
        }))
    }

    fn copy_file(&self, from: &Path, to: &Path, opts: CopyOptions) -> HostResult<()> {
        require_absolute(from)?;
        require_absolute(to)?;
        let mut src = io_at(platform::open_input(from), from)?;
        let metadata = io_at(src.metadata(), from)?;
        if !metadata.is_file() {
            return Err(SyntheticError::InvalidInput.at(Some(from)));
        }
        let mut dst = io_at(
            OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(false)
                .open(to),
            to,
        )?;
        if io_at(platform::same_file(&src, &dst), to)? {
            return Err(SyntheticError::InvalidInput.at(Some(to)));
        }
        io_at(dst.set_len(0), to)?;
        io_at(io::copy(&mut src, &mut dst), to)?;
        if opts.preserve_permissions {
            io_at(dst.set_permissions(metadata.permissions()), to)?;
        }
        if opts.preserve_mtime {
            io_at(platform::preserve_mtime(&dst, &metadata), to)?;
        }
        Ok(())
    }

    fn symlink(&self, target: &Path, link: &Path) -> HostResult<()> {
        require_absolute(link)?;
        io_at(platform::symlink(target, link), link)
    }

    fn hard_link(&self, src: &Path, dst: &Path) -> HostResult<()> {
        require_absolute(src)?;
        require_absolute(dst)?;
        io_at(platform::hard_link(src, dst), dst)
    }

    fn read_link(&self, p: &Path) -> HostResult<PathBuf> {
        require_absolute(p)?;
        io_at(fs::read_link(p), p).map(platform::normalize_path)
    }

    fn canonicalize(&self, p: &Path) -> HostResult<PathBuf> {
        require_absolute(p)?;
        #[cfg(windows)]
        {
            use std::path::Component;

            // Resolve a symlink before applying its parent component.
            if p.components().any(|part| part == Component::ParentDir) {
                let mut resolved = PathBuf::new();
                for part in p.components() {
                    match part {
                        Component::ParentDir => {
                            let directory = io_at(fs::canonicalize(&resolved), p)?;
                            if !io_at(fs::metadata(&directory), p)?.is_dir() {
                                return Err(SyntheticError::NotADirectory.at(Some(p)));
                            }
                            resolved = directory;
                            resolved.pop();
                        }
                        Component::CurDir => {}
                        other => resolved.push(other.as_os_str()),
                    }
                }
                return io_at(fs::canonicalize(&resolved), p).map(platform::normalize_path);
            }
        }
        // macOS realpath can simplify ".." after an entry that is not a directory.
        io_at(fs::metadata(p), p)?;
        io_at(fs::canonicalize(p), p).map(platform::normalize_path)
    }

    fn set_permissions(&self, p: &Path, mode: u32) -> HostResult<()> {
        require_absolute(p)?;
        io_at(platform::set_permissions(p, mode), p)
    }

    fn set_times(
        &self,
        p: &Path,
        atime: Option<FileTime>,
        mtime: Option<FileTime>,
    ) -> HostResult<()> {
        self.set_entry_times(p, atime, mtime, Follow::Yes)
    }

    fn set_symlink_times(
        &self,
        p: &Path,
        atime: Option<FileTime>,
        mtime: Option<FileTime>,
    ) -> HostResult<()> {
        self.set_entry_times(p, atime, mtime, Follow::No)
    }

    #[cfg(windows)]
    fn registry_host_view(&self) -> Option<crate::RegistryView> {
        Some(platform::registry::host_view(self))
    }

    #[cfg(windows)]
    fn registry_value(
        &self,
        root: crate::RegistryRoot,
        subkey: &[u8],
        name: Option<&[u8]>,
        view: crate::RegistryView,
    ) -> HostResult<Option<BString>> {
        Ok(platform::registry::value(self, root, subkey, name, view))
    }

    fn env_get(&self, key: &[u8]) -> Option<BString> {
        let guard = self.env.read().ok()?;
        guard.get(&env_key(key)).cloned()
    }

    fn env_set(&self, key: &[u8], value: Option<&[u8]>) {
        if let Ok(mut guard) = self.env.write() {
            match value {
                Some(v) => {
                    guard.insert(env_key(key), BString::from(v));
                }
                None => {
                    guard.remove(&env_key(key));
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
        req: &RunRequest,
        on_output: &mut dyn FnMut(Stream, &[u8]),
    ) -> HostResult<RunOutput> {
        validate_run(req, require_absolute)?;
        process::run_pipeline(req, None, usize::MAX, on_output, None, None)
            .map(|result| result.output)
    }

    fn run_with_timeout_signal(
        &self,
        req: &RunRequest,
        timeout: GracefulTimeout,
        on_output: &mut dyn FnMut(Stream, &[u8]),
    ) -> HostResult<RunOutput> {
        validate_run(req, require_absolute)?;
        process::run_pipeline(req, Some(timeout), usize::MAX, on_output, None, None)
            .map(|result| result.output)
    }

    fn run_controlled(
        &self,
        req: &RunRequest,
        graceful: Option<GracefulTimeout>,
        hard_limit: Option<Duration>,
        update: &mut crate::DeadlineUpdate<'_>,
    ) -> HostResult<crate::ControlledOutput> {
        validate_run(req, require_absolute)?;
        process::run_pipeline(
            req,
            graceful,
            usize::MAX,
            &mut |_, _| {},
            hard_limit,
            Some(update),
        )
    }

    fn run_bounded(&self, req: &RunRequest, limit: usize) -> HostResult<RunOutput> {
        validate_run(req, require_absolute)?;
        process::run_pipeline(req, None, limit, &mut |_, _| {}, None, None)
            .map(|result| result.output)
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
        let mut system = platform::system();
        system.cpu_count = std::thread::available_parallelism()
            .map(|n| u32::try_from(n.get()).unwrap_or(1))
            .unwrap_or(1);
        system.pid = std::process::id();
        system
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sets_link_times_without_changing_the_target() -> HostResult<()> {
        #[cfg(windows)]
        const ERROR_PRIVILEGE_NOT_HELD: i32 = 1314;

        let dir = tempfile::tempdir().map_err(|err| HostError::from_io(err, None))?;
        let target = dir.path().join("target");
        let link = dir.path().join("link");
        let host = NativeHost::new()?;
        host.write(&target, b"file", WriteMode::CreateNew)?;
        match host.symlink(&target, &link) {
            #[cfg(windows)]
            Err(err) if err.errno == Some(ERROR_PRIVILEGE_NOT_HELD) => return Ok(()),
            result => result?,
        }
        let target_before = host.metadata(&target, Follow::No)?;
        let link_before = host.metadata(&link, Follow::No)?;

        let mtime = FileTime {
            secs: 1_700_000_000,
            nanos: 0,
        };
        host.set_symlink_times(&link, None, Some(mtime))?;
        let updated = host.metadata(&link, Follow::No)?;
        assert_eq!(updated.mtime, mtime);
        assert_eq!(updated.atime, link_before.atime);
        assert_eq!(host.metadata(&target, Follow::No)?, target_before);

        host.remove_file(&target)?;
        host.set_symlink_times(&link, Some(mtime), None)?;
        assert_eq!(host.metadata(&link, Follow::No)?.atime, mtime);
        assert_eq!(host.metadata(&link, Follow::No)?.mtime, mtime);
        Ok(())
    }

    #[test]
    fn hard_link_rename_preserves_an_existing_destination() -> io::Result<()> {
        let dir = std::env::temp_dir().join(format!("rcmake-link-rename-{}", std::process::id()));
        fs::create_dir(&dir)?;
        let result = (|| -> io::Result<()> {
            let from = dir.join("from");
            let to = dir.join("to");
            fs::write(&from, b"source")?;
            fs::write(&to, b"destination")?;
            let result = rename_noreplace_with_link(&from, &to);
            assert!(matches!(result, Err(err) if err.kind == ErrorKind::AlreadyExists));
            assert_eq!(fs::read(&from)?, b"source");
            assert_eq!(fs::read(&to)?, b"destination");

            fs::remove_file(&to)?;
            assert!(rename_noreplace_with_link(&from, &to).is_ok());
            assert!(!from.exists());
            assert_eq!(fs::read(&to)?, b"source");
            Ok(())
        })();
        let cleanup = fs::remove_dir_all(&dir);
        result.and(cleanup)
    }

    #[test]
    fn creates_missing_parents_for_a_path_that_ends_in_a_dot() -> HostResult<()> {
        let dir = tempfile::tempdir().map_err(|err| HostError::from_io(err, None))?;
        let host = NativeHost::new()?;
        let target = dir.path().join("prefix").join("nested").join(".");
        host.create_dir_all(&target)?;
        let created = host.metadata(&dir.path().join("prefix").join("nested"), Follow::No)?;
        assert_eq!(created.kind, FileKind::Dir);
        Ok(())
    }
}
