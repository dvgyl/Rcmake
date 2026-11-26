//! The [`Host`] trait provides a common interface for system operations.
//! [`PureHost`] works in memory. `NativeHost` uses the operating system on Unix and Windows.
//! Both use the shared [`conformance`] suite.
//!
//! All engine crates interact with the filesystem, environment, child processes, stdio,
//! clock, and system identity exclusively through [`Host`] (mechanically enforced via
//! `clippy.toml` `disallowed-methods`).

#![forbid(unsafe_code)]

mod cmake_root;
pub mod conformance;
pub mod error;
mod forward;
pub mod memo;
#[cfg(any(unix, windows))]
pub mod native;
pub mod no_process;
pub mod overlay;
pub mod path;
pub mod pure;
pub mod timestamp;

use core::time::Duration;
use std::path::{Path, PathBuf};

pub use jiff::tz::TimeZone;
#[doc(hidden)]
pub use rcmake_base::BString;

pub use cmake_root::discover_cmake_root;
pub use error::{HostError, HostResult};
pub use memo::{MemoHost, MemoStats};
#[cfg(any(unix, windows))]
pub use native::NativeHost;
mod stream;
pub use stream::{ReadSeek, ScratchFile};
mod registry;
pub use no_process::{DeniedCommand, DeniedLog, DeniedMethod, NoProcessHost};
pub use overlay::{OverlayAudit, OverlayHost, OverlayPathPolicy, OverlayPathStyle};
pub use pure::PureHost;
pub use registry::{RegistryRoot, RegistryView};

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

/// Absolute paths that a process can write, as certified by its caller.
/// An empty scope requests full invalidation.
#[derive(Clone, Copy)]
pub struct WriteScope<'a>(pub &'a [&'a Path]);

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
    /// Time of the last inode or content change, if the host can establish it.
    pub ctime: Option<FileTime>,
    /// Access timestamp.
    pub atime: FileTime,
    /// Device ID (for symlink cycle detection in glob walks).
    pub dev: u64,
    /// Inode number (for symlink cycle detection in glob walks).
    pub ino: u64,
    /// Hard-link count.
    pub nlink: u64,
    /// User ID of the owner, or `None` if the host has no POSIX user IDs.
    pub owner: Option<u32>,
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

/// Signals that a test can receive when it reaches its time limit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TimeoutSignal {
    /// SIGINT.
    Interrupt,
    /// SIGQUIT.
    Quit,
    /// SIGTERM.
    Terminate,
    /// SIGUSR1.
    User1,
    /// SIGUSR2.
    User2,
}

/// A signal to send before forcibly terminating a timed-out process.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GracefulTimeout {
    /// Signal to send when the time limit expires.
    pub signal: TimeoutSignal,
    /// Additional time before forceful termination.
    pub grace: Duration,
}

/// Return a replacement timeout measured from receipt of an output chunk.
pub type DeadlineUpdate<'a> = dyn FnMut(Stream, &[u8]) -> Option<Duration> + 'a;

/// Output from a process with an adjustable deadline.
#[derive(Debug, Default)]
pub struct ControlledOutput {
    /// Process status and captured output.
    pub output: RunOutput,
    /// Signal sent on timeout if the child completed during the grace period.
    pub timeout_signal: Option<TimeoutSignal>,
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
    /// Total physical memory in bytes. Zero means unknown.
    pub total_memory: u64,
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

const FILE_CHUNK_SIZE: usize = 256 * 1024;

/// Core OS abstraction trait for Rcmake (ADR-004).
///
/// Every filesystem path argument MUST be absolute; relative paths return
/// [`HostError::not_absolute`]. All methods take `&self` so hosts can be shared via `Arc`.
pub trait Host: Send + Sync {
    /// Query filesystem metadata (following or not following symlinks).
    fn metadata(&self, p: &Path, follow: Follow) -> HostResult<Metadata>;

    /// Read metadata like [`Host::metadata`], but never from a cache.
    ///
    /// Security checks use this method, so that an earlier result cannot hide a later
    /// change. A host that caches metadata must override it. A wrapper host must forward it.
    fn metadata_fresh(&self, p: &Path, follow: Follow) -> HostResult<Metadata> {
        self.metadata(p, follow)
    }

    /// Read bytes from an absolute file path.
    fn read(&self, p: &Path, range: ReadRange) -> HostResult<Vec<u8>>;

    /// Read a file and preserve the caller's path when an overlay resolves it.
    fn read_with_origin(&self, p: &Path, range: ReadRange, _origin: &Path) -> HostResult<Vec<u8>> {
        self.read(p, range)
    }

