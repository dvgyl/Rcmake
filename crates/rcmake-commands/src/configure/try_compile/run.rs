use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};

use rcmake_base::{BString, list};
use rcmake_build::{BuildOptions, BuildOutcome};
use rcmake_eval::Cx;
use rcmake_genex::ConfigId;
use rcmake_host::{CommandSpec, Input, Output, ProcessResult, RunRequest, path};
use rcmake_model::{DirId, GeneratorKind, ProjectModel};
use rcmake_resolve::{BuildGraph, BuildStep, GenerationResult, GraphTarget};

mod apple;
pub(super) mod cross;
mod file_generate;

use super::args::Options;
use super::child::ProbeProfile;
use super::seed::toolchain_variable;
use super::{TIMEOUT, absolute, set_result};

pub(super) enum ChildError {
    Generate(String),
    Build(String),
}

struct ChildConfigPlan {
    header_config: ConfigId,
    resolve_configs: Vec<ConfigId>,
    file_config: ConfigId,
    build_configs: Vec<ConfigId>,
}

pub(super) fn build_child(
    child: &mut Cx<'_>,
    directory: &Path,
    explicit_config: Option<&BString>,
    target: Option<&[u8]>,
    source_signature: bool,
    generator_facts: Option<std::sync::Arc<dyn rcmake_resolve::GeneratorFacts>>,
    profile: Option<&ProbeProfile>,
) -> Result<(BuildOutcome, Option<PathBuf>, BString), ChildError> {
    let resolve_start = profile.map(ProbeProfile::now);
    let mut model = child.project().clone();
    let plan = match model.generator {
        Some(GeneratorKind::NinjaMultiConfig) => {
            ninja_multi_config_selection(child, explicit_config)?
        }
        Some(generator) if generator.multi_config() => {
            let config = ConfigId(explicit_config.cloned().unwrap_or_else(|| "Debug".into()));
            ChildConfigPlan {
                header_config: config.clone(),
                resolve_configs: vec![config.clone()],
                file_config: config.clone(),
                build_configs: vec![config],
            }
        }
        _ => {
            let config = ConfigId(child.var_or_empty(b"CMAKE_BUILD_TYPE"));
            ChildConfigPlan {
                header_config: config.clone(),
                resolve_configs: vec![config.clone()],
                file_config: config.clone(),
                build_configs: vec![config],
            }
        }
    };
    // Lowering reads toolchain and multi-config variables from the directory model or cache.
    for name in child.variable_names() {
        if toolchain_variable(&name)
            || matches!(
                name.as_slice(),
                b"CMAKE_COMMAND"
                    | b"CMAKE_CONFIGURATION_TYPES"
                    | b"CMAKE_CROSS_CONFIGS"
                    | b"CMAKE_DEFAULT_BUILD_TYPE"
                    | b"CMAKE_DEFAULT_CONFIGS"
            )
        {
            let value = child.var_or_empty(&name);
            model.dir_mut(DirId::ROOT).props.set(name, value);
        }
    }
    for config in &plan.resolve_configs {
        apple::resolve(child, &mut model, directory, config).map_err(ChildError::Generate)?;
    }
    let resolved = match generator_facts {
        Some(facts) => {
            rcmake_resolve::resolve_with_facts(&model, child.host(), &plan.resolve_configs, facts)
        }
        None => rcmake_resolve::resolve(&model, child.host(), &plan.resolve_configs),
    }
    .map_err(|error| ChildError::Generate(error.to_string()))?;
    if resolved.graphs.is_empty() {
        return Err(ChildError::Generate(
            "The resolver returned no build graph.".into(),
        ));
    }
    if model
        .global
        .props
        .get(b"_RCMAKE_FILE_GENERATE_COUNT")
        .is_some()
    {
        file_generate::write(&model, &resolved, child.host()).map_err(ChildError::Generate)?;
    }
    let mut graphs_to_run = Vec::with_capacity(plan.build_configs.len());
    for target_cfg in &plan.build_configs {
        graphs_to_run.push(select_graph(&resolved, &plan.file_config, target_cfg)?);
    }
    let mut artifact = None;
    for graph in &graphs_to_run {
        if artifact.is_none() {
            artifact = target
                .and_then(|name| {
                    graph
                        .targets
                        .iter()
                        .find(|target| target.name.as_slice() == name)
                })
                .and_then(|target| {
                    target
                        .steps
                        .iter()
                        .rev()
                        .find_map(|&id| match graph.steps.get(id) {
                            Some(BuildStep::Link { step, .. }) => {
                                Some(directory.join(path::to_native(&step.output)))
                            }
                            _ => None,
                        })
                });
        }
    }
    let requested: Vec<BString> = target.into_iter().map(BString::from).collect();
    let (merged_graph, merged_targets) = merge_probe_graphs(
        &model,
        &resolved,
        &plan.file_config,
        &plan.build_configs,
        &requested,
    )?;
    for (relative, contents) in &merged_graph.swift_output_file_maps {
        let full = directory.join(path::to_native(relative));
        if let Some(parent) = full.parent() {
            child
                .host()
                .create_dir_all(parent)
                .map_err(|error| ChildError::Generate(error.to_string()))?;
        }
        child
            .host()
            .write(&full, contents.as_slice(), rcmake_host::WriteMode::Truncate)
            .map_err(|error| ChildError::Generate(error.to_string()))?;
    }
    let header = build_log_header(
        child,
        directory,
        &plan.header_config,
        target,
        source_signature,
    );
    if let (Some(profile), Some(start)) = (profile, resolve_start) {
        profile.span(b"probe.resolve", start);
    }
    let build_start = profile.map(ProbeProfile::now);
    let options = BuildOptions {
        build_dir: directory.to_path_buf(),
        targets: merged_targets,
        verbose: source_signature,
        ..BuildOptions::default()
    };
    let combined = rcmake_build::execute(&merged_graph, child.host(), &options)
        .map_err(|error| ChildError::Build(error.to_string()))?;
    if let (Some(profile), Some(start)) = (profile, build_start) {
        profile.span(b"probe.build", start);
    }
    Ok((combined, artifact, header))
}

