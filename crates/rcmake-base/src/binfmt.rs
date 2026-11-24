// SPDX-License-Identifier: MIT OR Apache-2.0

//! Read runtime dependency metadata from ELF and PE images.

#![forbid(unsafe_code)]

use core::fmt;

use crate::BString;

mod elf;
mod pe;

/// Format of an executable or shared library image.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BinFormat {
    /// ELF with 32-bit addresses.
    Elf32,
    /// ELF with 64-bit addresses.
    Elf64,
    /// Portable Executable with a PE32 optional header.
    Pe32,
    /// Portable Executable with a PE32+ optional header.
    Pe32Plus,
    /// An image outside the formats that this module reads.
    Unknown,
}

/// Runtime dependency metadata for a recognized image.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum BinaryDeps {
    /// ELF dependency metadata.
    Elf(ElfDeps),
    /// PE dependency metadata.
    Pe(PeDeps),
}

impl BinaryDeps {
    /// Return the format of the image.
    #[must_use]
    pub fn format(&self) -> BinFormat {
        match self {
            Self::Elf(deps) => deps.format,
            Self::Pe(deps) => deps.format,
        }
    }
}

/// Dependency names and search paths from the ELF dynamic table.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ElfDeps {
    /// ELF address width.
    pub format: BinFormat,
    /// `DT_NEEDED` names in table order.
    pub needed: Vec<BString>,
    /// `DT_RPATH` components with empty components and tokens such as `$ORIGIN` preserved.
    pub rpaths: Vec<BString>,
    /// `DT_RUNPATH` components with empty components and tokens such as `${ORIGIN}` preserved.
    pub runpaths: Vec<BString>,
    /// `DT_SONAME` name, if present.
    pub soname: Option<BString>,
}

/// Dynamic link library (DLL) names from the PE import directories.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PeDeps {
    /// PE optional header format.
    pub format: BinFormat,
    /// DLL names in import table order, with their original case.
    pub imports: Vec<BString>,
    /// DLL names in delay-import table order, with their original case.
    pub delay_imports: Vec<BString>,
}

/// Failure to read runtime dependency metadata.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BinfmtError {
    /// The specified field or data extends past the available bytes.
    Truncated(&'static str),
    /// The specified field or data has an invalid value or structure.
    Invalid(&'static str),
    /// The specified structure exceeds a parser limit.
    LimitExceeded(&'static str),
}

impl fmt::Display for BinfmtError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Truncated(part) => write!(f, "Truncated {part}"),
            Self::Invalid(part) => write!(f, "Invalid {part}"),
            Self::LimitExceeded(part) => write!(f, "Size limit exceeded for {part}"),
        }
    }
}

impl core::error::Error for BinfmtError {}

/// Read runtime dependency metadata from the bytes of an ELF or PE image.
///
/// Returns `Ok(None)` if the bytes have no ELF or PE signature. A recognized
/// image with no dependency entries returns empty metadata. Names and paths
/// retain their original bytes. This function reads only the supplied slice.
///
/// # Errors
///
/// Returns an error if a recognized image has malformed dependency metadata,
/// truncated data, or data that exceeds a parser limit. This function accepts
/// at most 4,096 entries per dynamic or import table and at most 64 KiB per
/// string. It returns at most 4,096 names and path components, with a total
/// length of at most 1 MiB.
pub fn inspect_binary_deps(bytes: &[u8]) -> Result<Option<BinaryDeps>, BinfmtError> {
    if bytes.starts_with(b"\x7fELF") {
        return elf::inspect(bytes).map(|deps| Some(BinaryDeps::Elf(deps)));
    }
    if bytes.starts_with(b"MZ") {
        return pe::inspect(bytes).map(|deps| deps.map(BinaryDeps::Pe));
    }
    Ok(None)
}

const MAX_ENTRIES: usize = 4_096;
const MAX_STRING_BYTES: usize = 64 * 1024;
const MAX_OUTPUT_BYTES: usize = 1024 * 1024;

#[derive(Clone, Copy)]
enum Endian {
    Little,
    Big,
}

