//! Shared FetchContent storage, Git mirrors, and project lockfiles.

#![forbid(unsafe_code)]
#![deny(missing_docs)]

use std::{
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::Duration,
};

use rcmake_base::BString;
use rcmake_host::{
    CommandSpec, Host, Input, Output, ProcessResult, RunOutput, RunRequest, WriteMode,
    error::HostError, path::from_native,
};

mod archive;
mod git;
mod lock;
mod pool;

pub use archive::{ArchiveCache, ArchiveResult, ContentHash};
pub use git::{GitClone, GitMirror, classify_git_checkout, classify_git_clone};
pub use lock::{GitEntry, LockEntry, LockFile, LockMode, LockSession, UrlEntry};
pub use pool::{FetchPool, PrefetchSource, Ticket};

/// A fetch or lockfile operation error.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum FetchError {
    /// A host operation failed.
    #[error(transparent)]
    Host(#[from] HostError),
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

fn offline(host: &dyn Host) -> bool {
    host.env_get(b"RCMAKE_FETCH_OFFLINE")
        .is_some_and(|value| value == "1")
}

fn run(
    host: &dyn Host,
    program: &'static str,
    args: Vec<BString>,
    cwd: &Path,
    env: Vec<(BString, BString)>,
    limit: usize,
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
            timeout: Some(Duration::from_secs(300)),
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

fn atomic_write(host: &dyn Host, path: &Path, bytes: &[u8]) -> Result<(), FetchError> {
    let staging = staging_path(host, path);
    host.write(&staging, bytes, WriteMode::CreateNew)?;
    if let Err(error) = host.rename(&staging, path) {
        let _ = host.remove_file(&staging);
        return Err(error.into());
    }
    Ok(())
}
