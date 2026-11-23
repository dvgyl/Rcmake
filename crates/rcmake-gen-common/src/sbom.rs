//! SPDX 3.0.1 JSON-LD software bill of materials for `install(SBOM)`, `export(SBOM)`,
//! and `CMAKE_INSTALL_SBOM_FORMATS` (ADR-013).

use rcmake_base::BString;
use rcmake_model::{ProjectModel, SbomSpec};
use rcmake_resolve::GenerationResult;

use crate::GenError;

/// Render the SBOM document of `spec` for configuration `config` (empty for
/// `noconfig`).
///
/// # Errors
///
/// Returns [`GenError::Unimplemented`] until SBOM generation is implemented.
pub fn render_sbom(
    model: &ProjectModel,
    result: &GenerationResult,
    spec: &SbomSpec,
    config: &[u8],
) -> Result<BString, GenError> {
    let _ = (model, result, spec, config);
    Err(GenError::Unimplemented("SPDX SBOM generation"))
}
