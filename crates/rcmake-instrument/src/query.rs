//! Query file reading and merging (`<build>/.cmake/instrumentation/v1/query[/generated]/`).

use std::path::Path;

use rcmake_host::Host;

use crate::{InstrumentError, Query};

/// Parse one query file.
///
/// # Errors
///
/// Returns [`InstrumentError::Unsupported`] until query parsing is implemented.
pub fn parse_query(text: &[u8]) -> Result<Query, InstrumentError> {
    let _ = text;
    Err(InstrumentError::Unsupported(
        "instrumentation query parsing",
    ))
}

/// Read all query files of the build tree `build_dir` and the user configuration
/// directory, in deterministic order.
///
/// # Errors
///
/// Returns [`InstrumentError::Unsupported`] until query reading is implemented.
pub fn read_queries(host: &dyn Host, build_dir: &Path) -> Result<Vec<Query>, InstrumentError> {
    let _ = (host, build_dir);
    Err(InstrumentError::Unsupported(
        "instrumentation query reading",
    ))
}

/// Write `query` into `query/generated/` of `build_dir` (`cmake_instrumentation()`).
///
/// # Errors
///
/// Returns [`InstrumentError::Unsupported`] until query writing is implemented.
pub fn write_generated_query(
    host: &dyn Host,
    build_dir: &Path,
    query: &Query,
) -> Result<(), InstrumentError> {
    let _ = (host, build_dir, query);
    Err(InstrumentError::Unsupported(
        "instrumentation query writing",
    ))
}
