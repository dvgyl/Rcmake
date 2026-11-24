//! Build graph edges and custom commands.

use std::collections::BTreeMap;

use indexmap::{IndexMap, IndexSet};
use rcmake_base::{BString, list};
use rcmake_genex::ConfigId;
use rcmake_model::{
    BT, DirId, GeneratorKind, ProjectModel, Target, TargetEvent, TargetId, TargetKind,
};

use super::artifacts::build_relative;
use super::compile::{Settings, compile};
use super::custom::{
    Commands, attach_link_events, command_path, ide_custom_step, remove_own_outputs,
};
use super::link::link;
use super::pch::{self, PchFlags};
use super::{compute_artifacts, linker_language};
use crate::ide::{IdeCustomPhase, IdeCustomStep};
use crate::{
    ArtifactSet, BuildGraph, BuildStep, CompileUnit, CustomStep, GraphTarget, LinkFragmentRole,
    LinkKind, LinkStep, ResolveError, ResolvedTarget, sources::TargetInputs, unity::TargetUnity,
    usage::UsageResolver,
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
        &mut BTreeMap::new(),
    )
}

pub(crate) fn lower_with_inputs(
    model: &ProjectModel,
    targets: &[ResolvedTarget],
    config: &ConfigId,
    inputs: &IndexMap<TargetId, TargetInputs>,
    usage: &mut UsageResolver<'_>,
    ide_custom: &mut IndexMap<TargetId, Vec<IdeCustomStep>>,
    compile_inputs: &mut BTreeMap<BString, Vec<BString>>,
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
    let precompiled = pch::Plans::new(model, &resolved, config, usage)?;
    let rule_count = graph.steps.len();
    let mut unity_prerequisites = Vec::new();
    for (target, resolved) in local {
        let mut plan = TargetPlan::new(model, config, target, resolved, inputs.get(&resolved.id));
        let unity = usage
            .source_paths()
            .and_then(|paths| paths.unity.as_ref())
            .and_then(|plan| plan.get(&resolved.id));
        let unity_prerequisite = plan.add_source_commands(
            &commands,
            unity,
            &target_indices,
            &mut unity_prerequisites,
            &mut graph.steps,
        )?;
        let artifacts = artifacts(model, target, resolved);
        let objects = plan.add_compile_steps(
            &artifacts,
            &commands,
            &precompiled,
            &mut graph.steps,
            compile_inputs,
        )?;
        plan.add_link_and_target_commands(&artifacts, objects, &commands, usage, &mut graph.steps)?;

        if let Some(events) = plan.ide_steps {
            let rules = plan
                .steps
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
            steps: plan.steps.into_iter().collect(),
            depends: plan
                .dependencies
                .iter()
                .filter(|id| **id != resolved.id)
                .filter_map(|id| target_indices.get(id).copied())
                .chain(unity_prerequisite)
                .collect(),
            in_all: !resolved.metadata.exclude_from_all
                && !model.excluded_from_all(target)
                && (target.kind != TargetKind::InterfaceLibrary
                    || !resolved.compile_units.is_empty()),
        });
    }
    graph.targets.extend(unity_prerequisites);
    Ok(graph)
}

struct TargetPlan<'a> {
    model: &'a ProjectModel,
    config: &'a ConfigId,
    target: &'a Target,
    resolved: &'a ResolvedTarget,
    inputs: Option<&'a TargetInputs>,
    steps: IndexSet<usize>,
    dependencies: IndexSet<TargetId>,
    ide_steps: Option<Vec<IdeCustomStep>>,
}

impl<'a> TargetPlan<'a> {
    fn new(
        model: &'a ProjectModel,
        config: &'a ConfigId,
        target: &'a Target,
        resolved: &'a ResolvedTarget,
        inputs: Option<&'a TargetInputs>,
    ) -> Self {
        let mut dependencies: IndexSet<_> = resolved.dependencies.iter().copied().collect();
        dependencies.extend(
            target
                .utility_deps
                .iter()
                .filter_map(|dep| model.find_target(target.dir, &dep.value)),
        );
        Self {
            model,
            config,
            target,
            resolved,
            inputs,
            steps: IndexSet::new(),
            dependencies,
            ide_steps: resolved.ide.as_ref().map(|_| Vec::new()),
        }
    }

