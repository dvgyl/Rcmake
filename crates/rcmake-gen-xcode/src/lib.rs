//! Native Xcode generator for Rcmake (ADR-014).
//!
//! The generator writes `<Project>.xcodeproj/project.pbxproj`, the shared workspace
//! settings, the requested schemes, and `CMakeScripts/ReRunCMake.make`. Object IDs are
//! deterministic, so a second configure run does not change the project file.
//!
//! Milestone 4 Wave 0 freezes the module layout. Every entry point returns
//! [`GenError::Unimplemented`] until Wave 2.

#![forbid(unsafe_code)]

use std::path::Path;

use rcmake_gen_common::GenError;
use rcmake_host::Host;
use rcmake_model::ProjectModel;
use rcmake_resolve::GenerationResult;

pub mod build;
pub mod ids;
pub mod object;
pub mod phases;
pub mod plist;
pub mod project;
pub mod rerun;
pub mod scheme;
pub mod settings;
pub mod workspace;

pub use build::build_commands;

/// Write the Xcode project files of `result` into the build directory `bin`.
///
/// # Errors
///
/// Returns [`GenError::Unimplemented`] until Milestone 4 Wave 2.
pub fn generate(
    result: &GenerationResult,
    model: &ProjectModel,
    host: &dyn Host,
    bin: &Path,
) -> Result<(), GenError> {
    let _ = (result, model, host, bin);
    Err(GenError::Unimplemented("the Xcode generator"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use rcmake_host::PureHost;

    #[test]
    fn wave_zero_generate_is_unimplemented() {
        let result = generate(
            &GenerationResult::default(),
            &ProjectModel::default(),
            &PureHost::default(),
            Path::new("/out"),
        );
        assert_eq!(result, Err(GenError::Unimplemented("the Xcode generator")));
    }
}
