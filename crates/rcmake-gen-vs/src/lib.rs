//! Native Visual Studio generator for Rcmake (ADR-015).
//!
//! The generator writes `.sln` (`Visual Studio 17 2022`) or `.slnx`
//! (`Visual Studio 18 2026`) solutions, `.vcxproj` projects, and `.vcxproj.filters`
//! files. GUIDs are deterministic RFC 4122 version-3 UUIDs, as in CMake 4.5.1.
//!
//! The generators are available on Windows. On other hosts, only a build with the
//! `vs-cross` feature of `rcmake-model` makes them available (structural tests).
//!
//! Milestone 4 Wave 0 freezes the module layout. Every entry point returns
//! [`GenError::Unimplemented`] until Wave 2.

#![forbid(unsafe_code)]

use std::path::Path;

use rcmake_gen_common::GenError;
use rcmake_host::Host;
use rcmake_model::{ProjectModel, VsVersion};
use rcmake_resolve::GenerationResult;

pub mod build;
pub mod filters;
pub mod flags_table;
pub mod guid;
pub mod rules;
pub mod sln;
pub mod slnx;
pub mod vcxproj;
pub mod xml;

pub use build::build_commands;

/// Write the Visual Studio solution and project files of `result` into the build
/// directory `bin`.
///
/// # Errors
///
/// Returns [`GenError::Unimplemented`] until Milestone 4 Wave 2.
pub fn generate(
    version: VsVersion,
    result: &GenerationResult,
    model: &ProjectModel,
    host: &dyn Host,
    bin: &Path,
) -> Result<(), GenError> {
    let _ = (version, result, model, host, bin);
    Err(GenError::Unimplemented("the Visual Studio generator"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use rcmake_host::PureHost;

    #[test]
    fn wave_zero_generate_is_unimplemented() {
        for version in VsVersion::ALL {
            let result = generate(
                version,
                &GenerationResult::default(),
                &ProjectModel::default(),
                &PureHost::default(),
                Path::new("/out"),
            );
            assert_eq!(
                result,
                Err(GenError::Unimplemented("the Visual Studio generator"))
            );
        }
    }
}
