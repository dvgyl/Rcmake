use std::collections::BTreeMap;

use indexmap::{IndexMap, IndexSet};
use rcmake_base::{BString, list, truth};
use rcmake_diag::{Location, MessageKind};
use rcmake_genex::{ConfigId, EvalContext};
use rcmake_host::Host;
use rcmake_model::{CommandId, DirId, FileSetKind, GeneratorKind, ProjectModel, TargetId};

use crate::{
    BuildGraph, BuildStep, CrossConfiguration, GenerationResult, InstallActionKind, ResolveError,
    ResolveOptions, ResolvedTarget,
    cxx_modules::{generation_location, std_module_model},
    exports::resolve_exports,
    ide, lower,
    resolve_link::{attach_links, resolve_link},
    sources::{SourcePaths, TargetInputs},
    target::resolve_target,
    unity,
    usage::{ResolvedLinkKind, UsageResolver},
};

/// Resolve and lower the project in configuration order.
/// Use `CMAKE_BUILD_TYPE` if `configs` is empty.
///
/// This is [`resolve_with_options`] with the default [`ResolveOptions`].
pub fn resolve(
    model: &ProjectModel,
    host: &dyn Host,
    configs: &[ConfigId],
) -> Result<GenerationResult, ResolveError> {
    resolve_with_options(model, host, configs, &ResolveOptions::default())
}

/// Resolve and lower the project in configuration order, with `options`.
/// Use `CMAKE_BUILD_TYPE` if `configs` is empty.
///
/// With [`ResolveOptions::ide`] set, the IDE generators get [`ResolvedTarget::ide`].
pub fn resolve_with_options(
    model: &ProjectModel,
    host: &dyn Host,
    configs: &[ConfigId],
    options: &ResolveOptions,
) -> Result<GenerationResult, ResolveError> {
    crate::exports::source_sets::validate(model)?;
    lower::warn_about_wrappers(model, host);
    let configs = if configs.is_empty() {
        vec![ConfigId(
            lower::definition(model, DirId::ROOT, b"CMAKE_BUILD_TYPE")
                .cloned()
                .unwrap_or_default(),
        )]
    } else {
        configs.to_vec()
    };
    let mut paths = SourcePaths::new(host);
    paths.ide = options.ide;
    paths.unity = Some(unity::prepare(model, host, &mut paths, &configs)?);
    let configured_targets = configs
        .iter()
        .map(|config| resolve_targets(model, &mut paths, config))
        .collect::<Result<Vec<_>, _>>()?;
    validate_target_sources(model, &paths, &configured_targets)?;
    let plan = ConfigurationPlan {
        cross: CrossPlan::new(model, &configs)?,
        link_languages: if configs.len() > 1 {
            link_languages(model, &configured_targets)
        } else {
            IndexMap::new()
        },
    };
    let mut targets = Vec::new();
    let mut graphs = Vec::with_capacity(configs.len());
    let mut cross_graphs = IndexMap::new();
    let mut compile_inputs = BTreeMap::new();
    let mut xcode_link_error = None;
    let mut xcode_sources = IndexMap::new();
    for (config, configured) in configs.iter().zip(configured_targets) {
        if configs.len() != 1 {
            paths.index_generated(model, config)?;
            paths.index_objects(model, config);
        }
        let output = resolve_config(
            model,
            &paths,
            config,
            configured,
            &mut compile_inputs,
            &mut xcode_sources,
            &plan,
        )?;
        targets.extend(output.targets);
        cross_graphs.extend(output.cross_graphs);
        match output.graph {
            Ok(graph) => graphs.push(graph),
            Err(error) => {
                xcode_link_error.get_or_insert(error);
            }
        }
    }

    if options.ide == Some(ide::IdeFlavor::VisualStudio) {
        ide::finish_vs_sources(&mut targets);
    }
    if let Some(error) = xcode_link_error {
        return Err(error);
    }
    let mut install =
        lower::lower_install_manifest_with_configs(model, &targets, &configs, options.ide)?;
    let exports = resolve_exports(
        model,
        &mut UsageResolver::new(model).with_ide(options.ide),
        &targets,
        &install,
        &configs,
    )?;
    for action in &mut install.actions {
        if action.destination.is_empty()
            && matches!(
                action.kind,
                InstallActionKind::Files { .. } | InstallActionKind::Directory { .. }
            )
        {
            // Keep the empty destination in the generated file(INSTALL) command.
            action.kind = InstallActionKind::Code("file(INSTALL DESTINATION \"\")".into());
        }
    }
    let mut metadata = lower::collect_metadata(model);
    if let Some(toolchain) = metadata.toolchains.get_mut(b"CXX".as_slice())
        && toolchain.module_map_format.is_none()
        && let Some(format) = graphs
            .iter()
            .flat_map(|graph| &graph.steps)
            .find_map(|step| match step {
                BuildStep::Collate { modmap_format, .. } => Some(modmap_format),
                _ => None,
            })
    {
        toolchain.module_map_format = Some(format.as_str().into());
    }
    Ok(GenerationResult {
        configs,
        targets,
        graphs,
        cross_graphs,
        compile_inputs,
        install,
        exports,
        metadata,
    })
}

