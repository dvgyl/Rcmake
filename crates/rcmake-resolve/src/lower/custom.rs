//! Lower custom commands and attach target events to build recipes.

use std::cell::RefCell;

use indexmap::{IndexMap, IndexSet};
use rcmake_base::{BString, hash::HashAlgo, list, path_lex::Lexical, truth};
use rcmake_diag::{PolicyId, PolicyStatus};
use rcmake_genex::{ConfigId, EvalContext, EvalSite, evaluate, parse};
use rcmake_model::{
    BT, CommandId, CustomCommand, CustomCommandKind, DirId, GeneratorKind, ProjectModel, Target,
    TargetEvent, TargetId, TargetKind,
};

use super::graph::push;
use super::shell::direct;
use super::{build_relative, compute_artifacts, definition, join};
use crate::ide::{IdeCustomPhase, IdeCustomStep};
use crate::sources::{self, SourcePaths, absolute_path};
use crate::usage::{ModelGenexHost, ObjectPaths, UsageResolver, handle};
use crate::{ArtifactSet, BuildStep, CrossConfiguration, CustomStep, ResolveError, ResolvedTarget};

pub(super) struct Commands<'a> {
    model: &'a ProjectModel,
    targets: &'a IndexMap<TargetId, &'a ResolvedTarget>,
    objects: ObjectPaths<'a>,
    config: &'a ConfigId,
    cross: Option<CrossConfiguration<'a>>,
    outputs: IndexMap<BString, usize>,
    inputs: Vec<Vec<BString>>,
    pub(super) codegen: IndexSet<usize>,
    dependencies: Vec<IndexSet<TargetId>>,
}

impl<'a> Commands<'a> {
    pub(super) fn new(
        model: &'a ProjectModel,
        targets: &'a IndexMap<TargetId, &'a ResolvedTarget>,
        config: &'a ConfigId,
        cross: Option<CrossConfiguration<'a>>,
        usage: &mut UsageResolver<'_>,
        steps: &mut Vec<BuildStep>,
    ) -> Result<Self, ResolveError> {
        let mut commands = Self {
            model,
            targets,
            objects: ObjectPaths::new(targets.values().copied()),
            config,
            cross,
            outputs: IndexMap::new(),
            inputs: Vec::new(),
            codegen: IndexSet::new(),
            dependencies: Vec::new(),
        };
        for (id, command) in model
            .commands()
            .filter(|(_, command)| command.kind == CustomCommandKind::Output)
        {
            let (step, dependencies) = commands.lower_command(id, command, usage)?;
            let index = steps.len();
            if command.codegen {
                commands.codegen.insert(index);
            }
            for path in step.outputs.iter().chain(&step.byproducts) {
                commands.outputs.entry(path.clone()).or_insert(index);
            }
            commands.inputs.push(step.depends.clone());
            commands.dependencies.push(dependencies);
            push(steps, BuildStep::Custom(step));
        }
        Ok(commands)
    }

    pub(super) fn producer(&self, path: &BString) -> Option<usize> {
        self.outputs.get(path).copied()
    }

