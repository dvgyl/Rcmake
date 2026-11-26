//! Build graph edges and custom commands.

use std::collections::BTreeMap;

use indexmap::{IndexMap, IndexSet};
use rcmake_base::{BString, list, path_lex::Lexical, truth};
use rcmake_diag::{PolicyId, PolicyStatus};
use rcmake_genex::{ConfigId, EvalContext};
use rcmake_model::{BT, DirId, GeneratorKind, ProjectModel, Target, TargetEvent, TargetId};

use super::artifacts::build_relative;
use super::custom::{Commands, attach_link_events, command_path, ide_custom_step};
use super::link::link;
use super::pch;
use super::{compute_artifacts, linker_language};
use crate::ide::{IdeCustomPhase, IdeCustomStep};
use crate::{
    ArtifactSet, BuildGraph, BuildStep, CompileUnit, CrossConfiguration, CustomStep, GraphTarget,
    LinkFragmentRole, LinkKind, LinkStep, ResolveError, ResolvedTarget, sources::TargetInputs,
    unity::TargetUnity, usage::UsageResolver,
};

mod compile;
mod debug;
mod device;
mod membership;
mod order;
mod source_commands;
mod utility;

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
        &IndexMap::new(),
        &mut UsageResolver::new(model),
        (&mut pch::IdeLowering::default(), None),
        &mut BTreeMap::new(),
        (None, None),
        None,
    )
}

#[expect(
    clippy::too_many_arguments,
    reason = "Lowering writes compile inputs and IDE steps alongside the graph."
)]
pub(crate) fn lower_with_inputs(
    model: &ProjectModel,
    targets: &[ResolvedTarget],
    config: &ConfigId,
    inputs: &IndexMap<TargetId, TargetInputs>,
    fortran: &IndexMap<TargetId, super::fortran::FortranPlan>,
    usage: &mut UsageResolver<'_>,
    (ide, precompiled): (&mut pch::IdeLowering, Option<&pch::Plans>),
    compile_inputs: &mut BTreeMap<BString, Vec<BString>>,
    (cross, traced_commands): (
        Option<CrossConfiguration<'_>>,
        Option<&IndexMap<TargetId, IndexSet<rcmake_model::CommandId>>>,
    ),
    trace_host: Option<&dyn rcmake_host::Host>,
) -> Result<BuildGraph, ResolveError> {
    lower_with_inputs_workers(
        model,
        targets,
        config,
        inputs,
        fortran,
        usage,
        (ide, precompiled),
        compile_inputs,
        (cross, traced_commands),
        trace_host,
        None,
    )
}

