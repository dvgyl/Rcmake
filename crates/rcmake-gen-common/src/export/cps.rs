//! CPS files of `install(PACKAGE_INFO)` and `export(PACKAGE_INFO)` (ADR-013).

use rcmake_cps::CpsFileSet;
use rcmake_model::{PackageInfoSpec, ProjectModel};
use rcmake_resolve::GenerationResult;

use crate::GenError;

/// Build the CPS root or appendix file and its configuration supplements for `spec`.
///
/// # Errors
///
/// Returns [`GenError::Unimplemented`] until CPS export is implemented.
pub fn render_package_info(
    model: &ProjectModel,
    result: &GenerationResult,
    spec: &PackageInfoSpec,
) -> Result<CpsFileSet, GenError> {
    let _ = (model, result, spec);
    Err(GenError::Unimplemented("CPS package information export"))
}
