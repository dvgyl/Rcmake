use std::collections::BTreeMap;

use indexmap::{IndexMap, IndexSet};
use rcmake_base::{BString, list, truth};
use rcmake_diag::{
    DiagAction, DiagCategory, Diagnostic, Effect, Location, MessageKind, PolicyId, PolicyStatus,
    RenderOptions, render,
};
use rcmake_genex::{ConfigId, EvalContext, UsageTraversal};
use rcmake_host::{Host, Stream};
use rcmake_model::{CustomCommandKind, DirId, GeneratorKind, ProjectModel, Target, TargetId};

use crate::{
    BuildGraph, BuildStep, GenerationResult, InstallActionKind, ResolveError, ResolveOptions,
    ResolvedTarget,
    cxx_modules::{generation_location, module_error, stdlib},
    exports::resolve_exports,
    ide, lower,
    resolve_link::{ResolvedLink, attach_links, resolve_link},
    sources::{
        BufferedHost, DiagBuffer, DiagEvent, SourcePaths, TargetInputs,
        assign_file_set_source_groups, generation_source_location, validate_object_sources,
    },
    target::resolve_target,
    unity,
    usage::{MemoSnapshot, ResolvedLinkItem, ResolvedLinkKind, UsageResolver, handle},
};

mod autogen;
mod cross;
mod cuda;
mod fortran;
use crate::usage::provenance::Provenance;
use cross::CrossPlan;

pub(crate) mod file_set_membership;
use file_set_membership::validate_file_sets;
mod header_verify;
mod intdir;
mod link_tags;
mod rule_output_sets;

/// Resolve and lower the project in configuration order.
/// Use `CMAKE_BUILD_TYPE` if `configs` is empty.
///
/// Return the first missing source as an error.
pub fn resolve(
    model: &ProjectModel,
    host: &dyn Host,
    configs: &[ConfigId],
) -> Result<GenerationResult, ResolveError> {
    resolve_project(
        model,
        host,
        configs,
        &ResolveOptions::default(),
        false,
        None,
    )
}

/// Resolve with a capability source. Return the first missing source as an error.
pub fn resolve_with_facts(
    model: &ProjectModel,
    host: &dyn Host,
    configs: &[ConfigId],
    facts: std::sync::Arc<dyn crate::GeneratorFacts>,
) -> Result<GenerationResult, ResolveError> {
    let options = ResolveOptions {
        generator_facts: Some(facts),
        ..Default::default()
    };
    resolve_project(model, host, configs, &options, false, None)
}

/// Resolve and lower the project in configuration order, with `options`.
/// Use `CMAKE_BUILD_TYPE` if `configs` is empty.
/// Report missing sources with their call stacks before returning an error.
///
/// With [`ResolveOptions::ide`] set, the IDE generators get [`ResolvedTarget::ide`].
pub fn resolve_with_options(
    model: &ProjectModel,
    host: &dyn Host,
    configs: &[ConfigId],
    options: &ResolveOptions,
) -> Result<GenerationResult, ResolveError> {
    resolve_project(model, host, configs, options, true, None)
}

#[cfg(test)]
pub(crate) fn resolve_with_workers(
    model: &ProjectModel,
    host: &dyn Host,
    configs: &[ConfigId],
    options: &ResolveOptions,
    collect_source_errors: bool,
    workers: usize,
) -> Result<GenerationResult, ResolveError> {
    resolve_project(
        model,
        host,
        configs,
        options,
        collect_source_errors,
        Some(workers),
    )
}

