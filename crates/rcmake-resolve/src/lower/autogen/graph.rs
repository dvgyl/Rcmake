//! Model overlay and custom commands for Qt automatic generation.

mod ordering;
mod source;

use std::collections::{BTreeMap, BTreeSet};

use indexmap::{IndexMap, IndexSet};
use rcmake_base::{BString, list, path_lex::Lexical, truth};
use rcmake_diag::{Diagnostic, PolicyId, PolicyStatus};
use rcmake_genex::ConfigId;
use rcmake_host::path;
use rcmake_model::{
    BT, CustomCommand, CustomCommandKind, DirId, GeneratorKind, ProjectModel, Target, TargetEvent,
    TargetId, TargetKind, TargetOrigin, UsageField,
};

use super::checksum::PathChecksum;
use super::scan::{
    self, ConfigValues, QtVersion, ScanKind, SourceDiscovery, Tool, ToolDiscovery, ToolPlan,
};
use super::{AutogenPlan, ConfiguredSources, ResourceJob, TargetPlan};
use crate::ResolveError;
use crate::lower::{definition, join, object_dir, short_object_paths};
use crate::sources::{SourcePaths, absolute_path};
use source::{GeneratedSource, add_source, mark_configured, mark_source};

/// Add autogen inputs, outputs, and utilities to a copy of the project model.
pub(super) fn prepare(
    model: &ProjectModel,
    paths: &SourcePaths<'_>,
    configs: &[ConfigId],
    sources: &IndexMap<TargetId, Vec<ConfiguredSources>>,
) -> Result<Option<AutogenPlan>, ResolveError> {
    let Some(generator) = model.generator else {
        return Ok(None);
    };
    let mut targets = Vec::new();
    let mut diagnostics = Vec::new();
    for (id, target) in model.targets() {
        if target.is_imported()
            || !target.kind.compiles_objects()
            || target
                .props
                .get(b"LINKER_LANGUAGE")
                .is_some_and(|lang| lang == b"CSharp")
            || !Tool::ALL.into_iter().any(|tool| tool.requested(target))
        {
            continue;
        }
        let inventory = sources
            .get(&id)
            .map_or(&[] as &[ConfiguredSources], Vec::as_slice);
        let support = PlanContext {
            model,
            paths,
            configs,
            generator,
            inventory,
        };
        if let Some(plan) = support.target(id, target, &mut diagnostics)? {
            targets.push(plan);
        }
    }
    if targets.is_empty() && diagnostics.is_empty() {
        return Ok(None);
    }

    let mut plan = AutogenPlan {
        model: model.clone(),
        diagnostics,
        reconfigure_inputs: Vec::new(),
        info_outputs: Vec::new(),
        clean_paths: Vec::new(),
        targets,
    };
    let mut global = GlobalUtilities::default();
    for target_plan in &plan.targets {
        let Some(origin) = model.target(target_plan.id) else {
            continue;
        };
        let global_moc = variable_on(model, origin.dir, b"CMAKE_GLOBAL_AUTOGEN_TARGET");
        let global_rcc = variable_on(model, origin.dir, b"CMAKE_GLOBAL_AUTORCC_TARGET");
        let folder = utility_folder(model, origin);
        let overlay = &mut plan.model;
        overlay
            .target_mut(target_plan.id)
            .ok_or_else(missing_origin)?
            .props
            .set(
                "AUTOGEN_BETTER_GRAPH_MULTI_CONFIG".into(),
                if target_plan.better { "ON" } else { "OFF" }.into(),
            );
        plan.clean_paths.push(target_plan.build_dir.clone());
        if target_plan.moc_or_uic() {
            let inventory = sources
                .get(&target_plan.id)
                .map_or(&[] as &[ConfiguredSources], Vec::as_slice);
            if let Some(name) =
                add_autogen(overlay, target_plan, origin, inventory, &folder, global_moc)?
                && global_moc
            {
                global.autogen.push(name);
            }
            plan.info_outputs.push(target_plan.info_path());
            plan.clean_paths.extend(cache_paths(target_plan));
        }
        for job in &target_plan.resources {
            if let Some(name) = add_rcc(overlay, target_plan, origin, job, &folder, global_rcc)?
                && global_rcc
            {
                global.rcc.push(name);
            }
            if !job.generated && !global_rcc {
                plan.reconfigure_inputs.push(job.source.clone());
                let listfiles = &mut overlay.dir_mut(origin.dir).listfiles;
                if !listfiles.contains(&job.source) {
                    listfiles.push(job.source.clone());
                }
            }
            plan.info_outputs.push(job.info_path());
            let settings = concatenate(&[&job.prefix, b"_Used"]);
            plan.clean_paths
                .extend(config_file_paths(target_plan, &settings, b".txt"));
        }
        if target_plan.moc_or_uic() || (target_plan.multi && target_plan.rcc.enabled) {
            add_include(overlay, target_plan, origin)?;
        }
    }
    global.finish(&mut plan.model)?;
    stable_unique(&mut plan.clean_paths);
    stable_unique(&mut plan.reconfigure_inputs);
    Ok(Some(plan))
}

