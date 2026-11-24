//! Lowering of resolved targets into a build graph and install manifest.

pub(crate) mod apple;
mod artifacts;
mod compile;
mod custom;
pub(crate) mod framework;
mod graph;
mod install;
mod install_rpath;
mod launcher;
mod link;
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
pub(crate) use compile::language_flags;
pub(crate) use custom::command_outputs;
pub use graph::lower;
pub(crate) use graph::lower_with_inputs;
pub(crate) use install::lower_install_manifest_with_configs;
pub use install::{collect_metadata, lower_install_manifest};
pub use launcher::instrumentation_launcher;
pub(crate) use link::{ide_rpaths, install_name_dir, install_name_prefix};
pub(crate) use modules::scan_responses;
pub use modules::{bmi_script_path, lower_modules};
pub(crate) use policy_flags::warn_about_wrappers;
pub(crate) use standard::required_standard;

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

fn target_property<'a>(
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
