//! Custom command and target commands: `add_custom_command`, `add_custom_target`, and `add_custom_rule`.
//!
//! Wave 1 replaces each pending stub below with the real command in this module.

#![forbid(unsafe_code)]

use rcmake_eval::{CmdFlags, Registry};

pub(crate) fn register(registry: &mut Registry) {
    let project_only = CmdFlags { scriptable: false };
    for command in ["add_custom_command", "add_custom_target", "add_custom_rule"] {
        registry.add(command, crate::pending_project_command, project_only);
    }
}
