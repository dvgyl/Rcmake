//! Policy, diagnostic, argument parsing, file configuration, and metadata commands.

#![forbid(unsafe_code)]

use rcmake_base::BString;
use rcmake_eval::{CmdError, CmdFlags, Registry};
use rcmake_expr::ExpandedArg;

mod arguments;
mod files;
mod language;
mod policies;
mod properties;
mod system;

#[cfg(test)]
mod tests;

pub(crate) fn register(registry: &mut Registry) {
    let script = CmdFlags { scriptable: true };
    registry.add("cmake_minimum_required", policies::minimum_required, script);
    registry.add("cmake_policy", policies::policy, script);
    registry.add("cmake_diagnostic", policies::diagnostic, script);
    registry.add("cmake_parse_arguments", arguments::parse, script);
    registry.add("configure_file", files::configure_file, script);
    registry.add("get_filename_component", files::filename_component, script);
    registry.add("set_property", properties::set, script);
    registry.add("get_property", properties::get, script);
    registry.add("get_cmake_property", properties::get_cmake, script);
    registry.add("cmake_host_system_information", system::query, script);
    registry.add("cmake_language", language::language, script);
    registry.add(
        "define_property",
        properties::define,
        CmdFlags { scriptable: false },
    );
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
