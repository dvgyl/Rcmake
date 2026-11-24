//! Shared archive creation, extraction, and listing engine for tar and ZIP archives
//! backed exclusively by [`rcmake_host::Host`], shared across `file(ARCHIVE_*)`,
//! `cmake -E tar`, and `cpack`.

#![forbid(unsafe_code)]

use std::fmt::Display;
use std::io::{self, Read};
use std::path::{Component, Path, PathBuf};

use rcmake_base::{BString, Limits};
use rcmake_host::path::{from_native, to_native};
use rcmake_host::{FileTime, Host, HostError, ReadRange, Stream};
use thiserror::Error;

mod codec;
mod compress;
mod create;
mod external_filter;
mod extract;
mod pattern;
mod seven_zip;
mod tar_budget;
mod timestamp;
mod zip_codec;

pub use timestamp::parse_archive_mtime;

#[cfg(test)]
mod tests;

const MAX_DEPTH: usize = 256;
const MAX_ENTRIES: usize = 100_000;
const MAX_NAME_BYTES: usize = u16::MAX as usize;

/// Archive container and compression format.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ArchiveFormat {
    /// Uncompressed POSIX tar (`pax` or `paxr`).
    Tar,
    /// Gzip-compressed tar (`.tar.gz` / `.tgz`).
    TarGz,
    /// Bzip2-compressed tar (`.tar.bz2` / `.tbz2`).
    TarBz2,
    /// XZ-compressed tar (`.tar.xz` / `.txz`).
    TarXz,
    /// LZMA-alone compressed tar (`.tar.lzma`).
    TarLzma,
    /// Zstandard-compressed tar (`.tar.zst`).
    TarZstd,
    /// Unix compress tar (`.tar.Z`).
    TarZ,
    /// Uncompressed GNU tar.
    GnuTar,
    /// Gzip-compressed GNU tar.
    GnuTarGz,
    /// GNU tar with the specified compression filter.
    GnuTarCompressed(TarCompression),
    /// 7-Zip archive with the selected compression method and thread count.
    SevenZip {
        /// Compression method for file entries.
        compression: SevenZipCompression,
        /// Requested number of codec threads. Zero selects the default.
        threads: u32,
    },
    /// ZIP archive with deflate compression (`.zip`).
    Zip,
    /// ZIP archive with stored entries.
    ZipStored,
    /// ZIP archive with a codec supplied by the archive engine.
    ZipCompressed(ZipCompression),
}

/// Compression method for 7-Zip file entries.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SevenZipCompression {
    /// Store file data without compression.
    Store,
    /// LZMA.
    Lzma,
    /// LZMA2.
    Lzma2,
    /// Deflate.
    Deflate,
    /// Bzip2.
    Bzip2,
    /// PPMd.
    Ppmd,
    /// Zstandard.
    Zstd,
}

/// Compression method for ZIP file entries.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ZipCompression {
    /// Bzip2.
    Bzip2,
    /// LZMA.
    Lzma,
    /// XZ.
    Xz,
    /// PPMd, which ZIP creation rejects.
    Ppmd,
    /// Zstandard.
    Zstd,
}

/// Compression filter for a tar archive.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TarCompression {
    /// Gzip.
    Gzip,
    /// Bzip2.
    Bzip2,
    /// XZ.
    Xz,
    /// LZMA-alone.
    Lzma,
    /// Zstandard.
    Zstd,
    /// Unix compress (LZW).
    Compress,
}

/// Options for [`create_archive`].
#[derive(Clone, Debug)]
pub struct CreateOptions {
    /// Archive format.
    pub format: ArchiveFormat,
    /// Optional explicit modification time in Unix seconds (`MTIME`).
    pub mtime: Option<u64>,
    /// Optional compression level (`0..=9`, or `0..=19` for Zstandard in tar and 7-Zip).
    /// ZIP deflate, Bzip2, and Zstandard use their default levels for zero.
    pub compression_level: Option<u32>,
    /// Write archive entry names to the host standard output stream.
    pub verbose: bool,
}

/// Creation options with an explicit directory walk policy.
pub struct CreateRequest<'a> {
    options: &'a CreateOptions,
    deterministic_order: bool,
    skip_apple_double: bool,
}

impl CreateOptions {
    /// Sort children by their raw name bytes. Keep the order of explicit operands.
    pub fn with_deterministic_order(&self) -> CreateRequest<'_> {
        CreateRequest {
            deterministic_order: true,
            ..self.into()
        }
    }
}

impl CreateRequest<'_> {
    /// Skip paired AppleDouble files during recursive walks.
    pub fn skipping_apple_double(mut self) -> Self {
        self.skip_apple_double = true;
        self
    }
}