#[expect(
    clippy::too_many_arguments,
    reason = "Lowering writes compile inputs and IDE steps alongside the graph."
)]
pub(crate) fn lower_with_inputs_workers(
    model: &ProjectModel,
    targets: &[ResolvedTarget],
    config: &ConfigId,
    inputs: &IndexMap<TargetId, TargetInputs>,
    fortran: &IndexMap<TargetId, super::fortran::FortranPlan>,
    usage: &mut UsageResolver<'_>,
    (ide, precompiled): (&mut pch::IdeLowering, Option<&pch::Plans>),
    compile_inputs: &mut BTreeMap<BString, Vec<BString>>,
    (cross, traced_commands): (
        Option<CrossConfiguration<'_>>,
        Option<&IndexMap<TargetId, IndexSet<rcmake_model::CommandId>>>,
    ),
    trace_host: Option<&dyn rcmake_host::Host>,
    workers_override: Option<usize>,
) -> Result<BuildGraph, ResolveError> {
    usage.runtime_install_targets = super::install::transitive::collect(model, usage, targets)?
        .into_values()
        .flat_map(|targets| {
            targets
                .into_iter()
                .filter_map(|(id, configs)| configs.contains(config).then_some(id))
        })
        .collect();
    let resolved_targets: IndexMap<_, _> = targets
        .iter()
        .filter(|target| config.matches(&target.config.0))
        .map(|target| (target.id, target))
        .collect();
    let local = membership::emitted_targets(model, &resolved_targets);
    let target_indices: IndexMap<_, _> = local
        .iter()
        .enumerate()
        .map(|(index, (_, target))| (target.id, index))
        .collect();
    let mut graph = BuildGraph {
        config: config.clone(),
        ..BuildGraph::default()
    };
    let owned;
    let precompiled = if let Some(precompiled) = precompiled {
        precompiled
    } else {
        owned = pch::Plans::new(model, &resolved_targets, inputs, config, usage)?;
        &owned
    };
    let commands = Commands::new(
        model,
        &resolved_targets,
        config,
        cross,
        usage,
        &mut graph.steps,
    )?;
    if usage.ide().is_some() {
        for &(_, resolved) in &local {
            let headers = precompiled.ide(model, resolved);
            if !headers.is_empty() {
                ide.pch.insert(resolved.id, headers);
            }
        }
    }
    let rule_count = graph.steps.len();
    let mut unity_prerequisites = Vec::new();
    let mut strong_edges = Vec::new();
    graph.custom_output_owners = commands.owners.clone();
    let has_file_generate_count = model
        .global
        .props
        .get(b"_RCMAKE_FILE_GENERATE_COUNT")
        .is_some();
    if has_file_generate_count {
        graph.custom_main_sources = commands
            .owners
            .keys()
            .filter_map(|&index| Some((index, commands.main_source(index, usage.source_paths())?)))
            .collect();
    }
    graph.target_ids = local.iter().map(|(_, target)| target.id).collect();
    let workers = workers_override.unwrap_or_else(|| {
        crate::parallel::worker_count(
            local.len(),
            crate::parallel::TARGET_PARALLEL_THRESHOLD,
            crate::parallel::MAX_WORKERS,
        )
    });
    let unity_is_empty = usage
        .source_paths()
        .and_then(|paths| paths.unity.as_ref())
        .is_none_or(IndexMap::is_empty);
    let can_parallelize = workers > 1
        && local.len() > 1
        && fortran.is_empty()
        && unity_is_empty
        && !can_write_during_lower(model, &local, inputs, usage);
    if !can_parallelize {
        for (index, &(target, resolved)) in local.iter().enumerate() {
            let mut plan =
                TargetPlan::new(model, config, target, resolved, inputs.get(&resolved.id));
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
                &mut graph.custom_output_owners,
            )?;
            if has_file_generate_count && !plan.source_command_roots.is_empty() {
                graph.custom_source_steps.insert(
                    resolved.id,
                    commands.ordered_source_steps(std::mem::take(&mut plan.source_command_roots)),
                );
            }
            if let Some(traced) = traced_commands.and_then(|map| map.get(&resolved.id)) {
                commands.add_traced_commands(traced, &mut plan.steps, &mut plan.dependencies)?;
                source_commands::record_owners(
                    &mut graph.custom_output_owners,
                    resolved.id,
                    &plan.steps,
                );
            }
            if let Some(order) = plan
                .inputs
                .and_then(|inputs| inputs.file_set_order.as_ref())
            {
                graph.file_set_order.insert(index, order.clone());
            }
            let artifacts = artifacts(model, target, resolved);
            let lint = lint_settings(model, resolved.id, usage, config)?;
            let framework_dirs =
                if !resolved.compile_units.is_empty() && super::is_darwin(model, target.dir) {
                    let context = EvalContext {
                        config: config.clone(),
                        link_language: resolved.link.as_ref().map(|link| link.language.clone()),
                        ..Default::default()
                    };
                    let framework_dependencies =
                        usage.compile_framework_targets(resolved.id, &context)?;
                    crate::resolve_link::framework_dirs(
                        model,
                        target,
                        &framework_dependencies,
                        &|id| resolved_targets.get(&id)?.artifacts.file.as_ref(),
                        config,
                    )
                } else {
                    Vec::new()
                };
            let pch = precompiled.details(model, target, resolved, inputs.get(&resolved.id))?;
            if !pch.create.is_empty() || !pch.use_flags.is_empty() {
                graph.pch_details.insert(resolved.id, pch);
            }
            let objects = plan.add_compile_steps(
                &artifacts,
                &commands,
                precompiled,
                (&lint, &framework_dirs),
                fortran.get(&resolved.id),
                usage,
                (&mut graph.steps, 0, &mut graph.compile_launchers),
                (compile_inputs, ide.pch.get_mut(&resolved.id)),
            )?;
            let mut sink = GraphSink {
                steps: &mut graph.steps,
                step_offset: 0,
                utility_steps: &mut graph.utility_steps,
                link_depfiles: &mut graph.link_depfiles,
                install_name_dirs: &mut graph.install_name_dirs,
                link_extra_outputs: &mut graph.link_extra_outputs,
                swift_output_file_maps: &mut graph.swift_output_file_maps,
                link_descriptions: &mut graph.link_descriptions,
            };
            plan.add_link_and_target_commands(
                &artifacts,
                (objects, &framework_dirs),
                &commands,
                usage,
                &resolved_targets,
                &mut sink,
            )?;

            source_commands::record_owners(
                &mut graph.custom_output_owners,
                resolved.id,
                &plan.steps,
            );
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
                ide.custom
                    .insert(resolved.id, rules.chain(events).collect());
            }

            let (depends, strong) = membership::dependencies(
                model,
                &target_indices,
                resolved.id,
                &plan.dependencies,
                &resolved.dependencies,
            );
            strong_edges.push(strong);
            graph.targets.push(GraphTarget {
                name: resolved.metadata.name.clone(),
                steps: plan.steps.into_iter().collect(),
                depends: depends.into_iter().chain(unity_prerequisite).collect(),
                in_all: !resolved.metadata.exclude_from_all,
            });
        }
    } else {
        let chunks = {
            let rule_steps = graph.steps.get(..rule_count).unwrap_or_default();
            let record_provenance = usage.provenance.is_some();
            let template = usage.worker_template();
            let shared_paths = usage
                .source_paths()
                .map(|paths| (paths.shared_index(), paths.host));
            let indexed: Vec<(
                usize,
                &Target,
                &ResolvedTarget,
                Option<Vec<crate::ide::IdePch>>,
            )> = local
                .iter()
                .enumerate()
                .map(|(index, &(target, resolved))| {
                    (index, target, resolved, ide.pch.get(&resolved.id).cloned())
                })
                .collect();
            crate::parallel::map_chunks(&indexed, workers, |chunk| {
                let events = crate::sources::DiagBuffer::new();
                let buffered =
                    shared_paths.map(|(_, host)| crate::sources::BufferedHost::new(host, &events));
                let worker_paths =
                    shared_paths
                        .zip(buffered.as_ref())
                        .map(|((index, _), buffered)| {
                            let host_ref: &dyn rcmake_host::Host = buffered;
                            crate::sources::SourcePaths::for_worker(host_ref, index, &events)
                        });
                let mut worker_usage = template.build(worker_paths.as_ref(), record_provenance);
                let mut outputs = Vec::with_capacity(chunk.len());
                for &(index, target, resolved, ref target_ide_pch) in chunk {
                    if let Some(paths) = &worker_paths {
                        paths.reset_local_dedup();
                    }
                    let result = lower_one_target_parallel(
                        model,
                        config,
                        index,
                        target,
                        resolved,
                        inputs,
                        &resolved_targets,
                        &target_indices,
                        &commands,
                        precompiled,
                        traced_commands,
                        rule_steps,
                        has_file_generate_count,
                        target_ide_pch.clone(),
                        &mut worker_usage,
                    );
                    let diag_events = events.take_events();
                    let target_provenance = worker_usage.provenance.as_mut().map(std::mem::take);
                    outputs.push(TargetLowerWork {
                        diag_events,
                        provenance: target_provenance,
                        result,
                    });
                }
                outputs
            })
        };
        for work in chunks.into_iter().flatten() {
            if let Some(paths) = usage.source_paths() {
                paths.replay_events(work.diag_events);
            }
            if let (Some(dest), Some(src)) = (&mut usage.provenance, work.provenance) {
                dest.extend(src);
            }
            let out = work.result?;
            let base = graph.steps.len();
            let remap = |step: usize| {
                if step >= rule_count {
                    step - rule_count + base
                } else {
                    step
                }
            };
            graph.steps.extend(out.steps);
            for step in out.utility_steps {
                graph.utility_steps.insert(remap(step));
            }
            graph.compile_launchers.extend(out.compile_launchers);
            compile_inputs.extend(out.compile_inputs);
            graph.link_depfiles.extend(out.link_depfiles);
            graph.install_name_dirs.extend(out.install_name_dirs);
            graph.link_extra_outputs.extend(out.link_extra_outputs);
            graph
                .swift_output_file_maps
                .extend(out.swift_output_file_maps);
            graph.link_descriptions.extend(out.link_descriptions);
            if let Some(source_steps) = out.custom_source_steps {
                graph.custom_source_steps.insert(out.id, source_steps);
            }
            if let Some(order) = out.file_set_order {
                graph.file_set_order.insert(out.index, order);
            }
            if let Some(pch) = out.pch_details {
                graph.pch_details.insert(out.id, pch);
            }
            if let Some(custom) = out.ide_custom {
                ide.custom.insert(out.id, custom);
            }
            if let Some(pch) = out.ide_pch {
                ide.pch.insert(out.id, pch);
            }
            let mut target = out.target;
            for step in &mut target.steps {
                *step = remap(*step);
            }
            for &step in &target.steps {
                if step < rule_count
                    && let Some(owners) = graph.custom_output_owners.get_mut(&step)
                {
                    owners.targets.insert(out.id);
                }
            }
            strong_edges.push(out.strong);
            graph.targets.push(target);
        }
    }
    graph.targets.extend(unity_prerequisites);
    let trace =
        trace_host.map(|host| debug::Trace::new(host, model, &local, &target_indices, &graph));
    order::dependencies(
        model,
        &local,
        &strong_edges,
        &mut graph.targets,
        trace.as_ref(),
    )?;
    if matches!(
        model.generator,
        Some(GeneratorKind::Ninja | GeneratorKind::NinjaMultiConfig | GeneratorKind::UnixMakefiles)
    ) && model
        .dir(DirId::ROOT)
        .final_policies
        .is_some_and(|policies| policies.get(PolicyId::new(171)) == PolicyStatus::New)
    {
        add_codegen_target(model, &local, &target_indices, &commands, &mut graph)?;
    }
    if usage.ide() == Some(crate::ide::IdeFlavor::Xcode) {
        remove_inherited_xcode_rules(&graph, &target_indices, rule_count, &mut ide.custom);
    }
    graph.native_custom_steps = commands.native_steps;
    Ok(graph)
}

