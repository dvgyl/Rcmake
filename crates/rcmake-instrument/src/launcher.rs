//! Command launcher that records a snippet for a wrapped command (`ctest --instrument`
//! and the compile and link launchers of instrumented builds).

use std::path::Path;

use rcmake_base::BString;
use rcmake_host::Host;

use crate::{InstrumentError, SnippetRole};

/// Run `command` in `build_dir`, record a snippet with `role`, and return the exit
/// code of the command.
///
/// # Errors
///
/// Returns [`InstrumentError::Unsupported`] until the launcher is implemented.
pub fn run_instrumented(
    host: &dyn Host,
    build_dir: &Path,
    role: SnippetRole,
    command: &[BString],
) -> Result<i32, InstrumentError> {
    let _ = (host, build_dir, role, command);
    Err(InstrumentError::Unsupported("the instrumentation launcher"))
}