    fn lower_command(
        &self,
        id: CommandId,
        command: &CustomCommand,
        usage: &mut UsageResolver<'_>,
    ) -> Result<(CustomStep, IndexSet<TargetId>), ResolveError> {
        let binary = &self.model.dir(command.dir).binary_dir;
        let artifact_dependencies = RefCell::new(IndexSet::new());
        let paths = usage.source_paths();
        let command_config = self
            .cross
            .as_ref()
            .map_or(self.config, |cross| cross.command);
        let argument_config = if self
            .cross
            .as_ref()
            .is_some_and(|cross| cross.shared_outputs.contains(&id))
        {
            command_config
        } else {
            self.config
        };
        let mut eval =
            CommandEvaluation::new(self.model, command, command_config, self.config, usage);
        eval.host = eval.host.with_object_paths(&self.objects);
        let outputs = eval.paths(binary, &command.outputs)?;
        let byproducts = eval.paths(binary, &command.byproducts)?;
        eval.context.config = argument_config.clone();
        let working_dir = eval
            .optional(&command.working_directory)?
            .map_or_else(|| binary.clone(), |dir| join(binary, &dir));
        let working_dir = (!working_dir.is_empty()
            && working_dir != self.model.dir(DirId::ROOT).binary_dir)
            .then_some(working_dir);
        let comment = eval.optional(&command.comment)?;
        let depfile = eval
            .optional(&command.depfile)?
            .filter(|path| !path.is_empty());
        let mut dependencies = IndexSet::new();
        let mut depends = IndexSet::new();
        eval.host.artifact_dependencies = Some(&artifact_dependencies);

        for dep in command
            .main_dependency
            .iter()
            .chain(command.depends.iter().map(|entry| &entry.value))
            .chain(command.implicit_depends.iter().map(|(_, file)| file))
        {
            for dep in list::split(&eval.value(dep)?, list::Empty::Drop) {
                depends.insert(self.dependency_path(
                    command.dir,
                    &dep,
                    paths,
                    command_config,
                    &mut dependencies,
                ));
            }
        }
        let mut invocations = Vec::new();
        for line in &command.command_lines {
            let mut argv = Vec::new();
            for (index, arg) in line.iter().enumerate() {
                eval.context.config = if index == 0 {
                    command_config
                } else {
                    argument_config
                }
                .clone();
                let value = eval.value(arg)?;
                if command.command_expand_lists {
                    argv.extend(list::split(&value, list::Empty::Drop));
                } else {
                    argv.push(value);
                }
            }
            let Some(program) = argv.first_mut().filter(|program| !program.is_empty()) else {
                continue;
            };
            if let Some(id) = self.model.find_target(command.dir, program)
                && let Some(target) = self
                    .model
                    .target(id)
                    .filter(|target| target.kind == TargetKind::Executable)
                && let Some(file) = self.artifact(id, target, command_config)
            {
                artifact_dependencies
                    .borrow_mut()
                    .insert((id, command_config.clone()));
                *program = file;
            }
            invocations.push(direct(argv, working_dir.clone()));
        }
        for (id, config) in artifact_dependencies.into_inner() {
            if Some(id) != command_target(command)
                && let Some(target) = self.model.target(id).filter(|target| !target.is_imported())
            {
                if config == *self.config {
                    dependencies.insert(id);
                }
                // File inputs also order custom commands whose target has no compile steps.
                if let Some(file) = self.artifact(id, target, &config) {
                    depends.insert(build_relative(self.model, &file));
                }
            }
        }
        let relative = |paths: Vec<BString>| {
            paths
                .into_iter()
                .map(|path| build_relative(self.model, &path))
                .collect::<IndexSet<_>>()
                .into_iter()
                .collect()
        };
        let symbolic = outputs.first().is_some_and(|output| {
            sources::properties(self.model.dir(command.dir), output)
                .get(b"SYMBOLIC")
                .is_some_and(|value| truth::is_on(value))
        });
        let depfile = depfile.map(|path| self.depfile(command, &path, &outputs, &mut invocations));
        let mut step = CustomStep {
            outputs: relative(outputs),
            byproducts: relative(byproducts),
            depends: depends.into_iter().collect(),
            invocations,
            comment,
            depfile,
            uses_terminal: command.uses_terminal,
            job_pool: command.job_pool.clone(),
            symbolic,
        };
        step.byproducts.retain(|path| !step.outputs.contains(path));
        Ok((step, dependencies))
    }