struct ConfigOutput {
    targets: Vec<ResolvedTarget>,
    graph: Result<BuildGraph, ResolveError>,
    cross_graphs: IndexMap<(ConfigId, ConfigId), BuildGraph>,
}

struct ConfigurationPlan<'a> {
    cross: CrossPlan<'a>,
    link_languages: IndexMap<TargetId, BString>,
}

#[derive(Default)]
struct CrossPlan<'a> {
    commands: &'a [ConfigId],
    outputs: IndexSet<ConfigId>,
    shared_outputs: IndexSet<CommandId>,
}

impl<'a> CrossPlan<'a> {
    fn new(model: &ProjectModel, configs: &'a [ConfigId]) -> Result<Self, ResolveError> {
        let Some(setting) = lower::definition(model, DirId::ROOT, b"CMAKE_CROSS_CONFIGS")
            .filter(|_| model.generator == Some(GeneratorKind::NinjaMultiConfig))
        else {
            return Ok(Self::default());
        };
        let values = list::split(setting, list::Empty::Drop);
        let outputs: IndexSet<_> = configs
            .iter()
            .filter(|config| setting == "all" || values.contains(&config.0))
            .cloned()
            .collect();
        if outputs.is_empty() || !model.commands().any(|_| true) {
            return Ok(Self::default());
        }
        let mut shared_outputs = IndexSet::new();
        let mut usage = UsageResolver::new(model);
        for (id, command) in model.commands() {
            let mut seen = IndexSet::new();
            for config in configs {
                let (outputs, byproducts) =
                    lower::command_outputs(model, command, config, &mut usage)?;
                let paths: IndexSet<_> = outputs.into_iter().chain(byproducts).collect();
                if paths.into_iter().any(|path| !seen.insert(path)) {
                    shared_outputs.insert(id);
                    break;
                }
            }
        }
        Ok(Self {
            commands: configs,
            outputs,
            shared_outputs,
        })
    }

    fn commands_for(&self, output: &ConfigId) -> impl Iterator<Item = CrossConfiguration<'_>> {
        self.commands
            .iter()
            .filter(move |command| *command != output && self.outputs.contains(output))
            .map(|command| CrossConfiguration {
                command,
                shared_outputs: &self.shared_outputs,
            })
    }
}

#[derive(Default)]
struct ConfigTargets {
    targets: IndexMap<TargetId, ResolvedTarget>,
    inputs: IndexMap<TargetId, TargetInputs>,
    linked_object_sources: IndexSet<TargetId>,
}

