//! Path syntax and checks for evaluated usage properties.

use rcmake_base::BString;
use rcmake_model::{DirId, GeneratorKind, ProjectModel};

use super::{SourcePaths, UsageResolver};

impl UsageResolver<'_> {
    pub(super) fn usage_path(&self, dir: DirId, property: &[u8], mut value: BString) -> BString {
        if self.property_reads == 0
            && (property.ends_with(b"INCLUDE_DIRECTORIES")
                || property.ends_with(b"LINK_DIRECTORIES"))
            && !value.starts_with(b"$<")
            && (windows_path_rules(self.model, dir, self.source_paths())
                && (value.get(1) == Some(&b':') || matches!(value.first(), Some(b'/' | b'\\'))))
        {
            for byte in value.iter_mut() {
                if *byte == b'\\' {
                    *byte = b'/';
                }
            }
        }
        value
    }
}

pub(super) fn windows_path_rules(
    model: &ProjectModel,
    dir: DirId,
    paths: Option<&SourcePaths<'_>>,
) -> bool {
    paths.map_or(cfg!(windows), |paths| {
        paths.host.system().sysname == b"Windows"
    }) || (crate::lower::definition(model, dir, b"CMAKE_SYSTEM_NAME")
        .is_none_or(|system| system != b"CYGWIN")
        && crate::lower::artifacts::is_dll_platform(model, dir))
        || model
            .generator
            .is_some_and(|generator| matches!(generator, GeneratorKind::VisualStudio(_)))
}

pub(crate) fn usage_path_is_full(
    model: &ProjectModel,
    paths: &SourcePaths<'_>,
    dir: DirId,
    path: &[u8],
) -> bool {
    full_path(path, windows_path_rules(model, dir, Some(paths)))
}

/// A path in an interface property that CMake rejects for a consumer.
pub(super) enum PathProblem<'a> {
    /// An include directory of an imported target does not exist.
    Missing(&'a BString),
    /// The path is not a full path.
    Relative(&'a BString),
}

/// Find the first rejected path in the evaluated `INTERFACE_INCLUDE_DIRECTORIES` or
/// `INTERFACE_SOURCES` of `target`, in CMake's order of checks.
pub(super) fn interface_path_problem<'a>(
    paths: &SourcePaths<'_>,
    target: &rcmake_model::Target,
    property: &[u8],
    values: &'a [BString],
    windows: bool,
) -> Option<PathProblem<'a>> {
    let imported = property == b"INTERFACE_INCLUDE_DIRECTORIES" && target.is_imported();
    values.iter().find_map(|value| {
        if imported
            && paths
                .host
                .metadata(
                    &rcmake_host::path::to_native(value),
                    rcmake_host::Follow::Yes,
                )
                .is_err()
        {
            Some(PathProblem::Missing(value))
        } else if !full_path(value, windows) {
            Some(PathProblem::Relative(value))
        } else {
            None
        }
    })
}
fn full_path(path: &[u8], windows: bool) -> bool {
    if windows {
        path.get(1) == Some(&b':') || path.starts_with(b"/")
    } else {
        matches!(path.first(), Some(b'/' | b'~'))
    }
}
