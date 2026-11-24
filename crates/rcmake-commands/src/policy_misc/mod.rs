//! Policy, diagnostic, argument parsing, file configuration, and metadata commands.

#![forbid(unsafe_code)]

use rcmake_base::BString;
use rcmake_eval::{CmdFlags, Registry};
use rcmake_expr::ExpandedArg;

mod arguments;
pub(crate) mod experimental;
pub(crate) mod files;
pub(crate) mod introspection;
mod language;
mod policies;
mod project;
mod properties;
mod system;

pub(crate) use policies::issue_warning;

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
    registry.add_raw("cmake_language", language::language, script);
    registry.add("site_name", system::site_name, script);
    registry.add("variable_watch", system::variable_watch, script);
    registry.add("define_property", properties::define, project_only);
    registry.add("project", project::project, project_only);
}

fn values(args: &[ExpandedArg]) -> Vec<&[u8]> {
    args.iter().map(|arg| arg.value.as_slice()).collect()
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