fn add_codegen_target(
    model: &ProjectModel,
    local: &[(&Target, &ResolvedTarget)],
    indices: &IndexMap<TargetId, usize>,
    commands: &Commands<'_>,
    graph: &mut BuildGraph,
) -> Result<(), ResolveError> {
    // Each entry is a target and the directory of the target that reached it.
    // The codegen target of that directory also runs the reached commands,
    // because a dependency can be in an excluded directory.
    let mut reachable: IndexSet<(usize, DirId)> = local
        .iter()
        .enumerate()
        .filter_map(|(index, (target, _))| {
            let excluded = target
                .props
                .get(b"EXCLUDE_FROM_ALL")
                .is_some_and(|value| truth::is_on(value));
            (!excluded).then_some((index, target.dir))
        })
        .collect();
    let mut pending: Vec<_> = reachable.iter().rev().copied().collect();
    let mut roots = BTreeMap::<DirId, IndexSet<usize>>::new();
    let mut requirements = IndexMap::<usize, IndexSet<usize>>::new();
    while let Some((index, origin)) = pending.pop() {
        let Some(((target, _), owner)) = local.get(index).zip(graph.targets.get(index)) else {
            continue;
        };
        let explicit = target.utility_deps.iter().filter_map(|dep| {
            let id = model.find_target(target.dir, &dep.value)?;
            indices.get(&id).copied()
        });
        for dependency in explicit.clone() {
            if reachable.insert((dependency, origin)) {
                pending.push((dependency, origin));
            }
        }
        for &step in &owner.steps {
            if commands.codegen.contains(&step) {
                roots.entry(target.dir).or_default().insert(step);
                roots.entry(origin).or_default().insert(step);
                let owner_deps: IndexSet<_> = explicit
                    .clone()
                    .filter(|&dep| {
                        dep != index
                            && graph
                                .targets
                                .get(dep)
                                .is_some_and(|target| !target.steps.contains(&step))
                    })
                    .collect();
                requirements
                    .entry(step)
                    .and_modify(|common| common.retain(|dep| owner_deps.contains(dep)))
                    .or_insert(owner_deps);
            }
        }
    }
    for (step, required) in requirements {
        if let Some(BuildStep::Custom(command)) = graph.steps.get_mut(step) {
            for target in required
                .into_iter()
                .filter_map(|index| graph.targets.get(index))
            {
                let name = &target.name;
                if !command.depends.contains(name) {
                    command.depends.push(name.clone());
                }
            }
        }
    }
    let offset = graph.targets.len();
    let directories = model
        .dirs()
        .enumerate()
        .map(|(index, (id, _))| (id, offset + index))
        .collect::<BTreeMap<_, _>>();
    for (dir, directory) in model.dirs() {
        let mut steps = IndexSet::new();
        let mut dependencies = IndexSet::new();
        commands.add_producers(
            roots.remove(&dir).unwrap_or_default(),
            &mut steps,
            &mut dependencies,
        )?;
        let prefix = super::build_relative(model, &directory.binary_dir);
        graph.targets.push(GraphTarget {
            name: super::join(&prefix, b"codegen"),
            steps: steps.into_iter().collect(),
            depends: dependencies
                .iter()
                .filter_map(|id| indices.get(id).copied())
                .chain(directory.children.iter().filter_map(|child| {
                    (!model.dir(*child).exclude_from_all)
                        .then(|| directories.get(child).copied())
                        .flatten()
                }))
                .collect(),
            in_all: false,
        });
    }
    Ok(())
}