struct PlanContext<'a, 'host> {
    model: &'a ProjectModel,
    paths: &'a SourcePaths<'host>,
    configs: &'a [ConfigId],
    generator: GeneratorKind,
    inventory: &'a [ConfiguredSources],
}

impl PlanContext<'_, '_> {
    fn target(
        &self,
        id: TargetId,
        target: &Target,
        diagnostics: &mut Vec<Diagnostic>,
    ) -> Result<Option<TargetPlan>, ResolveError> {
        let model = self.model;
        let dir = model.dir(target.dir);
        let root = model.dir(DirId::ROOT);
        let host = self.paths.host;
        let version = scan::qt_version(model, host, id, target, self.inventory)?;
        let multi = self.generator.multi_config();
        let cross = self.generator == GeneratorKind::NinjaMultiConfig
            && definition(model, DirId::ROOT, b"CMAKE_CROSS_CONFIGS")
                .is_some_and(|value| !truth::is_off(value));
        let better = target
            .props
            .get(b"AUTOGEN_BETTER_GRAPH_MULTI_CONFIG")
            .map_or(version >= QtVersion(6, 8), |value| truth::is_on(value));
        let discovery = ToolDiscovery {
            model,
            host,
            id,
            target,
            version,
            configurations: self.configs,
            per_config: multi && better,
        };
        let moc = discovery.find(Tool::Moc, diagnostics)?;
        let uic = discovery.find(Tool::Uic, diagnostics)?;
        let rcc = discovery.find(Tool::Rcc, diagnostics)?;
        if !(moc.enabled || uic.enabled || rcc.enabled) {
            return Ok(None);
        }
        let checksums = PathChecksum::new(
            host,
            [
                &dir.source_dir,
                &dir.binary_dir,
                &root.source_dir,
                &root.binary_dir,
            ],
        );
        let scan = SourceDiscovery {
            model,
            paths: self.paths,
            target,
            moc: moc.enabled,
            uic: uic.enabled,
            rcc: rcc.enabled,
        }
        .scan(self.inventory, &checksums, diagnostics)?;
        let name = &target.name;
        let (info_dir, default_build) = if short_object_paths(model) {
            let support = absolute_path(
                &root.binary_dir,
                &object_dir(model, target, &ConfigId::default()),
            );
            (join(&support, b"autogen_info"), join(&support, b"autogen"))
        } else {
            (
                join(
                    &dir.binary_dir,
                    &concatenate(&[b"CMakeFiles/", name, b"_autogen.dir"]),
                ),
                join(&dir.binary_dir, &concatenate(&[name, b"_autogen"])),
            )
        };
        let build_dir = match target
            .props
            .get(b"AUTOGEN_BUILD_DIR")
            .filter(|value| !value.is_empty())
        {
            Some(value) if !Lexical::new(value).absolute() => {
                return Err(ResolveError::Message(
                    format!("AUTOGEN_BUILD_DIR for target \"{name}\" must be an absolute path. The current value is \"{value}\".").into(),
                ));
            }
            Some(value) => Lexical::new(value).normalize(),
            None => default_build,
        };
        let cmake = definition(model, target.dir, b"CMAKE_COMMAND")
            .filter(|value| !value.is_empty())
            .cloned()
            .or_else(|| {
                host.current_exe()
                    .ok()
                    .map(|path| path::from_native_cmake(&path))
            })
            .ok_or_else(|| {
                ResolveError::Message("Cannot find the CMake executable for AUTOGEN.".into())
            })?;
        let use_stamp = version >= QtVersion(5, 15);
        let verbosity = definition(model, target.dir, b"CMAKE_AUTOGEN_VERBOSE")
            .map_or(0, |value| {
                scan::uint(value).unwrap_or_else(|| u32::from(truth::is_on(value)))
            });
        let parallel = target.props.get(b"AUTOGEN_PARALLEL").map_or_else(
            || host.system().cpu_count.clamp(1, 64),
            |value| {
                if value.is_empty() || value == b"AUTO" {
                    host.system().cpu_count.clamp(1, 64)
                } else if let Some(count) = scan::uint(value).filter(|count| *count > 0) {
                    count
                } else {
                    diagnostics.push(scan::warning(target, None, format!(
                        "AUTOGEN_PARALLEL=\"{value}\" for target \"{name}\" is not valid. Using AUTOGEN_PARALLEL=1"
                    ).into()));
                    1
                }
            },
        );
        let command_length =
            (host.system().sysname.eq_ignore_ascii_case(b"Windows") || cfg!(windows)).then(|| {
                target
                    .props
                    .get(b"AUTOGEN_COMMAND_LINE_LENGTH_MAX")
                    .map_or(32_000, |value| {
                        scan::uint(value)
                            .filter(|count| *count > 0)
                            .unwrap_or(u32::MAX)
                    })
            });
        let mut plan = TargetPlan {
            id,
            version,
            generator: self.generator,
            configs: self.configs.to_vec(),
            multi,
            cross,
            better,
            use_stamp,
            cmake,
            info_dir,
            build_dir,
            moc,
            uic,
            rcc,
            scan,
            resources: Vec::new(),
            verbosity,
            parallel,
            command_length,
        };
        plan.resources = self.resources(target, &plan, &checksums)?;
        Ok(Some(plan))
    }

