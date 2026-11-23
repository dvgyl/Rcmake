//! Target commands: `add_executable`, `add_library`, `add_dependencies`, `target_sources`, and the `target_*` usage-requirement commands.
//!
//! Wave 1 replaces each pending stub below with the real command in this module.

#![forbid(unsafe_code)]

use rcmake_eval::{CmdFlags, Registry};

pub(crate) fn register(registry: &mut Registry) {
    let project_only = CmdFlags { scriptable: false };
    for command in [
        "add_executable",
        "add_library",
        "add_dependencies",
        "target_sources",
        "target_include_directories",
        "target_link_libraries",
        "target_compile_definitions",
        "target_compile_options",
        "target_compile_features",
        "target_link_options",
        "target_link_directories",
    ] {
        registry.add(command, crate::pending_project_command, project_only);
    }
}
