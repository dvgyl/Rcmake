//! Configure-time toolchain commands: `enable_language`, `try_compile`, and `try_run` (ADR-008).
//!
//! Wave 1 replaces each pending stub below with the real command in this module.

#![forbid(unsafe_code)]

use rcmake_eval::{CmdFlags, Registry};

pub(crate) fn register(registry: &mut Registry) {
    let project_only = CmdFlags { scriptable: false };
    registry.add(
        "enable_language",
        crate::pending_project_command,
        project_only,
    );
}
