//! `cmake --build` plan for the Visual Studio generators (M4 design section 1.8).
//!
//! Owner after Milestone 4 Wave 0: Wave 2 worker B. The planned argv is:
//!
//! ```text
//! <MSBuild> <target or ALL_BUILD>.vcxproj /p:Configuration=<config or Debug>
//!     /p:Platform=<CMAKE_VS_PLATFORM_NAME> /p:VisualStudioVersion=<17.0|18.0>
//!     [/m | /m:<N>] /v:<m|n> [native args]
//! ```
//!
//! `clean` uses the solution file with `/t:Clean`.

use rcmake_gen_common::GenError;
use rcmake_gen_common::ide::{BuildCommand, BuildRequest};

/// The MSBuild processes of `cmake --build`, in run order.
///
/// # Errors
///
/// Returns [`GenError::Unimplemented`] until Milestone 4 Wave 2.
pub fn build_commands(req: &BuildRequest<'_>) -> Result<Vec<BuildCommand>, GenError> {
    let _ = req;
    Err(GenError::Unimplemented(
        "cmake --build for the Visual Studio generators",
    ))
}
