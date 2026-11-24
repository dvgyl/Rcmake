//! Target names in exported link interfaces.

use indexmap::IndexSet;
use rcmake_base::{BString, list, truth};
use rcmake_genex::{GenexNode, PreprocessMode, parse, render, rewrite_install_prefix};
use rcmake_model::{DirId, ExportKind, InstallKind, ProjectModel, Target, TargetId, TargetKind};
use std::collections::BTreeMap;

use crate::{
    ExportedTarget, ResolveError,
    exports::{export_name, export_property_items},
};

const LINK_LIBRARIES: &[u8] = b"INTERFACE_LINK_LIBRARIES";

#[derive(Clone)]
pub(crate) struct ExportPlacement {
    pub(crate) set: BString,
    pub(crate) namespace: BString,
    pub(crate) file: BString,
}

pub(crate) type ExportCatalog = BTreeMap<TargetId, Vec<ExportPlacement>>;

pub(crate) fn qualify(
    model: &ProjectModel,
    id: TargetId,
    members: &[TargetId],
    namespace: &[u8],
    mode: PreprocessMode,
    catalog: &ExportCatalog,
    exported: &mut ExportedTarget,
) -> Result<IndexSet<BString>, ResolveError> {
    let mut packages = IndexSet::new();
    let Some(target) = model.target(id) else {
        return Ok(packages);
    };
    if target.kind == TargetKind::Executable
        && !target
            .props
            .get(b"ENABLE_EXPORTS")
            .is_some_and(|flag| truth::is_on(flag))
    {
        return Ok(packages);
    }
    let contains = |set: &[u8]| {
        model
            .exports
            .get(set)
            .is_some_and(|set| set.targets.contains(&id))
    };
    let package_export = match mode {
        PreprocessMode::Install => model.installs.iter().any(|rule| {
            matches!(&rule.kind, InstallKind::PackageInfo(spec) if contains(&spec.export))
                && !model.dir_excluded_from_all(rule.dir)
        }),
        _ => model.build_exports.iter().any(
            |rule| matches!(&rule.kind, ExportKind::PackageInfo(spec) if contains(&spec.export)),
        ),
    };
    for key in [
        LINK_LIBRARIES,
        b"INTERFACE_LINK_LIBRARIES_DIRECT",
        b"INTERFACE_LINK_LIBRARIES_DIRECT_EXCLUDE",
    ] {
        let mut items = Vec::new();
        let mut append = |value: &[u8], dir: DirId| -> Result<(), ResolveError> {
            let mut name = |input: &[u8]| -> Result<Option<BString>, ResolveError> {
                let Some(id) = model.find_target(dir, input) else {
                    return Ok(None);
                };
                let Some(dependency) = model.target(id) else {
                    return Ok(None);
                };
                if dependency.is_imported() {
                    if let Some(package) = dependency
                        .props
                        .get(b"EXPORT_FIND_PACKAGE_NAME")
                        .filter(|name| !name.is_empty())
                        .or_else(|| dependency.props.get(b"_RCMAKE_FIND_PACKAGE_NAME"))
                    {
                        packages.insert(package.clone());
                    }
                    return Ok(Some(dependency.name.clone()));
                }
                let exported = export_name(dependency);
                if members.contains(&id) {
                    return Ok(Some([namespace, exported.as_slice()].concat().into()));
                }
                let placements = catalog.get(&id).map_or(&[][..], Vec::as_slice);
                let Some(first) = placements.first() else {
                    return Err(closure_error(
                        model,
                        target,
                        dependency,
                        mode,
                        "that is not in any export set.".into(),
                    ));
                };
                let multiple_sets = placements
                    .iter()
                    .any(|placement| placement.set != first.set);
                let multiple_namespaces = placements
                    .iter()
                    .any(|placement| placement.namespace != first.namespace);
                if multiple_sets || multiple_namespaces {
                    if package_export {
                        // Package export validation reports the combined placements.
                        return Ok(None);
                    }
                    let scenario = if multiple_sets {
                        "in multiple other export sets"
                    } else {
                        "in another export set which is exported multiple times with different namespaces"
                    };
                    let mut files = placements
                        .iter()
                        .map(|placement| placement.file.as_slice())
                        .collect::<Vec<_>>();
                    files.sort_unstable();
                    let files = files
                        .into_iter()
                        .map(String::from_utf8_lossy)
                        .collect::<Vec<_>>()
                        .join(", ");
                    return Err(closure_error(
                        model,
                        target,
                        dependency,
                        mode,
                        format!(
                            "that is not in this export set, but {scenario}: {files}.\n\nAn exported target cannot depend upon another target which is exported in more than one export set or with more than one namespace.  Consider consolidating the exports of the \"{}\" target to a single export.",
                            dependency.name
                        ),
                    ));
                }
                Ok(Some(
                    [first.namespace.as_slice(), exported.as_slice()]
                        .concat()
                        .into(),
                ))
            };
            for item in export_property_items(value, mode)? {
                let mut ast = parse(&item)?;
                let mut item = if ast.is_plain() {
                    name(&item)?.unwrap_or(item)
                } else {
                    qualify_expressions(&mut ast.nodes, &mut name)?;
                    render(&ast.nodes)
                };
                if mode == PreprocessMode::Install {
                    item = rewrite_install_prefix(&item, b"${_IMPORT_PREFIX}");
                }
                items.push(item);
            }
            Ok(())
        };
        if let Some(links) = target
            .usage
            .interface_link_libraries
            .as_ref()
            .filter(|_| key == LINK_LIBRARIES)
        {
            for link in links {
                append(&link.text.value, link.lookup_dir)?;
            }
        } else if let Some(value) = target.props.get(key) {
            append(value, target.dir)?;
        }
        if !items.is_empty() {
            exported.properties.insert(key.into(), list::join(items));
        }
    }
    Ok(packages)
}