fn resolve_project(
    model: &ProjectModel,
    host: &dyn Host,
    configs: &[ConfigId],
    options: &ResolveOptions,
    collect_source_errors: bool,
    workers_override: Option<usize>,
) -> Result<GenerationResult, ResolveError> {
    // Direct callers can supply a model before finalization.
    let mut entries = crate::usage::includes::pending_current_dir_interfaces(model).peekable();
    let includes = if entries.peek().is_some() {
        let mut finalized = model.clone();
        crate::usage::includes::append_current_dir_interfaces(&mut finalized, entries);
        Some(finalized)
    } else {
        None
    };
    let model = includes.as_ref().unwrap_or(model);
    let output_sets = rule_output_sets::prepare(model)?;
    let model = output_sets.as_ref().unwrap_or(model);
    let rules = crate::target::instantiate_rules(model, host)?;
    let model = rules.as_ref().unwrap_or(model);
    crate::exports::source_sets::validate(model)?;
    let strategies = intdir::prepare(model, host)?;
    let model = strategies.as_ref().unwrap_or(model);
    intdir::warn_collisions(model, host);
    link_tags::validate(model, host)?;
    lower::custom::warn_depfile_policy(model, host);
    let configs = if configs.is_empty() {
        vec![ConfigId(
            lower::definition(model, DirId::ROOT, b"CMAKE_BUILD_TYPE")
                .cloned()
                .unwrap_or_default(),
        )]
    } else {
        configs.to_vec()
    };
    if model
        .commands()
        .any(|(_, command)| command.codegen && !command.implicit_depends.is_empty())
    {
        let body = "Cannot append IMPLICIT_DEPENDS to existing CODEGEN custom command.";
        host.write_out(
            Stream::Stderr,
            format!("CMake Error:\n  {body}\n\n\n").as_bytes(),
        );
        return Err(ResolveError::Reported(body.into()));
    }
    crate::InstallManifest::validate_bmi_destinations(model, &configs, host)?;
    let mut paths = SourcePaths::new(host);
    paths.set_ide(options.ide);
    paths.set_generator_facts(options.generator_facts.as_deref());
    paths.set_collect_source_errors(collect_source_errors);
    for (dir, _) in model.dirs().filter(|(id, _)| lower::is_darwin(model, *id)) {
        lower::apple::arch::sdk_map(model, dir)?;
    }
    paths.index_source_records(model);
    let autogen = autogen::prepare(model, &mut paths, &configs)?;
    let model = autogen.as_ref().map_or(model, |plan| &plan.model);
    paths.index_source_records(model);
    let unity = unity::prepare(model, host, &mut paths, &configs)?;
    paths.set_unity(Some(unity));
    check_notfound_target_properties(model, &paths)?;
    index_header_file_sets(model, &mut paths, &configs)?;
    let mut configured_targets = configs
        .iter()
        .map(|config| {
            resolve_targets(
                model,
                &mut paths,
                config,
                options.provenance,
                workers_override,
            )
        })
        .collect::<Result<Vec<_>, _>>()?;
    let prepared = header_verify::prepare(model, &paths, &configs, &configured_targets)?;
    let model = if let Some(prepared) = &prepared {
        for (config, configured) in configs.iter().zip(&mut configured_targets) {
            configured.memos = MemoSnapshot::default();
            resolve_verification_targets(model, &prepared.model, &mut paths, config, configured)?;
        }
        &prepared.model
    } else {
        model
    };
    if matches!(
        model.generator,
        Some(GeneratorKind::UnixMakefiles | GeneratorKind::Ninja | GeneratorKind::NinjaMultiConfig)
    ) {
        validate_languages(model, &paths, &configured_targets)?;
    }
    cuda::validate(
        model,
        &paths,
        configured_targets
            .iter()
            .flat_map(|config| config.targets.values()),
    )?;
    validate_target_sources(model, &paths, &configs, &configured_targets)?;
    if paths.missing_sources() {
        return Err(ResolveError::Reported("Cannot find source file.".into()));
    }
    let plan = ConfigurationPlan {
        cross: CrossPlan::new(model, &configs)?,
        traced_commands: if configs.len() > 1 {
            trace_cross_config_commands(model, &configs, &configured_targets)?
        } else {
            IndexMap::new()
        },
        link_languages: if configs.len() > 1 {
            link_languages(model, &configured_targets)
        } else {
            IndexMap::new()
        },
        verification: prepared.as_ref(),
        trace_config: configs.first().filter(|_| {
            model
                .global
                .props
                .get(b"GLOBAL_DEPENDS_DEBUG_MODE")
                .is_some_and(|value| truth::is_on(value))
        }),
    };
    let file_generate_cycles = configured_targets
        .iter()
        .flat_map(|config| config.file_generate_cycles.iter().cloned())
        .collect();
    let mut provenance = Provenance::default();
    let mut targets = Vec::new();
    let mut runtime_languages = IndexMap::new();
    let mut graphs = Vec::with_capacity(configs.len());
    let mut cross_graphs = IndexMap::new();
    let mut compile_inputs = BTreeMap::new();
    let mut xcode_link_error = None;
    let mut xcode_sources = IndexMap::new();
    let mut shared_memos = MemoSnapshot::default();
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
            workers_override,
        )?;
        if let Some(memos) = output.memos {
            shared_memos.merge_from(memos);
        }
        provenance.extend(output.provenance.unwrap_or_default());
        runtime_languages.extend(
            output
                .runtime_languages
                .into_iter()
                .map(|(id, languages)| ((id, config.clone()), languages)),
        );
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
    if let Some(autogen) = &autogen {
        autogen.write_info(host, &targets)?;
    }
    warn_link_flags(model, host, &targets);
    lower::cmp0128_warnings(model, host, &targets);
    let (mut install, install_targets) = lower::lower_install_manifest_with_configs(
        model,
        &targets,
        &configs,
        options.ide,
        host,
        &runtime_languages,
    )
    .map_err(|error| match error {
        ResolveError::Genex(error) => ResolveError::Genex(error.without_location()),
        other => other,
    })?;
    let exports = resolve_exports(
        model,
        &mut UsageResolver::new(model)
            .with_ide(options.ide)
            .with_memos(&shared_memos),
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
    if let Some(autogen) = &autogen {
        metadata
            .listfiles
            .extend(autogen.reconfigure_inputs.iter().cloned());
        metadata.generated_files.clone_from(&autogen.info_outputs);
        metadata.clean_files.clone_from(&autogen.clean_paths);
    }
    if model.generator == Some(GeneratorKind::UnixMakefiles)
        && let Some(graph) = graphs.first()
    {
        let aware: IndexSet<_> = model
            .commands()
            .filter_map(|(_, command)| match command.kind {
                CustomCommandKind::Utility(target)
                | CustomCommandKind::TargetEvent { target, .. }
                    if command.job_server_aware =>
                {
                    Some(target)
                }
                _ => None,
            })
            .collect();
        for target in &graph.targets {
            if model
                .find_target(DirId::ROOT, &target.name)
                .is_some_and(|id| aware.contains(&id))
                && let Some(&step) = target.steps.last()
            {
                metadata.jobserver_steps.insert(step);
            }
        }
    }
    if let Some(toolchain) = metadata.toolchains.get_mut(b"CXX".as_slice())
        && toolchain.module_map_format.is_none()
        && let Some(format) = graphs
            .iter()
            .flat_map(|graph| &graph.steps)
            .find_map(|step| match step {
                BuildStep::Collate {
                    modmap_format,
                    depend_info,
                    ..
                } if matches!(depend_info.as_ref(), crate::DependInfo::Cxx(_)) => {
                    modmap_format.as_ref()
                }
                _ => None,
            })
    {
        toolchain.module_map_format = Some(format.as_str().into());
    }
    let traced_sources = crate::sources::traced_sources(model, &configs, &graphs)?;
    let target_depends = crate::target_depends::compute(
        model,
        &configs,
        (&targets, &graphs),
        &paths,
        &mut UsageResolver::with_source_paths(model, &paths)
            .with_ide(options.ide)
            .with_memos(&shared_memos),
    )?;
    for target in &mut targets {
        target.dependencies = target_depends
            .direct_depends(target.id)
            .iter()
            .map(|depend| depend.target)
            .collect();
    }
    crate::file_sets::clean_lint_fixes(model, host, &graphs);
    let resolved_model = prepared
        .map(|prepared| Box::new(prepared.model))
        .or_else(|| autogen.map(|autogen| Box::new(autogen.model)))
        .or_else(|| strategies.map(Box::new))
        .or_else(|| rules.map(Box::new))
        .or_else(|| output_sets.map(Box::new))
        .or_else(|| includes.map(Box::new));
    Ok(GenerationResult {
        configs,
        targets,
        graphs,
        cross_graphs,
        compile_inputs,
        install,
        install_targets,
        exports,
        file_generate_cycles,
        traced_sources,
        target_depends,
        metadata,
        provenance_entries: provenance.entries,
        chains: options.provenance.then(|| Box::new(provenance.chains)),
        resolved_model,
    })
}

