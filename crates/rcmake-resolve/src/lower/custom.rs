//! Lower custom commands and attach target events to build recipes.

use std::cell::RefCell;

use indexmap::{IndexMap, IndexSet};
use rcmake_base::{BString, ByteSlice, list, path_lex::Lexical, truth};
use rcmake_diag::{PolicyId, PolicyStatus};
use rcmake_genex::{ConfigId, EvalContext, EvalSite, evaluate_diagnostics, parse};
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

mod attachments;
mod depfile;
mod outputs;

pub(crate) use outputs::checked_command_outputs;

pub(super) struct Commands<'a> {
    model: &'a ProjectModel,
    targets: &'a IndexMap<TargetId, &'a ResolvedTarget>,
    objects: ObjectPaths<'a>,
    config: &'a ConfigId,
    cross: Option<CrossConfiguration<'a>>,
    outputs: IndexMap<BString, usize>,
    file_outputs: IndexMap<BString, usize>,
    pub(super) byproduct_owners: IndexMap<BString, TargetId>,
    attachments: attachments::Attachments,
    command_dirs: Vec<DirId>,
    inputs: Vec<Vec<BString>>,
    pub(super) codegen: IndexSet<usize>,
    pub(super) native_steps: IndexSet<usize>,
    pub(super) owners: IndexMap<usize, crate::CustomOutputOwners>,
    dependencies: Vec<IndexSet<TargetId>>,
}

/// Combine the errors of several commands into one error, in order.
pub(crate) fn combine_errors(errors: Vec<ResolveError>) -> Result<(), ResolveError> {
    command_result((), errors)
}

fn command_result<T>(result: T, errors: Vec<ResolveError>) -> Result<T, ResolveError> {
    combine(errors).map_or(Ok(result), Err)
}

