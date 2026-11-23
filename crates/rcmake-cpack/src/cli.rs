//! `cpack` command-line parsing.

use rcmake_base::BString;

use crate::{CPackOptions, CpackError};

/// Parse the `cpack` arguments (without `argv[0]`).
///
/// # Errors
///
/// Returns [`CpackError::Unsupported`] until command-line parsing is implemented.
pub fn parse(args: &[BString]) -> Result<CPackOptions, CpackError> {
    let _ = args;
    Err(CpackError::Unsupported("cpack"))
}