impl<'a> From<&'a CreateOptions> for CreateRequest<'a> {
    fn from(options: &'a CreateOptions) -> Self {
        Self {
            options,
            deterministic_order: false,
            skip_apple_double: false,
        }
    }
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
    /// Entry name patterns to exclude. Exclusions take priority over selections.
    pub excludes: Vec<BString>,
    /// Only list entries without writing files (`LIST_ONLY` / `-t`).
    pub list_only: bool,
    /// Do not restore archive mtimes (`TOUCH`).
    pub touch: bool,
    /// Write selected entry names to the host standard output stream.
    pub verbose: bool,
}

/// Extraction options with an explicit link policy.
pub struct ExtractRequest<'a> {
    options: &'a ExtractOptions,
    confined_links: bool,
}

impl ExtractOptions {
    /// Restrict symbolic links to relative targets inside the destination.
    ///
    /// Parent components are permitted before the first name in a target.
    /// A target cannot refer to a symbolic link already at the destination.
    /// Hard links must refer to a regular member written by this extraction.
    pub fn with_confined_links(&self) -> ExtractRequest<'_> {
        ExtractRequest {
            options: self,
            confined_links: true,
        }
    }
}

impl<'a> From<&'a ExtractOptions> for ExtractRequest<'a> {
    fn from(options: &'a ExtractOptions) -> Self {
        Self {
            options,
            confined_links: false,
        }
    }
}

/// Error returned by archive operations.
#[derive(Clone, Debug, PartialEq, Eq, Error)]
#[error("{message}")]
pub struct ArchiveError {
    /// Human-readable error message.
    pub message: String,
    unsafe_path: Option<(&'static str, BString)>,
}

/// Create an archive at absolute path `output` from `inputs` resolved relative to `base_dir`.
///
/// The returned names use slash separators. Directory names end with a slash.
/// ZIP creation requires UTF-8 names and preserves the nine access permission bits.
/// Shared ZIP creation preserves timestamps to a resolution of 100 nanoseconds.
///
/// # Errors
///
/// Returns an error for invalid options, unsupported entries, size limits, or host errors.
pub fn create_archive<'a>(
    host: &dyn Host,
    output: &Path,
    base_dir: &Path,
    inputs: &[PathBuf],
    options: impl Into<CreateRequest<'a>>,
) -> Result<Vec<BString>, ArchiveError> {
    create::run(
        host,
        output,
        base_dir,
        inputs,
        options.into(),
        create::Selection::default(),
    )
}

/// Create an archive for `cmake -E tar`. The input paths can be outside `base_dir`.
///
/// TAR and ZIP store whole-second modification times. TAR stores each source
/// file as a separate member, including files that share a hard link.
///
/// # Errors
///
/// Returns the same errors as [`create_archive`] except for paths outside the base directory.
pub fn create_command_archive<'a>(
    host: &dyn Host,
    output: &Path,
    base_dir: &Path,
    inputs: &[PathBuf],
    options: impl Into<CreateRequest<'a>>,
) -> Result<Vec<BString>, ArchiveError> {
    create::run(
        host,
        output,
        base_dir,
        inputs,
        options.into(),
        create::Selection {
            allow_outside: true,
            command: true,
            ..Default::default()
        },
    )
}

/// Create an archive and omit entries that match an exclusion pattern.
///
/// # Errors
/// Returns an error for an invalid option, an empty pattern, or a host error.
pub fn create_filtered_archive<'a>(
    host: &dyn Host,
    output: &Path,
    base_dir: &Path,
    inputs: &[PathBuf],
    options: impl Into<CreateRequest<'a>>,
    excludes: &[BString],
    threads: u32,
) -> Result<Vec<BString>, ArchiveError> {
    if excludes.iter().any(|pattern| pattern.is_empty()) {
        return Err(ArchiveError::new("exclusion pattern must not be empty"));
    }
    create::run(
        host,
        output,
        base_dir,
        inputs,
        options.into(),
        create::Selection {
            excludes,
            threads,
            allow_outside: true,
            command: false,
        },
    )
}

/// Extract or list an archive at absolute path `archive_path`.
///
/// By default, symbolic links preserve their targets. Extraction rejects paths that pass
/// through symbolic links inside the destination and skips tar device and FIFO entries.
///
/// # Errors
///
/// Returns an error for an invalid archive, unsafe paths, unmatched patterns,
/// unsupported formats or entries, size limits, or host errors.
pub fn extract_archive<'a>(
    host: &dyn Host,
    archive_path: &Path,
    options: impl Into<ExtractRequest<'a>>,
) -> Result<Vec<BString>, ArchiveError> {
    extract::run(host, archive_path, options.into(), extract::Listing::Shared)
}

