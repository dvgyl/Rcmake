use indexmap::IndexSet;
use rcmake_base::{BString, truth};
use rcmake_model::{ProjectModel, Target, TargetKind};

use crate::{GraphTarget, ResolveError};

pub(super) fn dependencies(
    model: &ProjectModel,
    local: &[(&Target, &crate::ResolvedTarget)],
    strong_edges: &[IndexSet<usize>],
    graph: &mut [GraphTarget],
) -> Result<(), ResolveError> {
    let mut reverse = vec![Vec::new(); graph.len()];
    for (id, target) in graph.iter().enumerate() {
        for &dep in &target.depends {
            if let Some(incoming) = reverse.get_mut(dep) {
                incoming.push(id);
            }
        }
    }
    let mut finished = Vec::new();
    let mut visited = IndexSet::new();
    let mut stack = Vec::new();
    for id in 0..graph.len() {
        stack.push((id, false));
        while let Some((node, done)) = stack.pop() {
            if done {
                finished.push(node);
            } else if visited.insert(node) {
                stack.push((node, true));
                if let Some(target) = graph.get(node) {
                    stack.extend(target.depends.iter().map(|&dep| (dep, false)));
                }
            }
        }
    }
    visited.clear();
    for id in finished.into_iter().rev() {
        if visited.contains(&id) {
            continue;
        }
        let mut component = IndexSet::new();
        let mut pending = vec![id];
        while let Some(node) = pending.pop() {
            if visited.insert(node) {
                component.insert(node);
                pending.extend(reverse.get(node).into_iter().flatten().copied());
            }
        }
        if component.len() < 2 {
            continue;
        }
        if model
            .global
            .props
            .get(b"GLOBAL_DEPENDS_NO_CYCLES")
            .is_some_and(|v| truth::is_on(v))
            || !component.iter().all(|&id| {
                local
                    .get(id)
                    .is_some_and(|(raw, _)| raw.kind == TargetKind::StaticLibrary)
            })
        {
            return Err(cycle_error(
                local,
                &component,
                "Only static libraries may have cyclic link dependencies.",
            ));
        }
        let mut remaining = component.clone();
        while !remaining.is_empty() {
            let next = remaining
                .iter()
                .copied()
                .filter(|&id| {
                    strong_edges
                        .get(id)
                        .into_iter()
                        .flatten()
                        .all(|dep| !remaining.contains(dep))
                })
                .max_by_key(|&id| local.get(id).map(|(target, _)| &target.name));
            let Some(next) = next else {
                return Err(cycle_error(
                    local,
                    &component,
                    "The component has a cycle of strong dependencies.",
                ));
            };
            remaining.shift_remove(&next);
            if let Some(target) = graph.get_mut(next) {
                target.depends.retain(|dep| {
                    !component.contains(dep)
                        || strong_edges
                            .get(next)
                            .is_some_and(|edges| edges.contains(dep))
                });
                if let Some(&previous) = pending.last()
                    && !target.depends.contains(&previous)
                {
                    target.depends.push(previous);
                }
            }
            pending.push(next);
        }
    }
    Ok(())
}

fn cycle_error(
    local: &[(&Target, &crate::ResolvedTarget)],
    members: &IndexSet<usize>,
    reason: &str,
) -> ResolveError {
    let names = members
        .iter()
        .filter_map(|&id| local.get(id))
        .map(|(target, _)| target.name.as_slice())
        .collect::<Vec<_>>();
    let names = BString::from(names.join(b", ".as_slice()));
    ResolveError::Message(
        format!("The inter-target dependency graph contains a cycle ({names}). {reason}").into(),
    )
}