    /// Read a file in order and pass each chunk to `consume`.
    fn read_chunks(&self, p: &Path, consume: &mut dyn FnMut(&[u8])) -> HostResult<()> {
        let mut offset = 0;
        loop {
            let bytes = self.read(
                p,
                ReadRange::Slice {
                    offset,
                    len: Some(FILE_CHUNK_SIZE as u64),
                },
            )?;
            if !bytes.is_empty() {
                consume(&bytes);
            }
            if bytes.len() < FILE_CHUNK_SIZE {
                return Ok(());
            }
            offset = offset
                .checked_add(FILE_CHUNK_SIZE as u64)
                .ok_or_else(|| error::SyntheticError::FileTooLarge.at(Some(p)))?;
        }
    }

    /// Write bytes to an absolute file path.
    fn write(&self, p: &Path, data: &[u8], mode: WriteMode) -> HostResult<()>;

    /// List entries of an absolute directory path (order unspecified; callers sort bytewise).
    fn read_dir(&self, p: &Path) -> HostResult<Vec<DirEntry>>;

    /// Recursively create directory `p` and missing parents.
    fn create_dir_all(&self, p: &Path) -> HostResult<()>;

    /// Create the one directory `p` so that only the current user can use it.
    ///
    /// On POSIX hosts the directory has mode `0o700` or less. The call fails with
    /// [`ErrorKind::AlreadyExists`](std::io::ErrorKind::AlreadyExists) if `p` exists.
    ///
    /// The default implementation creates the directory with [`Host::create_dir_all`] and then
    /// sets mode `0o700` with [`Host::set_permissions`]. A host that can create the directory
    /// with the final mode in one step must override this method. A wrapper host must forward it.
    fn create_private_dir(&self, p: &Path) -> HostResult<()> {
        match self.metadata(p, Follow::No) {
            Ok(_) => return Err(error::SyntheticError::AlreadyExists.at(Some(p))),
            Err(error) if error.kind == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        if let Some(parent) = p.parent() {
            self.metadata(parent, Follow::Yes)?;
        }
        self.create_dir_all(p)?;
        self.set_permissions(p, 0o700)
    }

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
        HostError::require_absolute(from)?;
        HostError::require_absolute(to)?;
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
        HostError::require_absolute(path)?;
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

    /// Resolve a path like [`Host::canonicalize`], but never from a cache.
    ///
    /// Security checks use this method. A host that caches results must override it. A
    /// wrapper host must forward it.
    fn canonicalize_fresh(&self, p: &Path) -> HostResult<PathBuf> {
        self.canonicalize(p)
    }

    /// Set POSIX permission bits on `p`.
    fn set_permissions(&self, p: &Path, mode: u32) -> HostResult<()>;

    /// Set access and/or modification timestamps on `p`.
    fn set_times(
        &self,
        p: &Path,
        atime: Option<FileTime>,
        mtime: Option<FileTime>,
    ) -> HostResult<()>;

    /// Set access and modification times on the entry at `path`.
    ///
    /// Set the times on the link itself if the final path component is a symbolic link.
    /// Each `None` value preserves the corresponding timestamp.
    /// The default returns [`std::io::ErrorKind::Unsupported`] for absolute paths.
    fn set_symlink_times(
        &self,
        path: &Path,
        _atime: Option<FileTime>,
        _mtime: Option<FileTime>,
    ) -> HostResult<()> {
        HostError::require_absolute(path)?;
        Err(HostError::unsupported(
            "This host cannot set symbolic link timestamps",
        ))
    }

    /// Read an environment variable from the host snapshot + overlay.
    fn env_get(&self, key: &[u8]) -> Option<BString>;

    /// Set (`Some`) or unset (`None`) an environment variable in the host overlay.
    fn env_set(&self, key: &[u8], value: Option<&[u8]>);

    /// Return all active environment variables as sorted `(key, value)` pairs.
    fn env_all(&self) -> Vec<(BString, BString)>;

    /// Return `false` if the host always rejects process requests.
    ///
    /// This query does not run a command. The default returns `true`.
    fn supports_process_execution(&self) -> bool {
        true
    }

    /// Open a regular file for bounded streaming reads.
    fn open_read(&self, path: &Path) -> HostResult<Box<dyn ReadSeek + '_>> {
        stream::open_read(self, path)
    }

    /// Create an anonymous temporary file for internal data.
    fn scratch_file(&self) -> HostResult<Box<dyn ScratchFile + '_>> {
        Ok(stream::scratch())
    }