    fn resources(
        &self,
        target: &Target,
        plan: &TargetPlan,
        checksums: &PathChecksum<'_>,
    ) -> Result<Vec<ResourceJob>, ResolveError> {
        let mut stems = BTreeMap::new();
        for source in plan.scan.qrc.keys() {
            *stems
                .entry(Lexical::new(source).stem_extension(true).0)
                .or_insert(0usize) += 1;
        }
        let base_options = target_list(target, b"AUTORCC_OPTIONS");
        let mut jobs = Vec::new();
        let mut artifacts = BTreeSet::new();
        for (source, file) in &plan.scan.qrc {
            let stem = Lexical::new(source).stem_extension(true).0;
            let checksum = checksums.for_file(source);
            let unique = stems.get(stem) == Some(&1);
            let prefix = join(
                &plan.info_dir,
                &concatenate(&[b"AutoRcc_", stem, b"_", &checksum]),
            );
            if !artifacts.insert(prefix.clone()) {
                return Err(ResolveError::Message(
                    format!("An AUTORCC output name already exists for {source}.").into(),
                ));
            }
            let output_prefix = join(
                &join(&plan.build_dir, &checksum),
                &concatenate(&[b"qrc_", stem]),
            );
            let output = named(
                &output_prefix,
                if plan.better {
                    plan.config_pattern()
                } else {
                    None
                },
                b".cpp",
            );
            let mut name: BString = stem
                .iter()
                .map(|&byte| if byte == b'-' { b'_' } else { byte })
                .collect();
            if !unique {
                name.extend_from_slice(b"_");
                name.extend_from_slice(&checksum);
            }
            let mut options = base_options.clone();
            for additional in [vec!["-name".into(), name], file.options.clone()] {
                scan::merge_options(
                    &mut options,
                    additional,
                    &[b"name", b"root", b"compress", b"threshold"],
                    plan.version.0 >= 5,
                );
            }
            let mut inputs = ConfigValues::default();
            if !file.generated {
                let host = self.paths.host;
                if plan.multi && plan.better {
                    for (config, executable) in &plan.rcc.executable.per_config {
                        let options = plan
                            .rcc
                            .list_options
                            .per_config
                            .get(config)
                            .map_or(&[] as &[BString], Vec::as_slice);
                        inputs.per_config.insert(
                            config.clone(),
                            scan::resource_inputs(host, source, executable, options)?,
                        );
                    }
                } else {
                    inputs.default = scan::resource_inputs(
                        host,
                        source,
                        &plan.rcc.executable.default,
                        &plan.rcc.list_options.default,
                    )?;
                }
            }
            jobs.push(ResourceJob {
                source: source.clone(),
                stem: stem.into(),
                checksum,
                prefix,
                output,
                options,
                inputs,
                generated: file.generated,
                unique,
            });
        }
        Ok(jobs)
    }
}

