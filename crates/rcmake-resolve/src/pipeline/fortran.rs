use indexmap::{IndexMap, IndexSet};
use rcmake_genex::EvalContext;
use rcmake_model::{GeneratorKind, ProjectModel, TargetId};

use crate::{
    ResolveError, ResolvedTarget,
    lower::fortran::{FortranPlan, forwarded_object_libraries},
    sources::{SourcePaths, TargetInputs},
    usage::UsageResolver,
};

pub(super) fn prepare(
    model: &ProjectModel,
    paths: &SourcePaths<'_>,
    usage: &mut UsageResolver<'_>,
    targets: &mut IndexMap<TargetId, ResolvedTarget>,
    inputs: &IndexMap<TargetId, TargetInputs>,
    context: &EvalContext,
) -> Result<IndexMap<TargetId, FortranPlan>, ResolveError> {
    if !targets.values().any(|facts| {
        facts
            .compile_groups
            .iter()
            .any(|group| group.language == "Fortran")
    }) {
        return Ok(IndexMap::new());
    }
    let order = dependency_order(targets);
    let mut plans = IndexMap::new();
    for (&id, facts) in targets.iter() {
        let Some(inputs) = inputs.get(&id) else {
            continue;
        };
        let context = EvalContext {
            link_language: facts.link.as_ref().map(|link| link.language.clone()),
            ..context.clone()
        };
        let links = usage.collect_link_items(id, &context)?;
        let forwarded = forwarded_object_libraries(model, paths, inputs, &links);
        let linked = usage.link_targets(id, &context)?;
        let linked = linked
            .iter()
            .chain(&forwarded)
            .filter_map(|id| targets.get(id));
        let Some(mut plan) = FortranPlan::new(model, facts, linked, &forwarded, &order)? else {
            continue;
        };
        plan.create_module_dir(paths.host)?;
        if model.generator == Some(GeneratorKind::UnixMakefiles) {
            plan.set_make_depend_info();
        }
        plans.insert(id, plan);
    }
    for (&id, plan) in &plans {
        if let Some(target) = targets.get_mut(&id) {
            target.fortran = Some(plan.info.clone());
        }
    }
    Ok(plans)
}

fn dependency_order(targets: &IndexMap<TargetId, ResolvedTarget>) -> IndexSet<TargetId> {
    let mut visited = IndexSet::new();
    let mut order = IndexSet::new();
    let mut stack = Vec::new();
    for &id in targets.keys() {
        stack.push((id, false));
        while let Some((id, ready)) = stack.pop() {
            if ready {
                order.insert(id);
            } else if visited.insert(id) {
                stack.push((id, true));
                if let Some(target) = targets.get(&id) {
                    stack.extend(target.dependencies.iter().rev().map(|&dep| (dep, false)));
                }
            }
        }
    }
    order
}
