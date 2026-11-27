// SPDX-License-Identifier: MIT OR Apache-2.0

//! A strict gzip tar format for portable probe-cache archives.
//!
//! The writer emits one canonical form: POSIX ustar headers, regular files only,
//! sorted names, mode 0644, owner 0, and time 0. The reader accepts only that
//! form. It checks every header before it allocates member data, and it rejects
//! links, devices, extension headers, unknown names, duplicates, trailing data,
//! and sizes over the limits.

use std::collections::BTreeSet;
use std::io::{self, Read, Write};

use flate2::{Compression, GzBuilder, bufread::GzDecoder};

use crate::ArchiveError;

const BLOCK: usize = 512;
const MANIFEST: &[u8] = b"manifest.json";
const ENTRY_PREFIX: &[u8] = b"entries/";
const BLOB_PREFIX: &[u8] = b"blobs/";

/// Name of one member of a portable cache archive.
///
/// The order of this type is the member order in the archive: the manifest,
/// then entries by key, then blobs by digest.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum PortableName {
    /// The `manifest.json` member.
    Manifest,
    /// An `entries/<64 lowercase hex digits>` member with this key.
    Entry([u8; 32]),
    /// A `blobs/<64 lowercase hex digits>` member with this digest.
    Blob([u8; 32]),
}

impl PortableName {
    /// Return the exact member name.
    pub fn to_name(self) -> String {
        match self {
            Self::Manifest => "manifest.json".to_owned(),
            Self::Entry(key) => format!("entries/{}", hex(&key)),
            Self::Blob(digest) => format!("blobs/{}", hex(&digest)),
        }
    }

    /// Parse an exact member name. Return `None` for any other name.
    pub fn parse(name: &[u8]) -> Option<Self> {
        if name == MANIFEST {
            return Some(Self::Manifest);
        }
        if let Some(rest) = name.strip_prefix(ENTRY_PREFIX) {
            return parse_hex(rest).map(Self::Entry);
        }
        name.strip_prefix(BLOB_PREFIX)
            .and_then(parse_hex)
            .map(Self::Blob)
    }
}

/// Size limits for [`read_portable`] and [`write_portable`].
///
/// The reader applies each limit before allocation. Give the writer the same
/// value as the reader, so that every written archive passes the read checks.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PortableLimits {
    /// Maximum size of the compressed archive.
    pub archive_bytes: u64,
    /// Maximum size of the decompressed tar stream.
    pub tar_bytes: u64,
    /// Maximum size of one entry or blob member.
    pub member_bytes: u64,
    /// Maximum size of the manifest member.
    pub manifest_bytes: u64,
    /// Maximum combined size of all members.
    pub total_bytes: u64,
    /// Maximum number of members, including the manifest.
    pub members: usize,
}

/// One member read from a portable cache archive.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PortableMember {
    /// Member name.
    pub name: PortableName,
    /// Member contents.
    pub data: Vec<u8>,
}

