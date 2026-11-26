//! `install(TARGETS)` argument parsing.

use rcmake_base::BString;
use rcmake_eval::Cx;
use rcmake_model::{InstallKind, InstallOptions, TargetId, UsageScope};

use super::{
    Args, Result, apple, artifact, common, components, destination, fail, keyword, runtime,
    target_ids, unknown, value, values,
};

pub(super) fn parse(
    cx: &mut Cx<'_>,
    args: &mut Args<'_>,
    options: &mut InstallOptions,
    destinations: &mut Vec<(usize, BString, bool)>,
) -> Result<(InstallKind, bool, std::collections::BTreeSet<BString>)> {
    let argument_count = args.len() + 1;
    let names = values(args);
    let mut export = None;
    let mut artifact_options = InstallOptions::default();
    let mut destination_argument = None;
    let mut group: Option<&[u8]> = None;
    let mut anonymous_dependencies = false;
    let mut dependency_set = None;
    let mut transitive = false;
    let mut namelink = None;
    let mut namelink_component = None;
    let mut component_groups = Vec::new();
    let mut file_set = None;
    let mut library_namelink_only = false;
    let mut apple_destinations = Vec::new();
    let mut file_sets = Vec::new();
    while let Some(key) = args.next() {
        match key {
            b"EXPORT" if group.is_none() => export = Some(value(args, key)?.into()),
            b"RUNTIME_DEPENDENCY_SET" if group.is_none() => {
                dependency_set = Some(value(args, key)?).filter(|name| !name.is_empty());
            }
            b"RUNTIME_DEPENDENCIES" if group.is_none() => anonymous_dependencies = true,
            b"RUNTIME_DEPENDENCY_TARGETS" if group.is_none() => transitive = true,
            key if anonymous_dependencies && group.is_none() && runtime::filter_keyword(key) => {
                values(args);
            }
            b"INCLUDES" => {
                if args.next_if_eq(&b"DESTINATION".as_slice()).is_some()
                    || args.next_if(|token| !keyword(token)).is_some()
                {
                    options
                        .includes_destination
                        .extend(values(args).into_iter().map(BString::from));
                }
            }
            b"NAMELINK_COMPONENT" | b"NAMELINK_ONLY" | b"NAMELINK_SKIP" => {
                if !matches!(group, Some(b"LIBRARY" | b"ARCHIVE")) {
                    return Err(fail(format!(
                        "TARGETS given {key} option not in LIBRARY or ARCHIVE group.  The {key} option may be specified only following LIBRARY or ARCHIVE.",
                        key = BString::from(key),
                    )));
                }
                if key == b"NAMELINK_COMPONENT" {
                    namelink_component = Some(value(args, key)?);
                } else if namelink
                    .replace(key)
                    .is_some_and(|previous| previous != key)
                {
                    return Err(fail("TARGETS given both NAMELINK_ONLY and NAMELINK_SKIP."));
                }
                library_namelink_only |= group == Some(b"LIBRARY") && key == b"NAMELINK_ONLY";
            }
            key if artifact(key) => {
                if let Some(group) = group {
                    component_groups.push(components::Group {
                        key: group,
                        file_set,
                        component: artifact_options.component.take(),
                        namelink,
                        namelink_component,
                    });
                }
                destinations.extend(
                    destination_argument
                        .take()
                        .zip(artifact_options.destination.take())
                        .map(|(index, path)| (index, path, group == Some(b"CXX_MODULES_BMI"))),
                );
                artifact_options = InstallOptions::default();
                group = Some(key);
                namelink = None;
                namelink_component = None;
                file_set = None;
                if key == b"FILE_SET" {
                    let name = args
                        .next_if(|token| !token.is_empty() && !keyword(token))
                        .ok_or_else(|| {
                            fail("TARGETS given FILE_SET option without file set name.")
                        })?;
                    file_sets.push(name);
                    file_set = Some(name);
                }
            }
            _ => {
                if key == b"DESTINATION" && args.peek().is_none_or(|next| keyword(next)) {
                    continue;
                }
                if group.is_some() && key == b"DESTINATION" {
                    destination_argument = Some(argument_count - args.len());
                    if let (Some(group @ (b"BUNDLE" | b"FRAMEWORK")), Some(path)) =
                        (group, args.peek())
                    {
                        apple_destinations.push((group.into(), destination(cx, path)));
                    }
                }
                let options = if group.is_some() {
                    &mut artifact_options
                } else {
                    &mut *options
                };
                if !common(cx, b"TARGETS", options, key, args)? {
                    return Err(unknown(key));
                }
            }
        }
    }
    if transitive && export.is_some() {
        return Err(fail(
            "TARGETS RUNTIME_DEPENDENCY_TARGETS cannot be used with EXPORT.",
        ));
    }
    runtime::validate_target_dependencies(
        cx,
        anonymous_dependencies,
        dependency_set,
        options,
        &apple_destinations,
    )?;
    let targets = target_ids(cx, names, true)?;
    if let Some(group) = group {
        component_groups.push(components::Group {
            key: group,
            file_set,
            component: artifact_options.component.take(),
            namelink,
            namelink_component,
        });
    }
    destinations.extend(
        destination_argument
            .zip(artifact_options.destination)
            .map(|(index, path)| (index, path, group == Some(b"CXX_MODULES_BMI"))),
    );
    apple::validate_destinations(
        cx,
        &targets,
        options,
        &apple_destinations,
        library_namelink_only,
    )?;
    if export.is_some() {
        require_exported_file_sets(cx, &targets, &file_sets)?;
    }
    if anonymous_dependencies || dependency_set.is_some() {
        runtime::register_target_bundles(cx, dependency_set, &targets)?;
    }
    let components = components::for_targets(
        cx,
        &targets,
        options,
        &component_groups,
        anonymous_dependencies,
    );
    Ok((
        InstallKind::Targets { targets, export },
        library_namelink_only,
        components,
    ))
}

fn require_exported_file_sets(
    cx: &mut Cx<'_>,
    targets: &[TargetId],
    file_sets: &[&[u8]],
) -> Result<()> {
    let project = cx.project();
    for target in targets.iter().filter_map(|id| project.target(*id)) {
        if target
            .file_sets
            .iter()
            .filter_map(|id| project.file_set(*id))
            .any(|set| {
                set.scope != UsageScope::Private && !file_sets.contains(&set.name.as_slice())
            })
        {
            return Err(fail(
                [
                    b"TARGETS target ",
                    target.name.as_slice(),
                    b" is exported but not all of its interface file sets are installed",
                ]
                .concat(),
            ));
        }
    }
    Ok(())
}