fn add_autogen(
    model: &mut ProjectModel,
    plan: &TargetPlan,
    origin: &Target,
    inventory: &[ConfiguredSources],
    folder: &BString,
    global: bool,
) -> Result<Option<BString>, ResolveError> {
    let mut generated = Vec::new();
    let mut tool_files = Vec::new();
    let mut ordering = ordering::origin_dependencies(model, plan.id, origin, inventory)?;
    let mut order_files = IndexSet::new();
    let configured = plan.config_pattern();
    let dir = origin.dir;
    for value in target_list(origin, b"AUTOGEN_TARGET_DEPENDS") {
        if model.find_target(dir, &value).is_some() {
            ordering.insert(value);
        } else {
            order_files.insert(value);
        }
    }
    for (path, file) in &plan.scan.files {
        if file.generated {
            if plan.multi && !plan.cross && file.configs.len() < plan.configs.len() {
                for config in &file.configs {
                    order_files.insert(in_config(config, path));
                }
            } else {
                order_files.insert(path.clone());
            }
        }
    }
    for tool in [&plan.moc, &plan.uic] {
        order_tool(model, tool, &mut ordering);
        tool_files.extend(tool_dependencies(tool, plan.cross && plan.better));
    }
    if plan.moc.enabled {
        let file = plan.compilation_file(configured);
        add_source(
            model,
            plan,
            origin,
            file.clone(),
            GeneratedSource::MocCompilation,
        )?;
        if plan.cross && !plan.better {
            generated.extend(
                plan.configs
                    .iter()
                    .map(|config| plan.compilation_file(Some(&config.0))),
            );
        } else {
            generated.push(file);
        }
    }
    if plan.version >= QtVersion(6, 13) {
        for file in plan
            .scan
            .files
            .values()
            .filter(|file| file.kind == ScanKind::Module && file.moc)
        {
            let include_config = plan.multi.then_some(b"$<CONFIG>".as_slice());
            let moc = join(&plan.include_dir(include_config), &file.moc_output);
            // The shared stamp runs moc for all configurations. Its byproducts
            // must list each path even though each source has a config condition.
            let shared_stamp = plan.cross && !plan.better;
            if plan.multi && (shared_stamp || file.configs.len() < plan.configs.len()) {
                for config in plan
                    .configs
                    .iter()
                    .filter(|cfg| file.configs.is_empty() || file.configs.contains(*cfg))
                {
                    let actual = join(&plan.include_dir(Some(&config.0)), &file.moc_output);
                    let source = in_config(config, &actual);
                    add_source(
                        model,
                        plan,
                        origin,
                        source.clone(),
                        GeneratedSource::MocModule,
                    )?;
                    mark_source(model, origin, actual.clone(), GeneratedSource::MocModule);
                    generated.push(if shared_stamp { actual } else { source });
                }
            } else {
                add_source(model, plan, origin, moc.clone(), GeneratedSource::MocModule)?;
                generated.push(moc);
            }
        }
    }
    let mut ui_byproducts = Vec::new();
    if plan.uic.enabled {
        let source_dir = &model.dir(dir).source_dir;
        for ui in plan.scan.ui.keys() {
            let path = Lexical::new(ui);
            let relative = Lexical::new(path.parent()).relative(Lexical::new(source_dir));
            let Some(relative) =
                relative.filter(|relative| !relative.starts_with(b"../") && relative != b"..")
            else {
                continue;
            };
            let header = concatenate(&[b"ui_", path.stem_extension(true).0, b".h"]);
            let include = plan.include_dir(plan.multi.then_some(b"$<CONFIG>"));
            let include = if relative == b"." {
                include
            } else {
                join(&include, &relative)
            };
            ui_byproducts.push(join(&include, &header));
        }
        for file in &ui_byproducts {
            mark_configured(model, plan, origin, file, GeneratedSource::Auxiliary);
        }
    }

    let name = concatenate(&[&origin.name, b"_autogen"]);
    let tools = match (plan.moc.enabled, plan.uic.enabled) {
        (true, true) => "MOC and UIC",
        (true, false) => "MOC",
        _ => "UIC",
    };
    let mut driver = command(model, origin, CustomCommandKind::Output);
    driver.command_lines = plan.tool_lines("cmake_autogen", &plan.info_path());
    driver.comment = Some(format!("Automatic {tools} for target {}", origin.name).into());

    let visual_studio = matches!(plan.generator, GeneratorKind::VisualStudio(_));
    if visual_studio && !global && order_files.is_empty() {
        if plan.use_stamp {
            attach_stamp(
                model,
                plan,
                origin,
                driver,
                tool_files,
                generated.into_iter().chain(ui_byproducts).collect(),
            )?;
        } else {
            driver.kind = CustomCommandKind::TargetEvent {
                target: plan.id,
                event: TargetEvent::PreBuild,
            };
            driver.byproducts = backtrace(origin, generated.into_iter().chain(ui_byproducts));
            let event = model.add_command(driver).map_err(model_error)?;
            model
                .target_mut(plan.id)
                .ok_or_else(missing_origin)?
                .event_commands
                .push(event);
        }
        for dep in ordering {
            add_utility_dep(model, plan.id, origin, dep)?;
        }
        return Ok(None);
    }

    if plan.use_stamp {
        let order_name = concatenate(&[&origin.name, b"_autogen_timestamp_deps"]);
        let mut order = command(model, origin, CustomCommandKind::Output);
        order.depends = backtrace(origin, order_files);
        let order_id = add_utility(model, origin.dir, origin, &order_name, folder, order)?;
        for dep in ordering {
            add_utility_dep(model, order_id, origin, dep)?;
        }
        tool_files.insert(0, order_name);
        let stamp = attach_stamp(model, plan, origin, driver, tool_files, generated)?;
        let mut utility = command(model, origin, CustomCommandKind::Output);
        utility.depends = backtrace(origin, [stamp]);
        utility.byproducts = backtrace(origin, ui_byproducts);
        add_utility(model, dir, origin, &name, folder, utility)?;
    } else {
        driver.depends = backtrace(origin, order_files.into_iter().chain(tool_files));
        driver.byproducts = backtrace(origin, generated.into_iter().chain(ui_byproducts));
        let utility = add_utility(model, dir, origin, &name, folder, driver)?;
        for dep in ordering {
            add_utility_dep(model, utility, origin, dep)?;
        }
    }
    add_utility_dep(model, plan.id, origin, name.clone())?;
    Ok(Some(name))
}

