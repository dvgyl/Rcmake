//! `cmake --workflow --preset <name> [--fresh] [--list-presets[=defined]]` (ADR-011):
//! run the configure, build, test, and package presets of a workflow preset in order.

use std::sync::Arc;

use rcmake_base::BString;
use rcmake_host::Host;

use crate::emit_cli_error;

/// Run `cmake --workflow` with `args` (the arguments after `--workflow`) and return
/// the exit code.
pub(crate) fn run(args: &[BString], host: Arc<dyn Host>) -> i32 {
    let _ = args;
    emit_cli_error(host.as_ref(), "--workflow is not supported yet");
    1
}