struct ConfigOutput {
    provenance: Option<Provenance>,
    targets: Vec<ResolvedTarget>,
    runtime_languages: IndexMap<TargetId, IndexSet<BString>>,
    graph: Result<BuildGraph, ResolveError>,
    cross_graphs: IndexMap<(ConfigId, ConfigId), BuildGraph>,
    memos: Option<MemoSnapshot>,
}

struct ConfigurationPlan<'a> {
    cross: CrossPlan<'a>,
    traced_commands: IndexMap<TargetId, IndexSet<rcmake_model::CommandId>>,
    trace_config: Option<&'a ConfigId>,
    link_languages: IndexMap<TargetId, BString>,
    verification: Option<&'a header_verify::Verification>,
}

fn trace_cross_config_commands(
    model: &ProjectModel,
    configs: &[ConfigId],
    configured_targets: &[ConfigTargets],
) -> Result<IndexMap<TargetId, IndexSet<rcmake_model::CommandId>>, ResolveError> {
    if !model
        .commands()
        .any(|(_, command)| command.kind == CustomCommandKind::Output)
    {
        return Ok(IndexMap::new());
    }
    let mut usage = UsageResolver::new(model);
    let mut traced: IndexMap<TargetId, IndexSet<rcmake_model::CommandId>> = IndexMap::new();
    for (config, configured) in configs.iter().zip(configured_targets) {
        let mut producers: IndexMap<BString, rcmake_model::CommandId> = IndexMap::new();
        for (id, command) in model
            .commands()
            .filter(|(_, command)| command.kind == CustomCommandKind::Output)
        {
            let (outputs, byproducts) = lower::command_outputs(model, command, config, &mut usage)?;
            for path in outputs.into_iter().chain(byproducts) {
                producers
                    .entry(lower::build_relative(model, &path))
                    .or_insert(id);
            }
        }
        if producers.is_empty() {
            continue;
        }
        for (&target_id, inputs) in &configured.inputs {
            let Some(target) = model.target(target_id) else {
                continue;
            };
            let dir = model.dir(target.dir);
            let mut matched = IndexSet::new();
            for source in &inputs.sources {
                for base in [&dir.source_dir, &dir.binary_dir] {
                    let path =
                        lower::build_relative(model, &crate::sources::absolute_path(base, source));
                    if let Some(&cmd_id) = producers.get(&path) {
                        matched.insert(cmd_id);
                    }
                }
            }
            for (_, command) in model
                .commands()
                .filter(|(_, command)| command.kind == CustomCommandKind::Utility(target_id))
            {
                let ctx = EvalContext {
                    config: config.clone(),
                    head: Some(handle(target_id)),
                    current: Some(handle(target_id)),
                    ..EvalContext::default()
                };
                let mut host = usage.host(command.dir);
                host.current = Some(handle(target_id));
                let cmd_dir = model.dir(command.dir);
                for dep in &command.depends {
                    let expanded = if dep.value.windows(2).any(|w| w == b"$<") {
                        let Ok(ast) = rcmake_genex::parse(&dep.value) else {
                            continue;
                        };
                        let Ok((val, _)) = rcmake_genex::evaluate(&ast, &ctx, &mut host) else {
                            continue;
                        };
                        val
                    } else {
                        dep.value.clone()
                    };
                    for item in list::split(&expanded, list::Empty::Drop) {
                        for base in [&cmd_dir.source_dir, &cmd_dir.binary_dir] {
                            let path = lower::build_relative(
                                model,
                                &crate::sources::absolute_path(base, &item),
                            );
                            if let Some(&cmd_id) = producers.get(&path) {
                                matched.insert(cmd_id);
                            }
                        }
                    }
                }
            }
            if !matched.is_empty() {
                traced.entry(target_id).or_default().extend(matched);
            }
        }
    }
    Ok(traced)
}

#[derive(Default)]
struct ConfigTargets {
    provenance: Option<Provenance>,
    targets: IndexMap<TargetId, ResolvedTarget>,
    inputs: IndexMap<TargetId, TargetInputs>,
    linked_object_sources: IndexSet<TargetId>,
    file_generate_cycles: IndexSet<(DirId, BString)>,
    memos: MemoSnapshot,
}

/// Print a target evaluation error that permits the next target to resolve.
///
/// Return the error that ends generation after the last target. Return any
/// other error as the failure.
fn report_target_error(
    paths: &SourcePaths<'_>,
    error: ResolveError,
) -> Result<ResolveError, ResolveError> {
    let text = match &error {
        ResolveError::Reported(text) => return Ok(ResolveError::Reported(text.clone())),
        ResolveError::Genex(_) => error.to_string(),
        ResolveError::Message(_) => return Err(error),
    };
    if !text.starts_with("CMake Error") {
        return Err(error);
    }
    let mut bytes = text.into_bytes();
    if !bytes.ends_with(b"\n") {
        bytes.push(b'\n');
    }
    paths.host.write_out(rcmake_host::Stream::Stderr, &bytes);
    Ok(ResolveError::Reported(bytes.into()))
}

/// Collect effective build-side native `HEADERS` file-set members across all
/// generated configurations. CMake marks the shared source record in the
/// target's directory as `HEADER_FILE_ONLY`, which also suppresses compilation
/// of that path in other targets and configurations in the same directory.
fn index_header_file_sets(
    model: &ProjectModel,
    paths: &mut SourcePaths<'_>,
    configs: &[ConfigId],
) -> Result<(), ResolveError> {
    let has_native_headers = model
        .targets()
        .filter(|(_, target)| !target.is_imported())
        .flat_map(|(_, target)| target.file_sets.iter())
        .filter_map(|&id| model.file_set(id))
        .any(|set| {
            set.kind == rcmake_model::FileSetKind::Headers
                && set.scope != rcmake_model::UsageScope::Interface
                && set.props.get(b"_RCMAKE_RULE").is_none()
        });
    if !has_native_headers {
        return Ok(());
    }
    let mut discovered = Vec::new();
    for config in configs {
        paths.index_generated(model, config)?;
        let mut usage = UsageResolver::with_source_paths(model, paths);
        for (id, target) in model.targets().filter(|(_, target)| !target.is_imported()) {
            let dir = model.dir(target.dir);
            let ctx = EvalContext {
                config: config.clone(),
                head: Some(handle(id)),
                ..EvalContext::default()
            };
            for &set_id in &target.file_sets {
                let Some(set) = model.file_set(set_id) else {
                    continue;
                };
                if set.kind != rcmake_model::FileSetKind::Headers
                    || set.scope == rcmake_model::UsageScope::Interface
                    || set.props.get(b"_RCMAKE_RULE").is_some()
                {
                    continue;
                }
                let Ok(files) = usage.file_set_target_sources(set, &ctx) else {
                    continue;
                };
                for file in files {
                    if let Some(path) = crate::target::find_file_set_member(paths, dir, &file) {
                        discovered.push((target.dir, path));
                    }
                }
            }
        }
    }
    for (dir, path) in discovered {
        paths.mark_header_only(dir, path);
    }
    Ok(())
}