fn attach_stamp(
    model: &mut ProjectModel,
    plan: &TargetPlan,
    origin: &Target,
    mut driver: CustomCommand,
    depends: Vec<BString>,
    byproducts: Vec<BString>,
) -> Result<BString, ResolveError> {
    let stamp = plan.stamp(plan.stamp_config());
    driver.outputs = backtrace(origin, [stamp.clone()]);
    driver.byproducts = backtrace(origin, byproducts);
    driver.depends = backtrace(origin, depends);
    driver.depfile = Some(plan.depfile(plan.stamp_config()));
    driver.command_lines.push(vec![
        plan.cmake.clone(),
        "-E".into(),
        "touch".into(),
        stamp.clone(),
    ]);
    model.add_command(driver).map_err(model_error)?;
    add_source(
        model,
        plan,
        origin,
        stamp.clone(),
        GeneratedSource::Auxiliary,
    )?;
    Ok(stamp)
}

fn add_rcc(
    model: &mut ProjectModel,
    plan: &TargetPlan,
    origin: &Target,
    job: &ResourceJob,
    folder: &BString,
    global: bool,
) -> Result<Option<BString>, ResolveError> {
    add_source(
        model,
        plan,
        origin,
        job.output.clone(),
        GeneratedSource::Resource,
    )?;
    let info = job.info_path();
    let mut driver = command(model, origin, CustomCommandKind::Output);
    driver.command_lines = plan.tool_lines("cmake_autorcc", &info);
    driver.comment = Some(format!("Automatic RCC for {}", job.source).into());
    driver.depends = backtrace(origin, [job.source.clone(), info]);
    let mut ordering = IndexSet::new();
    order_tool(model, &plan.rcc, &mut ordering);
    if job.generated || global {
        let mut name = concatenate(&[&origin.name, b"_arcc_", &job.stem]);
        if !job.unique {
            name.extend_from_slice(&concatenate(&[b"_", &job.checksum]));
        }
        driver.byproducts = backtrace(origin, [job.output.clone()]);
        let utility = add_utility(model, origin.dir, origin, &name, folder, driver)?;
        for dep in ordering {
            add_utility_dep(model, utility, origin, dep)?;
        }
        add_utility_dep(model, plan.id, origin, name.clone())?;
        Ok(Some(name))
    } else {
        let files = job.inputs.default.iter().cloned().chain(
            job.inputs
                .per_config
                .iter()
                .flat_map(|(config, paths)| paths.iter().map(|path| in_config(config, path))),
        );
        driver.depends.extend(backtrace(
            origin,
            files
                .chain(ordering)
                .chain(tool_dependencies(&plan.rcc, plan.cross && plan.better)),
        ));
        driver.outputs = backtrace(origin, [job.output.clone()]);
        model.add_command(driver).map_err(model_error)?;
        Ok(None)
    }
}