fn remove_inherited_xcode_rules(
    graph: &BuildGraph,
    targets: &IndexMap<TargetId, usize>,
    rule_count: usize,
    ide_custom: &mut IndexMap<TargetId, Vec<IdeCustomStep>>,
) {
    for (&id, &index) in targets {
        let Some(target) = graph.targets.get(index) else {
            continue;
        };
        let Some(steps) = ide_custom.get_mut(&id) else {
            continue;
        };
        if !target.steps.iter().any(|step| *step < rule_count) {
            continue;
        }
        let mut pending = target.depends.clone();
        let mut visited = IndexSet::from([index]);
        let mut inherited = IndexSet::<usize>::new();
        while let Some(dependency) = pending.pop() {
            if visited.insert(dependency)
                && let Some(dependency) = graph.targets.get(dependency)
            {
                pending.extend(&dependency.depends);
                inherited.extend(
                    dependency
                        .steps
                        .iter()
                        .copied()
                        .filter(|step| *step < rule_count),
                );
            }
        }
        let mut rules = target.steps.iter().filter(|step| **step < rule_count);
        steps.retain(|step| {
            step.phase != IdeCustomPhase::Rule
                || rules.next().is_none_or(|rule| !inherited.contains(rule))
        });
    }
}

struct TargetPlan<'a> {
    model: &'a ProjectModel,
    config: &'a ConfigId,
    target: &'a Target,
    resolved: &'a ResolvedTarget,
    inputs: Option<&'a TargetInputs>,
    steps: IndexSet<usize>,
    source_command_roots: IndexSet<usize>,
    source_file_inputs: IndexMap<BString, IndexSet<BString>>,
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
        let dependencies = target
            .utility_deps
            .iter()
            .filter_map(|dep| model.find_target(target.dir, &dep.value))
            .collect();
        Self {
            model,
            config,
            target,
            resolved,
            inputs,
            steps: IndexSet::new(),
            source_command_roots: IndexSet::new(),
            source_file_inputs: IndexMap::new(),
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
        owners: &mut IndexMap<usize, crate::CustomOutputOwners>,
    ) -> Result<Option<usize>, ResolveError> {
        let (model, resolved) = (self.model, self.resolved);
        let unity_inputs = self.trace_source_commands(commands, unity)?;
        source_commands::record_owners(owners, resolved.id, &self.steps);

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
                    depends: membership::dependencies(
                        model,
                        target_indices,
                        resolved.id,
                        &self.dependencies,
                        &resolved.dependencies,
                    )
                    .0
                    .into_iter()
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

    fn add_link_and_target_commands(
        &mut self,
        artifacts: &ArtifactSet,
        (mut objects, framework_dirs): (Vec<BString>, &[(BString, bool)]),
        commands: &Commands<'_>,
        usage: &mut UsageResolver<'_>,
        resolved_targets: &IndexMap<TargetId, &ResolvedTarget>,
        sink: &mut GraphSink<'_>,
    ) -> Result<(), ResolveError> {
        let (model, config, target, resolved) =
            (self.model, self.config, self.target, self.resolved);
        if let Some(step) = link_step(model, target, resolved, artifacts)? {
            let mut before = commands.events(
                resolved.id,
                &[Some(TargetEvent::PreBuild), Some(TargetEvent::PreLink)],
                usage,
                &mut self.steps,
                &mut self.dependencies,
                self.ide_steps.as_mut(),
            )?;
            let mut after = commands.events(
                resolved.id,
                &[Some(TargetEvent::PostBuild)],
                usage,
                &mut self.steps,
                &mut self.dependencies,
                self.ide_steps.as_mut(),
            )?;
            let position_independent = self.inputs.and_then(|inputs| inputs.position_independent);
            let mut linker_artifacts = artifacts.clone();
            let bundle = crate::ide::bundle_kind(target);
            let framework = matches!(bundle, crate::ide::BundleKind::Framework { .. });
            let app = matches!(bundle, crate::ide::BundleKind::App { .. });
            if super::is_darwin(model, target.dir)
                && (app || framework || artifacts.import_file.is_some())
                && usage.ide().is_none()
                && let Some(paths) = usage.source_paths()
            {
                let mut resources = super::framework::prepare(
                    model,
                    target,
                    resolved,
                    artifacts,
                    self.inputs,
                    paths.host,
                )?;
                if !resources.invocations.is_empty() {
                    resources.outputs = std::mem::take(&mut resources.byproducts);
                    before.depends.extend(resources.outputs.iter().cloned());
                    let kind = if app { "bundle" } else { "framework" };
                    resources.comment =
                        Some(format!("Copying {kind} resources for {}", target.name).into());
                    self.steps.insert(sink.push(BuildStep::Custom(resources)));
                }
                super::framework::append_text_stub(
                    model,
                    target,
                    resolved.id,
                    config,
                    artifacts,
                    &mut after,
                )?;
                if framework {
                    linker_artifacts
                        .linker_file
                        .clone_from(&linker_artifacts.file);
                }
            }
            let fallback;
            let languages = if let Some(inputs) = self.inputs {
                &inputs.link_languages
            } else {
                let ctx = rcmake_genex::EvalContext {
                    config: config.clone(),
                    link_language: Some(step.language.clone()),
                    ..Default::default()
                };
                fallback = usage
                    .language_closure(
                        resolved.id,
                        &|id| resolved_targets.get(&id).copied(),
                        &ctx,
                        false,
                    )?
                    .0;
                &fallback
            };
            let device = device::plan(model, target, resolved, &step, languages, &objects, usage)?;
            if let Some(device) = &device
                && !objects.contains(&device.output)
            {
                objects.push(device.output.clone());
            }
            let lowered = link(
                model,
                target,
                config,
                &linker_artifacts,
                step,
                objects,
                (resolved, self.inputs, framework_dirs),
                position_independent,
                languages,
                usage,
            )
            .map_err(|error| {
                if let ResolveError::Message(body) = &error
                    && let Some(paths) = usage.source_paths()
                {
                    return paths.target_error(model, target, body.clone());
                }
                error
            })?;
            let mut link = lowered.step;
            if let BuildStep::Link { step, .. } = &link {
                if let Some(depfile) = lowered.depfile {
                    sink.link_depfiles.insert(step.output.clone(), depfile);
                }
                if !lowered.install_name_dir.is_empty() {
                    sink.install_name_dirs
                        .insert(step.output.clone(), lowered.install_name_dir);
                }
                if !lowered.extra_outputs.is_empty() {
                    sink.link_extra_outputs
                        .insert(step.output.clone(), lowered.extra_outputs);
                }
            }
            if let Some((path, content)) = lowered.swift_output_file_map {
                sink.swift_output_file_maps.insert(path, content);
            }
            if let BuildStep::Link {
                inputs,
                implicit_inputs,
                step,
                ..
            } = &mut link
            {
                if step.language == b"Swift" && super::compile::is_one_step_swift(model, target) {
                    for unit in &resolved.compile_units {
                        let is_swift = resolved
                            .compile_groups
                            .get(unit.group)
                            .is_some_and(|group| group.language == b"Swift");
                        if is_swift
                            && let Some(extra) = self.source_file_inputs.get(&unit.source.value)
                        {
                            for dep in extra {
                                if !inputs.contains(dep) && !implicit_inputs.contains(dep) {
                                    implicit_inputs.push(dep.clone());
                                }
                            }
                        }
                    }
                }
                if let Some(target_inputs) = self.inputs {
                    for dependency in &target_inputs.xcframework_inputs {
                        let dependency = super::build_relative(model, dependency);
                        if !inputs.contains(&dependency) && !implicit_inputs.contains(&dependency) {
                            implicit_inputs.push(dependency);
                        }
                    }
                }
            }
            if let Some(device) = device
                && let BuildStep::Link {
                    implicit_inputs, ..
                } = &link
            {
                let mut inputs = device.inputs;
                for dependency in implicit_inputs {
                    if !inputs.contains(dependency) {
                        inputs.push(dependency.clone());
                    }
                }
                self.steps.insert(sink.push(BuildStep::DeviceLink {
                    lang: device.lang,
                    output: device.output,
                    inputs,
                    mode: device.mode,
                }));
            }
            let symlink = if model.generator.is_some_and(GeneratorKind::is_ninja) {
                separate_library_symlinks(&mut link, &mut after)
            } else {
                None
            };
            let mut link_outputs = artifacts.clone();
            if symlink.is_some() || framework {
                link_outputs.soname_file = None;
                link_outputs.linker_file = None;
            }
            let link = attach_link_events(
                model,
                &link_outputs,
                link,
                before,
                after,
                sink.link_descriptions,
            );
            self.steps.insert(sink.push(link));
            if let Some(symlink) = symlink {
                self.steps.insert(sink.push(BuildStep::Custom(symlink)));
            }
        } else {
            self.add_utility_commands(commands, usage, sink)?;
        }

        Ok(())
    }
}