    /// Create an anonymous temporary file on the filesystem that contains `dir`.
    /// In-memory hosts can use their default scratch storage.
    fn scratch_file_in(&self, _dir: &Path) -> HostResult<Box<dyn ScratchFile + '_>> {
        self.scratch_file()
    }

    /// Write exactly `len` bytes from a stream.
    fn write_from(
        &self,
        path: &Path,
        mode: WriteMode,
        input: &mut dyn std::io::Read,
        len: u64,
    ) -> HostResult<()> {
        stream::write_from(self, path, mode, input, len)
    }

    /// Return the native Windows registry view, if available.
    fn registry_host_view(&self) -> Option<RegistryView> {
        None
    }

    /// Read a Windows registry value. An absent or unsupported value is a miss.
    fn registry_value(
        &self,
        _root: RegistryRoot,
        _subkey: &[u8],
        _name: Option<&[u8]>,
        _view: RegistryView,
    ) -> HostResult<Option<BString>> {
        Ok(None)
    }

    /// Run a command or pipeline with explicit cwd, env, stdio, and optional timeout.
    fn run(
        &self,
        req: &RunRequest,
        on_output: &mut dyn FnMut(Stream, &[u8]),
    ) -> HostResult<RunOutput>;

    /// Run a process with a caller-certified write scope.
    fn run_confined(
        &self,
        req: &RunRequest,
        _scope: WriteScope<'_>,
        on_output: &mut dyn FnMut(Stream, &[u8]),
    ) -> HostResult<RunOutput> {
        self.run(req, on_output)
    }

    /// Run a process with a caller-certified write scope and output limit.
    fn run_bounded_confined(
        &self,
        req: &RunRequest,
        limit: usize,
        _scope: WriteScope<'_>,
    ) -> HostResult<RunOutput> {
        self.run_bounded(req, limit)
    }

    /// Run a process and allow it time to handle its timeout signal.
    fn run_with_timeout_signal(
        &self,
        req: &RunRequest,
        timeout: GracefulTimeout,
        on_output: &mut dyn FnMut(Stream, &[u8]),
    ) -> HostResult<RunOutput> {
        let _ = timeout;
        self.run(req, on_output)
    }

    /// Run a process with a deadline that can change when captured output arrives.
    ///
    /// The callback can replace the relative timeout. The separate hard limit,
    /// measured from launch, still applies. An update does not change signal grace.
    fn run_controlled(
        &self,
        req: &RunRequest,
        graceful: Option<GracefulTimeout>,
        hard_limit: Option<Duration>,
        update: &mut DeadlineUpdate<'_>,
    ) -> HostResult<ControlledOutput> {
        let mut request = req.clone();
        request.timeout = match (request.timeout, hard_limit) {
            (Some(base), Some(hard)) => Some(base.min(hard)),
            (base, hard) => base.or(hard),
        };
        request.tee = true;
        let mut observe = |stream, bytes: &[u8]| {
            let _ = update(stream, bytes);
        };
        let output = match graceful {
            Some(timeout) => self.run_with_timeout_signal(&request, timeout, &mut observe),
            None => self.run(&request, &mut observe),
        }?;
        Ok(ControlledOutput {
            output,
            timeout_signal: None,
        })
    }

    /// Run a process with a byte limit for all captured output.
    fn run_bounded(&self, req: &RunRequest, limit: usize) -> HostResult<RunOutput> {
        let output = self.run(req, &mut |_, _| {})?;
        if output.stdout.len().saturating_add(output.stderr.len()) > limit {
            return Err(HostError::unsupported(
                "Process output exceeds the size limit",
            ));
        }
        Ok(output)
    }

    /// Write bytes directly to stdout or stderr.
    fn write_out(&self, stream: Stream, bytes: &[u8]);

    /// Read up to `limit` bytes from stdin.
    fn read_stdin(&self, limit: u64) -> HostResult<Vec<u8>>;

    /// Return `true` if `stream` is attached to an interactive terminal.
    fn is_terminal(&self, stream: Stream) -> bool;

    /// Return the column width of `stream` when attached to a terminal.
    fn terminal_width(&self, _stream: Stream) -> Option<usize> {
        None
    }

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

    /// Effective user ID of this process, or `None` if the host has no POSIX user IDs.
    ///
    /// A wrapper host must forward this method. With the default `None`, a POSIX host
    /// refuses private cache directories, because their owners cannot be checked.
    fn effective_uid(&self) -> Option<u32> {
        None
    }
}

fn validate_run(req: &RunRequest, require_absolute: fn(&Path) -> HostResult<()>) -> HostResult<()> {
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
        return Err(error::SyntheticError::InvalidInput.at(None));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn process_capability_reports_the_host_policy_without_logging_a_denial() {
        assert!(PureHost::default().supports_process_execution());
        assert!(MemoHost::new(PureHost::default()).supports_process_execution());
        assert!(!OverlayHost::new(PureHost::default()).supports_process_execution());

        let disabled = std::sync::Arc::new(NoProcessHost::new(PureHost::default()));
        assert!(!disabled.supports_process_execution());
        let memo = MemoHost::from_shared(std::sync::Arc::clone(&disabled));
        assert!(!memo.supports_process_execution());
        assert!(disabled.denied_commands().is_empty());
    }
}
