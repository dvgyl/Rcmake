//! JUnit XML report (`--output-junit <file>`).

use rcmake_base::BString;

use crate::{CtestError, CtestOutcome};

/// Render the JUnit XML report of `outcome`.
#[expect(dead_code, reason = "--output-junit is not implemented yet")]
pub(crate) fn render(outcome: &CtestOutcome) -> Result<BString, CtestError> {
    let _ = outcome;
    Err(CtestError::Unsupported("--output-junit"))
}
