use std::fmt::Write;

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
        let no_cycles = model
            .global
            .props
            .get(b"GLOBAL_DEPENDS_NO_CYCLES")
            .is_some_and(|v| truth::is_on(v));
        let all_static = component.iter().all(|&id| {
            local
                .get(id)
                .is_some_and(|(raw, _)| raw.kind == TargetKind::StaticLibrary)
        });
        if no_cycles || !all_static {
            let reason = if no_cycles {
                "The GLOBAL_DEPENDS_NO_CYCLES global property is enabled, so cyclic dependencies are not allowed even among static libraries."
            } else {
                "At least one of these targets is not a STATIC_LIBRARY.  Cyclic dependencies are allowed only among static libraries."
            };
            return Err(cycle_error(local, graph, strong_edges, &component, reason));
        }
        let mut remaining = component.clone();
        let mut order = Vec::with_capacity(remaining.len());
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
                    graph,
                    strong_edges,
                    &component,
                    "The component contains at least one cycle consisting of strong dependencies (created by add_dependencies) that cannot be broken.",
                ));
            };
            remaining.shift_remove(&next);
            order.push(next);
        }
        for next in order {
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
    graph: &[GraphTarget],
    strong_edges: &[IndexSet<usize>],
    members: &IndexSet<usize>,
    reason: &str,
) -> ResolveError {
    let mut message = String::from(
        "The inter-target dependency graph contains the following strongly connected component (cycle):\n",
    );
    let mut ids: Vec<_> = members.iter().copied().collect();
    ids.sort_unstable();
    for id in ids {
        let (Some(&(target, _)), Some(node)) = (local.get(id), graph.get(id)) else {
            continue;
        };
        let _ = writeln!(
            message,
            "  \"{}\" of type {}",
            target.name,
            target.kind.as_str()
        );
        for &dep in &node.depends {
            if members.contains(&dep)
                && let Some(&(dependency, _)) = local.get(dep)
            {
                let strength = if strong_edges
                    .get(id)
                    .is_some_and(|edges| edges.contains(&dep))
                {
                    "strong"
                } else {
                    "weak"
                };
                let _ = writeln!(
                    message,
                    "    depends on \"{}\" ({strength})",
                    dependency.name
                );
            }
        }
    }
    message.push_str(reason);
    ResolveError::Message(BString::from(message))
}
