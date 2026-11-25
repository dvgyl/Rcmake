use super::{ConfigId, IndexMap, IndexSet, ProjectModel, ResolvedTarget, TargetId, TargetKind};
use crate::{ResolveError, usage::UsageResolver};
use rcmake_genex::EvalContext;
use rcmake_model::InstallKind;

pub(in crate::lower) type RuntimeTargets = IndexMap<usize, IndexMap<TargetId, IndexSet<ConfigId>>>;

pub(in crate::lower) fn collect(
    model: &ProjectModel,
    usage: &mut UsageResolver<'_>,
    targets: &[ResolvedTarget],
) -> Result<RuntimeTargets, ResolveError> {
    let mut plans = RuntimeTargets::new();
    for (index, rule, _) in super::ordered_install_rules(model) {
        let InstallKind::Targets { targets: roots, .. } = &rule.kind else {
            continue;
        };
        if !super::recorded_args(model, index).contains(&b"RUNTIME_DEPENDENCY_TARGETS".as_slice()) {
            continue;
        }
        let mut configs: IndexMap<&ConfigId, IndexMap<TargetId, &ResolvedTarget>> = IndexMap::new();
        for target in targets {
            configs
                .entry(&target.config)
                .or_default()
                .insert(target.id, target);
        }
        let mut dependencies = IndexMap::<_, IndexSet<_>>::new();
        for (config, facts) in configs {
            let mut seen: IndexSet<_> = roots.iter().copied().collect();
            let mut pending: Vec<_> = roots.to_vec();
            let mut next = 0;
            while let Some(&id) = pending.get(next) {
                next += 1;
                let Some(link) = facts.get(&id).and_then(|target| target.link.as_ref()) else {
                    continue;
                };
                let context = EvalContext {
                    config: config.clone(),
                    link_language: Some(link.language.clone()),
                    ..EvalContext::default()
                };
                for dep in usage.link_targets(id, &context)? {
                    if model.target(dep).is_some_and(|target| {
                        target.kind == TargetKind::SharedLibrary && !target.is_imported()
                    }) && seen.insert(dep)
                    {
                        dependencies.entry(dep).or_default().insert(config.clone());
                        pending.push(dep);
                    }
                }
            }
        }
        plans.insert(index, dependencies);
    }
    Ok(plans)
}
