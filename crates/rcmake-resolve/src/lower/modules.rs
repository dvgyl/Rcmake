//! Lowering of C++20 named modules: scan steps, the collate step, and the module map
//! and dyndep inputs of compile steps (ADR-012).

use rcmake_model::ProjectModel;

use crate::{BuildStep, ResolveError, ResolvedTarget};

/// Build steps that scan and collate the modules of `target`. Empty when the target
/// has no scanned units.
///
/// # Errors
///
/// Returns [`ResolveError::Unimplemented`] for a target with scanned units until module
/// lowering is implemented.
pub fn lower_modules(
    model: &ProjectModel,
    target: &ResolvedTarget,
) -> Result<Vec<BuildStep>, ResolveError> {
    let _ = model;
    if target.cxx_modules.is_none() && !target.compile_units.iter().any(|unit| unit.scan) {
        return Ok(Vec::new());
    }
    Err(ResolveError::Unimplemented("C++ module scanning"))
}
