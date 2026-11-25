use indexmap::{IndexMap, IndexSet};
use rcmake_model::{ProjectModel, Target, TargetId};

use crate::ResolvedTarget;

pub(super) fn emitted_targets<'a>(
    model: &'a ProjectModel,
    resolved: &IndexMap<TargetId, &'a ResolvedTarget>,
) -> Vec<(&'a Target, &'a ResolvedTarget)> {
    model
        .targets()
        .filter_map(|(id, target)| {
            model
                .is_in_build_system(target)
                .then_some((target, *resolved.get(&id)?))
        })
        .collect()
}

pub(super) fn dependencies(
    model: &ProjectModel,
    indices: &IndexMap<TargetId, usize>,
    origin: TargetId,
    explicit: &IndexSet<TargetId>,
    links: &[TargetId],
) -> (IndexSet<usize>, IndexSet<usize>) {
    let mut ordered = IndexSet::new();
    let mut strong = IndexSet::new();
    let mut visited = IndexSet::from([origin]);
    for (&id, is_strong) in explicit
        .iter()
        .map(|id| (id, true))
        .chain(links.iter().map(|id| (id, false)))
    {
        let mut pending = vec![(id, is_strong)];
        while let Some((id, is_strong)) = pending.pop() {
            if id == origin {
                continue;
            }
            if let Some(&index) = indices.get(&id) {
                ordered.insert(index);
                if is_strong {
                    strong.insert(index);
                }
            } else if visited.insert(id)
                && let Some(target) = model.target(id)
            {
                pending.extend(
                    target
                        .utility_deps
                        .iter()
                        .rev()
                        .filter_map(|dep| Some((model.find_target(target.dir, &dep.value)?, true))),
                );
            }
        }
    }
    (ordered, strong)
}
