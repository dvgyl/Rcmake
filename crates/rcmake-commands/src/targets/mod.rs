//! Configure-time target creation, properties, sources, and usage requirements.

#![forbid(unsafe_code)]

use rcmake_base::{BString, path_lex::Lexical};
use rcmake_diag::{Effect, MessageKind};
use rcmake_eval::{CmdError, CmdFlags, Cx, Registry};
use rcmake_expr::ExpandedArg;
use rcmake_model::{ModelError, TargetId, UsageScope};

mod create;
mod init;
mod link;
mod precompile;
mod properties;
mod sources;
mod usage;

pub(crate) use sources::{find_file_set, replace_file_set_paths, require_sycl, scope_name};

#[cfg(test)]
mod tests;

pub(crate) fn register(registry: &mut Registry) {
    let flags = CmdFlags { scriptable: false };
    registry.add("add_executable", create::executable, flags);
    registry.add("add_library", create::library, flags);
    registry.add("add_dependencies", properties::add_dependencies, flags);
    registry.add("set_target_properties", properties::set, flags);
    registry.add("get_target_property", properties::get, flags);
    registry.add("target_sources", sources::add, flags);
    registry.add(
        "target_include_directories",
        usage::include_directories,
        flags,
    );
    registry.add(
        "target_compile_definitions",
        usage::compile_definitions,
        flags,
    );
    registry.add("target_compile_options", usage::compile_options, flags);
    registry.add("target_compile_features", usage::compile_features, flags);
    crate::compat::project_command!(registry, "target_link_options", usage::link_options);
    crate::compat::project_command!(registry, "target_link_directories", usage::link_directories);
    registry.add("target_link_libraries", link::add, flags);
    crate::compat::project_command!(registry, "target_precompile_headers", precompile::add);
}

fn bytes(arg: &ExpandedArg) -> &[u8] {
    arg.value.as_slice()
}

fn status(message: impl Into<BString>) -> CmdError {
    CmdError::Status(message.into())
}

fn reported(cx: &mut Cx<'_>, message: impl Into<BString>) -> CmdError {
    cx.issue(MessageKind::Error, Effect::SetError, None, message.into());
    CmdError::Reported
}

fn model_error(error: ModelError) -> CmdError {
    status(error.to_string())
}

fn scope(value: &[u8]) -> Option<UsageScope> {
    match value {
        b"PRIVATE" => Some(UsageScope::Private),
        b"PUBLIC" => Some(UsageScope::Public),
        b"INTERFACE" => Some(UsageScope::Interface),
        _ => None,
    }
}

fn source_dir(cx: &mut Cx<'_>) -> BString {
    let dir = cx.current_dir_id();
    cx.project().dir(dir).source_dir.clone()
}

fn absolute(path: &[u8], dir: &[u8]) -> BString {
    if path.starts_with(b"$<") || Lexical::new(path).absolute() {
        return path.into();
    }
    let separator = if dir.ends_with(b"/") {
        b"".as_slice()
    } else {
        b"/"
    };
    [dir, separator, path].concat().into()
}

fn usage_target(cx: &mut Cx<'_>, name: &BString, what: &str) -> Result<TargetId, CmdError> {
    let dir = cx.current_dir_id();
    cx.project().find_target(dir, name).ok_or_else(|| {
        reported(
            cx,
            format!(
                "Cannot specify {what} for target \"{name}\" which is not built by this project."
            ),
        )
    })
}
