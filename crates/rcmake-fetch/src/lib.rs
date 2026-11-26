//! Shared FetchContent storage, Git mirrors, and project lockfiles.

#![forbid(unsafe_code)]
#![deny(missing_docs)]

#[cfg(all(feature = "native", any(unix, windows)))]
use std::{
    io::ErrorKind,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::Duration,
};

#[cfg(all(feature = "native", any(unix, windows)))]
use rcmake_base::BString;
#[cfg(all(feature = "native", any(unix, windows)))]
use rcmake_host::{
    CommandSpec, Host, Input, Output, ProcessResult, RunOutput, RunRequest, WriteMode,
    path::from_native,
};

#[cfg(all(feature = "native", any(unix, windows)))]
mod archive;
#[cfg(all(feature = "native", any(unix, windows)))]
mod error;
#[cfg(all(feature = "native", any(unix, windows)))]
mod git;
#[cfg(all(feature = "native", any(unix, windows)))]
mod lock;
#[cfg(all(feature = "native", any(unix, windows)))]
mod pool;

#[cfg(all(feature = "native", any(unix, windows)))]
pub use archive::{ArchiveCache, ArchiveResult, ContentHash};
mod clone_args;
pub use clone_args::{GitClone, classify_git_checkout, classify_git_clone};
#[cfg(all(feature = "native", any(unix, windows)))]
pub use error::ArchiveContext;
#[cfg(all(feature = "native", any(unix, windows)))]
pub use git::{GitGcReport, GitMirror};
#[cfg(all(feature = "native", any(unix, windows)))]
pub use lock::{CommitReport, GitEntry, LockEntry, LockFile, LockSession, UrlEntry};
#[cfg(all(feature = "native", any(unix, windows)))]
pub use pool::{FetchPool, PrefetchSource, Ticket};

/// Controls when a project reads, writes, or enforces lock entries.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum LockMode {
    /// Do not read or write the lockfile.
    Off,
    /// Read an optional lockfile and report changes in unpinned archive content.
    #[default]
    Auto,
    /// Record the content used by the current configure and write it on commit.
    Update,
    /// Require matching entries and enforce the locked content.
    Frozen,
}

impl LockMode {
    /// Read the lock mode from `RCMAKE_FETCH_LOCK`. The default is auto.
    pub fn from_host(host: &dyn rcmake_host::Host) -> Result<Self, FetchError> {
        let value = host
            .env_get(b"RCMAKE_FETCH_LOCK")
            .unwrap_or_else(|| "auto".into());
        match value.as_slice() {
            b"off" => Ok(Self::Off),
            b"auto" => Ok(Self::Auto),
            b"update" => Ok(Self::Update),
            b"frozen" => Ok(Self::Frozen),
            _ => Err(invalid("Invalid RCMAKE_FETCH_LOCK.")),
        }
    }
}

