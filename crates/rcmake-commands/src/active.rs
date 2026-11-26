use rcmake_eval::{BuiltinFn, CmdFlags, Registry};

use crate::util::values;

mod cache;
mod directories;
mod file_api;
mod test_driver;

#[cfg(test)]
mod tests;

pub(crate) fn register(registry: &mut Registry) {
    const PROJECT_COMMANDS: &[(&str, BuiltinFn)] = &[
        ("aux_source_directory", directories::aux_source_directory),
        ("cmake_file_api", file_api::query),
        ("create_test_sourcelist", test_driver::create),
        (
            "include_external_msproject",
            directories::include_external_msproject,
        ),
        (
            "include_regular_expression",
            directories::include_regular_expression,
        ),
    ];
    for &(name, command) in PROJECT_COMMANDS {
        registry.add(name, command, CmdFlags { scriptable: false });
    }
    registry.add("load_cache", cache::load, CmdFlags { scriptable: true });
}
