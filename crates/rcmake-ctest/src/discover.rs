//! Test-time expansion of `discover_tests()` entries: run the discovery command and
//! create one test for each output line that matches `DISCOVERY_MATCH`.

use rcmake_base::BString;

use crate::CtestError;

/// Names of the tests that the discovery output `output` defines.
#[expect(
    dead_code,
    reason = "discover_tests() expansion is not implemented yet"
)]
pub(crate) fn expand(output: &[u8]) -> Result<Vec<BString>, CtestError> {
    let _ = output;
    Err(CtestError::Unsupported("discover_tests() expansion"))
}