    fn add_source_commands(
        &mut self,
        commands: &Commands<'_>,
        unity: Option<&TargetUnity>,
        target_indices: &IndexMap<TargetId, usize>,
        unity_prerequisites: &mut Vec<GraphTarget>,
        graph_steps: &mut Vec<BuildStep>,
    ) -> Result<Option<usize>, ResolveError> {
        let (model, config, target, resolved) =
            (self.model, self.config, self.target, self.resolved);
        let dir = model.dir(target.dir);
        let mut unity_inputs = IndexMap::<_, IndexSet<_>>::new();
        let ninja = matches!(
            model.generator,
            Some(GeneratorKind::Ninja | GeneratorKind::NinjaMultiConfig)
        );
        let paths = if let Some(inputs) = self.inputs {
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
                    commands.add_producers([step], &mut self.steps, &mut self.dependencies)?;
                    if ninja
                        && let Some(file) =
                            unity.and_then(|plan| plan.file_for_source(&path, config))
                    {
                        unity_inputs.entry(file).or_default().insert(input);
                    }
                }
            }
        }

        let unity_prerequisite =
            if !self.steps.is_empty() && unity.is_some_and(|plan| !plan.files.is_empty()) {
                // An included source can be an output of a custom command.
                let index = target_indices.len() + unity_prerequisites.len();
                let mut name = BString::from(format!("cmake_object_order_depends_unity_{index}"));
                while model.find_target(DirId::ROOT, &name).is_some() {
                    name.push(b'_');
                }
                unity_prerequisites.push(GraphTarget {
                    name,
                    steps: std::mem::take(&mut self.steps).into_iter().collect(),
                    depends: self
                        .dependencies
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
            self.steps.insert(push(
                graph_steps,
                BuildStep::Custom(CustomStep {
                    outputs: vec![build_relative(model, file)],
                    depends: generated_sources.into_iter().collect(),
                    ..CustomStep::default()
                }),
            ));
        }
        Ok(unity_prerequisite)
    }

    fn add_compile_steps(
        &mut self,
        artifacts: &ArtifactSet,
        commands: &Commands<'_>,
        precompiled: &pch::Plans,
        graph_steps: &mut Vec<BuildStep>,
        compile_inputs: &mut BTreeMap<BString, Vec<BString>>,
    ) -> Result<Vec<BString>, ResolveError> {
        let (model, config, target, resolved) =
            (self.model, self.config, self.target, self.resolved);
        let dir = model.dir(target.dir);
        let mut objects = self
            .inputs
            .into_iter()
            .flat_map(|inputs| inputs.objects.iter().cloned())
            .collect::<Vec<_>>();
        let cxx_standard = self
            .inputs
            .and_then(|inputs| inputs.standard_requirements.get(b"CXX".as_slice()).copied());
        let position_independent = self.inputs.and_then(|inputs| inputs.position_independent);
        for step in super::modules::lower_modules_with_standard(
            model,
            resolved,
            cxx_standard,
            position_independent,
        )? {
            self.steps.insert(push(graph_steps, step));
        }
        let mut pch_languages = IndexSet::new();
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
            let required_standard = self
                .inputs
                .and_then(|inputs| inputs.standard_requirements.get(&group.language).copied());
            let pch = precompiled
                .find(resolved.id, &group.language)
                .filter(|_| !pch::skip_unit(model, target, unit));
            if let Some((owner, plan)) = pch {
                if owner != resolved.id {
                    self.dependencies.insert(owner);
                } else if pch_languages.insert(&group.language) {
                    let unit = CompileUnit {
                        source: BT::new(plan.source.clone(), target.backtrace.clone()),
                        object: plan.artifact.clone(),
                        ..CompileUnit::default()
                    };
                    let step = compile(
                        model,
                        target,
                        config,
                        artifacts,
                        &unit,
                        group,
                        Settings {
                            standard: required_standard,
                            pch: Some(PchFlags::Create(plan)),
                            position_independent,
                        },
                    )?;
                    let mut requires = vec![plan.header.clone()];
                    for header in &plan.project_headers {
                        let path = command_path(model, &dir.binary_dir, header);
                        if commands
                            .producer(&path)
                            .is_some_and(|producer| self.steps.contains(&producer))
                        {
                            requires.push(path);
                        }
                    }
                    compile_inputs.insert(plan.artifact.clone(), requires);
                    self.steps.insert(push(graph_steps, step));
                }
            }
            let mut step = compile(
                model,
                target,
                config,
                artifacts,
                unit,
                group,
                Settings {
                    standard: required_standard,
                    pch: pch.map(|(_, plan)| PchFlags::Use(plan)),
                    position_independent,
                },
            )?;
            if let BuildStep::Compile { object, dyndep, .. } = &mut step {
                if let Some((_, plan)) = pch {
                    compile_inputs.insert(
                        object.clone(),
                        vec![plan.header.clone(), plan.artifact.clone()],
                    );
                }
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
            self.steps.insert(push(graph_steps, step));
        }
        Ok(objects)
    }

    fn add_link_and_target_commands(
        &mut self,
        artifacts: &ArtifactSet,
        objects: Vec<BString>,
        commands: &Commands<'_>,
        usage: &mut UsageResolver<'_>,
        graph_steps: &mut Vec<BuildStep>,
    ) -> Result<(), ResolveError> {
        let (model, config, target, resolved) =
            (self.model, self.config, self.target, self.resolved);
        if let Some(step) = link_step(model, target, resolved, artifacts)? {
            let before = commands.events(
                resolved.id,
                &[Some(TargetEvent::PreBuild), Some(TargetEvent::PreLink)],
                usage,
                &mut self.steps,
                &mut self.dependencies,
                self.ide_steps.as_mut(),
            )?;
            let after = commands.events(
                resolved.id,
                &[Some(TargetEvent::PostBuild)],
                usage,
                &mut self.steps,
                &mut self.dependencies,
                self.ide_steps.as_mut(),
            )?;
            let position_independent = self.inputs.and_then(|inputs| inputs.position_independent);
            let link = link(
                model,
                target,
                config,
                artifacts,
                step,
                objects,
                position_independent,
            )?;
            let link = attach_link_events(model, artifacts, link, before, after);
            self.steps.insert(push(graph_steps, link));
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
                &mut self.steps,
                &mut self.dependencies,
                self.ide_steps.as_mut(),
            )?;
            if utility != CustomStep::default() {
                remove_own_outputs(&mut utility);
                self.steps
                    .insert(push(graph_steps, BuildStep::Custom(utility)));
            }
        }

        Ok(())
    }
}

fn link_step(
    model: &ProjectModel,
    target: &Target,
    resolved: &ResolvedTarget,
    artifacts: &ArtifactSet,
) -> Result<Option<LinkStep>, ResolveError> {
    let Some(mut step) = resolved
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
            let kind = LinkKind::for_target(target.kind)?;
            let output = build_relative(model, artifacts.file.as_ref()?);
            Some(LinkStep {
                kind,
                language: BString::default(),
                fragments: Vec::new(),
                output,
            })
        })
    else {
        return Ok(None);
    };
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
    Ok(Some(step))
}

fn artifacts(model: &ProjectModel, target: &Target, resolved: &ResolvedTarget) -> ArtifactSet {
    let mut artifacts = resolved.artifacts.clone();
    if artifacts.file.is_none() && artifacts.object_dir.is_none() {
        artifacts = compute_artifacts(model, target, &resolved.config);
    }
    artifacts
}

pub(super) fn push(steps: &mut Vec<BuildStep>, step: BuildStep) -> usize {
    let index = steps.len();
    steps.push(step);
    index
}
