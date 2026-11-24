//! Deterministic OS abstraction (`Host` trait) with [`NativeHost`] and [`PureHost`]
//! implementations plus a shared [`conformance`] suite (ADR-004).
//!
//! All engine crates interact with the filesystem, environment, child processes, stdio,
//! clock, and system identity exclusively through [`Host`] (mechanically enforced via
//! `clippy.toml` `disallowed-methods`).

#![forbid(unsafe_code)]

mod cmake_root;
pub mod conformance;
pub mod error;
pub mod native;
pub mod path;
pub mod pure;

use core::time::Duration;
use std::path::{Path, PathBuf};

use jiff::tz::TimeZone;
use rcmake_base::BString;

pub use cmake_root::discover_cmake_root;
pub use error::{HostError, HostResult};
pub use native::NativeHost;
pub use pure::PureHost;

/// Whether a metadata query follows symbolic links (`stat` vs `lstat`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Follow {
    /// Follow symlinks (`stat`).
    Yes,
    /// Do not follow symlinks (`lstat`).
    No,
}

/// Byte range requested from [`Host::read`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReadRange {
    /// Read the entire file up to `limit` bytes.
    All {
        /// Maximum allowed bytes to read.
        limit: u64,
    },
    /// Read starting at `offset` for at most `len` bytes.
    Slice {
        /// Starting byte offset.
        offset: u64,
        /// Optional maximum length to read.
        len: Option<u64>,
    },
}

/// Write mode for [`Host::write`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WriteMode {
    /// Create or truncate existing file.
    Truncate,
    /// Create or append to existing file.
    Append,
    /// Create only if the path does not already exist.
    CreateNew,
}

/// Options for [`Host::copy_file`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CopyOptions {
    /// Preserve file permission bits (e.g., execute bit).
    pub preserve_permissions: bool,
    /// Preserve modification timestamp.
    pub preserve_mtime: bool,
}

/// Kind of filesystem entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FileKind {
    /// Regular file.
    File,
    /// Directory.
    Dir,
    /// Symbolic link.
    Symlink,
    /// Special file (FIFO, socket, device).
    Other,
}

/// High-resolution timestamp (seconds since Unix epoch + nanoseconds).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub struct FileTime {
    /// Whole seconds since `1970-01-01T00:00:00Z`.
    pub secs: i64,
    /// Sub-second nanoseconds (`0..1_000_000_000`).
    pub nanos: u32,
}

/// File metadata returned by [`Host::metadata`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Metadata {
    /// Entry kind.
    pub kind: FileKind,
    /// File length in bytes.
    pub len: u64,
    /// POSIX permission mode bits.
    pub mode: u32,
    /// Modification timestamp.
    pub mtime: FileTime,
    /// Access timestamp.
    pub atime: FileTime,
    /// Device ID (for symlink cycle detection in glob walks).
    pub dev: u64,
    /// Inode number (for symlink cycle detection in glob walks).
    pub ino: u64,
    /// Hard-link count.
    pub nlink: u64,
}

/// One entry returned by [`Host::read_dir`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DirEntry {
    /// Entry file name (single path component).
    pub name: BString,
    /// Entry kind (without following symlinks).
    pub kind: FileKind,
}

/// Standard I/O stream selector.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stream {
    /// Standard output.
    Stdout,
    /// Standard error.
    Stderr,
}

/// Standard input configuration for a child process or pipeline.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Input {
    /// Inherit parent stdin.
    Inherit,
    /// Connect to null device (`/dev/null`).
    Null,
    /// Read from an absolute file path.
    File(PathBuf),
    /// Feed fixed byte slice.
    Bytes(Vec<u8>),
}

/// Standard output/error configuration for a child process or pipeline.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Output {
    /// Capture into [`RunOutput`].
    Capture,
    /// Inherit parent stream.
    Inherit,
    /// Redirect to an absolute file path.
    File {
        /// Destination absolute file path.
        path: PathBuf,
        /// Append instead of truncating.
        append: bool,
    },
    /// Discard to null device.
    Null,
    /// Share the same pipe with stdout (valid for stderr only, preserving interleaving).
    SharedWithStdout,
}

/// Single command stage inside a [`RunRequest`] pipeline.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommandSpec {
    /// Program name or absolute path.
    pub program: BString,
    /// Command arguments (excluding argv0).
    pub args: Vec<BString>,
}

