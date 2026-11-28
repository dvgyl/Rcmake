//! Path syntax and checks for evaluated usage properties.

use rcmake_base::BString;
use rcmake_host::Host;
use rcmake_model::{DirId, ProjectModel};

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

fn windows_host_path_rules(model: &ProjectModel, dir: DirId, host: Option<&dyn Host>) -> bool {
    if let Some(host_system) = crate::lower::definition(model, dir, b"CMAKE_HOST_SYSTEM_NAME") {
        if host_system == b"CYGWIN" {
            return false;
        }
        if !host_system.is_empty() {
            return host_system.starts_with(b"Windows");
        }
    }
    if let Some(host) = host {
        return host.system().sysname.starts_with(b"Windows")
            || model.dir(DirId::ROOT).source_dir.get(1) == Some(&b':');
    }
    cfg!(windows) || model.dir(DirId::ROOT).source_dir.get(1) == Some(&b':')
}

pub(crate) fn windows_path_rules(
    model: &ProjectModel,
    dir: DirId,
    paths: Option<&SourcePaths<'_>>,
) -> bool {
    windows_host_path_rules(model, dir, paths.map(|paths| paths.host))
}

pub(crate) fn full_path_for_model(model: &ProjectModel, dir: DirId, path: &[u8]) -> bool {
    full_path(path, windows_host_path_rules(model, dir, None))
}

pub(crate) fn full_path_for_host(
    model: &ProjectModel,
    dir: DirId,
    host: &dyn Host,
    path: &[u8],
) -> bool {
    full_path(path, windows_host_path_rules(model, dir, Some(host)))
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
        path.get(1) == Some(&b':') || matches!(path.first(), Some(b'/' | b'\\'))
    } else {
        matches!(path.first(), Some(b'/' | b'~'))
    }
}