/// Write a portable cache archive with deterministic bytes.
///
/// The names must be unique and in [`PortableName`] order. The first member
/// must be the manifest. The archive must fit in `limits`, which the writer
/// checks before it returns any bytes.
///
/// # Errors
///
/// Returns an error for unsorted or duplicate names, a missing manifest, an
/// archive or member over `limits`, or a compression failure.
pub fn write_portable(
    members: &[(PortableName, &[u8])],
    limits: &PortableLimits,
) -> Result<Vec<u8>, ArchiveError> {
    if members.first().map(|(name, _)| *name) != Some(PortableName::Manifest) {
        return Err(ArchiveError::new(
            "A portable archive must start with its manifest.",
        ));
    }
    if !members.is_sorted_by(|(left, _), (right, _)| left < right) {
        return Err(ArchiveError::new(
            "Portable archive members must be unique and sorted.",
        ));
    }
    if members.len() > limits.members {
        return Err(ArchiveError::new(format!(
            "A portable archive can hold at most {} members.",
            limits.members
        )));
    }
    let mut total = 0u64;
    let mut tar = 2 * BLOCK as u64;
    for (name, data) in members {
        let size = data.len() as u64;
        let limit = if *name == PortableName::Manifest {
            limits.manifest_bytes
        } else {
            limits.member_bytes
        };
        if size > limit {
            return Err(ArchiveError::new(format!(
                "Portable archive member {} exceeds {limit} bytes.",
                name.to_name()
            )));
        }
        total = total.saturating_add(size);
        tar = tar.saturating_add(BLOCK as u64 + size + padding(size) as u64);
    }
    if total > limits.total_bytes || tar > limits.tar_bytes {
        return Err(ArchiveError::new(
            "The portable archive members exceed the archive size limits.",
        ));
    }
    let mut encoder = GzBuilder::new()
        .mtime(0)
        .operating_system(255)
        .write(Vec::new(), Compression::new(6));
    for (name, data) in members {
        let size = data.len() as u64;
        let header = ustar_header(name.to_name().as_bytes(), size)?;
        encoder.write_all(&header).map_err(io_error)?;
        encoder.write_all(data).map_err(io_error)?;
        let zeros = [0u8; BLOCK];
        encoder
            .write_all(zeros.get(..padding(size)).unwrap_or_default())
            .map_err(io_error)?;
    }
    encoder.write_all(&[0; 2 * BLOCK]).map_err(io_error)?;
    let archive = encoder.finish().map_err(io_error)?;
    if archive.len() as u64 > limits.archive_bytes {
        return Err(ArchiveError::new(format!(
            "The portable archive exceeds {} bytes.",
            limits.archive_bytes
        )));
    }
    Ok(archive)
}

/// Read and check a complete portable cache archive.
///
/// The reader accepts one gzip member that holds one ustar stream. Every
/// member must be a regular file with an exact name from [`PortableName`], in
/// sorted order, with the manifest first. Bytes after the end of the tar
/// stream must be zero, and no bytes can follow the gzip member.
///
/// # Errors
///
/// Returns an error for any format violation or exceeded limit. The error
/// message names the first problem found.
pub fn read_portable(
    archive: &[u8],
    limits: &PortableLimits,
) -> Result<Vec<PortableMember>, ArchiveError> {
    read_portable_checked(archive, limits, &mut |_| Ok(()))
}

/// Read a portable cache archive like [`read_portable`] and give each member to
/// `check` as soon as it is read.
///
/// The reader stops at the first member that `check` rejects, before it
/// decompresses the rest of the archive. A caller uses `check` to charge an
/// aggregate budget, for example the declared decoded size of each entry.
///
/// # Errors
///
/// Returns the errors of [`read_portable`] and the first error message of `check`.
pub fn read_portable_checked(
    archive: &[u8],
    limits: &PortableLimits,
    check: &mut dyn FnMut(&PortableMember) -> Result<(), String>,
) -> Result<Vec<PortableMember>, ArchiveError> {
    if archive.len() as u64 > limits.archive_bytes {
        return Err(ArchiveError::new(format!(
            "The archive exceeds {} bytes.",
            limits.archive_bytes
        )));
    }
    let mut decoder = GzDecoder::new(archive);
    let mut stream = Budget {
        inner: &mut decoder,
        left: limits.tar_bytes,
    };
    let members = read_members(&mut stream, limits, check)?;
    let trailing = decoder.into_inner();
    if !trailing.is_empty() {
        return Err(ArchiveError::new(
            "The archive has data after its gzip stream.",
        ));
    }
    Ok(members)
}

