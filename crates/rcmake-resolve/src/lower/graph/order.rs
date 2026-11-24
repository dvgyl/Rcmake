use std::fmt::Write;

use indexmap::IndexSet;
use rcmake_base::{BString, truth};
use rcmake_model::{CustomCommandKind, ProjectModel, Target, TargetKind};

use crate::{
    GraphTarget, ResolveError, ResolvedTarget, file_sets, lower::policy_flags::Flags,
    usage::UsageResolver,
};

pub(super) fn dependencies(
    model: &ProjectModel,
    local: &[(&Target, &crate::ResolvedTarget)],
    strong_edges: &[IndexSet<usize>],
    graph: &mut [GraphTarget],
) -> Result<(), ResolveError> {
    let mut usage = UsageResolver::new(model);
    for (target, resolved) in local {
        file_sets::validate_source_includes(model, &mut usage, target, resolved)?;
        for language in resolved
            .compile_groups
            .iter()
            .map(|group| &group.language)
            .collect::<IndexSet<_>>()
        {
            Flags::new(model, target, language).validate_visibility()?;
        }
    }
    prune_dependencies(model, local, strong_edges, graph);
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
    let mut endpoints: Vec<_> = (0..graph.len()).map(|id| (id, id)).collect();
    let mut dependencies = vec![IndexSet::new(); graph.len()];
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
        let Some(order) = component_order(graph, strong_edges, &component) else {
            return Err(cycle_error(
                local,
                graph,
                strong_edges,
                &component,
                "The component contains at least one cycle consisting of strong dependencies (created by add_dependencies) that cannot be broken.",
            ));
        };
        let (Some(&tail), Some(&head)) = (order.first(), order.last()) else {
            continue;
        };
        let mut previous = None;
        for next in order {
            if let Some(pair) = endpoints.get_mut(next) {
                *pair = (tail, head);
            }
            if let Some(deps) = dependencies.get_mut(next) {
                deps.extend(strong_dependencies(graph, strong_edges, &component, next));
                deps.extend(previous);
            }
            previous = Some(next);
        }
    }

    for (target, &(tail, _)) in graph.iter().zip(&endpoints) {
        let Some(edges) = dependencies.get_mut(tail) else {
            continue;
        };
        for &dep in &target.depends {
            if let Some(&(dep_tail, dep_head)) = endpoints.get(dep)
                && tail != dep_tail
            {
                edges.insert(dep_head);
            }
        }
    }
    for (target, edges) in graph.iter_mut().zip(dependencies) {
        target.depends = edges.into_iter().collect();
    }
    Ok(())
}

fn component_order(
    graph: &[GraphTarget],
    strong_edges: &[IndexSet<usize>],
    members: &IndexSet<usize>,
) -> Option<Vec<usize>> {
    let mut roots: Vec<_> = members.iter().copied().collect();
    roots.sort_unstable();
    let mut order = Vec::with_capacity(roots.len());
    let mut emitted = IndexSet::new();
    let mut active = IndexSet::new();
    let mut stack = Vec::new();
    for root in roots.into_iter().rev() {
        stack.push((root, false));
        while let Some((node, done)) = stack.pop() {
            if done {
                active.swap_remove(&node);
                emitted.insert(node);
                order.push(node);
            } else if !emitted.contains(&node) {
                if !active.insert(node) {
                    return None;
                }
                stack.push((node, true));
                stack.extend(
                    strong_dependencies(graph, strong_edges, members, node)
                        .rev()
                        .map(|dep| (dep, false)),
                );
            }
        }
    }
    Some(order)
}

fn strong_dependencies<'a>(
    graph: &'a [GraphTarget],
    strong_edges: &'a [IndexSet<usize>],
    members: &'a IndexSet<usize>,
    node: usize,
) -> impl DoubleEndedIterator<Item = usize> + 'a {
    let strong = strong_edges.get(node);
    graph
        .get(node)
        .into_iter()
        .flat_map(|target| target.depends.iter().copied())
        .filter(move |dep| members.contains(dep) && strong.is_some_and(|edges| edges.contains(dep)))
}

fn prune_dependencies(
    model: &ProjectModel,
    local: &[(&Target, &ResolvedTarget)],
    strong_edges: &[IndexSet<usize>],
    graph: &mut [GraphTarget],
) {
    let optimizable = |target: &Target| {
        matches!(
            target.kind,
            TargetKind::StaticLibrary | TargetKind::ObjectLibrary
        ) && target
            .props
            .get(b"OPTIMIZE_DEPENDENCIES")
            .is_some_and(|value| truth::is_on(value))
    };
    if !local.iter().any(|(target, _)| optimizable(target)) {
        return;
    }
    let command_count = model
        .commands()
        .filter(|(_, command)| command.kind == CustomCommandKind::Output)
        .count();
    let has_commands = |target: &Target, node: &GraphTarget| {
        !target.event_commands.is_empty()
            || node.steps.iter().any(|&step| step < command_count)
            || node.depends.iter().any(|&dep| dep >= local.len())
    };
    let mut updates = Vec::new();
    for (index, ((target, resolved), node)) in local.iter().zip(graph.iter()).enumerate() {
        if !optimizable(target) {
            continue;
        }
        let languages: IndexSet<_> = resolved
            .compile_groups
            .iter()
            .map(|group| group.language.as_slice())
            .filter(|language| {
                !matches!(
                    *language,
                    b"C" | b"CXX" | b"OBJC" | b"OBJCXX" | b"ASM" | b"CUDA" | b"HIP"
                )
            })
            .collect();
        let mut kept = IndexSet::new();
        let mut seen = IndexSet::from([index]);
        let mut pending = Vec::new();
        for &dep in &node.depends {
            if dep >= local.len()
                || strong_edges
                    .get(index)
                    .is_some_and(|edges| edges.contains(&dep))
            {
                kept.insert(dep);
            } else {
                pending.push(dep);
            }
        }
        pending.reverse();
        while let Some(dep) = pending.pop() {
            if !seen.insert(dep) {
                continue;
            }
            let Some(((target, resolved), node)) = local.get(dep).zip(graph.get(dep)) else {
                continue;
            };
            if has_commands(target, node)
                || resolved
                    .compile_groups
                    .iter()
                    .any(|group| languages.contains(group.language.as_slice()))
            {
                kept.insert(dep);
            }
            pending.extend(node.depends.iter().rev().copied());
        }
        updates.push((index, kept));
    }
    for (index, depends) in updates {
        if let Some(target) = graph.get_mut(index) {
            target.depends = depends.into_iter().collect();
        }
    }
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
