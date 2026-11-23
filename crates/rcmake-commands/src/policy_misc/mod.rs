//! Policy, diagnostic, argument parsing, file configuration, and metadata commands.

#![forbid(unsafe_code)]

use rcmake_base::BString;
use rcmake_eval::{CmdError, CmdFlags, CmdResult, Cx, Registry};
use rcmake_expr::ExpandedArg;

mod arguments;
mod files;
mod language;
mod policies;
mod project;
mod properties;
mod system;

#[cfg(test)]
mod tests;

pub(crate) fn register(registry: &mut Registry) {
    let script = CmdFlags { scriptable: true };
    let project_only = CmdFlags { scriptable: false };
    registry.add("cmake_minimum_required", policies::minimum_required, script);
    registry.add("cmake_policy", policies::policy, script);
    registry.add("cmake_diagnostic", policies::diagnostic, script);
    registry.add("cmake_parse_arguments", arguments::parse, script);
    registry.add("configure_file", files::configure_file, script);
    registry.add("get_filename_component", files::filename_component, script);
    registry.add("set_property", properties::set, script);
    registry.add("get_property", properties::get, script);
    registry.add("get_directory_property", properties::get_directory, script);
    registry.add(
        "set_directory_properties",
        properties::set_directory,
        script,
    );
    registry.add(
        "set_source_files_properties",
        properties::set_sources,
        project_only,
    );
    registry.add(
        "get_source_file_property",
        properties::get_source,
        project_only,
    );
    registry.add("get_cmake_property", properties::get_cmake, script);
    registry.add("cmake_host_system_information", system::query, script);
    registry.add("cmake_language", language::language, script);
    registry.add("site_name", system::site_name, script);
    registry.add("variable_watch", system::variable_watch, script);
    registry.add("define_property", properties::define, project_only);
    registry.add("project", project::project, project_only);
    for command in [
        "add_executable",
        "add_library",
        "add_subdirectory",
        "add_custom_target",
        "add_custom_command",
        "add_custom_rule",
        "add_dependencies",
        "target_sources",
        "target_include_directories",
        "target_link_libraries",
        "target_compile_definitions",
        "target_compile_options",
        "target_compile_features",
        "target_link_options",
        "target_link_directories",
        "install",
        "export",
        "enable_language",
        "enable_testing",
        "add_test",
        "discover_tests",
    ] {
        registry.add(command, pending_project_command, project_only);
    }
}

fn pending_project_command(_: &mut Cx<'_>, _: &[ExpandedArg]) -> CmdResult {
    Err(status("requires build-system support."))
}

fn values(args: &[ExpandedArg]) -> Vec<&[u8]> {
    args.iter().map(|arg| arg.value.as_slice()).collect()
}

fn status(text: impl Into<BString>) -> CmdError {
    CmdError::Status(text.into())
}

fn quoted(before: &str, value: &[u8], after: &str) -> BString {
    [before.as_bytes(), value, after.as_bytes()].concat().into()
}

fn unsigned(value: &[u8]) -> Option<u32> {
    if value.is_empty() || !value.iter().all(u8::is_ascii_digit) {
        return None;
    }
    core::str::from_utf8(value).ok()?.parse().ok()
}
