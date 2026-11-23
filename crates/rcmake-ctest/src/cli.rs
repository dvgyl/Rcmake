//! `ctest` command-line parsing (moves here from `rcmake-cli`, ADR-011).

use rcmake_base::BString;

use crate::{CtestError, CtestOptions};

/// Parse the `ctest` arguments (without `argv[0]`).
#[expect(dead_code, reason = "rcmake-cli still parses the ctest command line")]
pub(crate) fn parse(args: &[BString]) -> Result<CtestOptions, CtestError> {
    let _ = args;
    Err(CtestError::Unsupported(
        "ctest command-line parsing in rcmake-ctest",
    ))
}