fn lint_settings(
    model: &ProjectModel,
    target: TargetId,
    usage: &mut UsageResolver<'_>,
    config: &ConfigId,
) -> Result<IndexMap<BString, bool>, ResolveError> {
    let context = EvalContext {
        config: config.clone(),
        ..EvalContext::default()
    };
    let mut settings = IndexMap::new();
    for (_, set, _) in usage.source_file_sets(target, &context)? {
        let Some(value) = set.props.get(b"SKIP_LINTING").filter(|value| {
            !value.is_empty() && value.as_slice() != b"NOTFOUND" && !value.ends_with(b"-NOTFOUND")
        }) else {
            continue;
        };
        let Some(owner) = model.target(set.target) else {
            continue;
        };
        let files = usage.file_set_paths(set.target, b"SOURCES", &set.files, &context)?;
        let directory = Lexical::new(&model.dir(owner.dir).source_dir);
        for entry in files {
            settings
                .entry(Lexical::new(&directory.append(&entry.value)).normalize())
                .or_insert_with(|| truth::is_on(value));
        }
    }
    Ok(settings)
}

fn separate_library_symlinks(link: &mut BuildStep, after: &mut CustomStep) -> Option<CustomStep> {
    let BuildStep::Link {
        step, invocations, ..
    } = link
    else {
        return None;
    };
    let index = invocations.iter().position(|invocation| {
        invocation
            .argv
            .get(2)
            .is_some_and(|word| word == b"cmake_symlink_library")
    })?;
    let mut symlink = std::mem::take(after);
    let command = invocations.remove(index);
    let links: IndexSet<_> = command
        .argv
        .get(4..)?
        .iter()
        .filter(|path| **path != step.output)
        .cloned()
        .collect();
    symlink.comment = Some(format!("Creating library symlink {}", list::join(&links)).into());
    symlink.outputs.extend(links);
    symlink.depends.push(step.output.clone());
    symlink.invocations.insert(0, command);
    Some(symlink)
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

pub(super) struct GraphSink<'a> {
    pub(super) steps: &'a mut Vec<BuildStep>,
    pub(super) step_offset: usize,
    pub(super) utility_steps: &'a mut IndexSet<usize>,
    pub(super) link_depfiles: &'a mut IndexMap<BString, BString>,
    pub(super) install_name_dirs: &'a mut IndexMap<BString, BString>,
    pub(super) link_extra_outputs: &'a mut IndexMap<BString, Vec<BString>>,
    pub(super) swift_output_file_maps: &'a mut IndexMap<BString, BString>,
    pub(super) link_descriptions: &'a mut IndexMap<BString, BString>,
}

