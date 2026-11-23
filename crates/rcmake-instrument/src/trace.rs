//! Google trace file writer (`data/trace/`, option `trace`).

use rcmake_base::BString;

use crate::{InstrumentError, Snippet};

/// Write the trace JSON for `snippets`.
///
/// # Errors
///
/// Returns [`InstrumentError::Unsupported`] until the trace writer is implemented.
pub fn write_trace(snippets: &[Snippet]) -> Result<BString, InstrumentError> {
    let _ = snippets;
    Err(InstrumentError::Unsupported(
        "instrumentation trace writing",
    ))
}
