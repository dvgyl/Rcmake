//! Configure-time toolchain commands: `enable_language`, `try_compile`, and `try_run` (ADR-008).
//!
//! Wave 1 replaces each pending stub below with the real command in this module.

#![forbid(unsafe_code)]

use rcmake_eval::{CmdFlags, Registry};
mod try_compile;

pub(crate) fn register(registry: &mut Registry) {
    let project_only = CmdFlags { scriptable: false };
    registry.add("try_compile", try_compile::try_compile, project_only);
    registry.add("try_run", try_compile::try_run, project_only);
    registry.add(
        "enable_language",
        crate::pending_project_command,
        project_only,
    );
}