impl GraphSink<'_> {
    pub(super) fn push(&mut self, step: BuildStep) -> usize {
        push_offset(self.steps, self.step_offset, step)
    }
}

struct TargetLowerOutput {
    id: TargetId,
    index: usize,
    steps: Vec<BuildStep>,
    utility_steps: IndexSet<usize>,
    compile_launchers: IndexMap<BString, BString>,
    compile_inputs: BTreeMap<BString, Vec<BString>>,
    link_depfiles: IndexMap<BString, BString>,
    install_name_dirs: IndexMap<BString, BString>,
    link_extra_outputs: IndexMap<BString, Vec<BString>>,
    swift_output_file_maps: IndexMap<BString, BString>,
    link_descriptions: IndexMap<BString, BString>,
    custom_source_steps: Option<IndexSet<usize>>,
    file_set_order: Option<crate::FileSetOrder>,
    pch_details: Option<crate::PchDetails>,
    ide_custom: Option<Vec<IdeCustomStep>>,
    ide_pch: Option<Vec<crate::ide::IdePch>>,
    strong: IndexSet<usize>,
    target: GraphTarget,
}

struct TargetLowerWork {
    diag_events: Vec<crate::sources::DiagEvent>,
    provenance: Option<crate::usage::provenance::Provenance>,
    result: Result<TargetLowerOutput, ResolveError>,
}

