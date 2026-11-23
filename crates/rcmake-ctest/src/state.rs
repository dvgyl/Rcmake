//! Persistent test state in `Testing/Temporary/` (`LastTestsFailed.log`,
//! `CTestCostData.txt`) for `--rerun-failed` and cost-based scheduling.

use std::path::Path;

use rcmake_base::BString;
use rcmake_host::Host;

use crate::CtestError;

/// Names of the tests that failed in the last run in the build tree `dir`.
#[expect(dead_code, reason = "--rerun-failed is not implemented yet")]
pub(crate) fn last_failed(host: &dyn Host, dir: &Path) -> Result<Vec<BString>, CtestError> {
    let _ = (host, dir);
    Err(CtestError::Unsupported("--rerun-failed"))
}
