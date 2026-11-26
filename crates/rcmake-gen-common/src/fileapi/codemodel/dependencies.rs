//! Direct target dependencies in a codemodel reply.

use std::collections::BTreeMap;

use rcmake_base::{list, truth::is_on};
use rcmake_diag::Backtrace;
use rcmake_genex::{EvalContext, UsageTraversal};
use rcmake_model::{ProjectModel, Target, TargetId, TargetKind};
use rcmake_resolve::usage::UsageResolver;

use crate::GenError;

use super::super::{Members, Object, Value, members};
use super::{backtraces::Backtraces, optional_array, targets::TargetRef};

pub(super) fn write(
    model: &ProjectModel,
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
    let dep_backtraces = super::targets::dependency_backtraces(model, raw);
    let mut dependencies: BTreeMap<_, _> = target
        .dependencies
        .iter()
        .filter_map(|&id| {
            targets
                .get(&id)
                .filter(|entry| !entry.abstract_target)
                .map(|entry| {
                    (
                        entry.id.as_str(),
                        dep_backtraces.get(&id).map(|trace| (*trace).clone()),
                    )
                })
        })
        .collect();
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
        if !interface {
            for item in items.iter() {
                for id in item.targets() {
                    if let Some(entry) = targets.get(&id).filter(|entry| !entry.abstract_target) {
                        let trace = dependencies.entry(entry.id.as_str()).or_default();
                        if trace.is_none() {
                            *trace = Some(item.text.backtrace.clone());
                        }
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

    optional_array(
        body,
        "dependencies",
        dependencies.into_iter().map(|(id, trace)| {
            let mut entry = members([("id", Value::string(id))]);
            if let Some(trace) = trace {
                graph.tag(&mut entry, &trace);
            }
            Object(entry)
        }),
    );

    let object_dependencies = raw
        .sources
        .iter()
        .flat_map(|entry| {
            list::split(&entry.value, list::Empty::Drop)
                .into_iter()
                .filter_map(|source| {
                    let name = source
                        .strip_prefix(b"$<TARGET_OBJECTS:")?
                        .strip_suffix(b">")?;
                    model
                        .find_target(raw.dir, name)
                        .map(|id| (id, &entry.backtrace))
                })
        })
        .collect::<BTreeMap<_, _>>();
    optional_array(
        body,
        "objectDependencies",
        object_dependencies
            .iter()
            .filter_map(|(&id, trace)| tagged_dependency(id_value(id)?, trace, graph)),
    );

    let ordered = raw
        .utility_deps
        .iter()
        .filter_map(|entry| {
            model
                .find_target(raw.dir, &entry.value)
                .map(|id| (id, &entry.backtrace))
        })
        .collect::<BTreeMap<_, _>>();
    optional_array(
        body,
        "orderDependencies",
        ordered
            .iter()
            .filter_map(|(&id, trace)| tagged_dependency(id_value(id)?, trace, graph))
            .chain(object_dependencies.keys().filter_map(|&id| {
                if ordered.contains_key(&id) || model.target(id)?.is_imported() {
                    return None;
                }
                Some(Object(members([("id", id_value(id)?)])))
            })),
    );
    Ok(())
}

fn tagged_dependency(id: Value, trace: &Backtrace, graph: &mut Backtraces<'_>) -> Option<Value> {
    let mut entry = members([("id", id)]);
    graph.tag(&mut entry, trace);
    Some(Object(entry))
}