fn read_members(
    stream: &mut impl Read,
    limits: &PortableLimits,
    check: &mut dyn FnMut(&PortableMember) -> Result<(), String>,
) -> Result<Vec<PortableMember>, ArchiveError> {
    let mut members = Vec::new();
    let mut seen = BTreeSet::new();
    let mut total = 0u64;
    let mut header = [0u8; BLOCK];
    loop {
        read_block(stream, &mut header)?;
        if header.iter().all(|&byte| byte == 0) {
            read_block(stream, &mut header)?;
            if header.iter().any(|&byte| byte != 0) {
                return Err(ArchiveError::new("The tar end marker is incomplete."));
            }
            drain_zeros(stream)?;
            break;
        }
        let (name, size) = check_header(&header)?;
        if !seen.insert(name) {
            return Err(ArchiveError::new(format!(
                "The archive repeats member {}.",
                name.to_name()
            )));
        }
        if seen.len() > limits.members {
            return Err(ArchiveError::new(format!(
                "The archive has more than {} members.",
                limits.members
            )));
        }
        let previous = members.last().map(|member: &PortableMember| member.name);
        let ordered = match previous {
            None => name == PortableName::Manifest,
            Some(previous) => previous < name,
        };
        if !ordered {
            return Err(ArchiveError::new(format!(
                "Member {} is out of order.",
                name.to_name()
            )));
        }
        let cap = if name == PortableName::Manifest {
            limits.manifest_bytes
        } else {
            limits.member_bytes
        };
        total = total.saturating_add(size);
        if size > cap || total > limits.total_bytes {
            return Err(ArchiveError::new(format!(
                "Member {} exceeds the archive size limits.",
                name.to_name()
            )));
        }
        let len = usize::try_from(size)
            .map_err(|_| ArchiveError::new("A member size is not addressable."))?;
        let mut data = vec![0; len];
        stream
            .read_exact(&mut data)
            .map_err(|error| truncated(&error))?;
        let mut pad = [0u8; BLOCK];
        let pad = pad.get_mut(..padding(size)).unwrap_or_default();
        stream.read_exact(pad).map_err(|error| truncated(&error))?;
        if pad.iter().any(|&byte| byte != 0) {
            return Err(ArchiveError::new("A member has nonzero padding."));
        }
        let member = PortableMember { name, data };
        check(&member).map_err(ArchiveError::new)?;
        members.push(member);
    }
    if members.is_empty() {
        return Err(ArchiveError::new("The archive has no manifest."));
    }
    Ok(members)
}

/// Check one ustar header. Return the member name and data size.
fn check_header(header: &[u8; BLOCK]) -> Result<(PortableName, u64), ArchiveError> {
    let field = |start: usize, end: usize| header.get(start..end).unwrap_or_default();
    let recorded = parse_octal(field(148, 156))
        .ok_or_else(|| ArchiveError::new("A tar header has an invalid checksum field."))?;
    let actual: u64 = header
        .iter()
        .enumerate()
        .map(|(index, &byte)| {
            if (148..156).contains(&index) {
                u64::from(b' ')
            } else {
                u64::from(byte)
            }
        })
        .sum();
    if recorded != actual {
        return Err(ArchiveError::new("A tar header checksum does not match."));
    }
    if field(257, 263) != b"ustar\0" || field(263, 265) != b"00" {
        return Err(ArchiveError::new(
            "A tar header is not in the POSIX ustar format.",
        ));
    }
    if !matches!(header.get(156), Some(b'0' | 0)) {
        return Err(ArchiveError::new(
            "The archive contains a member that is not a regular file.",
        ));
    }
    if field(157, 257).iter().any(|&byte| byte != 0)
        || field(345, BLOCK).iter().any(|&byte| byte != 0)
    {
        return Err(ArchiveError::new(
            "A tar header has a link target or name prefix.",
        ));
    }
    let raw = field(0, 100);
    let len = raw.iter().position(|&byte| byte == 0).unwrap_or(raw.len());
    let (name, rest) = raw.split_at(len);
    if rest.iter().any(|&byte| byte != 0) {
        return Err(ArchiveError::new("A tar member name is not terminated."));
    }
    let name = PortableName::parse(name).ok_or_else(|| {
        ArchiveError::new(format!(
            "The archive contains an unexpected member {}.",
            String::from_utf8_lossy(name)
        ))
    })?;
    let size = parse_octal(field(124, 136))
        .ok_or_else(|| ArchiveError::new("A tar header has an invalid size field."))?;
    Ok((name, size))
}