enum TargetResolveOutcome {
    Ok(Box<(ResolvedTarget, TargetInputs)>),
    ResolveErr(ResolveError),
    ObjectSourcesErr(ResolveError),
}

struct TargetResolveWork {
    id: TargetId,
    dir: DirId,
    diag_events: Vec<DiagEvent>,
    provenance: Option<Provenance>,
    outcome: TargetResolveOutcome,
}

/// Return `true` when target resolution may invoke the Ninja C++20 dyndep
/// capability query or a caller-supplied [`GeneratorFacts`](crate::GeneratorFacts)
/// callback. Running serially in those cases preserves exact capability-query
/// attribution and avoids speculative probes past an earlier failing target.
fn may_probe_ninja_cxx_dyndep(model: &ProjectModel, paths: &SourcePaths<'_>) -> bool {
    if !matches!(
        model.generator,
        Some(GeneratorKind::Ninja | GeneratorKind::NinjaMultiConfig)
    ) || paths.ninja_cxx_dyndep.get().is_some()
    {
        return false;
    }
    if paths.generator_facts.is_some() {
        return true;
    }
    let supports_modules = |value: &[u8]| matches!(value, b"20" | b"23" | b"26");
    let has_cxx20_feature = |entries: &[rcmake_model::BT<BString>]| {
        entries.iter().any(|entry| {
            entry
                .value
                .windows(10)
                .any(|part| matches!(part, b"cxx_std_20" | b"cxx_std_23" | b"cxx_std_26"))
                || entry.value.windows(2).any(|part| part == b"$<")
        })
    };
    let mut has_cxx20 = false;
    let mut has_scan_trigger = false;
    for (_, target) in model.targets() {
        if target.file_sets.iter().any(|&id| {
            model
                .file_set(id)
                .is_some_and(|set| set.kind == rcmake_model::FileSetKind::CxxModules)
        }) {
            return true;
        }
        has_cxx20 |= has_cxx20_feature(
            target
                .usage
                .entries(rcmake_model::UsageField::CompileFeatures),
        ) || has_cxx20_feature(
            target
                .usage
                .entries(rcmake_model::UsageField::InterfaceCompileFeatures),
        );
        if target.is_imported() {
            continue;
        }
        has_cxx20 |= target
            .props
            .get(b"CXX_STANDARD")
            .or_else(|| lower::definition(model, target.dir, b"CMAKE_CXX_STANDARD"))
            .is_some_and(|value| supports_modules(value));
        has_scan_trigger |= lower::definition(model, target.dir, b"CMAKE_CXX_SCANDEP_SOURCE")
            .is_some()
            || target
                .props
                .get(b"CXX_SCAN_FOR_MODULES")
                .or_else(|| lower::definition(model, target.dir, b"CMAKE_CXX_SCAN_FOR_MODULES"))
                .is_some_and(|value| truth::is_on(value))
            || model.dir(target.dir).source_props.values().any(|props| {
                props
                    .get(b"CXX_SCAN_FOR_MODULES")
                    .is_some_and(|value| truth::is_on(value))
            });
    }
    has_cxx20 && has_scan_trigger
}

