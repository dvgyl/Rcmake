//! Native Xcode project and build command generation.

#![forbid(unsafe_code)]

use std::path::Path;

use rcmake_gen_common::{
    GenError,
    ide::{UtilityKind, project_roots, utility_targets},
    write_if_changed,
};
use rcmake_host::{
    Host,
    path::{from_native, to_native},
};
use rcmake_model::{DirId, GeneratorKind, ProjectModel};
use rcmake_resolve::GenerationResult;

pub mod build;
pub mod ids;
pub mod object;
pub mod phases;
pub mod plist;
pub mod project;
pub mod rerun;
pub mod scheme;
pub mod settings;
pub mod workspace;

pub use build::build_commands;

/// Add the Xcode GUIDs to the model cache before the caller saves it.
pub fn populate_guid_cache(result: &GenerationResult, model: &mut ProjectModel, host: &dyn Host) {
    project::populate_guid_cache(result, model, host);
}

/// Write the Xcode project files of `result` into the build directory `bin`.
///
/// # Errors
///
/// Returns an error if the project data is invalid or an output cannot be written.
pub fn generate(
    result: &GenerationResult,
    model: &ProjectModel,
    host: &dyn Host,
    bin: &Path,
) -> Result<(), GenError> {
    let utilities = utility_targets(GeneratorKind::Xcode, model, host);
    let projects = project_roots(GeneratorKind::Xcode, model);
    for root in &projects {
        let dir = if root.dir == DirId::ROOT {
            bin.to_owned()
        } else {
            to_native(&model.dir(root.dir).binary_dir)
        };
        project::generate(result, model, host, bin, &dir, root, &utilities)?;
    }
    if utilities
        .iter()
        .any(|target| target.kind == UtilityKind::ZeroCheck)
    {
        let source = text(&model.dir(DirId::ROOT).source_dir);
        rerun::write(
            host,
            bin,
            &source,
            &result.metadata,
            &project::cmake(result, model),
        )?;
    }
    Ok(())
}

pub(crate) fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

pub(crate) fn file_name(name: &str) -> Result<&str, GenError> {
    if name.is_empty() || matches!(name, "." | "..") || name.contains(['/', '\\', '\0', '\n', '\r'])
    {
        return Err(GenError::Invalid(
            format!("Invalid Xcode file name {name:?}").into(),
        ));
    }
    Ok(name)
}

pub(crate) fn write_file(host: &dyn Host, path: &Path, data: &str) -> Result<(), GenError> {
    if let Some(parent) = path.parent() {
        host.create_dir_all(parent)
            .map_err(|error| GenError::Write {
                path: from_native(parent),
                message: error.to_string().into(),
            })?;
    }
    write_if_changed(host, path, data.as_bytes())
}
