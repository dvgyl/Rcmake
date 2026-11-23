//! Dashboard files for `-T Test`: `Testing/TAG` and `Testing/<tag>/Test.xml`.

use rcmake_base::BString;

use crate::{CtestError, CtestOutcome};

/// Render `Test.xml` for `outcome`.
#[expect(dead_code, reason = "-T Test is not implemented yet")]
pub(crate) fn render_test_xml(outcome: &CtestOutcome) -> Result<BString, CtestError> {
    let _ = outcome;
    Err(CtestError::Unsupported("-T Test"))
}