fn closure_error(
    model: &ProjectModel,
    target: &Target,
    dependency: &Target,
    mode: PreprocessMode,
    reason: String,
) -> ResolveError {
    let command = if mode == PreprocessMode::Install {
        "install(EXPORT)"
    } else {
        "export"
    };
    let diagnostic = rcmake_diag::Diagnostic {
        kind: rcmake_diag::MessageKind::Error,
        effect: rcmake_diag::Effect::SetError,
        category: None,
        policy: None,
        location: crate::cxx_modules::generation_location(model, target),
        body: format!(
            "{command} called with target \"{}\" which requires target \"{}\" {reason}",
            target.name, dependency.name
        )
        .into(),
    };
    ResolveError::Message(rcmake_diag::render(
        &diagnostic,
        &rcmake_diag::RenderOptions {
            relative_base: Some(model.dir(DirId::ROOT).source_dir.clone()),
            ..Default::default()
        },
    ))
}

fn qualify_expressions(
    nodes: &mut [GenexNode],
    name: &mut impl FnMut(&[u8]) -> Result<Option<BString>, ResolveError>,
) -> Result<(), ResolveError> {
    for node in nodes {
        let GenexNode::Expr(expr) = node else {
            continue;
        };
        qualify_expressions(&mut expr.identifier, name)?;
        let Some(args) = &mut expr.params else {
            continue;
        };
        if let [GenexNode::Text(operator)] = expr.identifier.as_slice()
            && matches!(
                (operator.as_slice(), args.len()),
                (b"LINK_ONLY" | b"COMPILE_ONLY" | b"TARGET_NAME", 1) | (b"TARGET_PROPERTY", 2)
            )
            && let Some([GenexNode::Text(target)]) = args.first_mut().map(Vec::as_mut_slice)
            && let Some(qualified) = name(target)?
        {
            if operator == b"TARGET_NAME" {
                *node = GenexNode::Text(qualified);
                continue;
            }
            *target = qualified;
        }
        for arg in args {
            qualify_expressions(arg, name)?;
        }
    }
    Ok(())
}