fn order_tool(model: &ProjectModel, tool: &ToolPlan, ordering: &mut IndexSet<BString>) {
    for id in tool
        .dependency
        .into_iter()
        .chain(tool.artifact_deps.iter().map(|(id, _)| *id))
    {
        if let Some(tool_target) = model.target(id).filter(|target| !target.is_imported()) {
            ordering.insert(tool_target.name.clone());
        }
    }
}

fn tool_dependencies(tool: &ToolPlan, command_config: bool) -> impl Iterator<Item = BString> + '_ {
    std::iter::once(tool.executable.default.clone())
        .chain(
            tool.executable
                .per_config
                .iter()
                .filter(|(_, file)| !file.is_empty())
                .map(move |(config, file)| {
                    let path = in_config(config, file);
                    if command_config {
                        concatenate(&[b"$<COMMAND_CONFIG:", &path, b">"])
                    } else {
                        path
                    }
                }),
        )
        .filter(|file| !file.is_empty())
}

fn command(model: &ProjectModel, origin: &Target, kind: CustomCommandKind) -> CustomCommand {
    let mut command = CustomCommand::new(
        origin.dir,
        kind,
        origin.backtrace.clone(),
        origin.policies.policies,
    );
    // Depfile targets are relative to the origin's binary directory.
    let _ = command.policies.set(PolicyId::new(116), PolicyStatus::New);
    command.working_directory = Some(model.dir(origin.dir).binary_dir.clone());
    command.verbatim = true;
    command
}