fn resolve_targets(
    model: &ProjectModel,
    paths: &mut SourcePaths<'_>,
    config: &ConfigId,
) -> Result<ConfigTargets, ResolveError> {
    paths.index_generated(model, config)?;
    validate_file_sets(model, paths, config)?;
    paths.index_objects(model, config);
    let mut usage = UsageResolver::with_source_paths(model, paths);
    let ctx = EvalContext {
        config: config.clone(),
        ..EvalContext::default()
    };
    let mut configured = ConfigTargets::default();
    for (id, target) in model.targets().filter(|(_, target)| !target.is_imported()) {
        let (facts, target_inputs) = resolve_target(model, paths, &mut usage, id, target, &ctx)?;
        configured.targets.insert(id, facts);
        configured.inputs.insert(id, target_inputs);
    }
    for (&id, inputs) in &configured.inputs {
        if !inputs.sources.is_empty()
            || !model
                .target(id)
                .is_some_and(|target| target.kind.compiles_objects())
        {
            continue;
        }
        let links = usage.collect_link_items(id, &ctx)?;
        if links.iter().any(|link| {
            let ResolvedLinkKind::Objects(object_library) = link.kind else {
                return false;
            };
            configured.targets.get(&object_library).map_or_else(
                || {
                    usage
                        .target_objects(object_library, config, None)
                        .is_some_and(|objects| !objects.is_empty())
                },
                |object| !object.compile_units.is_empty(),
            )
        }) {
            configured.linked_object_sources.insert(id);
        }
    }
    Ok(configured)
}

fn validate_target_sources(
    model: &ProjectModel,
    paths: &SourcePaths<'_>,
    configurations: &[ConfigTargets],
) -> Result<(), ResolveError> {
    let sourced: IndexSet<_> = configurations
        .iter()
        .flat_map(|config| {
            config
                .inputs
                .iter()
                .filter_map(|(&id, inputs)| (!inputs.sources.is_empty()).then_some(id))
                .chain(config.linked_object_sources.iter().copied())
        })
        .collect();
    let mut failure = None;
    for (id, target) in model.targets() {
        if target.is_imported() || sourced.contains(&id) || !target.kind.compiles_objects() {
            continue;
        }
        let body = BString::from(format!("No SOURCES given to target: {}", target.name));
        paths.emit(
            model,
            MessageKind::Error,
            Location::Backtrace(target.backtrace.clone()),
            body.clone(),
        );
        failure.get_or_insert(ResolveError::Reported(body));
    }
    failure.map_or(Ok(()), Err)
}

fn link_languages(
    model: &ProjectModel,
    configurations: &[ConfigTargets],
) -> IndexMap<TargetId, BString> {
    model
        .targets()
        .filter_map(|(id, target)| {
            let languages = configurations
                .iter()
                .filter_map(|config| config.targets.get(&id))
                .flat_map(|facts| {
                    facts
                        .compile_groups
                        .iter()
                        .map(|group| group.language.as_slice())
                });
            lower::linker_language(model, target, languages).map(|language| (id, language))
        })
        .collect()
}

