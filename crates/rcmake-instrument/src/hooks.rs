//! Hook dispatch: index the snippets and run the query callbacks through [`Host`].

use std::path::Path;

use rcmake_host::Host;

use crate::{Hook, InstrumentError};

/// Run indexing and callbacks for `hook` in the build tree `build_dir`, when a query
/// asks for it.
///
/// # Errors
///
/// Returns [`InstrumentError::Unsupported`] until hook dispatch is implemented.
pub fn run_hook(host: &dyn Host, build_dir: &Path, hook: Hook) -> Result<(), InstrumentError> {
    let _ = (host, build_dir, hook);
    Err(InstrumentError::Unsupported("instrumentation hooks"))
}