fn add_utility(
    model: &mut ProjectModel,
    dir: DirId,
    origin: &Target,
    name: &BString,
    folder: &BString,
    mut command: CustomCommand,
) -> Result<TargetId, ResolveError> {
    let mut target = Target::new(
        name.clone(),
        TargetKind::Utility,
        TargetOrigin::Normal,
        dir,
        origin.backtrace.clone(),
        origin.policies.policies,
    );
    target.props.set("EXCLUDE_FROM_ALL".into(), "TRUE".into());
    if !folder.is_empty() {
        target.props.set("FOLDER".into(), folder.clone());
    }
    let id = model.add_target(target).map_err(|error| {
        ResolveError::Message(format!("Cannot add AUTOGEN target {name}. {error}").into())
    })?;
    command.kind = CustomCommandKind::Utility(id);
    command.dir = dir;
    model.add_command(command).map_err(model_error)?;
    Ok(id)
}

fn add_utility_dep(
    model: &mut ProjectModel,
    id: TargetId,
    origin: &Target,
    name: BString,
) -> Result<(), ResolveError> {
    let deps = &mut model
        .target_mut(id)
        .ok_or_else(missing_origin)?
        .utility_deps;
    if !deps.iter().any(|dep| dep.value == name) {
        deps.push(BT::new(name, origin.backtrace.clone()));
    }
    Ok(())
}

fn add_include(
    model: &mut ProjectModel,
    plan: &TargetPlan,
    origin: &Target,
) -> Result<(), ResolveError> {
    let system = origin.props.get(b"AUTOGEN_USE_SYSTEM_INCLUDE").map_or_else(
        || {
            model
                .dir(origin.dir)
                .final_policies
                .unwrap_or(origin.policies.policies)
                .get(PolicyId::new(151))
                == PolicyStatus::New
        },
        |value| truth::is_on(value),
    );
    let include = plan.include_dir(plan.multi.then_some(b"$<CONFIG>"));
    let entry = BT::new(include, origin.backtrace.clone());
    let target = model.target_mut(plan.id).ok_or_else(missing_origin)?;
    let entries = target
        .usage
        .fields
        .entry(UsageField::IncludeDirectories)
        .or_default();
    if system {
        let mut paths = target
            .props
            .get(b"SYSTEM_INCLUDE_DIRECTORIES")
            .map_or_else(Vec::new, |raw| list::split(raw, list::Empty::Drop));
        paths.push(concatenate(&[
            b"$<$<COMPILE_LANGUAGE:CXX>:",
            &entry.value,
            b">",
        ]));
        target.props.set(
            "SYSTEM_INCLUDE_DIRECTORIES".into(),
            list::join(paths.iter().map(|path| path.as_slice())),
        );
        entries.push(entry);
    } else {
        entries.insert(0, entry);
    }
    Ok(())
}

#[derive(Default)]
struct GlobalUtilities {
    autogen: Vec<BString>,
    rcc: Vec<BString>,
}

