//! Build graph edges and custom commands.

use indexmap::{IndexMap, IndexSet};
use rcmake_base::{BString, list, truth};
use rcmake_genex::ConfigId;
use rcmake_model::{DirId, GeneratorKind, ProjectModel, Target, TargetEvent, TargetId, TargetKind};

use super::artifacts::build_relative;
use super::compile::compile;
use super::custom::{
    Commands, attach_link_events, command_path, ide_custom_step, remove_own_outputs,
};
use super::link::link;
use super::{compute_artifacts, linker_language};
use crate::ide::{IdeCustomPhase, IdeCustomStep};
use crate::{
    ArtifactSet, BuildGraph, BuildStep, CustomStep, GraphTarget, LinkFragmentRole, LinkKind,
    LinkStep, ResolveError, ResolvedTarget, sources::TargetInputs, usage::UsageResolver,
};

/// Lower resolved targets for one configuration.
///
/// Targets and commands keep their model order where dependencies permit.
pub fn lower(
    model: &ProjectModel,
    targets: &[ResolvedTarget],
    config: &ConfigId,
) -> Result<BuildGraph, ResolveError> {
    lower_with_inputs(
        model,
        targets,
        config,
        &IndexMap::new(),
        &mut UsageResolver::new(model),
        &mut IndexMap::new(),
    )
}

