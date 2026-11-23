//! CPS package import for `find_package()` in Config mode (ADR-013): read
//! `<Pkg>.cps`, its appendices and configuration supplements, and create the imported
//! `<Pkg>::<component>` targets and the default `<Pkg>` target.

use rcmake_cps::{CpsError, CpsPackage};

/// Read the CPS root file at `path` with content `text`.
///
/// # Errors
///
/// Returns [`CpsError::Unsupported`] until CPS import is implemented.
#[expect(dead_code, reason = "find_package() does not search for CPS files yet")]
pub(crate) fn import_package(text: &[u8], path: &[u8]) -> Result<CpsPackage, CpsError> {
    rcmake_cps::reader::read_package(text, path)
}
