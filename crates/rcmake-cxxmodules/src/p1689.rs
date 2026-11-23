//! P1689r5 `.ddi` parser.

use crate::{ModulesError, P1689File};

/// Parse a P1689r5 `.ddi` file. `path` is used in messages.
///
/// # Errors
///
/// Returns [`ModulesError::Unsupported`] until the parser is implemented.
pub fn parse_ddi(text: &[u8], path: &[u8]) -> Result<P1689File, ModulesError> {
    let _ = (text, path);
    Err(ModulesError::Unsupported("P1689r5 dependency file parsing"))
}
