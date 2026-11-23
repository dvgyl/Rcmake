//! Build graph edges and custom commands.

use indexmap::{IndexMap, IndexSet};
use rcmake_base::{BString, list, path_lex::Lexical, truth};
use rcmake_genex::ConfigId;
use rcmake_model::{
    BT, CustomCommand, CustomCommandKind, DirId, ProjectModel, Target, TargetEvent, TargetId,
    TargetKind,
};

use super::artifacts::build_relative;
use super::compile::compile;
use super::link::link;
use super::shell::direct;
use super::{compute_artifacts, join, linker_language};
use crate::{
    ArtifactSet, BuildGraph, BuildStep, CustomStep, GraphTarget, LinkKind, LinkStep, ResolveError,
    ResolvedTarget, sources::TargetInputs,
};

/// Lower resolved targets for one configuration.
///
/// Targets and commands keep their model order where dependencies permit.
pub fn lower(
    model: &ProjectModel,
    targets: &[ResolvedTarget],
    config: &ConfigId,
) -> Result<BuildGraph, ResolveError> {
    lower_with_inputs(model, targets, config, &IndexMap::new())
}

pub(crate) fn lower_with_inputs(
    model: &ProjectModel,
    targets: &[ResolvedTarget],
    config: &ConfigId,
    inputs: &IndexMap<TargetId, TargetInputs>,
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
    let commands = Commands::new(model, &resolved, config, &mut graph.steps);
    for (target, resolved) in local {
        let mut dependencies: IndexSet<_> = resolved.dependencies.iter().copied().collect();
        dependencies.extend(
            target
                .utility_deps
                .iter()
                .filter_map(|dep| model.find_target(target.dir, &dep.value)),
        );
        let mut steps = IndexSet::new();
        let dir = model.dir(target.dir);
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
                if let Some(&step) = commands.outputs.get(&command_path(model, base, &path)) {
                    commands.add_producers([step], &mut steps, &mut dependencies)?;
                }
            }
        }

        let artifacts = artifacts(model, target, resolved);
        let mut objects = target_inputs
            .into_iter()
            .flat_map(|inputs| inputs.objects.iter().cloned())
            .collect::<Vec<_>>();
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
            let step = compile(model, target, config, &artifacts, unit, group)?;
            if let BuildStep::Compile { object, .. } = &step {
                objects.push(object.clone());
            }
            steps.insert(push(&mut graph.steps, step));
        }
        // The supported generators run PRE_BUILD with the pre-link events.
        commands.add_events(
            target,
            resolved.id,
            Some(TargetEvent::PreBuild),
            &mut graph.steps,
            &mut steps,
            &mut dependencies,
        )?;
        commands.add_events(
            target,
            resolved.id,
            Some(TargetEvent::PreLink),
            &mut graph.steps,
            &mut steps,
            &mut dependencies,
        )?;

        if let Some(mut step) = resolved.link.clone().or_else(|| {
            let kind = link_kind(target.kind)?;
            let output = build_relative(model, artifacts.file.as_ref()?);
            Some(LinkStep {
                kind,
                language: BString::default(),
                fragments: Vec::new(),
                output,
            })
        }) {
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
            steps.insert(push(
                &mut graph.steps,
                link(model, target, config, &artifacts, step, objects),
            ));
        }
        commands.add_events(
            target,
            resolved.id,
            Some(TargetEvent::PostBuild),
            &mut graph.steps,
            &mut steps,
            &mut dependencies,
        )?;
        commands.add_events(
            target,
            resolved.id,
            None,
            &mut graph.steps,
            &mut steps,
            &mut dependencies,
        )?;

        graph.targets.push(GraphTarget {
            name: resolved.metadata.name.clone(),
            steps: steps.into_iter().collect(),
            depends: dependencies
                .iter()
                .filter(|id| **id != resolved.id)
                .filter_map(|id| target_indices.get(id).copied())
                .collect(),
            in_all: !resolved.metadata.exclude_from_all
                && !excluded(model, target)
                && (target.kind != TargetKind::InterfaceLibrary
                    || !resolved.compile_units.is_empty()),
        });
    }
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

fn push(steps: &mut Vec<BuildStep>, step: BuildStep) -> usize {
    let index = steps.len();
    steps.push(step);
    index
}

struct Commands<'a> {
    model: &'a ProjectModel,
    targets: &'a IndexMap<TargetId, &'a ResolvedTarget>,
    config: &'a ConfigId,
    outputs: IndexMap<BString, usize>,
    inputs: Vec<Vec<BString>>,
    dependencies: Vec<IndexSet<TargetId>>,
}

impl<'a> Commands<'a> {
    fn new(
        model: &'a ProjectModel,
        targets: &'a IndexMap<TargetId, &'a ResolvedTarget>,
        config: &'a ConfigId,
        steps: &mut Vec<BuildStep>,
    ) -> Self {
        let mut commands = Self {
            model,
            targets,
            config,
            outputs: IndexMap::new(),
            inputs: Vec::new(),
            dependencies: Vec::new(),
        };
        for (_, command) in model
            .commands()
            .filter(|(_, command)| command.kind == CustomCommandKind::Output)
        {
            let (step, dependencies) = commands.lower_command(command);
            let index = steps.len();
            for path in step.outputs.iter().chain(&step.byproducts) {
                commands.outputs.entry(path.clone()).or_insert(index);
            }
            commands.inputs.push(step.depends.clone());
            commands.dependencies.push(dependencies);
            push(steps, BuildStep::Custom(step));
        }
        commands
    }