/// Specification for running a command or pipeline via [`Host::run`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RunRequest {
    /// Ordered pipeline stages (1 stage for ordinary execution, N stages piped stdout->stdin).
    pub pipeline: Vec<CommandSpec>,
    /// Explicit absolute working directory.
    pub cwd: PathBuf,
    /// Explicit complete child environment (`KEY`, `VALUE`).
    pub env: Vec<(BString, BString)>,
    /// Standard input for the first pipeline stage.
    pub stdin: Input,
    /// Standard output for the last pipeline stage.
    pub stdout: Output,
    /// Standard error for pipeline stages.
    pub stderr: Output,
    /// Optional wall-clock timeout (kills entire process group on expiry).
    pub timeout: Option<Duration>,
    /// Whether to stream captured output chunks to `on_output` as they arrive.
    pub tee: bool,
}

/// Completion outcome of one stage in a [`RunRequest`] pipeline.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProcessResult {
    /// Process exited normally with status code.
    Exited(i32),
    /// Process was terminated by POSIX signal number.
    Signaled(i32),
    /// Process exceeded [`RunRequest::timeout`] and was terminated.
    TimedOut,
    /// Failed to spawn process.
    SpawnFailed(HostError),
}

/// Output captured from [`Host::run`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RunOutput {
    /// Result of each pipeline stage in order.
    pub results: Vec<ProcessResult>,
    /// Captured stdout bytes (when [`Output::Capture`]).
    pub stdout: Vec<u8>,
    /// Captured stderr bytes (when [`Output::Capture`]).
    pub stderr: Vec<u8>,
}

/// Static host OS & hardware identity (used by `CMAKE_HOST_*` and `cmake_host_system_information`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SystemInfo {
    /// OS kernel name (e.g., `Darwin`, `Linux`).
    pub sysname: BString,
    /// Kernel release string.
    pub release: BString,
    /// Kernel version string.
    pub version: BString,
    /// Machine architecture (e.g., `arm64`, `x86_64`).
    pub machine: BString,
    /// Hostname.
    pub hostname: BString,
    /// Logical CPU count.
    pub cpu_count: u32,
    /// Current process ID.
    pub pid: u32,
}

/// An exclusive advisory lock on a file, returned by [`Host::lock_file`].
///
/// The lock stays held while the guard exists. Drop the guard to release the lock.
pub trait FileLockGuard: Send + core::fmt::Debug {
    /// Absolute path of the locked file.
    fn path(&self) -> &Path;
}

/// Core OS abstraction trait for Rcmake (ADR-004).
///
/// Every filesystem path argument MUST be absolute; relative paths return
/// [`HostError::not_absolute`]. All methods take `&self` so hosts can be shared via `Arc`.
pub trait Host: Send + Sync {
    /// Query filesystem metadata (following or not following symlinks).
    fn metadata(&self, p: &Path, follow: Follow) -> HostResult<Metadata>;

    /// Read bytes from an absolute file path.
    fn read(&self, p: &Path, range: ReadRange) -> HostResult<Vec<u8>>;

    /// Write bytes to an absolute file path.
    fn write(&self, p: &Path, data: &[u8], mode: WriteMode) -> HostResult<()>;

    /// List entries of an absolute directory path (order unspecified; callers sort bytewise).
    fn read_dir(&self, p: &Path) -> HostResult<Vec<DirEntry>>;

    /// Recursively create directory `p` and missing parents.
    fn create_dir_all(&self, p: &Path) -> HostResult<()>;

    /// Remove a regular file or symlink.
    fn remove_file(&self, p: &Path) -> HostResult<()>;

    /// Remove an empty directory.
    fn remove_dir(&self, p: &Path) -> HostResult<()>;

    /// Recursively remove a directory tree without following symlinks.
    fn remove_dir_all(&self, p: &Path) -> HostResult<()>;

    /// Rename/move a filesystem entry.
    fn rename(&self, from: &Path, to: &Path) -> HostResult<()>;

    /// Rename `from` to `to` atomically, but only if `to` does not exist.
    ///
    /// If `to` exists, the operation fails with [`std::io::ErrorKind::AlreadyExists`].
    /// If the host or the filesystem cannot do an atomic no-replace rename, the
    /// operation fails with [`std::io::ErrorKind::Unsupported`]
    /// ([`HostError::unsupported`]). The default implementation always fails with
    /// [`std::io::ErrorKind::Unsupported`].
    fn rename_noreplace(&self, from: &Path, to: &Path) -> HostResult<()> {
        require_absolute(from)?;
        require_absolute(to)?;
        Err(HostError::unsupported(
            "atomic no-replace rename is not supported",
        ))
    }

