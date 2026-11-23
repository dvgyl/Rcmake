//! Launcher wrapping of compile, link, and custom commands (instrumentation and
//! `<LANG>_COMPILER_LAUNCHER`).

use rcmake_base::BString;
use rcmake_model::ProjectModel;

use crate::ResolveError;

/// The command prefix that records instrumentation snippets for the build commands of
/// `model`. `None` when the project does not request instrumentation.
///
/// # Errors
///
/// Returns [`ResolveError::Unimplemented`] for a project with `cmake_instrumentation()`
/// requests until the launcher is implemented.
pub fn instrumentation_launcher(
    model: &ProjectModel,
) -> Result<Option<Vec<BString>>, ResolveError> {
    if model.global.instrumentation.is_empty() {
        return Ok(None);
    }
    Err(ResolveError::Unimplemented("the instrumentation launcher"))
}
