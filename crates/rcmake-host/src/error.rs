//! Structured host error type matching CMake's `strerror` formatting without Rust's
//! `" (os error N)"` suffix.

use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use thiserror::Error;

/// Convenience result alias for [`crate::Host`] operations.
pub type HostResult<T> = Result<T, HostError>;

/// Error returned by a [`crate::Host`] operation.
#[derive(Clone, Debug, PartialEq, Eq, Error)]
#[error("{os_message}")]
pub struct HostError {
    /// Portable `std::io::ErrorKind` classification.
    pub kind: ErrorKind,
    /// Optional raw OS errno.
    pub errno: Option<i32>,
    /// Clean `strerror`-style message without `" (os error N)"`.
    pub os_message: String,
    /// Optional absolute path associated with the error.
    pub path: Option<PathBuf>,
}

impl HostError {
    /// Error returned whenever a relative path is passed to a [`crate::Host`] FS method.
    #[must_use]
    pub fn not_absolute(path: PathBuf) -> Self {
        Self {
            kind: ErrorKind::InvalidInput,
            errno: None,
            os_message: format!("path must be absolute: {}", path.display()),
            path: Some(path),
        }
    }

    /// Construct a [`HostError`] from a `std::io::Error`, stripping `" (os error N)"`.
    #[must_use]
    pub fn from_io(err: std::io::Error, path: Option<PathBuf>) -> Self {
        let kind = err.kind();
        let errno = err.raw_os_error();
        let raw = err.to_string();
        let clean = strip_os_error_suffix(&raw).to_owned();
        Self {
            kind,
            errno,
            os_message: clean,
            path,
        }
    }

    /// Construct a timeout error for the operation on `path`.
    #[must_use]
    pub fn timed_out(msg: impl Into<String>, path: Option<PathBuf>) -> Self {
        Self {
            kind: ErrorKind::TimedOut,
            errno: None,
            os_message: msg.into(),
            path,
        }
    }

    /// Construct an unsupported-operation error.
    #[must_use]
    pub fn unsupported(msg: impl Into<String>) -> Self {
        Self {
            kind: ErrorKind::Unsupported,
            errno: None,
            os_message: msg.into(),
            path: None,
        }
    }
}

#[cfg(unix)]
pub(crate) fn from_errno(errno: rustix::io::Errno, path: Option<&Path>) -> HostError {
    HostError::from_io(errno.into(), path.map(Path::to_path_buf))
}

fn strip_os_error_suffix(msg: &str) -> &str {
    if let Some(idx) = msg.rfind(" (os error ")
        && msg.ends_with(')')
    {
        return msg.get(..idx).unwrap_or(msg);
    }
    msg
}

pub(crate) enum SyntheticError {
    NotFound,
    InvalidInput,
    IsADirectory,
    NotADirectory,
    FileTooLarge,
    AlreadyExists,
    ResourceBusy,
    DirectoryNotEmpty,
    PermissionDenied,
    LinkLoop,
}

impl SyntheticError {
    pub(crate) fn at(self, path: Option<&Path>) -> HostError {
        #[cfg(unix)]
        {
            use rustix::io::Errno;
            let errno = match self {
                Self::NotFound => Errno::NOENT,
                Self::InvalidInput => Errno::INVAL,
                Self::IsADirectory => Errno::ISDIR,
                Self::NotADirectory => Errno::NOTDIR,
                Self::FileTooLarge => Errno::FBIG,
                Self::AlreadyExists => Errno::EXIST,
                Self::ResourceBusy => Errno::BUSY,
                Self::DirectoryNotEmpty => Errno::NOTEMPTY,
                Self::PermissionDenied => Errno::PERM,
                Self::LinkLoop => Errno::LOOP,
            };
            from_errno(errno, path)
        }
        #[cfg(windows)]
        {
            let (kind, message) = match self {
                Self::NotFound => (ErrorKind::NotFound, "No such file or directory"),
                Self::InvalidInput => (ErrorKind::InvalidInput, "Invalid argument"),
                Self::IsADirectory => (ErrorKind::IsADirectory, "Is a directory"),
                Self::NotADirectory => (ErrorKind::NotADirectory, "Not a directory"),
                Self::FileTooLarge => (ErrorKind::FileTooLarge, "File too large"),
                Self::AlreadyExists => (ErrorKind::AlreadyExists, "File exists"),
                Self::ResourceBusy => (ErrorKind::ResourceBusy, "Device or resource busy"),
                Self::DirectoryNotEmpty => (ErrorKind::DirectoryNotEmpty, "Directory not empty"),
                Self::PermissionDenied => (ErrorKind::PermissionDenied, "Operation not permitted"),
                Self::LinkLoop => (ErrorKind::Other, "Too many levels of symbolic links"),
            };
            HostError::from_io(
                std::io::Error::new(kind, message),
                path.map(Path::to_path_buf),
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_rust_os_error_suffix() {
        assert_eq!(
            strip_os_error_suffix("No such file or directory (os error 2)"),
            "No such file or directory"
        );
        assert_eq!(
            strip_os_error_suffix("Permission denied"),
            "Permission denied"
        );
    }
}
