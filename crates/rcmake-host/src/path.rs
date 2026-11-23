//! Byte-preserving conversions between CMake byte paths and native [`Path`]/[`PathBuf`].

use std::path::{Path, PathBuf};

use rcmake_base::BString;

/// Convert CMake byte path to a native [`PathBuf`] without requiring UTF-8 on Unix.
#[must_use]
pub fn to_native(p: &[u8]) -> PathBuf {
    #[cfg(unix)]
    {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;
        PathBuf::from(OsStr::from_bytes(p))
    }
    #[cfg(not(unix))]
    {
        PathBuf::from(String::from_utf8_lossy(p).into_owned())
    }
}

/// Convert a native [`Path`] to a CMake [`BString`] without losing non-UTF-8 bytes on Unix.
#[must_use]
pub fn from_native(p: &Path) -> BString {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        BString::from(p.as_os_str().as_bytes())
    }
    #[cfg(not(unix))]
    {
        BString::from(p.to_string_lossy().as_bytes())
    }
}

pub(crate) fn directory_removal_path(p: &Path) -> crate::HostResult<PathBuf> {
    use rustix::io::Errno;

    crate::require_absolute(p)?;
    let bytes = from_native(p);
    let bytes = bytes.as_slice();
    let end = bytes
        .iter()
        .rposition(|byte| *byte != b'/')
        .map(|index| index + 1)
        .ok_or_else(|| crate::error::from_errno(Errno::BUSY, Some(p)))?;
    let trimmed = bytes.get(..end).unwrap_or_default();
    if matches!(
        trimmed.rsplit(|byte| *byte == b'/').next(),
        Some(b"." | b"..")
    ) {
        return Err(crate::error::from_errno(Errno::INVAL, Some(p)));
    }
    Ok(to_native(trimmed))
}
