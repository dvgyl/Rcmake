//! Directory and test commands: `add_subdirectory` (through `Cx::run_subdirectory`), directory usage commands, `enable_testing`, `add_test`, and `discover_tests`.
//!
//! Wave 1 replaces each pending stub below with the real command in this module.

#![forbid(unsafe_code)]

use rcmake_eval::{CmdFlags, Registry};

pub(crate) fn register(registry: &mut Registry) {
    let project_only = CmdFlags { scriptable: false };
    for command in [
        "add_subdirectory",
        "enable_testing",
        "add_test",
        "discover_tests",
    ] {
        registry.add(command, crate::pending_project_command, project_only);
    }
}