/// Parse an octal ustar number. Leading spaces and trailing spaces or NUL bytes are
/// permitted. Base-256 numbers are rejected.
fn parse_octal(field: &[u8]) -> Option<u64> {
    let start = field.iter().position(|&byte| byte != b' ')?;
    let digits = field.get(start..)?;
    let end = digits
        .iter()
        .position(|&byte| matches!(byte, 0 | b' '))
        .unwrap_or(digits.len());
    let (number, tail) = digits.split_at(end);
    if number.is_empty() || tail.iter().any(|&byte| !matches!(byte, 0 | b' ')) {
        return None;
    }
    number.iter().try_fold(0u64, |value, &byte| {
        let digit = char::from(byte).to_digit(8)?;
        value.checked_mul(8)?.checked_add(u64::from(digit))
    })
}

fn ustar_header(name: &[u8], size: u64) -> Result<Vec<u8>, ArchiveError> {
    if name.len() >= 100 {
        return Err(ArchiveError::new("A portable member name is too long."));
    }
    let mut header = Vec::with_capacity(BLOCK);
    let mut field = |bytes: &[u8], width: usize| {
        header.extend_from_slice(bytes);
        header.resize(header.len() + width - bytes.len(), 0);
    };
    field(name, 100);
    field(b"0000644", 8);
    field(b"0000000", 8);
    field(b"0000000", 8);
    field(format!("{size:011o}").as_bytes(), 12);
    field(b"00000000000", 12);
    field(b"        ", 8);
    field(b"0", 1);
    field(b"", 100);
    field(b"ustar\0", 6);
    field(b"00", 2);
    field(b"", BLOCK - 265);
    if header.len() != BLOCK {
        return Err(ArchiveError::new("A portable member size is too large."));
    }
    let sum: u64 = header.iter().map(|&byte| u64::from(byte)).sum();
    let checksum = format!("{sum:06o}\0 ");
    if let Some(slot) = header.get_mut(148..156) {
        slot.copy_from_slice(checksum.as_bytes());
    }
    Ok(header)
}

fn read_block(stream: &mut impl Read, block: &mut [u8; BLOCK]) -> Result<(), ArchiveError> {
    stream.read_exact(block).map_err(|error| truncated(&error))
}

fn drain_zeros(stream: &mut impl Read) -> Result<(), ArchiveError> {
    let mut buffer = [0u8; 8192];
    loop {
        let read = match stream.read(&mut buffer) {
            Ok(0) => return Ok(()),
            Ok(read) => read,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(truncated(&error)),
        };
        if buffer
            .get(..read)
            .unwrap_or_default()
            .iter()
            .any(|&b| b != 0)
        {
            return Err(ArchiveError::new(
                "The archive has data after its tar end marker.",
            ));
        }
    }
}

fn padding(size: u64) -> usize {
    ((BLOCK as u64 - size % BLOCK as u64) % BLOCK as u64) as usize
}

fn truncated(error: &io::Error) -> ArchiveError {
    if error.kind() == io::ErrorKind::UnexpectedEof {
        ArchiveError::new("The archive is truncated.")
    } else {
        ArchiveError::new(format!("The archive is invalid: {error}."))
    }
}

fn io_error(error: io::Error) -> ArchiveError {
    ArchiveError::new(error.to_string())
}

fn hex(bytes: &[u8; 32]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn parse_hex(text: &[u8]) -> Option<[u8; 32]> {
    if text.len() != 64 {
        return None;
    }
    let mut bytes = [0u8; 32];
    let (pairs, _) = text.as_chunks::<2>();
    for (out, &[high, low]) in bytes.iter_mut().zip(pairs) {
        let digit = |byte: u8| match byte {
            b'0'..=b'9' => Some(byte - b'0'),
            b'a'..=b'f' => Some(byte - b'a' + 10),
            _ => None,
        };
        *out = (digit(high)? << 4) | digit(low)?;
    }
    Some(bytes)
}

/// Limit the bytes read from the decompressed stream.
struct Budget<R> {
    inner: R,
    left: u64,
}

impl<R: Read> Read for Budget<R> {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        let read = self.inner.read(out)?;
        self.left = self
            .left
            .checked_sub(read as u64)
            .ok_or_else(|| io::Error::other("The decompressed archive exceeds its size limit."))?;
        Ok(read)
    }
}

#[cfg(test)]
mod tests;