fn can_write_during_lower(
    model: &ProjectModel,
    local: &[(&Target, &ResolvedTarget)],
    inputs: &IndexMap<TargetId, TargetInputs>,
    usage: &UsageResolver<'_>,
) -> bool {
    if usage.source_paths().is_none() {
        return false;
    }
    let ide_is_none = usage.ide().is_none();
    let supports_def_list = matches!(
        usage.ide(),
        None | Some(crate::ide::IdeFlavor::VisualStudio)
    );
    local.iter().any(|&(target, resolved)| {
        let darwin_bundle = ide_is_none
            && super::is_darwin(model, target.dir)
            && matches!(
                crate::ide::bundle_kind(target),
                crate::ide::BundleKind::App { .. } | crate::ide::BundleKind::Framework { .. }
            );
        let pch_pdb = super::definition(model, target.dir, b"CMAKE_PCH_COPY_COMPILE_PDB")
            .is_some_and(|value| truth::is_on(value));
        let def_list = supports_def_list
            && (target
                .props
                .get(b"WINDOWS_EXPORT_ALL_SYMBOLS")
                .is_some_and(|value| truth::is_on(value))
                || inputs.get(&resolved.id).map_or_else(
                    || {
                        target
                            .sources
                            .iter()
                            .filter(|source| {
                                source
                                    .value
                                    .last_chunk::<4>()
                                    .is_some_and(|suffix| suffix.eq_ignore_ascii_case(b".def"))
                            })
                            .count()
                            > 1
                    },
                    |inputs| {
                        inputs
                            .sources
                            .iter()
                            .filter(|path| {
                                path.last_chunk::<4>()
                                    .is_some_and(|suffix| suffix.eq_ignore_ascii_case(b".def"))
                            })
                            .count()
                            > 1
                    },
                ));
        darwin_bundle || pch_pdb || def_list
    })
}