impl GlobalUtilities {
    fn finish(self, model: &mut ProjectModel) -> Result<(), ResolveError> {
        for (default, key, deps) in [
            (
                "autogen",
                b"CMAKE_GLOBAL_AUTOGEN_TARGET_NAME".as_slice(),
                self.autogen,
            ),
            (
                "autorcc",
                b"CMAKE_GLOBAL_AUTORCC_TARGET_NAME".as_slice(),
                self.rcc,
            ),
        ] {
            let Some(origin) = model
                .targets()
                .find(|(_, target)| deps.iter().any(|name| target.name == *name))
                .map(|(_, target)| target.clone())
            else {
                continue;
            };
            let name = definition(model, DirId::ROOT, key)
                .filter(|name| !name.is_empty())
                .cloned()
                .unwrap_or_else(|| default.into());
            let folder = utility_folder(model, &origin);
            let mut command = command(model, &origin, CustomCommandKind::Output);
            command.working_directory = Some(model.dir(DirId::ROOT).binary_dir.clone());
            let id = add_utility(model, DirId::ROOT, &origin, &name, &folder, command)?;
            for dep in deps {
                add_utility_dep(model, id, &origin, dep)?;
            }
        }
        Ok(())
    }
}

fn utility_folder(model: &ProjectModel, target: &Target) -> BString {
    [
        b"AUTOMOC_TARGETS_FOLDER".as_slice(),
        b"AUTOGEN_TARGETS_FOLDER",
    ]
    .into_iter()
    .find_map(|key| model.global.props.get(key))
    .or_else(|| target.props.get(b"FOLDER"))
    .cloned()
    .unwrap_or_default()
}

fn cache_paths(plan: &TargetPlan) -> impl Iterator<Item = BString> {
    [b"AutogenUsed".as_slice(), b"ParseCache"]
        .into_iter()
        .flat_map(|name| config_file_paths(plan, &join(&plan.info_dir, name), b".txt"))
}

fn config_file_paths(plan: &TargetPlan, base: &[u8], suffix: &[u8]) -> Vec<BString> {
    std::iter::once(named(base, None, suffix))
        .chain(
            plan.configs
                .iter()
                .filter(|_| plan.multi)
                .map(|config| named(base, Some(&config.0), suffix)),
        )
        .collect()
}

/// Append an optional configuration and a file extension to a path.
pub(super) fn named(base: &[u8], config: Option<&[u8]>, extension: &[u8]) -> BString {
    if let Some(config) = config {
        concatenate(&[base, b"_", config, extension])
    } else {
        concatenate(&[base, extension])
    }
}

/// Join byte strings without a separator.
pub(super) fn concatenate(parts: &[&[u8]]) -> BString {
    parts.concat().into()
}

/// Read a target property as a list of non-empty values.
pub(super) fn target_list(target: &Target, key: &[u8]) -> Vec<BString> {
    target
        .props
        .get(key)
        .map_or_else(Vec::new, |value| list::split(value, list::Empty::Drop))
}

fn in_config(config: &ConfigId, path: &[u8]) -> BString {
    if config.0.is_empty() {
        path.into()
    } else {
        concatenate(&[b"$<$<CONFIG:", &config.0, b">:", path, b">"])
    }
}

fn backtrace(origin: &Target, files: impl IntoIterator<Item = BString>) -> Vec<BT<BString>> {
    files
        .into_iter()
        .map(|file| BT::new(file, origin.backtrace.clone()))
        .collect()
}

fn variable_on(model: &ProjectModel, dir: DirId, name: &[u8]) -> bool {
    definition(model, dir, name).is_some_and(|value| truth::is_on(value))
}

fn model_error(error: rcmake_model::ModelError) -> ResolveError {
    ResolveError::Message(error.to_string().into())
}

fn missing_origin() -> ResolveError {
    ResolveError::Message("The AUTOGEN origin target is missing.".into())
}

fn stable_unique(values: &mut Vec<BString>) {
    let mut seen = BTreeSet::new();
    values.retain(|value| seen.insert(value.clone()));
}
