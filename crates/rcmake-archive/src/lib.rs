//! Shared archive creation, extraction, and listing engine (`tar`, `tar.gz`, `zip`)
//! backed exclusively by [`rcmake_host::Host`], shared across `file(ARCHIVE_*)`,
//! `cmake -E tar`, and `cpack`.

#![forbid(unsafe_code)]

use std::path::{Path, PathBuf};

use rcmake_base::BString;
use rcmake_host::Host;
use thiserror::Error;

/// Archive container and compression format.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ArchiveFormat {
    /// Uncompressed POSIX tar (`pax`/`gnutar`).
    Tar,
    /// Gzip-compressed tar (`.tar.gz` / `.tgz`).
    TarGz,
    /// ZIP archive with deflate compression (`.zip`).
    Zip,
}

/// Options for [`create_archive`].
#[derive(Clone, Debug)]
pub struct CreateOptions {
    /// Archive format.
    pub format: ArchiveFormat,
    /// Optional explicit modification time in Unix seconds (`MTIME`).
    pub mtime: Option<u64>,
    /// Optional compression level (`0..=9`).
    pub compression_level: Option<u32>,
    /// Verbose entry callback (invoked with archive-relative path bytes).
    pub verbose: bool,
}

/// Options for [`extract_archive`].
#[derive(Clone, Debug, Default)]
pub struct ExtractOptions {
    /// Extract into this absolute destination directory.
    pub destination: PathBuf,
    /// Optional list of specific entry patterns/paths to extract.
    pub patterns: Vec<BString>,
    /// Only list entries without writing files (`LIST_ONLY` / `-t`).
    pub list_only: bool,
    /// Do not restore archive mtimes (`TOUCH`).
    pub touch: bool,
    /// Emit verbose entry names.
    pub verbose: bool,
}

/// Error returned by archive operations.
#[derive(Clone, Debug, PartialEq, Eq, Error)]
#[error("{message}")]
pub struct ArchiveError {
    /// Human-readable error message.
    pub message: String,
}

/// Create an archive at absolute path `output` from `inputs` resolved relative to `base_dir`.
pub fn create_archive(
    _host: &dyn Host,
    _output: &Path,
    _base_dir: &Path,
    _inputs: &[PathBuf],
    _options: &CreateOptions,
) -> Result<Vec<BString>, ArchiveError> {
    Err(ArchiveError {
        message: "archive creation not yet implemented (scheduled for M1-W2)".to_owned(),
    })
}

/// Extract or list an archive at absolute path `archive_path`.
pub fn extract_archive(
    _host: &dyn Host,
    _archive_path: &Path,
    _options: &ExtractOptions,
) -> Result<Vec<BString>, ArchiveError> {
    Err(ArchiveError {
        message: "archive extraction not yet implemented (scheduled for M1-W2)".to_owned(),
    })
}
