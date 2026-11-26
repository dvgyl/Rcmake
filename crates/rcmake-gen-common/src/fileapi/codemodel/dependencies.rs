//! Direct target dependencies in a codemodel reply.

use std::collections::BTreeMap;

use rcmake_base::{list, truth::is_on};
use rcmake_diag::Backtrace;
use rcmake_genex::{EvalContext, UsageTraversal};
use rcmake_model::{ProjectModel, Target, TargetId, TargetKind};
use rcmake_resolve::{TargetDependGraph, usage::UsageResolver};

use crate::GenError;

use super::super::{Members, Object, Value, members};
use super::{backtraces::Backtraces, optional_array, targets::TargetRef};

#[allow(clippy::too_many_arguments)]
pub(super) fn write(
    model: &ProjectModel,
    target_depends: &TargetDependGraph,
    reference: &TargetRef<'_>,
    targets: &BTreeMap<TargetId, TargetRef<'_>>,
    raw: &Target,
    usage: &mut UsageResolver<'_>,
    graph: &mut Backtraces<'_>,
    body: &mut Members,
) -> Result<(), GenError> {
    let target = reference.target;
    let mut context = EvalContext {
        config: target.config.clone(),
        link_language: target.link.as_ref().map(|link| link.language.clone()),
        ..EvalContext::default()
    };
    let id_value = |id| {
        targets
            .get(&id)
            .map(|entry| Value::string(entry.id.as_str()))
    };
    let use_depend_graph = !target_depends.targets().is_empty();
    let emit_dependencies = !reference.abstract_target && !raw.is_imported();
    let dep_backtraces = super::targets::dependency_backtraces(model, raw);
    let mut dependencies: BTreeMap<TargetId, (&str, Option<Backtrace>)> = if emit_dependencies {
        if use_depend_graph {
            target_depends
                .direct_depends(target.id)
                .iter()
                .filter_map(|dep| {
                    targets
                        .get(&dep.target)
                        .filter(|entry| !entry.abstract_target)
                        .map(|entry| {
                            (
                                dep.target,
                                (
                                    entry.id.as_str(),
                                    (!dep.backtrace.is_empty()).then(|| dep.backtrace.clone()),
                                ),
                            )
                        })
                })
                .collect()
        } else {
            target
                .dependencies
                .iter()
                .filter_map(|&id| {
                    targets
                        .get(&id)
                        .filter(|entry| !entry.abstract_target)
                        .map(|entry| {
                            (
                                id,
                                (
                                    entry.id.as_str(),
                                    dep_backtraces.get(&id).map(|trace| (*trace).clone()),
                                ),
                            )
                        })
                })
                .collect()
        }
    } else {
        BTreeMap::new()
    };
    for (field, interface, traversal) in [
        ("linkLibraries", false, UsageTraversal::Link),
        ("interfaceLinkLibraries", true, UsageTraversal::Link),
        ("compileDependencies", false, UsageTraversal::Compile),
        (
            "interfaceCompileDependencies",
            true,
            UsageTraversal::Compile,
        ),
    ] {
        if interface
            && raw.kind == TargetKind::Executable
            && !raw
                .props
                .get(b"ENABLE_EXPORTS")
                .is_some_and(|value| is_on(value))
        {
            continue;
        }
        context.traversal = traversal;
        let items = usage
            .direct_link_items(target.id, interface, &context)
            .map_err(|error| GenError::Invalid(error.to_string().into()))?;
        if !interface && emit_dependencies && !use_depend_graph {
            for item in items.iter() {
                for id in item.targets() {
                    if let Some((_, trace)) = dependencies.get_mut(&id)
                        && trace.is_none()
                    {
                        *trace = Some(item.text.backtrace.clone());
                    }
                }
            }
        }
        optional_array(
            body,
            field,
            items.iter().filter_map(|item| {
                let mut value = if let Some(id) = item.targets().next().and_then(id_value) {
                    members([("id", id)])
                } else if traversal == UsageTraversal::Link {
                    members([("fragment", Value::string(item.text.value.clone()))])
                } else {
                    return None;
                };
                if let Some(provider) = item.injected_by.and_then(id_value) {
                    value.insert("fromDependency".into(), Object(members([("id", provider)])));
                }
                graph.tag(&mut value, &item.text.backtrace);
                Some(Object(value))
            }),
        );
    }

    if emit_dependencies {
        optional_array(
            body,
            "dependencies",
            dependencies.into_values().map(|(id, trace)| {
                let mut entry = members([("id", Value::string(id))]);
                if let Some(trace) = trace {
                    graph.tag(&mut entry, &trace);
                }
                Object(entry)
            }),
        );
    }

    let mut object_dependencies = BTreeMap::<&str, &Backtrace>::new();
    for entry in &raw.sources {
        for source in list::split(&entry.value, list::Empty::Drop) {
            let Some(name) = source
                .strip_prefix(b"$<TARGET_OBJECTS:")
                .and_then(|name| name.strip_suffix(b">"))
            else {
                continue;
            };
            if let Some(id) = model.find_target(raw.dir, name)
                && let Some(target_ref) = targets.get(&id)
            {
                object_dependencies
                    .entry(target_ref.id.as_str())
                    .or_insert(&entry.backtrace);
            }
        }
    }
    optional_array(
        body,
        "objectDependencies",
        object_dependencies
            .iter()
            .filter_map(|(&id, trace)| tagged_dependency(Value::string(id), trace, graph)),
    );

    let mut ordered = BTreeMap::<(&str, bool), Option<&Backtrace>>::new();
    if use_depend_graph {
        for item in target_depends.order_dependencies(model, target.id) {
            if let Some(target_ref) = targets.get(&item.target) {
                ordered
                    .entry((target_ref.id.as_str(), item.cross))
                    .or_insert((!item.backtrace.is_empty()).then_some(&item.backtrace));
            }
        }
    } else {
        for entry in &raw.utility_deps {
            if is_reserved_target(&entry.value) {
                continue;
            }
            if let Some(id) = model.find_target(raw.dir, &entry.value)
                && !model
                    .target(id)
                    .is_some_and(|dep| is_reserved_target(&dep.name))
                && let Some(target_ref) = targets.get(&id)
            {
                ordered
                    .entry((target_ref.id.as_str(), false))
                    .or_insert(Some(&entry.backtrace));
            }
        }
        for &id in object_dependencies.keys() {
            let name = id
                .split_once("::@")
                .map_or(id.as_bytes(), |(n, _)| n.as_bytes());
            if !is_reserved_target(name) {
                ordered.entry((id, false)).or_insert(None);
            }
        }
    }
    optional_array(
        body,
        "orderDependencies",
        ordered
            .into_iter()
            .filter_map(|((id, _), trace)| match trace {
                Some(trace) => tagged_dependency(Value::string(id), trace, graph),
                None => Some(Object(members([("id", Value::string(id))]))),
            }),
    );
    Ok(())
}

fn is_reserved_target(name: &[u8]) -> bool {
    matches!(
        name,
        b"all"
            | b"ALL_BUILD"
            | b"help"
            | b"install"
            | b"INSTALL"
            | b"preinstall"
            | b"clean"
            | b"edit_cache"
            | b"rebuild_cache"
            | b"ZERO_CHECK"
    )
}

fn tagged_dependency(id: Value, trace: &Backtrace, graph: &mut Backtraces<'_>) -> Option<Value> {
    let mut entry = members([("id", id)]);
    graph.tag(&mut entry, trace);
    Some(Object(entry))
}
