//! Dashboard scripts (`ctest -S`) and the `ctest_start`, `ctest_configure`,
//! `ctest_build`, and `ctest_test` commands.

use std::path::Path;

use rcmake_host::Host;

use crate::CtestError;

/// Run the dashboard script at `script` and return the exit code.
#[expect(dead_code, reason = "ctest -S is not implemented yet")]
pub(crate) fn run_script(host: &dyn Host, script: &Path) -> Result<i32, CtestError> {
    let _ = (host, script);
    Err(CtestError::Unsupported("ctest -S"))
}
