use indexmap::IndexMap;
use rcmake_base::truth;
use rcmake_genex::{ConfigId, EvalContext};
use rcmake_host::Stream;
use rcmake_model::{DirId, FileSetKind, ProjectModel, Target};

use crate::{
    ResolveError,
    lower::autogen::{self, AutogenPlan, ConfiguredSources, SourceInput},
    sources::{SourcePaths, absolute_path},
    target::collect_file_sets,
    usage::UsageResolver,
};

pub(super) fn prepare(
    model: &ProjectModel,
    paths: &mut SourcePaths<'_>,
    configs: &[ConfigId],
) -> Result<Option<AutogenPlan>, ResolveError> {
    let requested = |target: &Target| {
        !target.is_imported()
            && target.kind.compiles_objects()
            && [b"AUTOMOC".as_slice(), b"AUTOUIC", b"AUTORCC"]
                .iter()
                .any(|key| {
                    target
                        .props
                        .get(key)
                        .is_some_and(|value| truth::is_on(value))
                })
    };
    if !model.targets().any(|(_, target)| requested(target)) {
        return Ok(None);
    }
    let mut inventories = IndexMap::<_, Vec<_>>::new();
    for config in configs {
        paths.index_generated(model, config)?;
        paths.index_objects(model, config);
        let mut usage = UsageResolver::with_source_paths(model, paths);
        let context = EvalContext {
            config: config.clone(),
            ..Default::default()
        };
        for (id, target) in model.targets().filter(|(_, target)| requested(target)) {
            let dir = model.dir(target.dir);
            let (_, members) = collect_file_sets(
                model,
                paths,
                &mut usage,
                id,
                &context,
                &mut Default::default(),
            )?;
            let mut sources = IndexMap::new();
            for source in usage.collect_property(id, b"SOURCES", &context)?.entries {
                let path = paths
                    .find(dir, &source.value)
                    .unwrap_or_else(|| absolute_path(&dir.source_dir, &source.value));
                let generated = paths.is_generated(dir, &path);
                let cxx_module = members
                    .get(&path)
                    .is_some_and(|member| member.set.kind == FileSetKind::CxxModules);
                sources.insert(
                    path.clone(),
                    SourceInput {
                        path,
                        cxx_module,
                        generated,
                    },
                );
            }
            for (path, member) in members {
                let generated = paths.is_generated(dir, &path);
                sources.entry(path.clone()).or_insert(SourceInput {
                    path,
                    cxx_module: member.set.kind == FileSetKind::CxxModules,
                    generated,
                });
            }
            let (links, _) = usage.direct_links(id, &context)?;
            inventories.entry(id).or_default().push(ConfiguredSources {
                config: config.clone(),
                sources: sources.into_values().collect(),
                link_targets: links.iter().flat_map(|item| item.targets()).collect(),
            });
        }
    }
    let plan = autogen::prepare(model, paths, configs, &inventories)?;
    if let Some(plan) = &plan {
        let options = rcmake_diag::RenderOptions {
            relative_base: Some(model.dir(DirId::ROOT).source_dir.clone()),
            ..Default::default()
        };
        for diagnostic in &plan.diagnostics {
            paths
                .host
                .write_out(Stream::Stderr, &rcmake_diag::render(diagnostic, &options));
        }
    }
    Ok(plan)
}