impl Endian {
    fn u16(self, bytes: &[u8], offset: usize, field: &'static str) -> Result<u16, BinfmtError> {
        let bytes = fixed(bytes, offset, field)?;
        Ok(match self {
            Self::Little => u16::from_le_bytes(bytes),
            Self::Big => u16::from_be_bytes(bytes),
        })
    }

    fn u32(self, bytes: &[u8], offset: usize, field: &'static str) -> Result<u32, BinfmtError> {
        let bytes = fixed(bytes, offset, field)?;
        Ok(match self {
            Self::Little => u32::from_le_bytes(bytes),
            Self::Big => u32::from_be_bytes(bytes),
        })
    }

    fn u64(self, bytes: &[u8], offset: usize, field: &'static str) -> Result<u64, BinfmtError> {
        let bytes = fixed(bytes, offset, field)?;
        Ok(match self {
            Self::Little => u64::from_le_bytes(bytes),
            Self::Big => u64::from_be_bytes(bytes),
        })
    }
}

fn fixed<const N: usize>(
    bytes: &[u8],
    offset: usize,
    field: &'static str,
) -> Result<[u8; N], BinfmtError> {
    bytes
        .get(offset..)
        .and_then(<[u8]>::first_chunk::<N>)
        .copied()
        .ok_or(BinfmtError::Truncated(field))
}

fn span<'a>(
    bytes: &'a [u8],
    offset: u64,
    length: u64,
    field: &'static str,
) -> Result<&'a [u8], BinfmtError> {
    let truncated = BinfmtError::Truncated(field);
    let start = usize::try_from(offset).map_err(|_| truncated)?;
    let length = usize::try_from(length).map_err(|_| truncated)?;
    let end = start.checked_add(length).ok_or(truncated)?;
    bytes.get(start..end).ok_or(truncated)
}

fn c_string<'a>(bytes: &'a [u8], field: &'static str) -> Result<&'a [u8], BinfmtError> {
    let end = bytes
        .iter()
        .take(MAX_STRING_BYTES + 1)
        .position(|&byte| byte == 0);
    match end {
        Some(end) => bytes.get(..end).ok_or(BinfmtError::Truncated(field)),
        None if bytes.len() > MAX_STRING_BYTES => Err(BinfmtError::LimitExceeded(field)),
        None => Err(BinfmtError::Truncated(field)),
    }
}

#[derive(Default)]
struct OutputBudget {
    strings: usize,
    bytes: usize,
}

impl OutputBudget {
    fn copy(&mut self, bytes: &[u8]) -> Result<BString, BinfmtError> {
        if self.strings == MAX_ENTRIES || bytes.len() > MAX_OUTPUT_BYTES - self.bytes {
            return Err(BinfmtError::LimitExceeded("dependency metadata"));
        }
        self.strings += 1;
        self.bytes += bytes.len();
        Ok(bytes.into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(super) fn write(bytes: &mut [u8], offset: usize, data: &[u8]) {
        let target = bytes.get_mut(offset..offset + data.len());
        assert!(target.is_some_and(|target| {
            target.copy_from_slice(data);
            true
        }));
    }

    #[test]
    fn unrecognized_signatures() {
        for bytes in [b"".as_slice(), b"plain text", b"\x7fEL", b"MZ"] {
            assert_eq!(inspect_binary_deps(bytes), Ok(None));
        }
    }

    #[test]
    fn string_and_output_limits() {
        let mut bytes = vec![b'x'; MAX_STRING_BYTES + 1];
        assert_eq!(
            c_string(&bytes, "name"),
            Err(BinfmtError::LimitExceeded("name"))
        );
        write(&mut bytes, MAX_STRING_BYTES, b"\0");
        let name = c_string(&bytes, "name").unwrap_or_default();
        assert_eq!(name.len(), MAX_STRING_BYTES);
        let mut budget = OutputBudget::default();
        for _ in 0..MAX_OUTPUT_BYTES / MAX_STRING_BYTES {
            assert!(budget.copy(name).is_ok());
        }
        assert_eq!(
            budget.copy(b"x"),
            Err(BinfmtError::LimitExceeded("dependency metadata"))
        );
    }
}