fn select_graph<'a>(
    resolved: &'a GenerationResult,
    file_config: &ConfigId,
    target_cfg: &ConfigId,
) -> Result<&'a BuildGraph, ChildError> {
    let graph = if target_cfg != file_config {
        resolved
            .cross_graphs
            .get(&(file_config.clone(), target_cfg.clone()))
            .or_else(|| resolved.graphs.iter().find(|g| g.config == *target_cfg))
    } else {
        resolved.graphs.iter().find(|g| g.config == *target_cfg)
    };
    graph.ok_or_else(|| {
        ChildError::Generate(format!(
            "The resolver returned no build graph for configuration '{}'.",
            target_cfg.0
        ))
    })
}

/// Merge all reachable per-configuration and cross-configuration graphs into one unified
/// [`BuildGraph`] so `rcmake_build::execute` schedules every target and deduplicates
/// shared steps across all configurations in a single pass.
fn merge_probe_graphs(
    model: &ProjectModel,
    resolved: &GenerationResult,
    file_config: &ConfigId,
    build_configs: &[ConfigId],
    requested: &[BString],
) -> Result<(BuildGraph, Vec<BString>), ChildError> {
    struct Merger<'a> {
        model: &'a ProjectModel,
        resolved: &'a GenerationResult,
        file_config: &'a ConfigId,
        merged: BuildGraph,
        target_map: HashMap<(ConfigId, usize), usize>,
        step_map: HashMap<(ConfigId, usize), usize>,
        shared_custom: HashMap<Vec<BString>, usize>,
        pending: VecDeque<(ConfigId, usize, usize)>,
    }

    impl<'a> Merger<'a> {
        fn import_step(
            &mut self,
            cfg: &ConfigId,
            graph: &BuildGraph,
            orig_step: usize,
        ) -> Result<usize, ChildError> {
            let key = (cfg.clone(), orig_step);
            if let Some(&existing) = self.step_map.get(&key) {
                return Ok(existing);
            }
            let Some(step) = graph.steps.get(orig_step) else {
                return Err(ChildError::Generate(format!(
                    "Missing build step {orig_step} in configuration '{}'.",
                    cfg.0
                )));
            };
            let is_swift_action = graph.swift_actions.contains_key(&orig_step);
            if !is_swift_action
                && let BuildStep::Custom(custom) = step
                && !custom.symbolic
                && !custom.outputs.is_empty()
                && let Some(&existing) = self.shared_custom.get(&custom.outputs)
                && self.merged.steps.get(existing) == Some(step)
            {
                self.step_map.insert(key, existing);
                return Ok(existing);
            }
            let merged_idx = self.merged.steps.len();
            if !is_swift_action
                && let BuildStep::Custom(custom) = step
                && !custom.symbolic
                && !custom.outputs.is_empty()
            {
                self.shared_custom
                    .entry(custom.outputs.clone())
                    .or_insert(merged_idx);
            }
            self.merged.steps.push(step.clone());
            if let Some(action) = graph.swift_actions.get(&orig_step) {
                self.merged.swift_actions.insert(merged_idx, action.clone());
            }
            self.step_map.insert(key, merged_idx);
            Ok(merged_idx)
        }

        /// Reserve a target slot in `merged.targets` and enqueue it for iterative expansion.
        fn reserve_target(&mut self, cfg: &ConfigId, orig_idx: usize) -> Result<usize, ChildError> {
            let key = (cfg.clone(), orig_idx);
            if let Some(&existing) = self.target_map.get(&key) {
                return Ok(existing);
            }
            let graph = select_graph(self.resolved, self.file_config, cfg)?;
            let Some(orig_target) = graph.targets.get(orig_idx) else {
                return Err(ChildError::Generate(format!(
                    "Missing build target {orig_idx} in configuration '{}'.",
                    cfg.0
                )));
            };
            for (path, content) in &graph.swift_output_file_maps {
                self.merged
                    .swift_output_file_maps
                    .entry(path.clone())
                    .or_insert_with(|| content.clone());
            }
            for (path, extra) in &graph.link_extra_outputs {
                self.merged
                    .link_extra_outputs
                    .entry(path.clone())
                    .or_insert_with(|| extra.clone());
            }
            let merged_idx = self.merged.targets.len();
            self.target_map.insert(key, merged_idx);
            let synthetic_name: BString =
                format!("__rcmake_probe_{merged_idx}__{}", orig_target.name).into();
            self.merged.targets.push(GraphTarget {
                name: synthetic_name,
                steps: Vec::new(),
                depends: Vec::new(),
                in_all: orig_target.in_all,
            });
            self.pending.push_back((cfg.clone(), orig_idx, merged_idx));
            Ok(merged_idx)
        }

        fn drain(&mut self) -> Result<(), ChildError> {
            while let Some((cfg, orig_idx, merged_idx)) = self.pending.pop_front() {
                let graph = select_graph(self.resolved, self.file_config, &cfg)?;
                let Some(orig_target) = graph.targets.get(orig_idx) else {
                    return Err(ChildError::Generate(format!(
                        "Missing build target {orig_idx} in configuration '{}'.",
                        cfg.0
                    )));
                };
                let orig_depends = orig_target.depends.clone();
                let orig_steps = orig_target.steps.clone();
                let mut merged_depends = Vec::with_capacity(orig_depends.len());
                for dep_idx in orig_depends {
                    let merged_dep = self.reserve_target(&cfg, dep_idx)?;
                    if !merged_depends.contains(&merged_dep) {
                        merged_depends.push(merged_dep);
                    }
                }

                let mut merged_steps = Vec::with_capacity(orig_steps.len());
                for orig_step in orig_steps {
                    let cross_deps = match graph.steps.get(orig_step) {
                        Some(BuildStep::Custom(custom)) => custom
                            .target_dependencies
                            .iter()
                            .filter(|(_, dep_cfg)| dep_cfg != &cfg)
                            .cloned()
                            .collect::<Vec<_>>(),
                        _ => Vec::new(),
                    };
                    for (dep_id, dep_cfg) in cross_deps {
                        let dep_graph = select_graph(self.resolved, self.file_config, &dep_cfg)?;
                        let dep_orig = dep_graph
                            .target_ids
                            .iter()
                            .position(|id| *id == dep_id)
                            .or_else(|| {
                                self.model.target(dep_id).and_then(|t| {
                                    dep_graph.targets.iter().position(|gt| gt.name == t.name)
                                })
                            });
                        if let Some(dep_orig) = dep_orig {
                            let merged_dep = self.reserve_target(&dep_cfg, dep_orig)?;
                            if !merged_depends.contains(&merged_dep) {
                                merged_depends.push(merged_dep);
                            }
                        }
                    }
                    let merged_step = self.import_step(&cfg, graph, orig_step)?;
                    merged_steps.push(merged_step);
                }

                if let Some(slot) = self.merged.targets.get_mut(merged_idx) {
                    slot.steps = merged_steps;
                    slot.depends = merged_depends;
                }
            }
            Ok(())
        }
    }

    let mut merger = Merger {
        model,
        resolved,
        file_config,
        merged: BuildGraph {
            config: file_config.clone(),
            ..BuildGraph::default()
        },
        target_map: HashMap::new(),
        step_map: HashMap::new(),
        shared_custom: HashMap::new(),
        pending: VecDeque::new(),
    };

    let mut root_names = Vec::new();
    for target_cfg in build_configs {
        let graph = select_graph(resolved, file_config, target_cfg)?;
        let root_indices: Vec<usize> = if requested.is_empty() {
            graph
                .targets
                .iter()
                .enumerate()
                .filter_map(|(idx, target)| target.in_all.then_some(idx))
                .collect()
        } else {
            let mut indices = Vec::with_capacity(requested.len());
            for name in requested {
                let idx = graph
                    .targets
                    .iter()
                    .position(|target| &target.name == name)
                    .ok_or_else(|| ChildError::Build(format!("unknown target {name}")))?;
                indices.push(idx);
            }
            indices
        };
        for orig_idx in root_indices {
            let merged_idx = merger.reserve_target(target_cfg, orig_idx)?;
            if let Some(target) = merger.merged.targets.get(merged_idx) {
                root_names.push(target.name.clone());
            }
        }
    }
    merger.drain()?;
    Ok((merger.merged, root_names))
}

