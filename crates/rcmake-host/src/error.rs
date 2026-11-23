//! Structured host error type matching CMake's `strerror` formatting without Rust's
//! `" (os error N)"` suffix.

use std::io::ErrorKind;
use std::path::PathBuf;

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

fn strip_os_error_suffix(msg: &str) -> &str {
    if let Some(idx) = msg.rfind(" (os error ")
        && msg.ends_with(')')
    {
        return msg.get(..idx).unwrap_or(msg);
    }
    msg
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
