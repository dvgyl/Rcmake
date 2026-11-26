//! Validate the target intermediate directory strategies.

use indexmap::{IndexMap, map::Entry};
use rcmake_base::BString;
use rcmake_diag::MessageKind;
use rcmake_host::{Host, Stream};
use rcmake_model::{CacheEntry, CacheType, DirId, ProjectModel, Target, TargetKind};

use crate::{
    ResolveError, cxx_modules::generation_location, lower, sources::SourcePaths,
    sources::absolute_path,
};

/// Reject an unknown `CMAKE_INTERMEDIATE_DIR_STRATEGY` or
/// `CMAKE_AUTOGEN_INTERMEDIATE_DIR_STRATEGY`.
///
/// The cache entry has priority over the environment variable. The supported
/// strategies are `FULL` and `SHORT`.
pub(super) fn prepare(
    model: &ProjectModel,
    host: &dyn Host,
) -> Result<Option<ProjectModel>, ResolveError> {
    let mut initialized = None;
    for (name, kind) in [
        (b"CMAKE_INTERMEDIATE_DIR_STRATEGY".as_slice(), ""),
        (b"CMAKE_AUTOGEN_INTERMEDIATE_DIR_STRATEGY", "autogen "),
    ] {
        let value = if let Some(entry) = model.cache.get(name) {
            entry.value.clone()
        } else if let Some(value) = host.env_get(name) {
            initialized
                .get_or_insert_with(|| model.clone())
                .cache
                .insert(
                    name.into(),
                    CacheEntry {
                        value: value.clone(),
                        ty: CacheType::String,
                        help: Default::default(),
                        advanced: false,
                        props: Default::default(),
                    },
                );
            value
        } else {
            continue;
        };
        if matches!(value.as_slice(), b"FULL" | b"SHORT") {
            continue;
        }
        let body = format!("Unsupported {kind}intermediate directory strategy '{value}'");
        host.write_out(
            Stream::Stderr,
            format!("CMake Error:\n  {body}\n\n\n").as_bytes(),
        );
        return Err(ResolveError::Reported(body.into()));
    }
    Ok(initialized)
}

/// Warn when two targets select the same `SHORT` intermediate directory.
///
/// The `SHORT` strategy names the directory after hashes of the target name
/// and its directory, so two names can collide. CMake keeps the shared
/// directory and reports the first target and each later target that uses it.
///
/// Only generators with hashed support directories take part. Imported,
/// generator-created, and `try_compile` targets do not register a directory.
pub(super) fn warn_collisions(model: &ProjectModel, host: &dyn Host) {
    if !lower::short_object_paths(model) || lower::cuda::in_try_compile(model) {
        return;
    }
    let root = &model.dir(DirId::ROOT).binary_dir;
    let mut owners: IndexMap<BString, &Target> = IndexMap::new();
    for (_, target) in model.targets() {
        if target.is_imported()
            || matches!(
                target.kind,
                TargetKind::InterfaceLibrary | TargetKind::GlobalTarget
            )
        {
            continue;
        }
        let path = absolute_path(root, &lower::artifacts::short_support_dir(model, target));
        match owners.entry(path) {
            Entry::Vacant(entry) => {
                entry.insert(target);
            }
            Entry::Occupied(entry) => {
                let body = format!(
                    "The '{}' and '{}' targets share an intermediate directory\n    {}\nwhich may cause problems with the build graph.  This project is not compatible with the `SHORT` target intermediate directory strategy.  Possible remedies include: moving the target into different directories or renaming a target.",
                    entry.get().name,
                    target.name,
                    entry.key()
                );
                SourcePaths::new(host).emit(
                    model,
                    MessageKind::Warning,
                    generation_location(model, target),
                    body.into(),
                );
            }
        }
    }
}