    fn depfile(
        &self,
        command: &CustomCommand,
        path: &[u8],
        outputs: &[BString],
        invocations: &mut Vec<crate::Invocation>,
    ) -> BString {
        let root = self.model.dir(DirId::ROOT);
        let dir = self.model.dir(command.dir);
        let Some(generator) = self.model.generator else {
            return command_path(self.model, &dir.binary_dir, path);
        };
        let ninja = matches!(
            generator,
            GeneratorKind::Ninja | GeneratorKind::NinjaMultiConfig
        );
        if ninja && command.policies.get(PolicyId::new(116)) != PolicyStatus::New {
            return command_path(self.model, &root.binary_dir, path);
        }
        let original = absolute_path(&dir.binary_dir, path);
        let mut identity = HashAlgo::Sha256.hasher();
        let xcode = generator == GeneratorKind::Xcode;
        let declared_depfile = command
            .depfile
            .as_deref()
            .map_or(path, |value| value.as_ref());
        let id = if xcode {
            declared_depfile
        } else {
            original.as_slice()
        };
        identity.update(id);
        identity.update(b"\0");
        if xcode {
            identity.update(&dir.binary_dir);
            identity.update(b"\0");
        }
        let declared = command.outputs.iter().map(|entry| &entry.value);
        let resolved = outputs.iter();
        let parts: Box<dyn Iterator<Item = &BString>> = if xcode {
            Box::new(declared)
        } else {
            Box::new(resolved)
        };
        for part in parts {
            identity.update(part);
            identity.update(b"\0");
        }
        let config = if xcode { ".$(CONFIGURATION)" } else { "" };
        let name = BString::from(format!(
            "CMakeFiles/rcmake-depfile-{}{config}.d",
            identity.finish_hex()
        ));
        let transformed = absolute_path(&root.binary_dir, &name);
        let cmake = definition(self.model, command.dir, b"CMAKE_COMMAND")
            .cloned()
            .unwrap_or_else(|| "cmake".into());
        if !invocations.is_empty()
            && matches!(
                generator,
                GeneratorKind::Ninja
                    | GeneratorKind::NinjaMultiConfig
                    | GeneratorKind::UnixMakefiles
                    | GeneratorKind::Xcode
            )
        {
            invocations.push(direct(
                vec![
                    cmake,
                    "-E".into(),
                    "cmake_transform_depfile".into(),
                    generator.name().into(),
                    if generator == GeneratorKind::UnixMakefiles {
                        "makedepfile"
                    } else {
                        "gccdepfile"
                    }
                    .into(),
                    root.source_dir.clone(),
                    dir.source_dir.clone(),
                    root.binary_dir.clone(),
                    dir.binary_dir.clone(),
                    original,
                    transformed,
                ],
                None,
            ));
        }
        name
    }

    fn dependency_path(
        &self,
        dir: DirId,
        dep: &BString,
        paths: Option<&SourcePaths<'_>>,
        command_config: &ConfigId,
        dependencies: &mut IndexSet<TargetId>,
    ) -> BString {
        if let Some(id) = self.model.find_target(dir, dep)
            && let Some(target) = self.model.target(id)
        {
            if command_config == self.config {
                dependencies.insert(id);
            }
            return self.artifact(id, target, command_config).map_or_else(
                || {
                    if command_config == self.config {
                        dep.clone()
                    } else {
                        [dep.as_slice(), b":", &command_config.0].concat().into()
                    }
                },
                |path| build_relative(self.model, &path),
            );
        }
        let dir = self.model.dir(dir);
        let source = absolute_path(&dir.source_dir, dep);
        let binary = absolute_path(&dir.binary_dir, dep);
        let in_source = paths.map_or_else(
            || dir.source_props.contains_key(&source),
            |paths| paths.is_source_dependency(dir, &source, &binary),
        );
        build_relative(self.model, if in_source { &source } else { &binary })
    }

    fn artifact(&self, id: TargetId, target: &Target, config: &ConfigId) -> Option<BString> {
        self.targets
            .get(&id)
            .filter(|resolved| resolved.config == *config)
            .and_then(|resolved| resolved.artifacts.file.clone())
            .or_else(|| compute_artifacts(self.model, target, config).file)
            .map(|path| join(&self.model.dir(DirId::ROOT).binary_dir, &path))
    }