    fn lower_command(&self, command: &CustomCommand) -> (CustomStep, IndexSet<TargetId>) {
        let binary = &self.model.dir(command.dir).binary_dir;
        let paths = |entries: &[BT<BString>]| {
            entries
                .iter()
                .map(|entry| command_path(self.model, binary, &entry.value))
                .collect()
        };
        let working_dir = command
            .working_directory
            .as_ref()
            .map_or_else(|| binary.clone(), |dir| join(binary, dir));
        let working_dir = (!working_dir.is_empty()
            && working_dir != self.model.dir(DirId::ROOT).binary_dir)
            .then_some(working_dir);
        let mut dependencies = IndexSet::new();
        let invocations = command
            .command_lines
            .iter()
            .filter_map(|line| {
                let mut line = if command.command_expand_lists {
                    line.iter()
                        .flat_map(|arg| list::split(arg, list::Empty::Keep))
                        .collect()
                } else {
                    line.clone()
                };
                let program = line.first_mut()?;
                if program.is_empty() {
                    return None;
                }
                if let Some(id) = self.model.find_target(command.dir, program)
                    && let Some(target) = self
                        .model
                        .target(id)
                        .filter(|target| target.kind == TargetKind::Executable)
                    && let Some(file) = self.artifact(id, target)
                {
                    dependencies.insert(id);
                    *program = file;
                }
                Some(direct(line, working_dir.clone()))
            })
            .collect();
        let depends = command
            .main_dependency
            .iter()
            .chain(command.depends.iter().map(|entry| &entry.value))
            .chain(command.implicit_depends.iter().map(|(_, file)| file))
            .map(|dep| {
                if let Some(id) = self.model.find_target(command.dir, dep)
                    && let Some(target) = self.model.target(id)
                {
                    dependencies.insert(id);
                    return self
                        .artifact(id, target)
                        .map_or_else(|| dep.clone(), |path| build_relative(self.model, &path));
                }
                let source = &self.model.dir(command.dir).source_dir;
                let file = join(source, dep);
                if self.model.dir(command.dir).source_props.contains_key(&file) {
                    build_relative(self.model, &file)
                } else {
                    command_path(self.model, binary, dep)
                }
            })
            .collect::<IndexSet<_>>()
            .into_iter()
            .collect();
        let step = CustomStep {
            outputs: paths(&command.outputs),
            byproducts: paths(&command.byproducts),
            depends,
            invocations,
            comment: command.comment.clone(),
            depfile: command
                .depfile
                .as_ref()
                .map(|path| command_path(self.model, binary, path)),
            uses_terminal: command.uses_terminal,
            job_pool: command.job_pool.clone(),
        };
        (step, dependencies)
    }

    fn artifact(&self, id: TargetId, target: &Target) -> Option<BString> {
        self.targets
            .get(&id)
            .and_then(|resolved| resolved.artifacts.file.clone())
            .or_else(|| compute_artifacts(self.model, target, self.config).file)
            .map(|path| join(&self.model.dir(DirId::ROOT).binary_dir, &path))
    }

    fn add_events(
        &self,
        target: &Target,
        id: TargetId,
        event: Option<TargetEvent>,
        graph_steps: &mut Vec<BuildStep>,
        steps: &mut IndexSet<usize>,
        dependencies: &mut IndexSet<TargetId>,
    ) -> Result<(), ResolveError> {
        for (_, command) in self.model.commands().filter(|(_, command)| {
            command.kind
                == event.map_or(CustomCommandKind::Utility(id), |event| {
                    CustomCommandKind::TargetEvent { target: id, event }
                })
                && command.dir == target.dir
        }) {
            let (step, command_deps) = self.lower_command(command);
            dependencies.extend(command_deps);
            self.add_producers(
                step.depends
                    .iter()
                    .filter_map(|path| self.outputs.get(path).copied()),
                steps,
                dependencies,
            )?;
            steps.insert(push(graph_steps, BuildStep::Custom(step)));
        }
        Ok(())
    }

    fn add_producers(
        &self,
        producers: impl IntoIterator<Item = usize>,
        steps: &mut IndexSet<usize>,
        dependencies: &mut IndexSet<TargetId>,
    ) -> Result<(), ResolveError> {
        for index in producers {
            let mut pending = vec![(index, false)];
            let mut visiting = IndexSet::new();
            while let Some((step, traversed)) = pending.pop() {
                if steps.contains(&step) {
                    continue;
                }
                if traversed {
                    visiting.shift_remove(&step);
                    steps.insert(step);
                    dependencies.extend(self.dependencies.get(step).into_iter().flatten().copied());
                    continue;
                }
                if !visiting.insert(step) {
                    return Err(ResolveError::Message(
                        "Cycle in custom command outputs.".into(),
                    ));
                }
                pending.push((step, true));
                if let Some(inputs) = self.inputs.get(step) {
                    pending.extend(inputs.iter().rev().filter_map(|path| {
                        self.outputs.get(path).map(|&producer| (producer, false))
                    }));
                }
            }
        }
        Ok(())
    }
}

fn command_path(model: &ProjectModel, dir: &[u8], path: &[u8]) -> BString {
    let path = Lexical::new(&join(dir, path)).normalize();
    build_relative(model, &path)
}