pub(crate) fn lower_with_inputs(
    model: &ProjectModel,
    targets: &[ResolvedTarget],
    config: &ConfigId,
    inputs: &IndexMap<TargetId, TargetInputs>,
    usage: &mut UsageResolver<'_>,
    ide_custom: &mut IndexMap<TargetId, Vec<IdeCustomStep>>,
) -> Result<BuildGraph, ResolveError> {
    let resolved: IndexMap<_, _> = targets
        .iter()
        .filter(|target| config.matches(&target.config.0))
        .map(|target| (target.id, target))
        .collect();
    let local: Vec<_> = model
        .targets()
        .filter_map(|(id, target)| {
            let resolved = *resolved.get(&id)?;
            (!target.is_imported()).then_some((target, resolved))
        })
        .collect();
    let target_indices: IndexMap<_, _> = local
        .iter()
        .enumerate()
        .map(|(index, (_, target))| (target.id, index))
        .collect();
    let mut graph = BuildGraph {
        config: config.clone(),
        ..BuildGraph::default()
    };
    let commands = Commands::new(model, &resolved, config, usage, &mut graph.steps)?;
    let rule_count = graph.steps.len();
    let mut unity_prerequisites = Vec::new();
    for (target, resolved) in local {
        let mut ide_steps = resolved.ide.as_ref().map(|_| Vec::new());
        let mut dependencies: IndexSet<_> = resolved.dependencies.iter().copied().collect();
        dependencies.extend(
            target
                .utility_deps
                .iter()
                .filter_map(|dep| model.find_target(target.dir, &dep.value)),
        );
        let mut steps = IndexSet::new();
        let dir = model.dir(target.dir);
        let unity = usage
            .source_paths()
            .and_then(|paths| paths.unity.as_ref())
            .and_then(|plan| plan.get(&resolved.id));
        let mut unity_inputs = IndexMap::<_, IndexSet<_>>::new();
        let ninja = matches!(
            model.generator,
            Some(GeneratorKind::Ninja | GeneratorKind::NinjaMultiConfig)
        );
        let target_inputs = inputs.get(&resolved.id);
        let paths = if let Some(inputs) = target_inputs {
            Box::new(inputs.sources.iter().cloned()) as Box<dyn Iterator<Item = BString>>
        } else {
            Box::new(
                resolved
                    .compile_units
                    .iter()
                    .map(|unit| unit.source.value.clone())
                    .chain(
                        target
                            .sources
                            .iter()
                            .flat_map(|source| list::split(&source.value, list::Empty::Drop)),
                    ),
            )
        };
        for path in paths {
            for base in [&dir.source_dir, &dir.binary_dir] {
                let input = command_path(model, base, &path);
                if let Some(step) = commands.producer(&input) {
                    commands.add_producers([step], &mut steps, &mut dependencies)?;
                    if ninja
                        && let Some(file) =
                            unity.and_then(|plan| plan.file_for_source(&path, config))
                    {
                        unity_inputs.entry(file).or_default().insert(input);
                    }
                }
            }
        }

        let artifacts = artifacts(model, target, resolved);
        let unity_prerequisite =
            if !steps.is_empty() && unity.is_some_and(|plan| !plan.files.is_empty()) {
                // An included source can be an output of a custom command.
                let index = target_indices.len() + unity_prerequisites.len();
                let mut name = BString::from(format!("cmake_object_order_depends_unity_{index}"));
                while model.find_target(DirId::ROOT, &name).is_some() {
                    name.push(b'_');
                }
                unity_prerequisites.push(GraphTarget {
                    name,
                    steps: std::mem::take(&mut steps).into_iter().collect(),
                    depends: dependencies
                        .iter()
                        .filter(|id| **id != resolved.id)
                        .filter_map(|id| target_indices.get(id).copied())
                        .collect(),
                    in_all: false,
                });
                Some(index)
            } else {
                None
            };
        for (file, generated_sources) in unity_inputs {
            // Connect the unity file to its generated includes without changing its timestamp.
            steps.insert(push(
                &mut graph.steps,
                BuildStep::Custom(CustomStep {
                    outputs: vec![build_relative(model, file)],
                    depends: generated_sources.into_iter().collect(),
                    ..CustomStep::default()
                }),
            ));
        }
        let mut objects = target_inputs
            .into_iter()
            .flat_map(|inputs| inputs.objects.iter().cloned())
            .collect::<Vec<_>>();
        let cxx_standard = target_inputs
            .and_then(|inputs| inputs.standard_requirements.get(b"CXX".as_slice()).copied());
        for step in super::modules::lower_modules_with_standard(model, resolved, cxx_standard)? {
            steps.insert(push(&mut graph.steps, step));
        }
        for unit in &resolved.compile_units {
            let Some(group) = resolved.compile_groups.get(unit.group) else {
                return Err(ResolveError::Message(
                    [
                        b"The compile group is missing for ",
                        unit.source.value.as_slice(),
                    ]
                    .concat()
                    .into(),
                ));
            };
            let required_standard = target_inputs
                .and_then(|inputs| inputs.standard_requirements.get(&group.language).copied());
            let mut step = compile(
                model,
                target,
                config,
                &artifacts,
                unit,
                group,
                required_standard,
            )?;
            if let BuildStep::Compile { object, dyndep, .. } = &mut step {
                if !super::modules::is_std_bmi(model, target) {
                    objects.push(object.clone());
                }
                if unit.scan {
                    *dyndep = resolved
                        .cxx_modules
                        .as_ref()
                        .map(|info| info.dyndep.clone());
                }
            }
            steps.insert(push(&mut graph.steps, step));
        }
        if let Some(mut step) = resolved
            .link
            .as_ref()
            .map(|step| {
                if step.kind != LinkKind::StaticArchive {
                    return step.clone();
                }
                // The File API uses the full fragment list from the resolved target.
                // The archive command uses only its flags.
                LinkStep {
                    kind: step.kind,
                    language: step.language.clone(),
                    fragments: step
                        .fragments
                        .iter()
                        .filter(|fragment| fragment.role == LinkFragmentRole::Flags)
                        .cloned()
                        .collect(),
                    output: step.output.clone(),
                }
            })
            .or_else(|| {
                let kind = link_kind(target.kind)?;
                let output = build_relative(model, artifacts.file.as_ref()?);
                Some(LinkStep {
                    kind,
                    language: BString::default(),
                    fragments: Vec::new(),
                    output,
                })
            })
        {
            if let Some(language) = target
                .props
                .get(b"LINKER_LANGUAGE")
                .filter(|language| !language.is_empty())
            {
                step.language = language.clone();
            }
            if step.language.is_empty() {
                step.language = linker_language(
                    model,
                    target,
                    resolved
                        .compile_units
                        .iter()
                        .filter_map(|unit| resolved.compile_groups.get(unit.group))
                        .filter(|group| !group.language.is_empty())
                        .map(|group| group.language.as_slice()),
                )
                .ok_or_else(|| {
                    ResolveError::Message(
                        [
                            b"Cannot determine link language for target ",
                            target.name.as_slice(),
                        ]
                        .concat()
                        .into(),
                    )
                })?;
            }
            let before = commands.events(
                resolved.id,
                &[Some(TargetEvent::PreBuild), Some(TargetEvent::PreLink)],
                usage,
                &mut steps,
                &mut dependencies,
                ide_steps.as_mut(),
            )?;
            let after = commands.events(
                resolved.id,
                &[Some(TargetEvent::PostBuild)],
                usage,
                &mut steps,
                &mut dependencies,
                ide_steps.as_mut(),
            )?;
            let link = link(model, target, config, &artifacts, step, objects);
            let link = attach_link_events(model, &artifacts, link, before, after);
            steps.insert(push(&mut graph.steps, link));
        } else {
            // Ninja and Make run the utility command before PRE_BUILD and POST_BUILD.
            let mut utility = commands.events(
                resolved.id,
                &[
                    None,
                    Some(TargetEvent::PreBuild),
                    Some(TargetEvent::PostBuild),
                ],
                usage,
                &mut steps,
                &mut dependencies,
                ide_steps.as_mut(),
            )?;
            if utility != CustomStep::default() {
                remove_own_outputs(&mut utility);
                steps.insert(push(&mut graph.steps, BuildStep::Custom(utility)));
            }
        }

        if let Some(events) = ide_steps {
            let rules = steps
                .iter()
                .filter(|&&index| index < rule_count)
                .filter_map(|&index| {
                    let BuildStep::Custom(step) = graph.steps.get(index)? else {
                        return None;
                    };
                    Some(ide_custom_step(model, step, IdeCustomPhase::Rule, false))
                });
            ide_custom.insert(resolved.id, rules.chain(events).collect());
        }

        graph.targets.push(GraphTarget {
            name: resolved.metadata.name.clone(),
            steps: steps.into_iter().collect(),
            depends: dependencies
                .iter()
                .filter(|id| **id != resolved.id)
                .filter_map(|id| target_indices.get(id).copied())
                .chain(unity_prerequisite)
                .collect(),
            in_all: !resolved.metadata.exclude_from_all
                && !excluded(model, target)
                && (target.kind != TargetKind::InterfaceLibrary
                    || !resolved.compile_units.is_empty()),
        });
    }
    graph.targets.extend(unity_prerequisites);
    Ok(graph)
}