/// Match CMake's `cmGlobalNinjaMultiGenerator::InspectConfigTypeVariables` and
/// `rcmake-gen-ninja::multi::Selection::new` for child `try_compile` builds.
///
/// Returns the child's header config (empty for an implicit default `build.ninja`),
/// all child configurations to resolve (enabling cross-config graph lowering), the
/// default file configuration (`COMMAND_CONFIG`), and the effective target
/// configuration(s) to build (`OUTPUT_CONFIG`).
fn ninja_multi_config_selection(
    child: &mut Cx<'_>,
    explicit_config: Option<&BString>,
) -> Result<ChildConfigPlan, ChildError> {
    let raw_configs = list::split(
        &child.var_or_empty(b"CMAKE_CONFIGURATION_TYPES"),
        list::Empty::Drop,
    );
    let configs: Vec<ConfigId> = if raw_configs.is_empty() {
        vec![ConfigId("Debug".into())]
    } else {
        raw_configs.into_iter().map(ConfigId).collect()
    };
    let Some(first) = configs.first() else {
        return Err(ChildError::Generate(
            "Ninja Multi-Config requires at least one configuration.".into(),
        ));
    };
    let index = |name: &[u8]| configs.iter().position(|config| config.0 == name);
    let default_build_type = child.var_or_empty(b"CMAKE_DEFAULT_BUILD_TYPE");
    let default_name = if default_build_type.is_empty() {
        &first.0
    } else {
        &default_build_type
    };
    let default_idx = index(default_name).ok_or_else(|| {
        ChildError::Generate(format!(
            "The configuration specified by CMAKE_DEFAULT_BUILD_TYPE ({default_name}) is not present in CMAKE_CONFIGURATION_TYPES"
        ))
    })?;
    let file_config = configs
        .get(default_idx)
        .cloned()
        .unwrap_or_else(|| first.clone());
    let sorted = |mut selection: Vec<usize>| {
        selection.sort_unstable_by_key(|&i| configs.get(i).map(|config| &config.0));
        selection
    };
    let all = sorted((0..configs.len()).collect());
    let parse_subset = |value: &[u8],
                        all_indices: &[usize],
                        lookup: &dyn Fn(&[u8]) -> Option<usize>|
     -> Option<Vec<usize>> {
        let names = list::split(value, list::Empty::Drop);
        if matches!(names.as_slice(), [name] if name == "all") {
            return Some(all_indices.to_vec());
        }
        let mut selected = Vec::new();
        for name in names {
            let idx = lookup(&name)?;
            if !selected.contains(&idx) {
                selected.push(idx);
            }
        }
        Some(selected)
    };
    let cross_setting = child.var_or_empty(b"CMAKE_CROSS_CONFIGS");
    let cross = sorted(parse_subset(&cross_setting, &all, &index).ok_or_else(|| {
        ChildError::Generate(
            "CMAKE_CROSS_CONFIGS is not a subset of CMAKE_CONFIGURATION_TYPES".into(),
        )
    })?);
    let defaults_setting = child.var_or_empty(b"CMAKE_DEFAULT_CONFIGS");
    let defaults_name = if defaults_setting.is_empty() {
        default_name
    } else {
        &defaults_setting
    };
    if defaults_name != default_name && (default_name.is_empty() || cross.is_empty()) {
        return Err(ChildError::Generate(
            "CMAKE_DEFAULT_CONFIGS cannot be used without CMAKE_DEFAULT_BUILD_TYPE or CMAKE_CROSS_CONFIGS".into(),
        ));
    }
    let lookup_default =
        |name: &[u8]| index(name).filter(|idx| *idx == default_idx || cross.contains(idx));
    let defaults = sorted(
        parse_subset(defaults_name, &cross, &lookup_default).ok_or_else(|| {
            ChildError::Generate(
                "CMAKE_DEFAULT_CONFIGS is not a subset of CMAKE_CROSS_CONFIGS".into(),
            )
        })?,
    );
    if let Some(explicit) = explicit_config {
        if index(explicit).is_none() {
            return Err(ChildError::Generate(format!(
                "The configuration specified for the build ({explicit}) is not present in CMAKE_CONFIGURATION_TYPES"
            )));
        }
        let config = ConfigId(explicit.clone());
        return Ok(ChildConfigPlan {
            header_config: config.clone(),
            resolve_configs: configs,
            file_config: config.clone(),
            build_configs: vec![config],
        });
    }
    let selected: Vec<ConfigId> = defaults
        .into_iter()
        .filter_map(|idx| configs.get(idx).cloned())
        .collect();
    let build_configs = if selected.is_empty() {
        vec![file_config.clone()]
    } else {
        selected
    };
    Ok(ChildConfigPlan {
        header_config: ConfigId(BString::default()),
        resolve_configs: configs,
        file_config,
        build_configs,
    })
}