/// A fetch or lockfile operation error.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum FetchError {
    /// A host operation failed.
    #[error(transparent)]
    Host(#[from] rcmake_host::error::HostError),
    /// The transport needs network access, which the caller has disabled.
    #[error("Fetch requires network access in cache-only mode.")]
    Offline,
    /// The archive does not have the expected content.
    #[error("Archive hash mismatch for {algorithm}. Expected {expected}. Got {actual}.")]
    HashMismatch {
        /// Digest algorithm.
        algorithm: String,
        /// Expected digest in hexadecimal form.
        expected: String,
        /// Computed digest in hexadecimal form.
        actual: String,
    },
    /// Offline policy prevented a cache miss from using the network.
    #[cfg(all(feature = "native", any(unix, windows)))]
    #[error("{policy} {reason}")]
    ArchiveUnavailable {
        /// Setting that disabled network access.
        policy: &'static str,
        /// Reason the cache could not supply verified content.
        reason: &'static str,
    },
    /// An archive operation failed.
    #[cfg(all(feature = "native", any(unix, windows)))]
    #[error("{context} {cause}")]
    Archive {
        /// Archive identity and recovery information.
        context: Box<ArchiveContext>,
        /// Checksum, cache, or transport error.
        #[source]
        cause: Box<Self>,
    },
    /// A cache object could not be read or verified.
    #[cfg(all(feature = "native", any(unix, windows)))]
    #[error("Cannot use cache path {path:?}. {cause} Check the path or reseed the pin online.")]
    CacheRead {
        /// Path of the object or directory that caused the failure.
        path: PathBuf,
        /// Host or verification error.
        #[source]
        cause: rcmake_host::HostError,
    },
    /// Neither the archive cache nor the transport supplied verified bytes.
    #[cfg(all(feature = "native", any(unix, windows)))]
    #[error("{download} The cache lookup also failed ({cache}).")]
    CacheAndDownload {
        /// Cache lookup error.
        cache: Box<Self>,
        /// Transport or verification error.
        #[source]
        download: Box<Self>,
    },
    /// The fetch lockfile is missing, invalid, or disagrees with a declaration.
    #[cfg(all(feature = "native", any(unix, windows)))]
    #[error(
        "Fetch lockfile {path:?}, dependency {dependency:?}. {detail} Restore the lockfile or run --fetch-lock=update if the change is intentional."
    )]
    Lock {
        /// Lockfile path.
        path: PathBuf,
        /// Dependency name, if the problem is specific to one entry.
        dependency: Option<String>,
        /// Reason for the rejection.
        detail: String,
    },
    /// The host could not read the lockfile.
    #[cfg(all(feature = "native", any(unix, windows)))]
    #[error("Cannot read fetch lockfile {path:?}. {cause} Check the path and read permissions.")]
    LockRead {
        /// Lockfile path.
        path: PathBuf,
        /// Host error.
        #[source]
        cause: rcmake_host::HostError,
    },
    /// The host could not publish the lockfile.
    #[cfg(all(feature = "native", any(unix, windows)))]
    #[error("Cannot write fetch lockfile {path:?}. {cause}")]
    LockWrite {
        /// Lockfile path.
        path: PathBuf,
        /// Host or serialization error.
        #[source]
        cause: Box<Self>,
    },
    /// The store could not confirm that the archive is ready for offline use.
    #[cfg(all(feature = "native", any(unix, windows)))]
    #[error(
        "Archive publication returned {outcome:?}. Check the cache mode and path, then seed the pin again."
    )]
    Publication {
        /// Store publication result.
        outcome: rcmake_store::PublishOutcome,
    },
    /// A concurrent configure committed a different pin for the same dependency.
    #[cfg(all(feature = "native", any(unix, windows)))]
    #[error(
        "Concurrent update for {dependency:?} in {path:?}. Review the committed pin and run --fetch-lock=update again."
    )]
    LockConflict {
        /// Dependency name.
        dependency: String,
        /// Lockfile path.
        path: PathBuf,
    },
    /// A Git mirror operation failed.
    #[cfg(all(feature = "native", any(unix, windows)))]
    #[error("Git repository {repository:?}, commit {commit:?}, mirror {path:?}. {cause}")]
    Git {
        /// Repository with credentials removed.
        repository: String,
        /// Requested full commit, if pinned.
        commit: Option<String>,
        /// Local bare mirror path.
        path: PathBuf,
        /// Cache, transport, or checkout error.
        #[source]
        cause: Box<Self>,
    },
    /// A read-only cache mount needs explicit immutable snapshot mode.
    #[cfg(all(feature = "native", any(unix, windows)))]
    #[error(
        "Cannot lock the read-only Git mirror. {cause} Use RCMAKE_FETCH_GIT_READ_ONLY=1 in offline mode for an immutable cache snapshot."
    )]
    ReadOnlyGitCache {
        /// Host lock error.
        #[source]
        cause: rcmake_host::HostError,
    },
    /// Local repository metadata does not describe a valid bare mirror.
    #[cfg(all(feature = "native", any(unix, windows)))]
    #[error("The Git mirror is damaged. Check the cache path and repair it with an online fetch.")]
    DamagedGitMirror,
    /// The source, lockfile, or fetch configuration is invalid.
    #[error("{0}")]
    Invalid(String),
    /// A tool exited unsuccessfully.
    #[error("{program} failed with {status}.")]
    Process {
        /// Tool name.
        program: &'static str,
        /// Process status.
        status: String,
    },
}

fn invalid(message: impl Into<String>) -> FetchError {
    FetchError::Invalid(message.into())
}

#[cfg(all(feature = "native", any(unix, windows)))]
fn offline(host: &dyn Host) -> bool {
    host.env_get(b"RCMAKE_FETCH_OFFLINE")
        .is_some_and(|value| value == "1")
}

#[cfg(all(feature = "native", any(unix, windows)))]
#[allow(clippy::too_many_arguments)]
fn run(
    host: &dyn Host,
    program: &'static str,
    args: Vec<BString>,
    cwd: &Path,
    env: Vec<(BString, BString)>,
    input: Input,
    limit: usize,
    timeout: Option<Duration>,
) -> Result<RunOutput, FetchError> {
    run_command(
        host,
        program,
        CommandSpec {
            program: program.into(),
            args,
        },
        cwd,
        env,
        input,
        limit,
        timeout,
    )
}