    /// Get an exclusive advisory lock on the file `path` (`file(LOCK)`).
    ///
    /// The file is created if it does not exist. Its content does not change.
    ///
    /// - `timeout` = `None`: wait until the lock is free.
    /// - `timeout` = `Some(Duration::ZERO)`: try one time and do not wait.
    /// - `timeout` = `Some(d)`: wait at most `d`.
    ///
    /// If the lock is not free before the timeout, the operation fails with
    /// [`std::io::ErrorKind::TimedOut`]. The default implementation always fails with
    /// [`std::io::ErrorKind::Unsupported`].
    fn lock_file(
        &self,
        path: &Path,
        timeout: Option<Duration>,
    ) -> HostResult<Box<dyn FileLockGuard>> {
        require_absolute(path)?;
        let _ = timeout;
        Err(HostError::unsupported("file locks are not supported"))
    }

    /// Copy a file with optional permission and mtime preservation.
    fn copy_file(&self, from: &Path, to: &Path, opts: CopyOptions) -> HostResult<()>;

    /// Create a symbolic link at `link` pointing to `target`.
    fn symlink(&self, target: &Path, link: &Path) -> HostResult<()>;

    /// Create a hard link at `dst` pointing to `src`.
    fn hard_link(&self, src: &Path, dst: &Path) -> HostResult<()>;

    /// Read the raw target of a symbolic link.
    fn read_link(&self, p: &Path) -> HostResult<PathBuf>;

    /// Resolve all symlinks and `.`/`..` components to a canonical absolute path.
    fn canonicalize(&self, p: &Path) -> HostResult<PathBuf>;

    /// Set POSIX permission bits on `p`.
    fn set_permissions(&self, p: &Path, mode: u32) -> HostResult<()>;

    /// Set access and/or modification timestamps on `p`.
    fn set_times(
        &self,
        p: &Path,
        atime: Option<FileTime>,
        mtime: Option<FileTime>,
    ) -> HostResult<()>;

    /// Read an environment variable from the host snapshot + overlay.
    fn env_get(&self, key: &[u8]) -> Option<BString>;

    /// Set (`Some`) or unset (`None`) an environment variable in the host overlay.
    fn env_set(&self, key: &[u8], value: Option<&[u8]>);

    /// Return all active environment variables as sorted `(key, value)` pairs.
    fn env_all(&self) -> Vec<(BString, BString)>;

    /// Run a command or pipeline with explicit cwd, env, stdio, and optional timeout.
    fn run(
        &self,
        req: &RunRequest,
        on_output: &mut dyn FnMut(Stream, &[u8]),
    ) -> HostResult<RunOutput>;

    /// Write bytes directly to stdout or stderr.
    fn write_out(&self, stream: Stream, bytes: &[u8]);

    /// Read up to `limit` bytes from stdin.
    fn read_stdin(&self, limit: u64) -> HostResult<Vec<u8>>;

    /// Return `true` if `stream` is attached to an interactive terminal.
    fn is_terminal(&self, stream: Stream) -> bool;

    /// Current wall-clock timestamp.
    fn now(&self) -> FileTime;

    /// Monotonic elapsed duration since host creation.
    fn monotonic(&self) -> Duration;

    /// Sleep for `duration` (advances virtual clock on [`PureHost`]).
    fn sleep(&self, duration: Duration);

    /// Active local time zone.
    fn time_zone(&self) -> TimeZone;

    /// Absolute path of the currently executing binary.
    fn current_exe(&self) -> HostResult<PathBuf>;

    /// Initial working directory captured at startup.
    fn initial_cwd(&self) -> PathBuf;

    /// Static host OS and hardware identity.
    fn system(&self) -> SystemInfo;
}

fn require_absolute(path: &Path) -> HostResult<()> {
    if path.is_absolute() {
        Ok(())
    } else {
        Err(HostError::not_absolute(path.to_path_buf()))
    }
}

fn validate_run(req: &RunRequest) -> HostResult<()> {
    require_absolute(&req.cwd)?;
    if let Input::File(path) = &req.stdin {
        require_absolute(path)?;
    }
    for output in [&req.stdout, &req.stderr] {
        if let Output::File { path, .. } = output {
            require_absolute(path)?;
        }
    }
    if req.pipeline.is_empty() || req.stdout == Output::SharedWithStdout {
        return Err(error::from_errno(rustix::io::Errno::INVAL, None));
    }
    Ok(())
}
