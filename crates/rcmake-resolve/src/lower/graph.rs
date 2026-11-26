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
    if model
        .global
        .props
        .get(b"_RCMAKE_FILE_GENERATE_COUNT")
        .is_some()
    {
        graph.custom_main_sources = commands
            .owners
            .keys()
            .filter_map(|&index| Some((index, commands.main_source(index, usage.source_paths())?)))
            .collect();
    }
    graph.target_ids = local.iter().map(|(_, target)| target.id).collect();
    for (index, &(target, resolved)) in local.iter().enumerate() {
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
            &mut graph.custom_output_owners,
        )?;
        if model
            .global
            .props
            .get(b"_RCMAKE_FILE_GENERATE_COUNT")
            .is_some()
            && !plan.source_command_roots.is_empty()
        {
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
        let framework_dirs = if !resolved.compile_units.is_empty()
            && super::is_darwin(model, target.dir)
        {
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
            (&mut graph.steps, &mut graph.compile_launchers),
            (compile_inputs, ide.pch.get_mut(&resolved.id)),
        )?;
        plan.add_link_and_target_commands(
            &artifacts,
            (objects, &framework_dirs),
            &commands,
            usage,
            &resolved_targets,
            &mut graph,
        )?;

        source_commands::record_owners(&mut graph.custom_output_owners, resolved.id, &plan.steps);
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
        graph: &mut BuildGraph,
    ) -> Result<(), ResolveError> {
        let graph_steps = &mut graph.steps;
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
                    self.steps
                        .insert(push(graph_steps, BuildStep::Custom(resources)));
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
                    graph.link_depfiles.insert(step.output.clone(), depfile);
                }
                if !lowered.install_name_dir.is_empty() {
                    graph
                        .install_name_dirs
                        .insert(step.output.clone(), lowered.install_name_dir);
                }
                if !lowered.extra_outputs.is_empty() {
                    graph
                        .link_extra_outputs
                        .insert(step.output.clone(), lowered.extra_outputs);
                }
            }
            if let Some((path, content)) = lowered.swift_output_file_map {
                graph.swift_output_file_maps.insert(path, content);
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
                self.steps.insert(push(
                    graph_steps,
                    BuildStep::DeviceLink {
                        lang: device.lang,
                        output: device.output,
                        inputs,
                        mode: device.mode,
                    },
                ));
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
                &mut graph.link_descriptions,
            );
            self.steps.insert(push(graph_steps, link));
            if let Some(symlink) = symlink {
                self.steps
                    .insert(push(graph_steps, BuildStep::Custom(symlink)));
            }
        } else {
            self.add_utility_commands(commands, usage, graph)?;
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

pub(super) fn push(steps: &mut Vec<BuildStep>, step: BuildStep) -> usize {
    let index = steps.len();
    steps.push(step);
    index
}
