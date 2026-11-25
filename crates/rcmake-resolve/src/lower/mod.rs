//! Lowering of resolved targets into a build graph and install manifest.

pub(crate) mod apple;
pub(crate) mod artifacts;
#[allow(
    dead_code,
    reason = "The language activation wave uses this emitter or planner"
)]
pub(crate) mod autogen;
mod compile;
#[allow(
    dead_code,
    reason = "The language activation wave uses this emitter or planner"
)]
pub(crate) mod cuda;
pub use cuda::ide_flags as cuda_ide_flags;
#[allow(
    dead_code,
    reason = "The language activation wave uses this emitter or planner"
)]
pub(crate) mod fortran;
pub use fortran::{fortran_depend_info_json, fortran_make_depend_info};
pub(crate) mod custom;
pub(crate) mod framework;
mod graph;
mod install;
mod install_rpath;
mod launcher;
mod link;
pub(crate) use link::framework_bundle_name;
mod modules;
mod pch;
mod policy_flags;
mod shell;
mod standard;

#[cfg(test)]
mod property_tests;
#[cfg(test)]
mod tests;

pub(crate) use install::exported_object_path;

use rcmake_base::{BString, path_lex::Lexical};
use rcmake_model::{DirId, ProjectModel, Target};

pub(crate) use artifacts::{
    build_relative, compile_object_path, install_object_name_strategy, is_darwin,
};
pub use artifacts::{compute_artifacts, linker_language, object_path, source_language};
pub(crate) use artifacts::{object_dir, short_object_paths};
pub use compile::codemodel_compile_flags;
pub(crate) use compile::{language_flags, msvc::debug_options as msvc_debug_options};
pub(crate) use custom::{checked_command_outputs, command_outputs};
pub use graph::lower;
pub(crate) use graph::lower_with_inputs;
pub(crate) use install::lower_install_manifest_with_configs;
pub use install::{
    collect_metadata, install_target_group_options, installs_unnamed_runtime_dependencies,
    lower_install_manifest,
};
pub use launcher::instrumentation_launcher;
pub use link::codemodel_link_flags;
pub(crate) use link::{
    ide_rpaths, install_name_dir, install_name_prefix, warn_install_name_policy,
};
pub(crate) use modules::scan_responses;
pub use modules::{bmi_script_path, lower_modules};
pub(crate) use pch::{IdeLowering, Plans as PchPlans};
pub(crate) use policy_flags::warn_about_wrappers;
pub use shell::{
    cmake_posix_quote, custom_shell_operator, rule_command_chain, simple_args as rule_arguments,
};
pub(crate) use standard::{
    checked_standard_flag, cmp0128_warnings, cxx_standard_mode, required_standard,
};

pub(crate) fn definition<'a>(
    model: &'a ProjectModel,
    dir: DirId,
    key: &[u8],
) -> Option<&'a BString> {
    if let Some(value) = model.definition(dir, key) {
        return Some(value);
    }
    // Models without variable snapshots can supply definitions as properties.
    model
        .ancestors(dir)
        .find_map(|ancestor| model.dir(ancestor).props.get(key))
        .or_else(|| model.global.props.get(key))
}

/// Return the frames of a backtrace that a CMake call stack shows.
///
/// Frames of included files have no line and no command. A call stack also
/// ends at the `CMakeLists.txt` file of the directory, so it omits the
/// `add_subdirectory` call of the parent directory.
pub fn listed_backtrace(backtrace: &rcmake_diag::Backtrace) -> rcmake_diag::Backtrace {
    let frames: Vec<_> = backtrace
        .frames()
        .filter(|frame| frame.line != 0 || frame.name.is_some())
        .take_while(|frame| {
            !frame
                .name
                .as_deref()
                .is_some_and(|name| name.eq_ignore_ascii_case(b"add_subdirectory"))
        })
        .cloned()
        .collect();
    frames
        .into_iter()
        .rev()
        .fold(rcmake_diag::Backtrace::default(), |trace, frame| {
            trace.push(frame)
        })
}

pub(crate) fn target_property<'a>(
    model: &'a ProjectModel,
    target: &'a Target,
    key: &[u8],
) -> Option<&'a BString> {
    target
        .props
        .get(key)
        .or_else(|| definition(model, target.dir, &[b"CMAKE_", key].concat()))
}

pub(crate) fn join(base: &[u8], path: &[u8]) -> BString {
    if path.is_empty() {
        base.into()
    } else if Lexical::windows(base).absolute() || Lexical::windows(path).absolute() {
        Lexical::windows(base).append(path)
    } else {
        Lexical::new(base).append(path)
    }
}

pub(crate) fn without_trailing_slashes(path: &[u8]) -> &[u8] {
    let root = Lexical::new(path);
    let root_len = root.root_name().len() + root.root_directory().len();
    let end = path
        .iter()
        .rposition(|&byte| byte != b'/')
        .map_or(0, |i| i + 1);
    path.get(..end.max(root_len)).unwrap_or(path)
}

/// Expand the `<NAME>` placeholders of a rule template with `lookup`. Unknown
/// placeholders stay as written.
#[must_use]
pub fn expand_rule(template: &[u8], lookup: impl Fn(&[u8]) -> Option<BString>) -> BString {
    let mut out = Vec::with_capacity(template.len());
    let mut rest = template;
    while let Some(open) = rest.iter().position(|&b| b == b'<') {
        out.extend_from_slice(rest.get(..open).unwrap_or_default());
        let tail = rest.get(open + 1..).unwrap_or_default();
        let name_len = tail
            .iter()
            .position(|&b| !(b.is_ascii_alphanumeric() || b == b'_'))
            .unwrap_or(tail.len());
        let name = tail.get(..name_len).unwrap_or_default();
        match (tail.get(name_len), lookup(name)) {
            (Some(b'>'), Some(value)) if !name.is_empty() => {
                out.extend_from_slice(&value);
                rest = tail.get(name_len + 1..).unwrap_or_default();
            }
            _ => {
                out.push(b'<');
                rest = tail;
            }
        }
    }
    out.extend_from_slice(rest);
    out.into()
}

pub(crate) fn output_name_keys(class: &str, config: &[u8]) -> Vec<BString> {
    let class: BString = format!("{class}_OUTPUT_NAME").into();
    let mut keys = Vec::new();
    if !config.is_empty() {
        keys.push([class.as_slice(), b"_", config].concat().into());
    }
    keys.push(class);
    if !config.is_empty() {
        keys.push([b"OUTPUT_NAME_", config].concat().into());
        keys.push([config, b"_OUTPUT_NAME"].concat().into());
    }
    keys.push("OUTPUT_NAME".into());
    keys
}
