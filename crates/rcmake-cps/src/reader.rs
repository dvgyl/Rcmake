//! Validating CPS reader (`cps_version` `>= 0.13 && < 0.16`) for `find_package()`.

use crate::{CpsConfigSupplement, CpsError, CpsPackage};

/// Parse and validate a root or appendix file. `path` is the file path for messages
/// and `@prefix@` resolution.
///
/// # Errors
///
/// Returns [`CpsError::Unsupported`] until the CPS reader is implemented.
pub fn read_package(text: &[u8], path: &[u8]) -> Result<CpsPackage, CpsError> {
    let _ = (text, path);
    Err(CpsError::Unsupported("CPS package reading"))
}

/// Parse and validate a configuration supplement file.
///
/// # Errors
///
/// Returns [`CpsError::Unsupported`] until the CPS reader is implemented.
pub fn read_supplement(text: &[u8], path: &[u8]) -> Result<CpsConfigSupplement, CpsError> {
    let _ = (text, path);
    Err(CpsError::Unsupported(
        "CPS configuration supplement reading",
    ))
}