    pub(super) fn events(
        &self,
        id: TargetId,
        events: &[Option<TargetEvent>],
        usage: &mut UsageResolver<'_>,
        steps: &mut IndexSet<usize>,
        dependencies: &mut IndexSet<TargetId>,
        mut ide_steps: Option<&mut Vec<IdeCustomStep>>,
    ) -> Result<CustomStep, ResolveError> {
        let mut recipe = CustomStep::default();
        for &event in events {
            let kind = event.map_or(CustomCommandKind::Utility(id), |event| {
                CustomCommandKind::TargetEvent { target: id, event }
            });
            for (command_id, command) in self
                .model
                .commands()
                .filter(|(_, command)| command.kind == kind)
            {
                if event.is_some()
                    && self
                        .cross
                        .as_ref()
                        .is_some_and(|cross| cross.shared_outputs.contains(&command_id))
                {
                    continue;
                }
                let (step, command_deps) = self.lower_command(command_id, command, usage)?;
                dependencies.extend(command_deps);
                self.add_producers(
                    step.depends.iter().filter_map(|path| self.producer(path)),
                    steps,
                    dependencies,
                )?;
                if let Some(steps) = &mut ide_steps {
                    let phase = match event {
                        None => IdeCustomPhase::Rule,
                        Some(TargetEvent::PreBuild) => IdeCustomPhase::PreBuild,
                        Some(TargetEvent::PreLink) => IdeCustomPhase::PreLink,
                        Some(TargetEvent::PostBuild) => IdeCustomPhase::PostBuild,
                    };
                    steps.push(ide_custom_step(self.model, &step, phase, event.is_none()));
                }
                merge(&mut recipe, step);
            }
        }
        Ok(recipe)
    }

    pub(super) fn add_producers(
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
                    pending.extend(
                        inputs.iter().rev().filter_map(|path| {
                            self.producer(path).map(|producer| (producer, false))
                        }),
                    );
                }
            }
        }
        Ok(())
    }
}

struct CommandEvaluation<'model, 'resolver> {
    context: EvalContext,
    host: ModelGenexHost<'model, 'resolver>,
}

impl<'model, 'resolver> CommandEvaluation<'model, 'resolver> {
    fn new(
        model: &ProjectModel,
        command: &CustomCommand,
        command_config: &ConfigId,
        output_config: &ConfigId,
        usage: &'resolver mut UsageResolver<'model>,
    ) -> Self {
        let target = command_target(command).map(handle);
        let mut host = usage.host(command.dir);
        host.current = target;
        let multi_config = model.generator == Some(GeneratorKind::NinjaMultiConfig);
        Self {
            context: EvalContext {
                config: output_config.clone(),
                head: target,
                current: target,
                site: EvalSite {
                    backtrace: command.backtrace.clone(),
                    property: None,
                },
                command_config: multi_config.then(|| command_config.clone()),
                output_config: multi_config.then(|| output_config.clone()),
                ..EvalContext::default()
            },
            host,
        }
    }

    fn value(&mut self, value: &[u8]) -> Result<BString, ResolveError> {
        if !value.windows(2).any(|pair| pair == b"$<") {
            return Ok(value.into());
        }
        parse(value)
            .and_then(|ast| evaluate(&ast, &self.context, &mut self.host))
            .map(|(value, _)| value)
            .map_err(|error| {
                error
                    .at(&self.context.site.backtrace, self.host.source_root())
                    .into()
            })
    }

    fn optional(&mut self, value: &Option<BString>) -> Result<Option<BString>, ResolveError> {
        value.as_ref().map(|value| self.value(value)).transpose()
    }

    fn paths(
        &mut self,
        binary: &[u8],
        entries: &[BT<BString>],
    ) -> Result<Vec<BString>, ResolveError> {
        let mut paths = Vec::new();
        for entry in entries {
            let evaluated = self.value(&entry.value)?;
            paths.extend(
                list::split(&evaluated, list::Empty::Drop)
                    .into_iter()
                    .map(|path| Lexical::new(&join(binary, &path)).normalize()),
            );
        }
        Ok(paths)
    }
}

