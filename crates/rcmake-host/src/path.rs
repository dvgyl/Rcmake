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
    use crate::error::SyntheticError;

    crate::require_absolute(p)?;
    let bytes = from_native(p);
    let bytes = bytes.as_slice();
    let separator = |byte: &u8| *byte == b'/' || (cfg!(windows) && *byte == b'\\');
    if p.parent().is_none() {
        return Err(SyntheticError::ResourceBusy.at(Some(p)));
    }
    let end = bytes
        .iter()
        .rposition(|byte| !separator(byte))
        .map(|index| index + 1)
        .ok_or_else(|| SyntheticError::ResourceBusy.at(Some(p)))?;
    let trimmed = bytes.get(..end).unwrap_or_default();
    if matches!(trimmed.rsplit(separator).next(), Some(b"." | b"..")) {
        return Err(SyntheticError::InvalidInput.at(Some(p)));
    }
    Ok(to_native(trimmed))
}

/// Find an executable in an absolute directory from the host's PATH.
#[must_use]
pub fn executable_on_absolute_path(host: &dyn crate::Host, name: &str) -> Option<PathBuf> {
    let mut components = Path::new(name).components();
    if !matches!(
        (components.next(), components.next()),
        (Some(std::path::Component::Normal(_)), None)
    ) {
        return None;
    }
    let name = format!("{name}{}", std::env::consts::EXE_SUFFIX);
    let value = to_native(&host.env_get(b"PATH")?);
    std::env::split_paths(&value)
        .filter(|dir| dir.is_absolute())
        .map(|dir| dir.join(&name))
        .find(|file| {
            host.metadata(file, crate::Follow::Yes).is_ok_and(|meta| {
                meta.kind == crate::FileKind::File && (cfg!(windows) || meta.mode & 0o111 != 0)
            })
        })
}
