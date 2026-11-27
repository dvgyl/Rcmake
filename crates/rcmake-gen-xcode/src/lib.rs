//! Native Xcode project and build command generation.

#![forbid(unsafe_code)]

use std::collections::HashSet;
use std::path::Path;

use rcmake_gen_common::{
    GenError,
    compile_commands_native::MultiConfigCompileDatabase,
    ide::{UtilityKind, project_roots, utility_targets},
    write_output,
};
use rcmake_host::{Host, path::to_native};
use rcmake_model::{DirId, GeneratorKind, ProjectModel, TargetId};
use rcmake_resolve::GenerationResult;

pub mod build;
mod compile_db;
#[allow(
    dead_code,
    reason = "The language activation wave uses this emitter or planner"
)]
mod fortran_asm;
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
pub fn populate_guid_cache(_result: &GenerationResult, model: &mut ProjectModel, host: &dyn Host) {
    project::populate_guid_cache(model, host);
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
    generate_with_compile_db(result, model, host, bin, None).map(drop)
}

/// Write the Xcode project files of `result` into `bin` and, when `export_targets` is
/// `Some`, collect per-configuration native compile database rows.
///
/// # Errors
///
/// Returns an error if the project data is invalid or an output cannot be written.
pub fn generate_with_compile_db(
    result: &GenerationResult,
    model: &ProjectModel,
    host: &dyn Host,
    bin: &Path,
    export_targets: Option<&HashSet<TargetId>>,
) -> Result<Option<MultiConfigCompileDatabase>, GenError> {
    for (dir, _) in model.dirs() {
        validate_link_phase_mode(model.definition(dir, b"CMAKE_XCODE_LINK_BUILD_PHASE_MODE"))?;
    }
    for (_, target) in model.targets() {
        validate_link_phase_mode(target.props.get(b"XCODE_LINK_BUILD_PHASE_MODE"))?;
    }
    let utilities = utility_targets(GeneratorKind::Xcode, model, host);
    let projects = project_roots(GeneratorKind::Xcode, model);
    let indices = project::XcodeIndices::new(model, result);
    for root in &projects {
        let dir = if root.dir == DirId::ROOT {
            bin.to_owned()
        } else {
            to_native(&model.dir(root.dir).binary_dir)
        };
        project::generate(result, model, host, (bin, &dir), root, &utilities, &indices)?;
    }
    if utilities
        .iter()
        .any(|target| target.kind == UtilityKind::ZeroCheck)
    {
        let verify_globs = rcmake_gen_common::glob::write_verification(Some(model), bin, host)?;
        let source = text(&model.dir(DirId::ROOT).source_dir);
        rerun::write(
            host,
            bin,
            &source,
            &result.metadata,
            &project::cmake(result, model),
            verify_globs,
        )?;
    }
    let db = export_targets
        .map(|selected| compile_db::collect(result, model, host, bin, &indices, selected))
        .transpose()?;
    Ok(db)
}

fn validate_link_phase_mode(mode: Option<&rcmake_base::BString>) -> Result<(), GenError> {
    if let Some(mode) = mode
        && !matches!(mode.as_slice(), b"NONE" | b"BUILT_ONLY" | b"KNOWN_LOCATION")
    {
        return Err(GenError::Invalid(
            format!("Invalid value for XCODE_LINK_BUILD_PHASE_MODE: {mode}").into(),
        ));
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
    write_output(host, path, data.as_bytes())
}
