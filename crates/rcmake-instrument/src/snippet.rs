//! Snippet and index file writers (`<build>/.cmake/instrumentation/v1/data/`).

use std::path::{Path, PathBuf};

use rcmake_host::Host;

use crate::{IndexFile, InstrumentError, Snippet};

/// Write `snippet` into the data directory `data_dir`. Returns the file path.
///
/// # Errors
///
/// Returns [`InstrumentError::Unsupported`] until snippet writing is implemented.
pub fn write_snippet(
    host: &dyn Host,
    data_dir: &Path,
    snippet: &Snippet,
) -> Result<PathBuf, InstrumentError> {
    let _ = (host, data_dir, snippet);
    Err(InstrumentError::Unsupported(
        "instrumentation snippet writing",
    ))
}

/// Write `index` into `data_dir/index/`. Returns the file path.
///
/// # Errors
///
/// Returns [`InstrumentError::Unsupported`] until index writing is implemented.
pub fn write_index(
    host: &dyn Host,
    data_dir: &Path,
    index: &IndexFile,
) -> Result<PathBuf, InstrumentError> {
    let _ = (host, data_dir, index);
    Err(InstrumentError::Unsupported(
        "instrumentation index writing",
    ))
}
