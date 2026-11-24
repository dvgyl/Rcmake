//! `cmake --build` plan for the Xcode generator (M4 design section 1.7).
//!
//! Owner after Milestone 4 Wave 0: Wave 2 worker A. The planned argv is:
//!
//! ```text
//! <CMAKE_MAKE_PROGRAM> -project <CMAKE_PROJECT_NAME>.xcodeproj build
//!     -configuration <config or Debug>
//!     -parallelizeTargets [-jobs <N>] -hideShellScriptEnvironment
//!     -target <target or ALL_BUILD> [native args]
//! ```

use rcmake_gen_common::GenError;
use rcmake_gen_common::ide::{BuildCommand, BuildRequest};

/// The `xcodebuild` processes of `cmake --build`, in run order.
///
/// # Errors
///
/// Returns [`GenError::Unimplemented`] until Milestone 4 Wave 2.
pub fn build_commands(req: &BuildRequest<'_>) -> Result<Vec<BuildCommand>, GenError> {
    let _ = req;
    Err(GenError::Unimplemented(
        "cmake --build for the Xcode generator",
    ))
}
