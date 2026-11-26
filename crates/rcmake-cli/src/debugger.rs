//! `--debugger` Debug Adapter Protocol session lifecycle for top-level CLI invocations.

use std::sync::Arc;

use rcmake_eval::Interp;
use rcmake_host::Host;

use crate::options::Options;

/// Per-invocation active DAP session handle (owned by one top-level configure or `-P` run).
pub(crate) struct Session;

/// Report whether `-E capabilities` advertises DAP debugger support on this platform.
pub(crate) const fn capabilities_supported() -> bool {
    false
}

/// Start the debugger adapter if `--debugger` was requested on this invocation.
///
/// Called at CMake's startup point: after initial `-C` and automatic `PreLoad.cmake`
/// scripts succeed for configure mode, or before script execution for `-P` mode.
pub(crate) fn start_if_enabled(
    _host: Arc<dyn Host>,
    _opts: &Options,
) -> Result<Option<Session>, i32> {
    Ok(None)
}

/// Attach the active debugger session (if any) to `interp`.
pub(crate) fn attach(_session: Option<&Session>, _interp: &mut Interp) {}

/// Finish the active debugger session with `exit_code`.
pub(crate) fn finish(_session: Option<Session>, _exit_code: i32) {}