/// Join errors in order. Return `None` when there is no error.
fn combine(errors: Vec<ResolveError>) -> Option<ResolveError> {
    let mut errors = errors.into_iter();
    let first = errors.next()?;
    if errors.len() == 0 {
        return Some(first);
    }
    let mut diagnostics = first.to_string();
    for error in errors {
        if !diagnostics.ends_with('\n') {
            diagnostics.push('\n');
        }
        diagnostics.push_str(&error.to_string());
    }
    Some(ResolveError::Message(diagnostics.into()))
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
            file_outputs: IndexMap::new(),
            byproduct_owners: attachments::byproduct_owners(model, config, usage)?,
            attachments: attachments::Attachments::default(),
            command_dirs: Vec::new(),
            inputs: Vec::new(),
            codegen: IndexSet::new(),
            native_steps: IndexSet::new(),
            owners: IndexMap::new(),
            dependencies: Vec::new(),
        };
        let mut errors = Vec::new();
        for (id, command) in model
            .commands()
            .filter(|(_, command)| command.kind == CustomCommandKind::Output)
        {
            // CMake reports the errors of every command, not only of the first one.
            let (step, dependencies) = match commands.lower_command(id, command, usage) {
                Ok(lowered) => lowered,
                Err(error) => {
                    errors.push(error);
                    continue;
                }
            };
            let index = steps.len();
            if !commands
                .attachments
                .register(model, (id, command), usage.source_paths(), index)
            {
                continue;
            }
            let shared_output = commands.shared_output(id);
            let key = format!(
                "_RCMAKE_CUSTOM_COMMAND_{}_DEPENDS_EXPLICIT_ONLY",
                id.index()
            );
            let explicit_only = model
                .dir(command.dir)
                .props
                .get(key.as_bytes())
                .is_some_and(|value| truth::is_on(value));
            commands.owners.insert(
                index,
                crate::CustomOutputOwners {
                    command: id,
                    targets: IndexSet::new(),
                    explicit_only,
                    shared_output,
                },
            );
            if shared_output && commands.is_cross() {
                commands.native_steps.insert(index);
            }
            if command.codegen {
                commands.codegen.insert(index);
            }
            for path in step.outputs.iter().chain(&step.byproducts) {
                commands.outputs.entry(path.clone()).or_insert(index);
            }
            let file_outputs = step.outputs.iter().chain(
                step.byproducts
                    .iter()
                    .filter(|_| model.generator != Some(GeneratorKind::UnixMakefiles)),
            );
            for path in file_outputs {
                commands.file_outputs.entry(path.clone()).or_insert(index);
            }
            commands.command_dirs.push(command.dir);
            commands.inputs.push(step.depends.clone());
            commands.dependencies.push(dependencies);
            push(steps, BuildStep::Custom(step));
        }
        command_result(commands, errors)
    }

    pub(super) fn producer(&self, path: &BString) -> Option<usize> {
        self.outputs.get(path).copied()
    }

    pub(super) fn file_producer(&self, path: &BString) -> Option<usize> {
        self.file_outputs.get(path).copied()
    }

    pub(super) fn source_attachment(&self, dir: DirId, path: &BString) -> Option<usize> {
        self.attachments.get(dir, path)
    }

    fn shared_output(&self, id: CommandId) -> bool {
        self.cross
            .as_ref()
            .is_some_and(|cross| cross.shared_outputs.contains(&id))
    }

    fn is_cross(&self) -> bool {
        self.cross
            .as_ref()
            .is_some_and(|cross| cross.command != self.config)
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
        let argument_config = if self.shared_output(id) {
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
        let ninja_posix = self.model.generator.is_some_and(GeneratorKind::is_ninja)
            && !super::shell::windows_host(self.model, command.dir);
        let working_dir = (!working_dir.is_empty()
            && (ninja_posix || working_dir != self.model.dir(DirId::ROOT).binary_dir))
            .then_some(working_dir);
        let comment = eval.optional(&command.comment)?;
        let depfile = eval
            .optional(&command.depfile)?
            .filter(|path| !path.is_empty());
        let mut dependencies = IndexSet::new();
        let mut depends = IndexSet::new();
        eval.host.artifact_dependencies = Some(&artifact_dependencies);
        eval.host.warning_site = Some(command.backtrace.clone());

        let mut target_dependencies = IndexSet::new();
        for dep in command.depends.iter().map(|entry| &entry.value) {
            for dep in list::split(&eval.value(dep)?, list::Empty::Drop) {
                depends.insert(self.dependency_path(
                    command.dir,
                    &dep,
                    paths,
                    command_config,
                    &mut target_dependencies,
                ));
            }
        }
        for dep in command
            .main_dependency
            .iter()
            .chain(command.implicit_depends.iter().map(|(_, file)| file))
        {
            for dep in list::split(&eval.value(dep)?, list::Empty::Drop) {
                depends.insert(self.file_dependency(command.dir, &dep, paths));
            }
        }
        let mut invocations = Vec::new();
        // Errors and warnings of the arguments, in evaluation order, as CMake reports them.
        let mut errors = Vec::new();
        let mut failed = false;
        let warnings = std::rc::Rc::new(RefCell::new(Vec::new()));
        eval.host.deferred_warnings = Some(warnings.clone());
        let take_warnings = |errors: &mut Vec<ResolveError>| {
            errors.extend(warnings.borrow_mut().drain(..).map(ResolveError::Message));
        };
        for line in &command.command_lines {
            let mut argv = Vec::new();
            for (index, arg) in line.iter().enumerate() {
                eval.context.config = if index == 0 {
                    command_config
                } else {
                    argument_config
                }
                .clone();
                let value = eval.value(arg);
                take_warnings(&mut errors);
                let mut value = match value {
                    Ok(value) => value,
                    Err(error) => {
                        errors.push(error);
                        failed = true;
                        continue;
                    }
                };
                if self.model.generator == Some(GeneratorKind::NinjaMultiConfig) {
                    // `CMAKE_CFG_INTDIR` is `${CONFIGURATION}` with this generator.
                    value = expand_cfg_intdir(&value, &eval.context.config);
                }
                if command.command_expand_lists {
                    argv.extend(list::split(&value, list::Empty::Drop));
                } else {
                    argv.push(value);
                }
            }
            if failed {
                continue;
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
                if !target.is_imported()
                    && definition(self.model, command.dir, b"CMAKE_CROSSCOMPILING")
                        .is_some_and(|value| truth::is_on(value))
                    && let Some(emulator) = target.props.get(b"CROSSCOMPILING_EMULATOR")
                {
                    eval.context.config = command_config.clone();
                    let emulator = eval.value(emulator)?;
                    argv.splice(0..0, list::split(&emulator, list::Empty::Drop));
                }
            }
            let invocation = if ninja_posix {
                super::shell::ninja_custom(argv, working_dir.clone(), command.verbatim)
            } else {
                direct(argv, working_dir.clone())
            };
            invocations.push(invocation);
        }
        take_warnings(&mut errors);
        if failed {
            return Err(
                combine(errors).unwrap_or_else(|| ResolveError::Message(BString::default()))
            );
        }
        eval.host
            .write_warnings(errors.into_iter().map(|warning| warning.to_string().into()));
        eval.host.deferred_warnings = None;
        let mut artifact_dependencies = artifact_dependencies.into_inner();
        let mut next = 0;
        while let Some((id, config)) = artifact_dependencies.get_index(next).cloned() {
            next += 1;
            let Some(target) = self
                .model
                .target(id)
                .filter(|_| Some(id) != command_target(command))
            else {
                continue;
            };
            if target.is_imported() {
                artifact_dependencies.extend(target.utility_deps.iter().filter_map(|entry| {
                    let dependency = self.model.find_target(target.dir, &entry.value)?;
                    Some((dependency, config.clone()))
                }));
                continue;
            }
            target_dependencies.insert((id, config.clone()));
            // File inputs also order custom commands whose target has no compile steps.
            if let Some(file) = self.artifact(id, target, &config) {
                depends.insert(build_relative(self.model, &file));
            }
        }
        dependencies.extend(
            target_dependencies
                .iter()
                .filter(|(_, config)| config == self.config)
                .map(|(id, _)| *id),
        );
        let relative = |paths: Vec<BString>| {
            paths
                .into_iter()
                .map(|path| build_relative(self.model, &path))
                .collect::<IndexSet<_>>()
                .into_iter()
                .collect()
        };
        let symbolic = |output: &BString| {
            sources::properties(self.model.dir(command.dir), output)
                .get(b"SYMBOLIC")
                .is_some_and(|value| truth::is_on(value))
        };
        let symbolic_outputs = relative(
            outputs
                .iter()
                .filter(|output| symbolic(output))
                .cloned()
                .collect(),
        );
        let symbolic = outputs.first().is_some_and(symbolic);
        let depfile = depfile.map(|path| self.depfile(command, &path, &outputs, &mut invocations));
        if let Some(path) = depfile
            .as_ref()
            .filter(|path| path.starts_with(b"CMakeFiles/d/"))
            && let Some(paths) = paths
        {
            let parent = join(
                &self.model.dir(DirId::ROOT).binary_dir,
                Lexical::new(path).parent(),
            );
            paths
                .host
                .create_dir_all(&rcmake_host::path::to_native(&parent))
                .map_err(|error| {
                    ResolveError::Message(
                        format!("Cannot create depfile directory {parent}: {error}").into(),
                    )
                })?;
        }
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
            symbolic_outputs,
            target_dependencies: target_dependencies.into_iter().collect(),
        };
        step.byproducts.retain(|path| !step.outputs.contains(path));
        Ok((step, dependencies))
    }

    fn dependency_path(
        &self,
        dir: DirId,
        dep: &BString,
        paths: Option<&SourcePaths<'_>>,
        command_config: &ConfigId,
        dependencies: &mut IndexSet<(TargetId, ConfigId)>,
    ) -> BString {
        if let Some(id) = self.model.find_target(dir, dep)
            && let Some(target) = self.model.target(id)
        {
            dependencies.insert((id, command_config.clone()));
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
        self.file_dependency(dir, dep, paths)
    }

    fn file_dependency(
        &self,
        dir: DirId,
        dep: &BString,
        paths: Option<&SourcePaths<'_>>,
    ) -> BString {
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
                let shared = self.shared_output(command_id);
                if event.is_some() && shared && self.is_cross() {
                    continue;
                }
                let (mut step, command_deps) = self.lower_command(command_id, command, usage)?;
                if event.is_none()
                    && self.model.generator == Some(GeneratorKind::NinjaMultiConfig)
                    && let Some(target) = self.model.target(id)
                {
                    let binary = &self.model.dir(command.dir).binary_dir;
                    let name = [b"CMakeFiles/", target.name.as_slice()].concat();
                    let mut output = command_path(self.model, binary, &name);
                    if !shared {
                        output.push(b'-');
                        output.extend_from_slice(&self.config.0);
                    }
                    step.outputs.push(output);
                }
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
                if event.is_some()
                    && let (Some(existing), Some(comment)) = (&mut recipe.comment, &step.comment)
                {
                    existing.extend_from_slice(b"; ");
                    existing.extend_from_slice(comment);
                }
                if event.is_some()
                    && self.model.generator.is_some_and(GeneratorKind::is_ninja)
                    && !super::shell::windows_host(self.model, command.dir)
                    && let [first, rest @ ..] = step.invocations.as_slice()
                    && !rest.is_empty()
                    && rest.iter().all(|cmd| cmd.working_dir == first.working_dir)
                {
                    let shell = step
                        .invocations
                        .iter()
                        .map(|cmd| cmd.shell.as_slice())
                        .collect::<Vec<_>>()
                        .join(b" && ".as_slice());
                    let mut invocation = super::shell::command(shell.into());
                    invocation.working_dir.clone_from(&first.working_dir);
                    step.invocations = vec![invocation];
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
                    pending.extend(inputs.iter().rev().flat_map(|path| {
                        self.producer(path)
                            .into_iter()
                            .chain(
                                self.command_dirs
                                    .get(step)
                                    .and_then(|dir| self.source_attachment(*dir, path))
                                    .filter(|attached| *attached != step),
                            )
                            .map(|producer| (producer, false))
                    }));
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
        let ast = parse(value)
            .map_err(|error| error.at(&self.context.site.backtrace, self.host.source_root()))?;
        evaluate_diagnostics(&ast, &self.context, &mut self.host)
            .map(|(value, _)| value)
            .or_else(|errors| {
                let diagnostics = errors
                    .into_iter()
                    .map(|error| {
                        error
                            .at(&self.context.site.backtrace, self.host.source_root())
                            .into()
                    })
                    .collect();
                command_result(BString::default(), diagnostics)
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

/// Warn about `DEPFILE` custom commands that depend on policy CMP0116.
///
/// The Ninja generators warn when the policy is not set and the command is in
/// a subdirectory, where the transformation changes the paths, or when
/// `CMAKE_POLICY_WARNING_CMP0116` is on.
pub(crate) fn warn_depfile_policy(model: &ProjectModel, host: &dyn rcmake_host::Host) {
    if !model.generator.is_some_and(GeneratorKind::is_ninja) {
        return;
    }
    let policy = PolicyId::new(116);
    for (_, command) in model.commands() {
        if command
            .depfile
            .as_ref()
            .is_none_or(|depfile| depfile.is_empty())
            || command.policies.get(policy) != PolicyStatus::Warn
            || definition(model, command.dir, b"CMAKE_SUPPRESS_DEVELOPER_WARNINGS")
                .is_some_and(|value| truth::is_on(value))
        {
            continue;
        }
        let enabled = definition(model, command.dir, b"CMAKE_POLICY_WARNING_CMP0116")
            .is_some_and(|value| truth::is_on(value));
        if command.dir == DirId::ROOT && !enabled {
            continue;
        }
        let backtrace = super::listed_backtrace(&command.backtrace);
        let warning = rcmake_diag::Diagnostic {
            annotations: None,
            kind: rcmake_diag::MessageKind::AuthorWarning,
            effect: rcmake_diag::Effect::None,
            category: Some(rcmake_diag::DiagCategory::Policy),
            policy: Some(policy),
            location: rcmake_diag::Location::Backtrace(backtrace),
            body: "Policy CMP0116 is not set: Ninja generators transform DEPFILEs from add_custom_command().  Run \"cmake --help-policy CMP0116\" for policy details.  Use the cmake_policy command to set the policy and suppress this warning.".into(),
        };
        host.write_out(
            rcmake_host::Stream::Stderr,
            &rcmake_diag::render(
                &warning,
                &rcmake_diag::RenderOptions {
                    relative_base: Some(model.dir(DirId::ROOT).source_dir.clone()),
                    ..rcmake_diag::RenderOptions::default()
                },
            ),
        );
    }
}

/// Replace the `${CONFIGURATION}` value of `CMAKE_CFG_INTDIR` with a
/// configuration name.
fn expand_cfg_intdir(value: &[u8], config: &ConfigId) -> BString {
    const INTDIR: &[u8] = b"${CONFIGURATION}";
    if value.find(INTDIR).is_none() {
        return value.into();
    }
    value.replace(INTDIR, &config.0).into()
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
    recipe.symbolic_outputs.extend(step.symbolic_outputs);
    recipe.target_dependencies.extend(step.target_dependencies);
}

pub(super) fn remove_own_outputs(recipe: &mut CustomStep) {
    recipe.outputs = unique(std::mem::take(&mut recipe.outputs));
    recipe
        .symbolic_outputs
        .retain(|path| recipe.outputs.contains(path));
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
    descriptions: &mut IndexMap<BString, BString>,
) -> BuildStep {
    let BuildStep::Link {
        step,
        inputs,
        implicit_inputs,
        invocations,
    } = link
    else {
        return link;
    };
    if model.generator.is_some_and(GeneratorKind::is_ninja)
        && (before.comment.is_some() || after.comment.is_some())
    {
        let link = BString::from(crate::progress::link_description(&step, &step.output));
        let description = [before.comment.as_ref(), Some(&link), after.comment.as_ref()]
            .into_iter()
            .flatten()
            .filter(|text| !text.is_empty())
            .map(|text| text.as_slice())
            .collect::<Vec<_>>()
            .join(b"; ".as_slice());
        descriptions.insert(step.output.clone(), description.into());
    }
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
            if !inputs.contains(&dependency)
                && !implicit_inputs.contains(&dependency)
                && dependency != step.output
            {
                inputs.push(dependency);
            }
        }
        return BuildStep::Link {
            implicit_inputs,
            inputs,
            step,
            invocations: before.invocations,
        };
    }
    before
        .depends
        .splice(..0, inputs.into_iter().chain(implicit_inputs));
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

#[cfg(test)]
mod tests {
    use rcmake_genex::ConfigId;

    use super::expand_cfg_intdir;

    #[test]
    fn cfg_intdir_expands_to_the_configuration() {
        let config = ConfigId("Release".into());
        assert_eq!(
            expand_cfg_intdir(b"-DINTDIR=${CONFIGURATION}/x", &config),
            "-DINTDIR=Release/x"
        );
        assert_eq!(expand_cfg_intdir(b"plain", &config), "plain");
    }
}