fn build_log_header(
    child: &mut Cx<'_>,
    directory: &Path,
    config: &ConfigId,
    target: Option<&[u8]>,
    source_signature: bool,
) -> BString {
    let generator = child.project().generator;
    let make_program = child
        .var(b"CMAKE_MAKE_PROGRAM")
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| {
            match generator {
                Some(GeneratorKind::UnixMakefiles) | None => "make",
                Some(GeneratorKind::NMakeMakefiles) => "nmake",
                Some(GeneratorKind::Xcode) => "xcodebuild",
                Some(GeneratorKind::VisualStudio(_)) => "MSBuild.exe",
                _ => "ninja",
            }
            .into()
        });
    let mut command = Vec::new();
    if generator == Some(GeneratorKind::UnixMakefiles) {
        command.extend([
            child.var_or_empty(b"CMAKE_COMMAND"),
            "-E".into(),
            "env".into(),
            "VERBOSE=1".into(),
            make_program,
            "-f".into(),
            "Makefile".into(),
        ]);
    } else if generator == Some(GeneratorKind::NMakeMakefiles) {
        command.extend([
            make_program,
            "/nologo".into(),
            "-f".into(),
            "Makefile".into(),
            "VERBOSE=1".into(),
        ]);
    } else {
        command.push(make_program);
        if matches!(
            generator,
            Some(GeneratorKind::Ninja | GeneratorKind::NinjaMultiConfig)
        ) {
            command.push("-v".into());
        }
        if generator == Some(GeneratorKind::NinjaMultiConfig) && !config.0.is_empty() {
            command.extend(["-f".into(), format!("build-{}.ninja", config.0).into()]);
        }
    }
    if let Some(target) = target {
        let mut target = BString::from(target);
        if source_signature
            && matches!(
                generator,
                Some(GeneratorKind::UnixMakefiles | GeneratorKind::NMakeMakefiles)
            )
        {
            target.extend_from_slice(b"/fast");
        }
        command.push(target);
    }
    let command = command
        .iter()
        .map(|word| rcmake_base::shell::quote(word).to_string())
        .collect::<Vec<_>>()
        .join(" ");
    format!(
        "Change Dir: '{}'\n\nRun Build Command(s): {command}\n",
        path::from_native_cmake(directory)
    )
    .into()
}

