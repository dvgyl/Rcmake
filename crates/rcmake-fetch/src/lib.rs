//! Shared FetchContent storage, Git mirrors, and project lockfiles.

#![forbid(unsafe_code)]
#![deny(missing_docs)]

#[cfg(all(feature = "native", any(unix, windows)))]
use std::{
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
pub use git::GitMirror;
#[cfg(all(feature = "native", any(unix, windows)))]
pub use lock::{GitEntry, LockEntry, LockFile, LockSession, UrlEntry};
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
    /// The fetch needs network access in offline mode.
    #[error("Fetch requires network access. RCMAKE_FETCH_OFFLINE is set.")]
    Offline,
    /// The archive does not have the expected content.
    #[error("Archive hash mismatch. Expected {expected}. Got {actual}.")]
    HashMismatch {
        /// Expected digest.
        expected: String,
        /// Computed digest.
        actual: String,
    },
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
fn run(
    host: &dyn Host,
    program: &'static str,
    args: Vec<BString>,
    cwd: &Path,
    env: Vec<(BString, BString)>,
    limit: usize,
    timeout: Option<Duration>,
) -> Result<RunOutput, FetchError> {
    let result = host.run_bounded(
        &RunRequest {
            pipeline: vec![CommandSpec {
                program: program.into(),
                args,
            }],
            cwd: cwd.to_owned(),
            env,
            stdin: Input::Null,
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
            status: format!("exit code {code}"),
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
fn staging_path(host: &dyn Host, destination: &Path) -> PathBuf {
    static SEQUENCE: AtomicU64 = AtomicU64::new(0);
    let bytes = from_native(destination);
    let digest = rcmake_store::Digest::hash(&bytes);
    let now = host.now();
    let sequence = SEQUENCE.fetch_add(1, Ordering::Relaxed);
    destination.with_file_name(format!(
        ".rcmake-{digest}-{}-{}-{sequence}.tmp",
        now.secs, now.nanos
    ))
}

#[cfg(all(feature = "native", any(unix, windows)))]
fn atomic_write(host: &dyn Host, path: &Path, bytes: &[u8]) -> Result<(), FetchError> {
    let staging = staging_path(host, path);
    host.write(&staging, bytes, WriteMode::CreateNew)?;
    if let Err(error) = host.rename(&staging, path) {
        let _ = host.remove_file(&staging);
        return Err(error.into());
    }
    Ok(())
}
