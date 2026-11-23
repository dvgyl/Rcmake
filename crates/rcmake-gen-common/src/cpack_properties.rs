//! `CPackProperties.cmake`: the `CPACK_*` install properties that `cpack` reads (ADR-010).

use rcmake_base::BString;
use rcmake_model::ProjectModel;

use crate::GenError;

/// Render `CPackProperties.cmake`. `None` when the project sets no install property.
///
/// # Errors
///
/// Returns [`GenError::Unimplemented`] until the file is implemented.
pub fn render_cpack_properties(model: &ProjectModel) -> Result<Option<BString>, GenError> {
    let _ = model;
    Err(GenError::Unimplemented("CPackProperties.cmake generation"))
}