#[cfg(all(feature = "native", any(unix, windows)))]
#[allow(clippy::too_many_arguments)]
fn run_command(
    host: &dyn Host,
    program: &'static str,
    command: CommandSpec,
    cwd: &Path,
    env: Vec<(BString, BString)>,
    input: Input,
    limit: usize,
    timeout: Option<Duration>,
) -> Result<RunOutput, FetchError> {
    let result = host.run_bounded(
        &RunRequest {
            pipeline: vec![command],
            cwd: cwd.to_owned(),
            env,
            stdin: input,
            stdout: Output::Capture,
            stderr: Output::Capture,
            timeout,
            tee: false,
        },
        limit,
    )?;
    match result.results.as_slice() {
        [ProcessResult::Exited(0)] => Ok(result),
        [ProcessResult::SpawnFailed(error)] => Err(error.clone().into()),
        [ProcessResult::Exited(code)] => Err(FetchError::Process {
            program,
            status: process_status(program, *code, &result.stderr),
        }),
        [ProcessResult::Signaled(signal)] => Err(FetchError::Process {
            program,
            status: format!("signal {signal}"),
        }),
        [ProcessResult::TimedOut] => Err(FetchError::Process {
            program,
            status: "timeout".into(),
        }),
        _ => Err(invalid("Unexpected process result.")),
    }
}

#[cfg(all(feature = "native", any(unix, windows)))]
fn process_status(program: &str, code: i32, stderr: &[u8]) -> String {
    // Match known messages without copying process output into the error.
    let prefix = stderr.get(..4096).unwrap_or(stderr);
    let text = String::from_utf8_lossy(prefix).to_ascii_lowercase();
    let detail = if program == "git" {
        if text.contains("not a git repository")
            || text.contains("bad config line")
            || text.contains("invalid gitfile format")
        {
            " (invalid repository metadata)"
        } else if text.contains("authentication failed")
            || text.contains("permission denied (publickey")
            || text.contains("could not read username")
            || text.contains("access denied")
        {
            " (authentication failed)"
        } else if text.contains("could not resolve")
            || text.contains("failed to connect")
            || text.contains("connection timed out")
            || text.contains("unable to access")
        {
            " (connection failed)"
        } else {
            ""
        }
    } else if program == "curl" && matches!(code, 5 | 6 | 7 | 28) {
        " (connection failed)"
    } else {
        ""
    };
    format!("exit code {code}{detail}")
}

#[cfg(all(feature = "native", any(unix, windows)))]
fn staging_path(host: &dyn Host, destination: &Path) -> PathBuf {
    static SEQUENCE: AtomicU64 = AtomicU64::new(0);
    let bytes = from_native(destination);
    let digest = rcmake_store::Digest::hash(&bytes);
    let now = host.now();
    let sequence = SEQUENCE.fetch_add(1, Ordering::Relaxed);
    destination.with_file_name(format!(
        ".rcmake-{digest}-{}-{}-{}-{sequence}.tmp",
        host.system().pid,
        now.secs,
        now.nanos
    ))
}

#[cfg(all(feature = "native", any(unix, windows)))]
/// Publish a fetch file through a staging file in the destination directory.
///
/// # Errors
///
/// Return the host error if the staging write or rename fails.
pub fn atomic_write(host: &dyn Host, path: &Path, bytes: &[u8]) -> Result<(), FetchError> {
    atomic_write_with(host, path, |staging| {
        host.write(staging, bytes, WriteMode::CreateNew)
    })
}

#[cfg(all(feature = "native", any(unix, windows)))]
fn atomic_write_with(
    host: &dyn Host,
    path: &Path,
    write: impl FnOnce(&Path) -> rcmake_host::HostResult<()>,
) -> Result<(), FetchError> {
    let staging = staging_path(host, path);
    if let Err(error) = write(&staging) {
        if error.kind != ErrorKind::AlreadyExists {
            let _ = host.remove_file(&staging);
        }
        return Err(error.into());
    }
    if let Err(error) = host.rename(&staging, path) {
        let _ = host.remove_file(&staging);
        return Err(error.into());
    }
    Ok(())
}

#[cfg(all(test, feature = "native", any(unix, windows)))]
mod tests {
    use super::*;
    use rcmake_host::{HostError, PureHost, ReadRange};

    #[test]
    fn partial_staging_write_keeps_the_previous_destination() -> Result<(), HostError> {
        let host = PureHost::new("/work".into(), "/tools/rcmake".into());
        let path = Path::new("/work/archive.tar");
        host.seed_file(path, b"previous archive")?;
        let result = atomic_write_with(&host, path, |stage| {
            assert_eq!(stage.parent(), path.parent());
            host.write(stage, b"partial", WriteMode::CreateNew)?;
            Err(HostError::from_io(
                ErrorKind::StorageFull.into(),
                Some(stage.to_owned()),
            ))
        });
        assert!(
            matches!(result, Err(FetchError::Host(error)) if error.kind == ErrorKind::StorageFull)
        );
        assert_eq!(
            host.read(path, ReadRange::All { limit: 100 })?,
            b"previous archive"
        );
        assert_eq!(host.read_dir(Path::new("/work"))?.len(), 1);
        Ok(())
    }
}