/// List selected entries with their mode, size, and modification time.
///
/// # Errors
/// Returns the same errors as archive extraction.
pub fn list_archive_details(
    host: &dyn Host,
    archive_path: &Path,
    options: &ExtractOptions,
    format_time: &dyn Fn(FileTime) -> String,
) -> Result<Vec<BString>, ArchiveError> {
    let options = ExtractOptions {
        list_only: true,
        ..options.clone()
    };
    extract::run(
        host,
        archive_path,
        (&options).into(),
        extract::Listing::SharedDetails(format_time),
    )
}

/// List entries for `cmake -E tar`. `format_time` enables detailed output when supplied.
///
/// # Errors
///
/// Returns an error for an invalid archive, unmatched patterns, size limits, or host errors.
pub fn list_command_archive(
    host: &dyn Host,
    archive_path: &Path,
    patterns: Vec<BString>,
    format_time: Option<&dyn Fn(FileTime) -> String>,
) -> Result<Vec<BString>, ArchiveError> {
    extract::run(
        host,
        archive_path,
        (&ExtractOptions {
            patterns,
            list_only: true,
            ..Default::default()
        })
            .into(),
        format_time.map_or(extract::Listing::Names, extract::Listing::Details),
    )
}

impl ArchiveError {
    /// Return the reason and original name for an unsafe archive entry path.
    pub fn unsafe_path(&self) -> Option<(&'static str, &[u8])> {
        self.unsafe_path
            .as_ref()
            .map(|(reason, name)| (*reason, name.as_slice()))
    }

    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            unsafe_path: None,
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
    if path.has_root() {
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
    let path = checked_name(name)?;
    let reason = if rooted(name) {
        Some("Path is absolute")
    } else if name
        .split(|&byte| byte == b'/' || byte == b'\\')
        .any(|part| part == b"..")
    {
        Some("Path contains '..'")
    } else {
        None
    };
    if let Some(reason) = reason {
        let mut error = invalid_name(name);
        error.unsafe_path = Some((reason, BString::from(name)));
        return Err(error);
    }
    let mut relative = PathBuf::new();
    for part in path.components() {
        if let Component::Normal(part) = part {
            if cfg!(windows) && unsafe_windows_name(part.as_encoded_bytes()) {
                return Err(invalid_name(name));
            }
            relative.push(part);
        }
    }
    Ok(relative)
}

fn unsafe_windows_name(name: &[u8]) -> bool {
    if name.ends_with(b".")
        || name.ends_with(b" ")
        || name
            .iter()
            .any(|&byte| byte < b' ' || b"<>:\"|?*".contains(&byte))
    {
        return true;
    }
    let stem = name
        .split(|&byte| byte == b'.')
        .next()
        .unwrap_or_default()
        .trim_ascii_end()
        .to_ascii_uppercase();
    matches!(
        stem.as_slice(),
        b"CON" | b"PRN" | b"AUX" | b"NUL" | b"CONIN$" | b"CONOUT$"
    ) || matches!(
        stem.as_slice(),
        [b'C', b'O', b'M', number @ ..] | [b'L', b'P', b'T', number @ ..]
            if matches!(number, [b'1'..=b'9'] | b"\xc2\xb9" | b"\xc2\xb2" | b"\xc2\xb3")
    )
}

fn checked_name(name: &[u8]) -> Result<PathBuf, ArchiveError> {
    let path = to_native(name);
    if name.is_empty()
        || name.len() > MAX_NAME_BYTES
        || name.contains(&0)
        || path.components().count() > MAX_DEPTH
    {
        return Err(invalid_name(name));
    }
    Ok(path)
}

fn invalid_name(name: &[u8]) -> ArchiveError {
    ArchiveError::new(format!("Invalid archive path: {:?}", BString::from(name)))
}

fn read_bytes(host: &dyn Host, path: &Path, limit: u64) -> Result<Vec<u8>, ArchiveError> {
    Ok(host.read(path, ReadRange::All { limit })?)
}

fn read_bounded(reader: impl Read, limit: u64) -> Result<Vec<u8>, ArchiveError> {
    let mut data = Vec::new();
    let mut bounded = reader.take(limit);
    bounded.read_to_end(&mut data).map_err(archive_error)?;
    if bounded.limit() == 0
        && io::copy(&mut bounded.into_inner().take(1), &mut io::sink()).map_err(archive_error)? != 0
    {
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