pub(super) struct RunResult {
    pub(super) value: BString,
    pub(super) stdout: Vec<u8>,
    pub(super) stderr: Vec<u8>,
}

pub(super) fn run_executable(
    cx: &mut Cx<'_>,
    options: &Options<'_>,
    artifact: Option<&Path>,
    variable: &[u8],
    build_output: &[u8],
) -> RunResult {
    let default_cwd = cx.host().initial_cwd();
    let working_dir = options
        .one(b"WORKING_DIRECTORY")
        .map(|dir| absolute(&default_cwd, dir));
    let combined = options.has(b"RUN_OUTPUT_VARIABLE") || options.has(b"OUTPUT_VARIABLE");
    let mut command = Vec::new();
    let cross = cx.var_is_on(b"CMAKE_CROSSCOMPILING");
    if cross {
        command = list::split(
            &cx.var_or_empty(b"CMAKE_CROSSCOMPILING_EMULATOR"),
            list::Empty::Drop,
        );
    }
    let result = if let Some(artifact) = artifact {
        command.push(path::from_native(artifact));
        command.extend(options.many(b"ARGS").iter().copied().map(BString::from));
        command.split_first().and_then(|(program, args)| {
            if let Some(dir) = &working_dir {
                cx.host().create_dir_all(dir).ok()?;
            }
            cx.host()
                .run(
                    &RunRequest {
                        pipeline: vec![CommandSpec {
                            program: program.clone(),
                            args: args.to_vec(),
                        }],
                        cwd: working_dir.unwrap_or(default_cwd),
                        env: cx.host().env_all(),
                        stdin: Input::Null,
                        stdout: Output::Capture,
                        stderr: if combined {
                            Output::SharedWithStdout
                        } else {
                            Output::Capture
                        },
                        timeout: Some(TIMEOUT),
                        tee: false,
                        default_error_mode: false,
                    },
                    &mut |_, _| {},
                )
                .ok()
        })
    } else {
        None
    }
    .unwrap_or_default();

    let value: BString = match result.results.first() {
        Some(ProcessResult::Exited(code)) => code.to_string().into(),
        _ => "FAILED_TO_RUN".into(),
    };
    set_result(cx, variable, value.clone(), options.has(b"NO_CACHE"));
    let result = RunResult {
        value,
        stdout: result.stdout,
        stderr: result.stderr,
    };
    apply_output(cx, options, build_output, &result);
    result
}

fn apply_output(cx: &mut Cx<'_>, options: &Options<'_>, build_output: &[u8], result: &RunResult) {
    for (key, output) in [
        (b"RUN_OUTPUT_VARIABLE".as_slice(), result.stdout.as_slice()),
        (b"RUN_OUTPUT_STDOUT_VARIABLE", result.stdout.as_slice()),
        (b"RUN_OUTPUT_STDERR_VARIABLE", result.stderr.as_slice()),
    ] {
        if let Some(var) = options.one(key) {
            cx.set_var(var, output.into());
        }
    }
    if let Some(var) = options.one(b"OUTPUT_VARIABLE") {
        cx.set_var(
            var,
            [
                build_output,
                result.stdout.as_slice(),
                result.stderr.as_slice(),
            ]
            .concat()
            .into(),
        );
    }
}
