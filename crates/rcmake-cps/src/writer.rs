//! CPS 0.15.0 JSON writer for `install(PACKAGE_INFO)` and `export(PACKAGE_INFO)`.

use rcmake_base::BString;

use crate::{CpsConfigSupplement, CpsError, CpsPackage};

/// Write the JSON text of a root or appendix file.
///
/// # Errors
///
/// Returns [`CpsError::Unsupported`] until the CPS writer is implemented.
pub fn write_package(package: &CpsPackage) -> Result<BString, CpsError> {
    let _ = package;
    Err(CpsError::Unsupported("CPS package writing"))
}

/// Write the JSON text of a configuration supplement file.
///
/// # Errors
///
/// Returns [`CpsError::Unsupported`] until the CPS writer is implemented.
pub fn write_supplement(supplement: &CpsConfigSupplement) -> Result<BString, CpsError> {
    let _ = supplement;
    Err(CpsError::Unsupported(
        "CPS configuration supplement writing",
    ))
}
