//! Build order dependencies inherited from an AUTOGEN target's origin.

use std::collections::BTreeSet;

use indexmap::IndexSet;
use rcmake_base::{BString, truth};
use rcmake_genex::{ConfigId, EvalContext};
use rcmake_model::{ProjectModel, Target, TargetId, TargetKind};

use crate::{ResolveError, usage::UsageResolver};

use super::ConfiguredSources;

/// Keep dependencies present in every configuration that do not create a cycle.
pub(super) fn origin_dependencies(
    model: &ProjectModel,
    origin_id: TargetId,
    origin: &Target,
    inventory: &[ConfiguredSources],
) -> Result<IndexSet<BString>, ResolveError> {
    if origin
        .props
        .get(b"AUTOGEN_ORIGIN_DEPENDS")
        .is_some_and(|value| !truth::is_on(value))
    {
        return Ok(IndexSet::new());
    }
    let mut ordering: IndexSet<_> = origin
        .utility_deps
        .iter()
        .map(|dep| dep.value.clone())
        .collect();
    let Some(first) = inventory.first() else {
        return Ok(ordering);
    };
    let mut usage = UsageResolver::new(model);
    'candidate: for &id in &first.link_targets {
        let Some(target) = model.target(id) else {
            continue;
        };
        if id == origin_id || !inventory.iter().all(|cfg| cfg.link_targets.contains(&id)) {
            continue;
        }
        if origin.kind == TargetKind::StaticLibrary && target.kind == TargetKind::StaticLibrary {
            for cfg in inventory {
                if static_reaches(&mut usage, model, id, origin_id, &cfg.config)? {
                    continue 'candidate;
                }
            }
        }
        ordering.insert(target.name.clone());
    }
    Ok(ordering)
}

fn static_reaches(
    usage: &mut UsageResolver<'_>,
    model: &ProjectModel,
    dependency: TargetId,
    origin: TargetId,
    config: &ConfigId,
) -> Result<bool, ResolveError> {
    let mut seen = BTreeSet::from([dependency]);
    let mut pending = vec![dependency];
    while let Some(id) = pending.pop() {
        if id == origin {
            return Ok(true);
        }
        let Some(target) = model.target(id) else {
            continue;
        };
        let context = EvalContext {
            config: config.clone(),
            link_language: target.props.get(b"LINKER_LANGUAGE").cloned(),
            ..EvalContext::default()
        };
        let (links, _) = usage.direct_links(id, &context)?;
        pending.extend(
            links
                .iter()
                .flat_map(|item| item.targets())
                .filter(|&id| {
                    model
                        .target(id)
                        .is_some_and(|t| t.kind == TargetKind::StaticLibrary)
                })
                .filter(|&id| seen.insert(id)),
        );
    }
    Ok(false)
}
