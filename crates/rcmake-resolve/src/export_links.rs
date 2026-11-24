//! Target names in exported link interfaces.

use rcmake_base::{BString, list};
use rcmake_genex::{GenexNode, PreprocessMode, parse, render, rewrite_install_prefix};
use rcmake_model::{DirId, ProjectModel, TargetId};

use crate::{
    ExportedTarget, ResolveError,
    exports::{export_name, export_property_items},
};

const LINK_LIBRARIES: &[u8] = b"INTERFACE_LINK_LIBRARIES";

pub(crate) fn qualify(
    model: &ProjectModel,
    id: TargetId,
    members: &[TargetId],
    namespace: &[u8],
    mode: PreprocessMode,
    exported: &mut ExportedTarget,
) -> Result<(), ResolveError> {
    let Some(target) = model.target(id) else {
        return Ok(());
    };
    for key in [
        LINK_LIBRARIES,
        b"INTERFACE_LINK_LIBRARIES_DIRECT",
        b"INTERFACE_LINK_LIBRARIES_DIRECT_EXCLUDE",
    ] {
        let mut items = Vec::new();
        let mut append = |value: &[u8], dir: DirId| -> Result<(), ResolveError> {
            let name = |input: &[u8]| {
                let id = model.find_target(dir, input)?;
                let dependency = model.target(id)?;
                if dependency.is_imported() {
                    return Some(dependency.name.clone());
                }
                members.contains(&id).then(|| {
                    let exported = export_name(dependency);
                    [namespace, exported.as_slice()].concat().into()
                })
            };
            for item in export_property_items(value, mode)? {
                let mut ast = parse(&item)?;
                let mut item = if ast.is_plain() {
                    name(&item).unwrap_or(item)
                } else {
                    qualify_expressions(&mut ast.nodes, &name);
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
    Ok(())
}

fn qualify_expressions(nodes: &mut [GenexNode], name: &impl Fn(&[u8]) -> Option<BString>) {
    for node in nodes {
        let GenexNode::Expr(expr) = node else {
            continue;
        };
        qualify_expressions(&mut expr.identifier, name);
        let Some(args) = &mut expr.params else {
            continue;
        };
        if let [GenexNode::Text(operator)] = expr.identifier.as_slice()
            && matches!(
                (operator.as_slice(), args.len()),
                (b"LINK_ONLY" | b"COMPILE_ONLY" | b"TARGET_NAME", 1) | (b"TARGET_PROPERTY", 2)
            )
            && let Some([GenexNode::Text(target)]) = args.first_mut().map(Vec::as_mut_slice)
            && let Some(qualified) = name(target)
        {
            if operator == b"TARGET_NAME" {
                *node = GenexNode::Text(qualified);
                continue;
            }
            *target = qualified;
        }
        for arg in args {
            qualify_expressions(arg, name);
        }
    }
}