fn resolve_config(
    model: &ProjectModel,
    paths: &SourcePaths<'_>,
    config: &ConfigId,
    configured: ConfigTargets,
    compile_inputs: &mut BTreeMap<BString, Vec<BString>>,
    xcode_sources: &mut IndexMap<TargetId, (ConfigId, Vec<BString>)>,
    plan: &ConfigurationPlan<'_>,
) -> Result<ConfigOutput, ResolveError> {
    let ConfigTargets {
        targets: mut resolved,
        mut inputs,
        ..
    } = configured;
    let ctx = EvalContext {
        config: config.clone(),
        ..EvalContext::default()
    };
    let std_model = if resolved
        .values()
        .any(|target| target.compile_units.iter().any(|unit| unit.scan))
    {
        std_module_model(model, paths.host)?
    } else {
        None
    };
    let model = std_model.as_ref().map_or(model, |(model, _)| model);
    let mut usage = UsageResolver::with_source_paths(model, paths);
    let std_id = std_model.as_ref().map(|(_, id)| *id);
    if let Some(id) = std_id
        && let Some(target) = model.target(id)
    {
        let (facts, target_inputs) = resolve_target(model, paths, &mut usage, id, target, &ctx)?;
        resolved.insert(id, facts);
        inputs.insert(id, target_inputs);
    }
    if paths.ide == Some(ide::IdeFlavor::Xcode) {
        ide::check_xcode_sources(model, paths, xcode_sources, &inputs, config)?;
    }
    let links = model
        .targets()
        .filter(|(_, target)| !target.is_imported())
        .map(|(id, target)| {
            let ctx = EvalContext {
                link_language: plan.link_languages.get(&id).cloned(),
                ..ctx.clone()
            };
            resolve_link(model, &mut usage, paths, &resolved, id, target, &ctx)
        })
        .collect::<Result<Vec<_>, _>>();
    let links = match links {
        Err(error) if paths.ide == Some(ide::IdeFlavor::Xcode) => {
            // A configuration with no sources can also have no link language.
            // Compare the source lists before returning the link error.
            return Ok(ConfigOutput {
                targets: resolved.into_values().collect(),
                graph: Err(error),
                cross_graphs: IndexMap::new(),
            });
        }
        result => result?,
    };
    attach_links(&mut resolved, &mut inputs, links, std_id);
    for (&id, facts) in &mut resolved {
        if facts.cxx_modules.is_some()
            && let (Some(target), Some(inputs)) = (model.target(id), inputs.get(&id))
        {
            let standard = inputs.standard_requirements.get(b"CXX".as_slice()).copied();
            let responses =
                lower::scan_responses(model, target, facts, standard, inputs.position_independent)?;
            if let Some(modules) = &mut facts.cxx_modules {
                modules.scan_responses = responses;
            }
        }
    }
    let mut resolved = resolved.into_values().collect::<Vec<_>>();
    let mut ide_custom = IndexMap::new();
    let graph = lower::lower_with_inputs(
        model,
        &resolved,
        config,
        &inputs,
        &mut usage,
        &mut ide_custom,
        compile_inputs,
        None,
    )?;
    let mut cross_graphs = IndexMap::new();
    for settings in plan.cross.commands_for(config) {
        let key = (settings.command.clone(), config.clone());
        let graph = lower::lower_with_inputs(
            model,
            &resolved,
            config,
            &inputs,
            &mut usage,
            &mut IndexMap::new(),
            &mut BTreeMap::new(),
            Some(settings),
        )?;
        cross_graphs.insert(key, graph);
    }
    for target in &mut resolved {
        if let Some(facts) = &mut target.ide {
            facts.custom = ide_custom.swap_remove(&target.id).unwrap_or_default();
        }
    }
    Ok(ConfigOutput {
        targets: resolved,
        graph: Ok(graph),
        cross_graphs,
    })
}

fn validate_file_sets(
    model: &ProjectModel,
    paths: &SourcePaths<'_>,
    config: &ConfigId,
) -> Result<(), ResolveError> {
    let mut usage = UsageResolver::new(model).with_ide(paths.ide);
    let ctx = EvalContext {
        config: config.clone(),
        ..EvalContext::default()
    };
    for (id, target) in model.targets().filter(|(_, target)| !target.is_imported()) {
        for set in target.file_sets.iter().filter_map(|id| model.file_set(*id)) {
            if set.kind != FileSetKind::Headers {
                continue;
            }
            if target
                .props
                .get(b"FRAMEWORK")
                .is_some_and(|value| truth::is_on(value))
            {
                let body = format!(
                    "The file set \"{}\", of type \"HEADERS\", is incompatible with the \"FRAMEWORK\" target \"{}\".",
                    set.name, target.name,
                );
                paths.emit(
                    model,
                    MessageKind::Error,
                    generation_location(model, target),
                    body.clone().into(),
                );
                return Err(ResolveError::Message(body.into()));
            }
            if lower::definition(model, target.dir, b"CMAKE_GENERATOR")
                .is_none_or(|value| !value.starts_with(b"Ninja"))
                || !set
                    .props
                    .get(b"INDEPENDENT_FILES")
                    .is_some_and(|value| truth::is_on(value))
                || lower::definition(model, target.dir, b"CMAKE_SUPPRESS_DEVELOPER_WARNINGS")
                    .is_some_and(|value| truth::is_on(value))
            {
                continue;
            }
            let dir = model.dir(target.dir);
            if usage
                .file_set_paths(id, b"SOURCES", &set.files, &ctx)?
                .iter()
                .any(|file| paths.is_generated(dir, &file.value))
            {
                paths.emit(
                    model, MessageKind::AuthorWarning, generation_location(model, target),
                    format!(
                        "the \"INDEPENDENT_FILES\" property of the file set \"{}\" of the target \"{}\" will be ignored because it is incompatible with the file set type \"HEADERS\".",
                        set.name, target.name,
                    ).into(),
                );
            }
        }
    }
    Ok(())
}