fn resolve_targets(
    model: &ProjectModel,
    paths: &mut SourcePaths<'_>,
    config: &ConfigId,
    provenance: bool,
    workers_override: Option<usize>,
) -> Result<ConfigTargets, ResolveError> {
    paths.index_generated(model, config)?;
    validate_file_sets(model, paths, config)?;
    paths.index_objects(model, config);
    let non_imported: Vec<(TargetId, &Target)> = model
        .targets()
        .filter(|(_, target)| !target.is_imported())
        .collect();
    let workers = if may_probe_ninja_cxx_dyndep(model, paths) {
        1
    } else {
        workers_override.unwrap_or_else(|| {
            crate::parallel::worker_count(
                non_imported.len(),
                crate::parallel::TARGET_PARALLEL_THRESHOLD,
                crate::parallel::MAX_WORKERS,
            )
        })
    };
    let ctx = EvalContext {
        config: config.clone(),
        ..EvalContext::default()
    };
    let mut configured = ConfigTargets::default();
    let mut usage = UsageResolver::with_source_paths(model, paths);
    usage.provenance = provenance.then(Provenance::default);
    // CMake reports the evaluation errors of each target and then fails.
    let mut failed = None;
    if workers <= 1 || non_imported.len() <= 1 {
        for (id, target) in non_imported {
            let resolved = usage
                .validate_compatible_declarations(id, &ctx, paths)
                .and_then(|()| resolve_target(model, paths, &mut usage, id, target, &ctx));
            let (mut facts, target_inputs) = match resolved {
                Ok(resolved) => resolved,
                Err(error) => {
                    failed = Some(report_target_error(paths, error)?);
                    continue;
                }
            };
            validate_object_sources(
                model,
                paths,
                &mut usage,
                target,
                &facts,
                &target_inputs,
                &ctx,
            )?;
            assign_file_set_source_groups(model, target, &mut facts);
            for source in &target_inputs.sources {
                if paths
                    .output_source_reads
                    .get(source)
                    .is_some_and(|reads| reads.contains(&id))
                {
                    configured
                        .file_generate_cycles
                        .insert((target.dir, source.clone()));
                }
            }
            configured.targets.insert(id, facts);
            configured.inputs.insert(id, target_inputs);
        }
    } else {
        let template = usage.worker_template();
        let shared_index = paths.shared_index();
        let host = paths.host;
        let chunks = crate::parallel::map_chunks(&non_imported, workers, |chunk| {
            let events = DiagBuffer::new();
            let buffered = BufferedHost::new(host, &events);
            let worker_paths = SourcePaths::for_worker(&buffered, shared_index, &events);
            let mut worker_usage = template.build(Some(&worker_paths), provenance);
            let mut outputs = Vec::with_capacity(chunk.len());
            for &(id, target) in chunk {
                worker_paths.reset_local_dedup();
                let outcome = match worker_usage
                    .validate_compatible_declarations(id, &ctx, &worker_paths)
                    .and_then(|()| {
                        resolve_target(model, &worker_paths, &mut worker_usage, id, target, &ctx)
                    }) {
                    Ok((mut facts, target_inputs)) => {
                        match validate_object_sources(
                            model,
                            &worker_paths,
                            &mut worker_usage,
                            target,
                            &facts,
                            &target_inputs,
                            &ctx,
                        ) {
                            Ok(()) => {
                                assign_file_set_source_groups(model, target, &mut facts);
                                TargetResolveOutcome::Ok(Box::new((facts, target_inputs)))
                            }
                            Err(error) => TargetResolveOutcome::ObjectSourcesErr(error),
                        }
                    }
                    Err(error) => TargetResolveOutcome::ResolveErr(error),
                };
                let diag_events = events.take_events();
                let target_provenance = worker_usage.provenance.as_mut().map(std::mem::take);
                outputs.push(TargetResolveWork {
                    id,
                    dir: target.dir,
                    diag_events,
                    provenance: target_provenance,
                    outcome,
                });
            }
            (outputs, worker_usage.take_local_memos())
        });
        for (outputs, worker_memos) in chunks {
            usage.absorb_memos(worker_memos);
            for work in outputs {
                paths.replay_events(work.diag_events);
                if let (Some(dest), Some(src)) = (&mut usage.provenance, work.provenance) {
                    dest.extend(src);
                }
                let (facts, target_inputs) = match work.outcome {
                    TargetResolveOutcome::Ok(boxed) => *boxed,
                    TargetResolveOutcome::ResolveErr(error) => {
                        failed = Some(report_target_error(paths, error)?);
                        continue;
                    }
                    TargetResolveOutcome::ObjectSourcesErr(error) => return Err(error),
                };
                for source in &target_inputs.sources {
                    if paths
                        .output_source_reads
                        .get(source)
                        .is_some_and(|reads| reads.contains(&work.id))
                    {
                        configured
                            .file_generate_cycles
                            .insert((work.dir, source.clone()));
                    }
                }
                configured.targets.insert(work.id, facts);
                configured.inputs.insert(work.id, target_inputs);
            }
        }
    }
    if let Some(error) = failed {
        return Err(error);
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
    configured.provenance = usage.provenance.take();
    configured.memos = usage.memo_snapshot();
    Ok(configured)
}

fn resolve_verification_targets(
    original: &ProjectModel,
    prepared: &ProjectModel,
    paths: &mut SourcePaths<'_>,
    config: &ConfigId,
    configured: &mut ConfigTargets,
) -> Result<(), ResolveError> {
    paths.index_generated(prepared, config)?;
    paths.index_objects(prepared, config);
    let mut usage = UsageResolver::with_source_paths(prepared, paths);
    usage.provenance = configured.provenance.take();
    let ctx = EvalContext {
        config: config.clone(),
        ..EvalContext::default()
    };
    for (id, target) in prepared
        .targets()
        .filter(|(id, _)| original.target(*id).is_none())
    {
        let (facts, inputs) = resolve_target(prepared, paths, &mut usage, id, target, &ctx)?;
        configured.targets.insert(id, facts);
        configured.inputs.insert(id, inputs);
    }
    configured.provenance = usage.provenance;
    Ok(())
}

fn warn_link_flags(model: &ProjectModel, host: &dyn Host, targets: &[ResolvedTarget]) {
    let policy = PolicyId::new(210);
    let mut warned = IndexSet::new();
    for facts in targets {
        let Some(target) = model.target(facts.id).filter(|target| {
            matches!(
                target.kind,
                rcmake_model::TargetKind::SharedLibrary | rcmake_model::TargetKind::ModuleLibrary
            ) && target.policies.policies.get(policy) == PolicyStatus::Warn
        }) else {
            continue;
        };
        let Some(link) = &facts.link else { continue };
        let language = link.language.as_slice();
        let key = [b"CMAKE_", language, b"_LINK_FLAGS"].concat();
        let create = [b"CMAKE_EXECUTABLE_CREATE_", language, b"_FLAGS"].concat();
        if let Some(flags) = model
            .definition(target.dir, &key)
            .filter(|flags| !flags.is_empty())
            && model.definition(target.dir, &create) != Some(flags)
            && warned.insert(language)
        {
            policy_warning(
                model,
                host,
                policy,
                target,
                &format!(
                    "For compatibility with older versions of CMake, {} will be ignored for all non-EXECUTABLE targets which use these flags.",
                    BString::from(key)
                ),
            );
        }
    }
}

/// Check for a target with the reserved name `codegen`.
pub fn validate_codegen_name(model: &ProjectModel, host: &dyn Host) -> Result<(), ResolveError> {
    let policy = PolicyId::new(171);
    let status = model
        .dir(DirId::ROOT)
        .final_policies
        .map_or(PolicyStatus::Warn, |policies| policies.get(policy));
    let Some(target) = model
        .targets()
        .map(|(_, target)| target)
        .find(|target| target.name == "codegen" && !target.is_imported())
    else {
        return Ok(());
    };
    let message = "The target name \"codegen\" is reserved.";
    match status {
        PolicyStatus::Old => Ok(()),
        PolicyStatus::New => {
            SourcePaths::new(host).emit(
                model,
                MessageKind::Error,
                generation_source_location(Location::Backtrace(target.backtrace.clone())),
                message.into(),
            );
            Err(ResolveError::Reported(message.into()))
        }
        PolicyStatus::Warn => {
            if target
                .props
                .get(b"_RCMAKE_CODEGEN_NAME_DIAGNOSTIC")
                .and_then(|value| DiagAction::parse(value))
                != Some(DiagAction::Ignore)
            {
                policy_warning(model, host, policy, target, message);
            }
            Ok(())
        }
    }
}

pub(crate) fn render_policy_warning(
    model: &ProjectModel,
    policy: PolicyId,
    target: &rcmake_model::Target,
    message: &str,
) -> Option<BString> {
    if model
        .definition(target.dir, b"CMAKE_SUPPRESS_DEVELOPER_WARNINGS")
        .is_some_and(|value| truth::is_on(value))
    {
        return None;
    }
    let summary = match policy {
        id if id == PolicyId::new(171) => "'codegen' is a reserved target name.".into(),
        id if id == PolicyId::new(128) => {
            "Selection of language standard and extension flags improved.".into()
        }
        id if id == PolicyId::new(211) => {
            "A file may belong to at most one non-HEADERS file set in a target.".into()
        }
        id if id == PolicyId::new(210) => {
            "CMAKE_<LANG>_LINK_FLAGS adds link flags to all target types.".into()
        }
        _ => policy
            .info()
            .map_or("", |info| info.summary)
            .replace("``", ""),
    };
    let warning = Diagnostic { annotations: None,
        kind: MessageKind::AuthorWarning,
        effect: Effect::None,
        category: Some(DiagCategory::Policy),
        policy: Some(policy),
        location: if policy == PolicyId::new(171) {
            generation_source_location(Location::Backtrace(target.backtrace.clone()))
        } else {
            generation_location(model, target)
        },
        body: format!("Policy {policy} is not set: {summary}  Run \"cmake --help-policy {policy}\" for policy details.  Use the cmake_policy command to set the policy and suppress this warning.\n{message}").into(),
    };
    Some(render(
        &warning,
        &RenderOptions {
            relative_base: Some(model.dir(DirId::ROOT).source_dir.clone()),
            ..RenderOptions::default()
        },
    ))
}

pub(crate) fn policy_warning(
    model: &ProjectModel,
    host: &dyn Host,
    policy: PolicyId,
    target: &rcmake_model::Target,
    message: &str,
) {
    if let Some(rendered) = render_policy_warning(model, policy, target, message) {
        host.write_out(Stream::Stderr, &rendered);
    }
}

fn enabled_languages(model: &ProjectModel) -> Vec<BString> {
    model
        .global
        .props
        .get(b"ENABLED_LANGUAGES")
        .map_or_else(Vec::new, |value| list::split(value, list::Empty::Drop))
}

fn validate_languages(
    model: &ProjectModel,
    paths: &SourcePaths<'_>,
    configured: &[ConfigTargets],
) -> Result<(), ResolveError> {
    if model.global.props.get(b"ENABLED_LANGUAGES").is_none() {
        return Ok(());
    }
    let enabled = enabled_languages(model);
    let mut reported = IndexSet::new();
    let mut failure = None;
    for facts in configured.iter().flat_map(|config| config.targets.values()) {
        let Some(target) = model.target(facts.id) else {
            continue;
        };
        for language in facts.compile_groups.iter().map(|group| &group.language) {
            if !language.is_empty()
                && !enabled.contains(language)
                && reported.insert((target.dir, language))
            {
                let error = module_error(model, paths, target, format!(
                    "The language {language} was requested for compilation but was not enabled.  To enable a language it needs to be specified in a 'project' or 'enable_language' command in the root CMakeLists.txt"
                ).into());
                failure.get_or_insert(error);
            }
        }
    }
    failure.map_or(Ok(()), Err)
}

fn check_notfound_target_properties(
    model: &ProjectModel,
    paths: &SourcePaths<'_>,
) -> Result<(), ResolveError> {
    let mut not_found: BTreeMap<BString, BString> = BTreeMap::new();
    let record =
        |not_found: &mut BTreeMap<BString, BString>, var: &[u8], suffix_parts: &[&[u8]]| {
            let mut key = BString::from(var);
            if model.cache.get(var).is_some_and(|entry| entry.advanced) {
                key.extend_from_slice(b" (ADVANCED)");
            }
            let entry = not_found.entry(key).or_default();
            for part in suffix_parts {
                entry.extend_from_slice(part);
            }
        };
    for (_, dir) in model.dirs() {
        for &tid in &dir.targets {
            let Some(target) = model.target(tid).filter(|target| {
                !target.is_imported() && target.kind != rcmake_model::TargetKind::InterfaceLibrary
            }) else {
                continue;
            };
            if let Some(links) = &target.usage.link_libraries {
                for item in links {
                    if let Some(var) = item.text.value.strip_suffix(b"-NOTFOUND")
                        && !var.is_empty()
                    {
                        record(
                            &mut not_found,
                            var,
                            &[
                                b"\n    linked by target \"",
                                &target.name,
                                b"\" in directory ",
                                &dir.source_dir,
                            ],
                        );
                    }
                }
            }
            if let Some(inc_prop) = model.get_prop(
                &rcmake_model::PropScope::Target(tid),
                b"INCLUDE_DIRECTORIES",
            ) {
                let stripped = rcmake_genex::genex_strip(&inc_prop);
                for inc in list::split(&stripped, list::Empty::Drop) {
                    if let Some(var) = inc.strip_suffix(b"-NOTFOUND")
                        && !var.is_empty()
                    {
                        record(
                            &mut not_found,
                            var,
                            &[
                                b"\n   used as include directory in directory ",
                                &dir.source_dir,
                            ],
                        );
                    }
                }
            }
        }
    }
    if not_found.is_empty() {
        return Ok(());
    }
    let mut message = BString::from(
        "CMake Error: The following variables are used in this project, but they are set to NOTFOUND.\n\
         Please set them or make sure they are set and tested correctly in the CMake files:\n",
    );
    for (var, uses) in not_found {
        message.extend_from_slice(&var);
        message.extend_from_slice(&uses);
        message.push(b'\n');
    }
    message.push(b'\n');
    paths.host.write_out(Stream::Stderr, &message);
    Err(ResolveError::Reported(message))
}

fn validate_target_sources(
    model: &ProjectModel,
    paths: &SourcePaths<'_>,
    configs: &[ConfigId],
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
    let missing_targets: Vec<_> = model
        .targets()
        .filter(|(id, target)| {
            !target.is_imported() && !sourced.contains(id) && target.kind.compiles_objects()
        })
        .collect();
    if missing_targets.is_empty() {
        return Ok(());
    }
    if let (Some(config), Some(configured)) = (configs.first(), configurations.first()) {
        lower::custom::validate_custom_commands(model, paths, &configured.targets, config)?;
    }
    let mut failure = None;
    for (_, target) in missing_targets {
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

fn validate_link_items(
    model: &ProjectModel,
    paths: &SourcePaths<'_>,
    usage: &mut UsageResolver<'_>,
    resolved: &IndexMap<TargetId, ResolvedTarget>,
    ctx: &EvalContext,
    plan: &ConfigurationPlan<'_>,
) -> Result<(), ResolveError> {
    let only_targets = |target: &Target| {
        target
            .props
            .get(b"LINK_LIBRARIES_ONLY_TARGETS")
            .is_some_and(|value| truth::is_on(value))
    };
    if !model.targets().any(|(_, target)| {
        only_targets(target)
            || [
                &target.usage.link_libraries,
                &target.usage.interface_link_libraries,
            ]
            .into_iter()
            .flatten()
            .flatten()
            .any(|item| {
                item.text
                    .value
                    .windows(2)
                    .any(|pair| matches!(pair, b"::" | b"$<"))
            })
    }) {
        return Ok(());
    }
    let mut reported = IndexSet::new();
    let mut checked_interfaces = IndexSet::new();
    // CMake checks each target in model order: its links, then its interface.
    let mut pending = Vec::new();
    let mut check = |id: TargetId, interface, item: &ResolvedLinkItem| {
        let Some(target) = model.target(id) else {
            return;
        };
        let name = &item.text.value;
        if !matches!(item.kind, ResolvedLinkKind::Raw)
            || name
                .first()
                .is_none_or(|&byte| matches!(byte, b'-' | b'$' | b'`'))
            || name.iter().any(|&byte| matches!(byte, b'/' | b'\\'))
            || (!only_targets(target) && !name.windows(2).any(|pair| pair == b"::"))
            || !reported.insert((id, name.clone()))
        {
            return;
        }
        let message = if only_targets(target) {
            let verb = if interface {
                "its link interface contains"
            } else {
                "it links to"
            };
            format!(
                "Target \"{}\" has LINK_LIBRARIES_ONLY_TARGETS enabled, but {verb}:\n  {name}\nwhich is not a target.",
                target.name
            )
        } else {
            let head = if interface {
                format!("The link interface of target \"{}\" contains:", target.name)
            } else {
                format!("Target \"{}\" links to:", target.name)
            };
            format!("{head}\n  {name}\nbut the target was not found.")
        };
        let body = BString::from(format!(
            "{message}  Possible reasons include:\n  * There is a typo in the target name.\n  * A find_package call is missing for an IMPORTED target.\n  * An ALIAS target is missing.\n"
        ));
        let location = Location::Backtrace(item.text.backtrace.clone());
        pending.push(((id.index(), interface), location, body));
    };
    fn with_members(item: &ResolvedLinkItem) -> impl Iterator<Item = &ResolvedLinkItem> {
        let members = match &item.kind {
            ResolvedLinkKind::Group { members, .. } => members.as_slice(),
            _ => &[],
        };
        std::iter::once(item).chain(members)
    }
    for (id, target) in model.targets().filter(|(_, target)| {
        !target.is_imported()
            && target.kind.compiles_objects()
            && target
                .usage
                .link_libraries
                .as_ref()
                .is_some_and(|links| !links.is_empty())
    }) {
        let mut ctx = EvalContext {
            head: Some(handle(id)),
            traversal: UsageTraversal::Link,
            link_language: plan.link_languages.get(&id).cloned(),
            ..ctx.clone()
        };
        ctx.link_language = usage.select_link_language(id, target, resolved, &ctx)?;
        for item in usage
            .direct_links(id, &ctx)?
            .0
            .iter()
            .flat_map(|item| with_members(item))
        {
            check(id, false, item);
        }
        for dependency in usage.link_targets(id, &ctx)? {
            checked_interfaces.insert(dependency);
            for item in usage
                .graphviz_interface_links(dependency, &ctx)?
                .0
                .iter()
                .flat_map(|item| with_members(item))
            {
                check(dependency, true, item);
            }
        }
    }
    for (id, _) in model
        .targets()
        .filter(|(id, target)| !checked_interfaces.contains(id) && only_targets(target))
    {
        let ctx = EvalContext {
            head: Some(handle(id)),
            traversal: UsageTraversal::Link,
            ..ctx.clone()
        };
        for item in usage
            .graphviz_interface_links(id, &ctx)?
            .0
            .iter()
            .flat_map(|item| with_members(item))
        {
            check(id, true, item);
        }
    }
    pending.sort_by_key(|(order, ..)| *order);
    let mut failure = None;
    for (_, location, body) in pending {
        paths.emit(model, MessageKind::Error, location, body.clone());
        failure.get_or_insert(ResolveError::Reported(body));
    }
    failure.map_or(Ok(()), Err)
}

struct TargetLinkWork {
    id: TargetId,
    diag_events: Vec<DiagEvent>,
    provenance: Option<Provenance>,
    result: Result<ResolvedLink, ResolveError>,
}

#[expect(
    clippy::too_many_arguments,
    reason = "Config resolution threads shared model, paths, plan, and worker controls."
)]
fn resolve_links(
    model: &ProjectModel,
    usage: &mut UsageResolver<'_>,
    paths: &SourcePaths<'_>,
    resolved: &IndexMap<TargetId, ResolvedTarget>,
    ctx: &EvalContext,
    precompiled: &lower::PchPlans,
    plan: &ConfigurationPlan<'_>,
    workers_override: Option<usize>,
) -> Result<Vec<(TargetId, ResolvedLink)>, ResolveError> {
    let non_imported: Vec<(TargetId, &Target)> = model
        .targets()
        .filter(|(_, target)| !target.is_imported())
        .collect();
    let workers = workers_override.unwrap_or_else(|| {
        crate::parallel::worker_count(
            non_imported.len(),
            crate::parallel::TARGET_PARALLEL_THRESHOLD,
            crate::parallel::MAX_WORKERS,
        )
    });
    if workers <= 1 || non_imported.len() <= 1 {
        return non_imported
            .into_iter()
            .map(|(id, target)| {
                let ctx = EvalContext {
                    link_language: plan.link_languages.get(&id).cloned(),
                    ..ctx.clone()
                };
                resolve_link(model, usage, paths, resolved, id, target, &ctx, precompiled)
                    .map(|link| (id, link))
            })
            .collect();
    }
    let record_provenance = usage.provenance.is_some();
    let template = usage.worker_template();
    let shared_index = paths.shared_index();
    let host = paths.host;
    let chunks = crate::parallel::map_chunks(&non_imported, workers, |chunk| {
        let events = DiagBuffer::new();
        let buffered = BufferedHost::new(host, &events);
        let worker_paths = SourcePaths::for_worker(&buffered, shared_index, &events);
        let mut worker_usage = template.build(Some(&worker_paths), record_provenance);
        let mut outputs = Vec::with_capacity(chunk.len());
        for &(id, target) in chunk {
            worker_paths.reset_local_dedup();
            let target_ctx = EvalContext {
                link_language: plan.link_languages.get(&id).cloned(),
                ..ctx.clone()
            };
            let result = resolve_link(
                model,
                &mut worker_usage,
                &worker_paths,
                resolved,
                id,
                target,
                &target_ctx,
                precompiled,
            );
            let diag_events = events.take_events();
            let target_provenance = worker_usage.provenance.as_mut().map(std::mem::take);
            outputs.push(TargetLinkWork {
                id,
                diag_events,
                provenance: target_provenance,
                result,
            });
        }
        outputs
    });
    let mut links = Vec::with_capacity(non_imported.len());
    for work in chunks.into_iter().flatten() {
        paths.replay_events(work.diag_events);
        if let (Some(dest), Some(src)) = (&mut usage.provenance, work.provenance) {
            dest.extend(src);
        }
        links.push((work.id, work.result?));
    }
    Ok(links)
}

#[expect(
    clippy::too_many_arguments,
    reason = "Config resolution coordinates target resolution, link resolution, and lowering."
)]
fn resolve_config(
    model: &ProjectModel,
    paths: &SourcePaths<'_>,
    config: &ConfigId,
    configured: ConfigTargets,
    compile_inputs: &mut BTreeMap<BString, Vec<BString>>,
    xcode_sources: &mut IndexMap<TargetId, (ConfigId, Vec<BString>)>,
    plan: &ConfigurationPlan<'_>,
    workers_override: Option<usize>,
) -> Result<ConfigOutput, ResolveError> {
    let ConfigTargets {
        targets: mut resolved,
        mut inputs,
        provenance,
        memos,
        ..
    } = configured;
    let ctx = EvalContext {
        config: config.clone(),
        ..EvalContext::default()
    };
    let std_model = stdlib::providers(model, paths.host, &resolved, config)?;
    let model = std_model.as_ref().map_or(model, |(model, _)| model);
    let mut usage = UsageResolver::with_source_paths(model, paths);
    if std_model.is_none() {
        usage = usage.with_memos(&memos);
    }
    usage.provenance = provenance;
    usage.seed_target_languages(config, &resolved);
    let providers = std_model.as_ref().map(|(_, bindings)| bindings);
    let provider_ids = providers
        .into_iter()
        .flat_map(|bindings| bindings.values().copied())
        .collect::<IndexSet<_>>();
    for id in provider_ids {
        if let Some(target) = model.target(id) {
            let (facts, target_inputs) =
                resolve_target(model, paths, &mut usage, id, target, &ctx)?;
            resolved.insert(id, facts);
            inputs.insert(id, target_inputs);
        }
    }
    usage.seed_target_languages(config, &resolved);
    if paths.ide == Some(ide::IdeFlavor::Xcode) {
        ide::check_xcode_sources(model, paths, xcode_sources, &inputs, config)?;
    }
    validate_link_items(model, paths, &mut usage, &resolved, &ctx, plan)?;
    let precompiled = lower::PchPlans::new(
        model,
        &resolved.iter().map(|(id, target)| (*id, target)).collect(),
        &inputs,
        config,
        &mut usage,
    )?;
    precompiled.register_objects(model, config, &mut usage);
    let links = resolve_links(
        model,
        &mut usage,
        paths,
        &resolved,
        &ctx,
        &precompiled,
        plan,
        workers_override,
    );
    let links = match links {
        Err(error) if paths.ide == Some(ide::IdeFlavor::Xcode) => {
            // A configuration with no sources can also have no link language.
            // Compare the source lists before returning the link error.
            return Ok(ConfigOutput {
                provenance: usage.provenance,
                targets: resolved.into_values().collect(),
                runtime_languages: IndexMap::new(),
                graph: Err(error),
                cross_graphs: IndexMap::new(),
                memos: None,
            });
        }
        result => result?,
    };
    attach_links(&mut resolved, &mut inputs, links, providers);
    if let Some(verification) = plan.verification {
        verification.add_header_inputs(config, &mut resolved, &mut inputs);
    }
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
    let fortran = fortran::prepare(model, paths, &mut usage, &mut resolved, &inputs, &ctx)?;
    let mut resolved = resolved.into_values().collect::<Vec<_>>();
    let mut ide_lowering = lower::IdeLowering::default();
    let traced = (!plan.traced_commands.is_empty()).then_some(&plan.traced_commands);
    let graph = lower::lower_with_inputs_workers(
        model,
        &resolved,
        config,
        &inputs,
        &fortran,
        &mut usage,
        (&mut ide_lowering, Some(&precompiled)),
        compile_inputs,
        (plan.cross.native_for(config), traced),
        plan.trace_config
            .filter(|&first| first == config)
            .map(|_| paths.host),
        workers_override,
    )?;
    let mut cross_graphs = IndexMap::new();
    for settings in plan.cross.commands_for(config) {
        let key = (settings.command.clone(), config.clone());
        let graph = lower::lower_with_inputs_workers(
            model,
            &resolved,
            config,
            &inputs,
            &fortran,
            &mut usage,
            (&mut lower::IdeLowering::default(), Some(&precompiled)),
            &mut BTreeMap::new(),
            (Some(settings), traced),
            None,
            workers_override,
        )?;
        cross_graphs.insert(key, graph);
    }
    for target in &mut resolved {
        if let Some(facts) = &mut target.ide {
            facts.custom = ide_lowering
                .custom
                .swap_remove(&target.id)
                .unwrap_or_default();
            facts.pch = ide_lowering.pch.swap_remove(&target.id).unwrap_or_default();
            if paths.ide == Some(ide::IdeFlavor::VisualStudio) {
                ide::add_pch_sources(facts);
            }
        }
    }
    let output_memos = std_model.is_none().then(|| usage.memo_snapshot());
    Ok(ConfigOutput {
        provenance: usage.provenance,
        targets: resolved,
        runtime_languages: inputs
            .into_iter()
            .map(|(id, input)| (id, input.link_languages))
            .collect(),
        graph: Ok(graph),
        cross_graphs,
        memos: output_memos,
    })
}