pub(super) fn ide_custom_step(
    model: &ProjectModel,
    step: &CustomStep,
    phase: IdeCustomPhase,
    always_run: bool,
) -> IdeCustomStep {
    let root = &model.dir(DirId::ROOT).binary_dir;
    let paths = |paths: &[BString]| paths.iter().map(|path| absolute_path(root, path)).collect();
    IdeCustomStep {
        phase,
        always_run,
        commands: step
            .invocations
            .iter()
            .map(|invocation| invocation.argv.clone())
            .collect(),
        working_dir: step
            .invocations
            .first()
            .and_then(|invocation| invocation.working_dir.clone())
            .unwrap_or_else(|| root.clone()),
        outputs: paths(&step.outputs),
        byproducts: paths(&step.byproducts),
        depends: paths(&step.depends),
        comment: step.comment.clone(),
    }
}

fn command_target(command: &CustomCommand) -> Option<TargetId> {
    match command.kind {
        CustomCommandKind::Output => None,
        CustomCommandKind::TargetEvent { target, .. } | CustomCommandKind::Utility(target) => {
            Some(target)
        }
    }
}

pub(crate) fn command_outputs(
    model: &ProjectModel,
    command: &CustomCommand,
    config: &ConfigId,
    usage: &mut UsageResolver<'_>,
) -> Result<(Vec<BString>, Vec<BString>), ResolveError> {
    let mut eval = CommandEvaluation::new(model, command, config, config, usage);
    let binary = &model.dir(command.dir).binary_dir;
    Ok((
        eval.paths(binary, &command.outputs)?,
        eval.paths(binary, &command.byproducts)?,
    ))
}

pub(super) fn command_path(model: &ProjectModel, dir: &[u8], path: &[u8]) -> BString {
    build_relative(model, &join(dir, path))
}

fn merge(recipe: &mut CustomStep, step: CustomStep) {
    recipe.outputs.extend(step.outputs);
    recipe.byproducts.extend(step.byproducts);
    recipe.depends.extend(step.depends);
    recipe.invocations.extend(step.invocations);
    recipe.comment = recipe.comment.take().or(step.comment);
    recipe.depfile = recipe.depfile.take().or(step.depfile);
    recipe.uses_terminal |= step.uses_terminal;
    recipe.job_pool = recipe.job_pool.take().or(step.job_pool);
    recipe.symbolic |= step.symbolic;
}

pub(super) fn remove_own_outputs(recipe: &mut CustomStep) {
    recipe.outputs = unique(std::mem::take(&mut recipe.outputs));
    recipe.byproducts = unique(std::mem::take(&mut recipe.byproducts));
    recipe
        .byproducts
        .retain(|path| !recipe.outputs.contains(path));
    recipe.depends = unique(std::mem::take(&mut recipe.depends));
    recipe
        .depends
        .retain(|path| !recipe.outputs.contains(path) && !recipe.byproducts.contains(path));
}

fn unique(values: Vec<BString>) -> Vec<BString> {
    values
        .into_iter()
        .collect::<IndexSet<_>>()
        .into_iter()
        .collect()
}

pub(super) fn attach_link_events(
    model: &ProjectModel,
    artifacts: &ArtifactSet,
    link: BuildStep,
    mut before: CustomStep,
    after: CustomStep,
) -> BuildStep {
    let BuildStep::Link {
        step,
        inputs,
        invocations,
    } = link
    else {
        return link;
    };
    before.invocations.extend(invocations);
    merge(&mut before, after);

    // A link step has one output. Use a custom edge when the recipe must
    // publish more outputs or carry custom rule settings.
    if before.outputs.is_empty()
        && before.byproducts.is_empty()
        && before.depfile.is_none()
        && !before.uses_terminal
        && before.job_pool.is_none()
    {
        let mut inputs = inputs;
        for dependency in before.depends {
            if !inputs.contains(&dependency) && dependency != step.output {
                inputs.push(dependency);
            }
        }
        return BuildStep::Link {
            inputs,
            step,
            invocations: before.invocations,
        };
    }
    before.depends.splice(..0, inputs);
    before.outputs.insert(0, step.output);
    before.byproducts.extend(
        [
            &artifacts.soname_file,
            &artifacts.linker_file,
            &artifacts.import_file,
            &artifacts.pdb_file,
        ]
        .into_iter()
        .filter_map(Option::as_ref)
        .map(|path| build_relative(model, path)),
    );
    remove_own_outputs(&mut before);
    BuildStep::Custom(before)
}