fn excluded(model: &ProjectModel, target: &Target) -> bool {
    if let Some(value) = target.props.get(b"EXCLUDE_FROM_ALL") {
        return truth::is_on(value);
    }
    let mut dir = target.dir;
    loop {
        let directory = model.dir(dir);
        if directory.exclude_from_all {
            return true;
        }
        let Some(parent) = directory
            .parent
            .filter(|parent| parent.index() < dir.index())
        else {
            return false;
        };
        dir = parent;
    }
}

fn artifacts(model: &ProjectModel, target: &Target, resolved: &ResolvedTarget) -> ArtifactSet {
    let mut artifacts = resolved.artifacts.clone();
    if artifacts.file.is_none() && artifacts.object_dir.is_none() {
        artifacts = compute_artifacts(model, target, &resolved.config);
    }
    artifacts
}

fn link_kind(kind: TargetKind) -> Option<LinkKind> {
    match kind {
        TargetKind::Executable => Some(LinkKind::Executable),
        TargetKind::StaticLibrary => Some(LinkKind::StaticArchive),
        TargetKind::SharedLibrary => Some(LinkKind::SharedLibrary),
        TargetKind::ModuleLibrary => Some(LinkKind::ModuleLibrary),
        _ => None,
    }
}

pub(super) fn push(steps: &mut Vec<BuildStep>, step: BuildStep) -> usize {
    let index = steps.len();
    steps.push(step);
    index
}
