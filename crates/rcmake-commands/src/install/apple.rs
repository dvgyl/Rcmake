use rcmake_base::BString;
use rcmake_eval::Cx;
use rcmake_model::{InstallOptions, TargetId, TargetKind};
use rcmake_resolve::ide::{BundleKind, bundle_kind};

use super::{Result, fail};

pub(super) fn validate_destinations(
    cx: &mut Cx<'_>,
    targets: &[TargetId],
    generic: &InstallOptions,
    groups: &[(BString, BString)],
    namelink_only: bool,
) -> Result<()> {
    let apple = cx.var(b"CMAKE_SYSTEM_NAME").map_or_else(
        || cx.var_is_on(b"APPLE"),
        |system| {
            matches!(
                system.as_slice(),
                b"Darwin" | b"iOS" | b"tvOS" | b"watchOS" | b"visionOS"
            )
        },
    );
    if !apple
        || generic
            .destination
            .as_ref()
            .is_some_and(|path| !path.is_empty())
    {
        return Ok(());
    }
    for &id in targets {
        let Some(target) = cx.project().target(id) else {
            continue;
        };
        let (group, kind) = match (target.kind, bundle_kind(target)) {
            (TargetKind::Executable, BundleKind::App { .. }) => {
                ("BUNDLE", "MACOSX_BUNDLE executable")
            }
            (
                TargetKind::SharedLibrary | TargetKind::StaticLibrary,
                BundleKind::Framework { .. },
            ) if namelink_only => continue,
            (TargetKind::SharedLibrary, BundleKind::Framework { .. }) => {
                ("FRAMEWORK", "shared library FRAMEWORK")
            }
            (TargetKind::StaticLibrary, BundleKind::Framework { .. }) => {
                ("FRAMEWORK", "static library FRAMEWORK")
            }
            _ => continue,
        };
        let destination = groups.iter().rev().find(|(key, _)| key == group.as_bytes());
        if destination.is_none_or(|(_, path)| path.is_empty()) {
            return Err(fail(format!(
                "TARGETS given no {group} DESTINATION for {kind} target \"{}\".",
                target.name
            )));
        }
    }
    Ok(())
}