#[expect(
    clippy::too_many_arguments,
    reason = "Per-target parallel lowering reads immutable shared graph and model tables."
)]
fn lower_one_target_parallel(
    model: &ProjectModel,
    config: &ConfigId,
    index: usize,
    target: &Target,
    resolved: &ResolvedTarget,
    inputs: &IndexMap<TargetId, TargetInputs>,
    resolved_targets: &IndexMap<TargetId, &ResolvedTarget>,
    target_indices: &IndexMap<TargetId, usize>,
    commands: &Commands<'_>,
    precompiled: &pch::Plans,
    traced_commands: Option<&IndexMap<TargetId, IndexSet<rcmake_model::CommandId>>>,
    rule_steps: &[BuildStep],
    has_file_generate_count: bool,
    mut target_ide_pch: Option<Vec<crate::ide::IdePch>>,
    usage: &mut UsageResolver<'_>,
) -> Result<TargetLowerOutput, ResolveError> {
    let rule_count = rule_steps.len();
    let mut plan = TargetPlan::new(model, config, target, resolved, inputs.get(&resolved.id));
    let _ = plan.trace_source_commands(commands, None)?;
    let custom_source_steps = (has_file_generate_count && !plan.source_command_roots.is_empty())
        .then(|| commands.ordered_source_steps(std::mem::take(&mut plan.source_command_roots)));
    if let Some(traced) = traced_commands.and_then(|map| map.get(&resolved.id)) {
        commands.add_traced_commands(traced, &mut plan.steps, &mut plan.dependencies)?;
    }
    let file_set_order = plan.inputs.and_then(|inputs| inputs.file_set_order.clone());
    let artifacts = artifacts(model, target, resolved);
    let lint = lint_settings(model, resolved.id, usage, config)?;
    let framework_dirs =
        if !resolved.compile_units.is_empty() && super::is_darwin(model, target.dir) {
            let context = EvalContext {
                config: config.clone(),
                link_language: resolved.link.as_ref().map(|link| link.language.clone()),
                ..Default::default()
            };
            let framework_dependencies = usage.compile_framework_targets(resolved.id, &context)?;
            crate::resolve_link::framework_dirs(
                model,
                target,
                &framework_dependencies,
                &|id| resolved_targets.get(&id)?.artifacts.file.as_ref(),
                config,
            )
        } else {
            Vec::new()
        };
    let pch = precompiled.details(model, target, resolved, inputs.get(&resolved.id))?;
    let pch_details = (!pch.create.is_empty() || !pch.use_flags.is_empty()).then_some(pch);
    let mut steps = Vec::new();
    let mut compile_launchers = IndexMap::new();
    let mut compile_inputs = BTreeMap::new();
    let objects = plan.add_compile_steps(
        &artifacts,
        commands,
        precompiled,
        (&lint, &framework_dirs),
        None,
        usage,
        (&mut steps, rule_count, &mut compile_launchers),
        (&mut compile_inputs, target_ide_pch.as_mut()),
    )?;
    let mut utility_steps = IndexSet::new();
    let mut link_depfiles = IndexMap::new();
    let mut install_name_dirs = IndexMap::new();
    let mut link_extra_outputs = IndexMap::new();
    let mut swift_output_file_maps = IndexMap::new();
    let mut link_descriptions = IndexMap::new();
    let mut sink = GraphSink {
        steps: &mut steps,
        step_offset: rule_count,
        utility_steps: &mut utility_steps,
        link_depfiles: &mut link_depfiles,
        install_name_dirs: &mut install_name_dirs,
        link_extra_outputs: &mut link_extra_outputs,
        swift_output_file_maps: &mut swift_output_file_maps,
        link_descriptions: &mut link_descriptions,
    };
    plan.add_link_and_target_commands(
        &artifacts,
        (objects, &framework_dirs),
        commands,
        usage,
        resolved_targets,
        &mut sink,
    )?;
    let ide_custom = plan.ide_steps.map(|events| {
        let rules = plan
            .steps
            .iter()
            .filter(|&&step_index| step_index < rule_count)
            .filter_map(|&step_index| {
                let BuildStep::Custom(step) = rule_steps.get(step_index)? else {
                    return None;
                };
                Some(ide_custom_step(model, step, IdeCustomPhase::Rule, false))
            });
        rules.chain(events).collect()
    });
    let (depends, strong) = membership::dependencies(
        model,
        target_indices,
        resolved.id,
        &plan.dependencies,
        &resolved.dependencies,
    );
    Ok(TargetLowerOutput {
        id: resolved.id,
        index,
        steps,
        utility_steps,
        compile_launchers,
        compile_inputs,
        link_depfiles,
        install_name_dirs,
        link_extra_outputs,
        swift_output_file_maps,
        link_descriptions,
        custom_source_steps,
        file_set_order,
        pch_details,
        ide_custom,
        ide_pch: target_ide_pch,
        strong,
        target: GraphTarget {
            name: resolved.metadata.name.clone(),
            steps: plan.steps.into_iter().collect(),
            depends: depends.into_iter().collect(),
            in_all: !resolved.metadata.exclude_from_all,
        },
    })
}

pub(super) fn push(steps: &mut Vec<BuildStep>, step: BuildStep) -> usize {
    push_offset(steps, 0, step)
}

pub(super) fn push_offset(steps: &mut Vec<BuildStep>, offset: usize, step: BuildStep) -> usize {
    let index = offset + steps.len();
    steps.push(step);
    index
}
