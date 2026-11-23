//! Shared archive creation, extraction, and listing engine (`tar`, `tar.gz`, `zip`)
//! backed exclusively by [`rcmake_host::Host`], shared across `file(ARCHIVE_*)`,
//! `cmake -E tar`, and `cpack`.

#![forbid(unsafe_code)]

use std::fmt::Display;
use std::io::Read;
use std::path::{Component, Path, PathBuf};

use rcmake_base::{BString, Limits};
use rcmake_host::path::{from_native, to_native};
use rcmake_host::{Host, HostError, ReadRange, Stream};
use thiserror::Error;

mod create;
mod extract;
mod pattern;
mod timestamp;

#[cfg(test)]
mod tests;

const MAX_DEPTH: usize = 256;
const MAX_ENTRIES: usize = 100_000;
const MAX_NAME_BYTES: usize = u16::MAX as usize;

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
    /// Write archive entry names to the host standard output stream.
    pub verbose: bool,
}

/// Options for [`extract_archive`].
#[derive(Clone, Debug, Default)]
pub struct ExtractOptions {
    /// Extract into this absolute destination directory.
    pub destination: PathBuf,
    /// Entry paths or byte patterns with `*`, `?`, and bracket expressions.
    ///
    /// A directory match also selects its descendants unless the pattern ends with `$`.
    pub patterns: Vec<BString>,
    /// Only list entries without writing files (`LIST_ONLY` / `-t`).
    pub list_only: bool,
    /// Do not restore archive mtimes (`TOUCH`).
    pub touch: bool,
    /// Write selected entry names to the host standard output stream.
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
///
/// The returned names use slash separators. Directory names end with a slash.
/// ZIP creation requires UTF-8 names and preserves the nine access permission bits.
/// ZIP timestamps have a resolution of 100 nanoseconds.
///
/// # Errors
///
/// Returns an error for invalid options, unsupported entries, size limits, or host errors.
pub fn create_archive(
    host: &dyn Host,
    output: &Path,
    base_dir: &Path,
    inputs: &[PathBuf],
    options: &CreateOptions,
) -> Result<Vec<BString>, ArchiveError> {
    create::run(host, output, base_dir, inputs, options)
}

/// Extract or list an archive at absolute path `archive_path`.
///
/// Symbolic links preserve their targets. Extraction rejects paths that pass
/// through symbolic links inside the destination and skips tar device and FIFO entries.
///
/// # Errors
///
/// Returns an error for an invalid archive, unsafe paths, unmatched patterns,
/// unsupported formats or entries, size limits, or host errors.
pub fn extract_archive(
    host: &dyn Host,
    archive_path: &Path,
    options: &ExtractOptions,
) -> Result<Vec<BString>, ArchiveError> {
    extract::run(host, archive_path, options)
}

impl ArchiveError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl From<HostError> for ArchiveError {
    fn from(error: HostError) -> Self {
        Self::new(match error.path {
            Some(path) => format!("{}: {}", path.display(), error.os_message),
            None => error.os_message,
        })
    }
}

fn archive_error(error: impl Display) -> ArchiveError {
    ArchiveError::new(error.to_string())
}

fn absolute(path: &Path) -> Result<(), ArchiveError> {
    if path.is_absolute() {
        Ok(())
    } else {
        Err(HostError::not_absolute(path.to_owned()).into())
    }
}

fn rooted(path: &[u8]) -> bool {
    path.starts_with(b"/")
        || path.starts_with(b"\\")
        || matches!(path, [drive, b':', ..] if drive.is_ascii_alphabetic())
}

fn relative_path(name: &[u8]) -> Result<PathBuf, ArchiveError> {
    let path = to_native(name);
    if name.is_empty()
        || name.len() > MAX_NAME_BYTES
        || name.contains(&0)
        || rooted(name)
        || name
            .split(|&byte| byte == b'/' || byte == b'\\')
            .any(|part| part == b"..")
        || path.components().count() > MAX_DEPTH
        || path.components().any(|component| match component {
            Component::CurDir => false,
            Component::Normal(part) => cfg!(windows) && part.as_encoded_bytes().contains(&b':'),
            _ => true,
        })
    {
        return Err(ArchiveError::new(format!(
            "Invalid archive path: {:?}",
            BString::from(name)
        )));
    }
    Ok(path
        .components()
        .filter(|part| matches!(part, Component::Normal(_)))
        .collect())
}

fn read_bytes(host: &dyn Host, path: &Path, limit: u64) -> Result<Vec<u8>, ArchiveError> {
    Ok(host.read(path, ReadRange::All { limit })?)
}

fn read_bounded(reader: impl Read, limit: u64) -> Result<Vec<u8>, ArchiveError> {
    let mut data = Vec::new();
    reader
        .take(limit.saturating_add(1))
        .read_to_end(&mut data)
        .map_err(archive_error)?;
    if data.len() as u64 > limit {
        return Err(ArchiveError::new("Archive exceeds the size limit"));
    }
    Ok(data)
}

fn size_limit() -> u64 {
    Limits::default().max_file_read
}

fn emit_names(host: &dyn Host, names: &[BString], verbose: bool) {
    if verbose {
        for name in names {
            host.write_out(Stream::Stdout, name);
            host.write_out(Stream::Stdout, b"\n");
        }
    }
}

fn archive_name(path: &Path) -> BString {
    let mut name = Vec::new();
    for part in path.components() {
        if !name.is_empty() {
            name.push(b'/');
        }
        name.extend_from_slice(&from_native(Path::new(part.as_os_str())));
    }
    BString::from(name)
}
