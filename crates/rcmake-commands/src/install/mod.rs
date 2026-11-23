//! Install and export commands: `install` and `export`.
//!
//! Wave 1 replaces each pending stub below with the real command in this module.

#![forbid(unsafe_code)]

use rcmake_eval::{CmdFlags, Registry};

pub(crate) fn register(registry: &mut Registry) {
    let project_only = CmdFlags { scriptable: false };
    for command in ["install", "export"] {
        registry.add(command, crate::pending_project_command, project_only);
    }
}
