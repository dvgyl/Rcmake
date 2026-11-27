//! Build edges and target aliases for one configuration.

use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::Path;

use rcmake_base::{BString, path_lex::Lexical, truth};
use rcmake_gen_common::GenError;
use rcmake_gen_common::export::ModuleExportJob;
use rcmake_host::{Follow, Host, path};
use rcmake_model::{DirId, ProjectModel, PropScope, TargetKind};
use rcmake_resolve::{
    ArtifactSet, BuildGraph, BuildStep, GenerationResult, LinkFragmentRole, LinkKind, LinkStep,
    ScanResponse, progress,
};

use crate::commands::Commands;
use crate::syntax::{Edge, Ninja};
use crate::{generation_error, modules};

mod aliases;
mod implicit;
pub(crate) use implicit::CustomOwners;
mod order;
mod plan;
mod response;

const PARALLEL_STEP_THRESHOLD: usize = 64;
const PARALLEL_TARGET_THRESHOLD: usize = 64;
const MAX_RENDER_WORKERS: usize = 4;

#[cfg(not(target_arch = "wasm32"))]
fn render_worker_count(item_count: usize, threshold: usize, max_workers: usize) -> usize {
    if item_count < threshold || max_workers <= 1 {
        return 1;
    }
    let available = std::thread::available_parallelism().map_or(1, std::num::NonZeroUsize::get);
    available.min(max_workers).min(item_count).max(1)
}

#[cfg(target_arch = "wasm32")]
fn render_worker_count(_item_count: usize, _threshold: usize, _max_workers: usize) -> usize {
    1
}

#[cfg(not(target_arch = "wasm32"))]
fn map_render_chunks<T, R, F>(items: &[T], workers: usize, process: F) -> Vec<R>
where
    T: Sync,
    R: Send,
    F: Fn(&[T]) -> R + Sync,
{
    let effective = workers.min(items.len());
    let chunk_size = items.len().div_ceil(effective);
    let chunks: Vec<&[T]> = items.chunks(chunk_size).collect();
    let slots: Vec<std::sync::Mutex<Option<R>>> =
        std::iter::repeat_with(|| std::sync::Mutex::new(None))
            .take(chunks.len())
            .collect();
    let process_ref = &process;
    if let (Some((&first_chunk, rest_chunks)), Some((first_slot, rest_slots))) =
        (chunks.split_first(), slots.split_first())
    {
        std::thread::scope(|scope| {
            for (&chunk, slot) in rest_chunks.iter().zip(rest_slots.iter()) {
                if std::thread::Builder::new()
                    .spawn_scoped(scope, move || {
                        *slot
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner) =
                            Some(process_ref(chunk));
                    })
                    .is_err()
                {
                    *slot
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner) =
                        Some(process_ref(chunk));
                }
            }
            *first_slot
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) =
                Some(process_ref(first_chunk));
        });
    }
    slots
        .into_iter()
        .filter_map(|slot| {
            slot.into_inner()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
        })
        .collect()
}

enum PlannedStepNodes<'s, 'a> {
    Borrowed(&'s StepNodes<'a>),
    Owned(StepNodes<'a>),
}

impl<'a> PlannedStepNodes<'_, 'a> {
    fn as_ref(&self) -> &StepNodes<'a> {
        match self {
            Self::Borrowed(nodes) => nodes,
            Self::Owned(nodes) => nodes,
        }
    }
}

pub(crate) struct Graph<'a> {
    pub(crate) input: &'a BuildGraph,
    pub(crate) build_dir: &'a Path,
    pub(crate) config: Option<&'a [u8]>,
    pub(crate) prefix: &'a [u8],
    pub(crate) commands: Commands,
    build_root: BString,
    pub(crate) clean_files: BTreeSet<BString>,
    pub(crate) command_settings: crate::response::Settings<'a>,
    custom_scripts: HashMap<BString, BString>,
    link_commands: HashMap<BString, response::LinkCommand>,
    names: HashMap<BString, usize>,
    fortran_info: HashMap<&'a [u8], &'a rcmake_resolve::FortranTargetInfo>,
    scan_responses: HashMap<&'a [u8], &'a ScanResponse>,
    pub(crate) compile_languages: HashMap<&'a [u8], &'a [u8]>,
    pub(crate) launcher_slots: BTreeMap<BString, usize>,
    compile_inputs: Option<&'a std::collections::BTreeMap<BString, Vec<BString>>>,
    aliases: Vec<BString>,
    extra_aliases: BTreeMap<BString, Vec<BString>>,
    artifact_targets: HashSet<usize>,
    producers: HashMap<BString, usize>,
    assumed_sources: HashSet<BString>,
    generated_inputs: HashMap<BString, BString>,
    pub(crate) pools: HashMap<BString, BString>,
    steps: Vec<StepNodes<'a>>,
    order_only_alias_bytes: usize,
    compile_prerequisites: Vec<Vec<order::Prerequisite>>,
    target_meta: Vec<TargetMeta>,
    artifact_to_target: HashMap<BString, usize>,
    imported_shared_artifacts: HashSet<BString>,
    step_owner: HashMap<usize, usize>,
    step_command_dir: HashMap<usize, DirId>,
    exe_symlinks: BTreeMap<usize, ExecutableSymlink>,
    root_all_inputs: Option<Vec<BString>>,
}

#[derive(Clone)]
pub(super) struct TargetMeta {
    pub(super) kind: TargetKind,
    pub(super) dir: Option<DirId>,
    pub(super) dir_rel: BString,
    pub(super) artifact_outputs: Vec<BString>,
    pub(super) order_depends_target: Option<BString>,
    pub(super) order_depends_private: Option<BString>,
    pub(super) order_depends_inputs: Vec<BString>,
    pub(super) order_depends_private_inputs: Vec<BString>,
    pub(super) is_non_framework_shared: bool,
    pub(super) link_depends_no_shared: bool,
    pub(super) linker_path: Option<BString>,
}

#[derive(Clone)]
pub(super) struct ExecutableSymlink {
    pub(super) split_at: usize,
    pub(super) real_output: BString,
    pub(super) symlink_output: BString,
}

#[derive(Clone, Default)]
struct StepNodes<'a> {
    outputs: Vec<BString>,
    byproducts: Vec<BString>,
    order_only: Vec<OrderOnly<'a>>,
    link: bool,
}

#[derive(Clone)]
enum OrderOnly<'a> {
    Targets(Cow<'a, [usize]>),
    TargetClosure(Cow<'a, [usize]>),
    StepOutputs(usize),
    ObjectOrder(usize),
}

impl<'a> Graph<'a> {
    #[cfg(test)]
    pub(crate) fn new(
        input: &'a BuildGraph,
        artifacts: &BTreeMap<&[u8], &ArtifactSet>,
        build_dir: &'a Path,
        config: Option<&'a [u8]>,
    ) -> Result<Self, GenError> {
        Self::with_prefix(
            input,
            artifacts,
            build_dir,
            config,
            b"",
            Commands::default(),
        )
    }

    #[cfg(test)]
    pub(crate) fn with_prefix(
        input: &'a BuildGraph,
        artifacts: &BTreeMap<&[u8], &ArtifactSet>,
        build_dir: &'a Path,
        config: Option<&'a [u8]>,
        prefix: &'a [u8],
        commands: Commands,
    ) -> Result<Self, GenError> {
        Self::with_context(
            input, artifacts, build_dir, config, prefix, commands, None, None,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn with_context(
        input: &'a BuildGraph,
        artifacts: &BTreeMap<&[u8], &ArtifactSet>,
        build_dir: &'a Path,
        config: Option<&'a [u8]>,
        prefix: &'a [u8],
        commands: Commands,
        model: Option<&ProjectModel>,
        result: Option<&GenerationResult>,
    ) -> Result<Self, GenError> {
        let mut graph = Self {
            input,
            build_dir,
            config,
            prefix,
            build_root: commands.path(&path::from_native(build_dir)),
            commands,
            clean_files: BTreeSet::new(),
            command_settings: crate::response::Settings::default(),
            custom_scripts: HashMap::new(),
            link_commands: HashMap::new(),
            names: HashMap::with_capacity(input.targets.len()),
            fortran_info: HashMap::new(),
            scan_responses: HashMap::new(),
            compile_languages: HashMap::new(),
            launcher_slots: BTreeMap::new(),
            compile_inputs: None,
            aliases: Vec::with_capacity(input.targets.len()),
            extra_aliases: BTreeMap::new(),
            artifact_targets: HashSet::new(),
            producers: HashMap::with_capacity(input.steps.len()),
            assumed_sources: HashSet::new(),
            generated_inputs: HashMap::new(),
            pools: HashMap::new(),
            steps: Vec::with_capacity(input.steps.len()),
            order_only_alias_bytes: 0,
            compile_prerequisites: Vec::new(),
            target_meta: Vec::with_capacity(input.targets.len()),
            artifact_to_target: HashMap::new(),
            imported_shared_artifacts: HashSet::new(),
            step_owner: HashMap::new(),
            step_command_dir: HashMap::new(),
            exe_symlinks: BTreeMap::new(),
            root_all_inputs: None,
        };
        if let Some(model) = model {
            for (&step_index, owners) in &input.custom_output_owners {
                if let Some(command) = model.command(owners.command) {
                    graph.step_command_dir.insert(step_index, command.dir);
                }
            }
            for (_, raw) in model.targets() {
                if raw.is_imported() && raw.kind == TargetKind::SharedLibrary {
                    let set = rcmake_resolve::lower::compute_artifacts(model, raw, &input.config);
                    for path in [
                        set.file.as_ref(),
                        set.soname_file.as_ref(),
                        set.linker_file.as_ref(),
                        set.import_file.as_ref(),
                    ]
                    .into_iter()
                    .flatten()
                    {
                        graph.imported_shared_artifacts.insert(graph.node(path));
                    }
                }
            }
        }
        let resolved_by_name: HashMap<&[u8], &rcmake_resolve::ResolvedTarget> = result
            .into_iter()
            .flat_map(|res| &res.targets)
            .filter(|target| target.config == input.config)
            .map(|target| (target.metadata.name.as_slice(), target))
            .collect();
        for (index, target) in input.targets.iter().enumerate() {
            let name = graph.node(&target.name);
            if name.is_empty() || name == "all" || graph.names.insert(name.clone(), index).is_some()
            {
                return Err(generation_error(
                    build_dir,
                    "Invalid or duplicate Ninja target name.",
                ));
            }
            for &step_index in &target.steps {
                graph.step_owner.entry(step_index).or_insert(index);
            }
            let raw = model.and_then(|model| {
                input
                    .target_ids
                    .get(index)
                    .and_then(|id| model.target(*id))
                    .or_else(|| {
                        model
                            .find_target(DirId::ROOT, &target.name)
                            .and_then(|id| model.target(id))
                    })
            });
            let resolved = resolved_by_name.get(target.name.as_slice()).copied();
            let dir_rel = if let (Some(model), Some(raw)) = (model, raw) {
                if raw.dir == DirId::ROOT {
                    BString::default()
                } else {
                    let node = graph.node(&model.dir(raw.dir).binary_dir);
                    if node == b"." {
                        BString::default()
                    } else {
                        node
                    }
                }
            } else if let Some(resolved) = resolved {
                let node = graph.node(&resolved.metadata.binary_dir);
                if node == b"." || node.is_empty() {
                    BString::default()
                } else {
                    node
                }
            } else {
                BString::default()
            };
            let artifact_set = artifacts.get(target.name.as_slice()).copied();
            let kind =
                raw.map(|raw| raw.kind)
                    .or_else(|| resolved.map(|resolved| resolved.metadata.kind))
                    .unwrap_or_else(|| {
                        if let Some(link_kind) = target.steps.iter().find_map(|&step_idx| {
                            match input.steps.get(step_idx) {
                                Some(BuildStep::Link { step, .. }) => Some(step.kind),
                                _ => None,
                            }
                        }) {
                            match link_kind {
                                LinkKind::Executable => TargetKind::Executable,
                                LinkKind::SharedLibrary => TargetKind::SharedLibrary,
                                LinkKind::StaticArchive => TargetKind::StaticLibrary,
                                LinkKind::ModuleLibrary => TargetKind::ModuleLibrary,
                            }
                        } else if artifact_set.is_some_and(|a| a.file.is_some()) {
                            TargetKind::Executable
                        } else if target.steps.iter().any(|&step_idx| {
                            matches!(
                                input.steps.get(step_idx),
                                Some(BuildStep::Compile { .. } | BuildStep::Scan { .. })
                            )
                        }) {
                            TargetKind::ObjectLibrary
                        } else {
                            TargetKind::Utility
                        }
                    });
            let is_framework = model.is_some_and(|model| {
                raw.is_some_and(|raw| {
                    matches!(
                        rcmake_resolve::ide::platform_bundle_kind(model, raw),
                        rcmake_resolve::ide::BundleKind::Framework { .. }
                    )
                })
            });
            let is_non_framework_shared = kind == TargetKind::SharedLibrary && !is_framework;
            let link_depends_no_shared = raw
                .and_then(|raw| raw.props.get(b"LINK_DEPENDS_NO_SHARED"))
                .is_some_and(|value| truth::is_on(value));
            let alias = if matches!(
                kind,
                TargetKind::Utility
                    | TargetKind::InterfaceLibrary
                    | TargetKind::ObjectLibrary
                    | TargetKind::GlobalTarget
            ) && !dir_rel.is_empty()
            {
                let scoped = Lexical::new(&dir_rel).append(&target.name);
                qualified(&graph.node(&scoped), config)
            } else {
                qualified(&name, config)
            };
            graph.aliases.push(alias);
            if artifact_set.is_some_and(|artifacts| artifacts.file.is_some()) {
                graph.artifact_targets.insert(index);
            }
            if let Some(set) = artifact_set {
                for path in [
                    set.file.as_ref(),
                    set.soname_file.as_ref(),
                    set.linker_file.as_ref(),
                    set.import_file.as_ref(),
                ]
                .into_iter()
                .flatten()
                {
                    graph
                        .artifact_to_target
                        .entry(graph.node(path))
                        .or_insert(index);
                }
            }
            for &step_idx in &target.steps {
                if let Some(BuildStep::Link { step, .. }) = input.steps.get(step_idx) {
                    graph
                        .artifact_to_target
                        .entry(graph.node(&step.output))
                        .or_insert(index);
                }
            }
            graph.target_meta.push(TargetMeta {
                kind,
                dir: raw.map(|raw| raw.dir),
                dir_rel,
                artifact_outputs: Vec::new(),
                order_depends_target: None,
                order_depends_private: None,
                order_depends_inputs: Vec::new(),
                order_depends_private_inputs: Vec::new(),
                is_non_framework_shared,
                link_depends_no_shared,
                linker_path: None,
            });
        }
        let mut used_synthetic = HashSet::new();
        for (index, step) in input.steps.iter().enumerate() {
            let mut nodes = StepNodes::default();
            if let Some(action) = input.swift_actions.get(&index) {
                nodes
                    .outputs
                    .extend(action.outputs.iter().map(|output| graph.node(output)));
            } else {
                match step {
                    BuildStep::Compile {
                        object, byproducts, ..
                    } => {
                        nodes.outputs.push(graph.node(object));
                        nodes
                            .byproducts
                            .extend(byproducts.iter().map(|output| graph.node(output)));
                    }
                    BuildStep::Link {
                        step, invocations, ..
                    } => {
                        let real_output = graph.node(&step.output);
                        nodes.outputs.push(real_output.clone());
                        if let Some(extra) = input.link_extra_outputs.get(&step.output) {
                            for output in extra {
                                let node = graph.node(output);
                                if !nodes.outputs.contains(&node) {
                                    nodes.outputs.push(node);
                                }
                            }
                        }
                        if let Some(byproducts) = input.link_event_byproducts.get(&step.output) {
                            for output in byproducts {
                                let node = graph.node(output);
                                if !nodes.outputs.contains(&node) {
                                    nodes.outputs.push(node);
                                }
                            }
                        }
                        if step.kind == LinkKind::Executable
                            && let Some(split_at) = invocations.iter().position(is_exe_symlink_call)
                        {
                            let symlink_raw = invocations
                                .get(split_at)
                                .and_then(|inv| inv.argv.get(4))
                                .cloned()
                                .or_else(|| {
                                    graph
                                        .step_owner
                                        .get(&index)
                                        .and_then(|&owner| input.targets.get(owner))
                                        .and_then(|target| artifacts.get(target.name.as_slice()))
                                        .and_then(|set| set.linker_file.clone())
                                });
                            if let Some(symlink_raw) = symlink_raw {
                                let symlink_output = graph.node(&symlink_raw);
                                if symlink_output != real_output {
                                    nodes.outputs.retain(|out| out != &symlink_output);
                                    graph.exe_symlinks.insert(
                                        index,
                                        ExecutableSymlink {
                                            split_at,
                                            real_output,
                                            symlink_output,
                                        },
                                    );
                                }
                            }
                        }
                    }
                    BuildStep::DeviceLink { output, mode, .. } => {
                        nodes.outputs.push(graph.node(output));
                        if let rcmake_resolve::DeviceLinkMode::Explicit {
                            cubins,
                            fatbin_header,
                            register_header,
                            ..
                        } = mode
                        {
                            nodes
                                .byproducts
                                .extend(cubins.iter().map(|cubin| graph.node(&cubin.output)));
                            nodes
                                .byproducts
                                .extend([graph.node(fatbin_header), graph.node(register_header)]);
                        }
                    }
                    BuildStep::Scan {
                        ddi, preprocessed, ..
                    } => {
                        nodes.outputs.push(graph.node(ddi));
                        nodes
                            .byproducts
                            .extend(preprocessed.iter().map(|output| graph.node(output)));
                    }
                    BuildStep::Collate {
                        lang, dd, modmaps, ..
                    } => {
                        nodes.outputs.push(graph.node(dd));
                        nodes.byproducts.push(graph.node(&if lang == "Fortran" {
                            Lexical::new(Lexical::new(dd).parent()).append(b"FortranModules.json")
                        } else {
                            modules::modules_json(dd)
                        }));
                        nodes
                            .byproducts
                            .extend(modmaps.iter().map(|map| graph.node(map)));
                    }
                    BuildStep::Custom(custom) => {
                        nodes
                            .outputs
                            .extend(custom.outputs.iter().map(|name| graph.node(name)));
                        let owned_utility = if nodes.outputs.is_empty() {
                            graph.step_owner.get(&index).copied().filter(|&owner| {
                                graph.target_meta.get(owner).is_some_and(|meta| {
                                    matches!(
                                        meta.kind,
                                        TargetKind::Utility
                                            | TargetKind::InterfaceLibrary
                                            | TargetKind::GlobalTarget
                                    )
                                })
                            })
                        } else {
                            None
                        };
                        if let Some(owner) = owned_utility
                            && let Some(owner_target) = input.targets.get(owner)
                            && let Some(owner_meta) = graph.target_meta.get(owner)
                        {
                            let owner_name = &owner_target.name;
                            let dir_rel = &owner_meta.dir_rel;
                            let mut synth = Vec::new();
                            if !dir_rel.is_empty() {
                                synth.extend_from_slice(dir_rel);
                                synth.push(b'/');
                            }
                            synth.extend_from_slice(b"CMakeFiles/");
                            synth.extend_from_slice(owner_name);
                            let candidate = qualified(&graph.node(&synth), config);
                            if used_synthetic.insert(candidate.clone()) {
                                nodes.outputs.push(candidate);
                            }
                        }
                        for name in &custom.byproducts {
                            let node = graph.node(name);
                            if !nodes.outputs.contains(&node) {
                                nodes.outputs.push(node);
                            }
                        }
                        if nodes.outputs.is_empty() {
                            let name =
                                BString::from(format!("CMakeFiles/rcmake-custom-{index}.util"));
                            nodes.outputs.push(qualified(&graph.node(&name), config));
                        }
                    }
                }
            }
            graph.steps.push(nodes);
        }
        graph.add_link_byproducts(artifacts);
        for (index, nodes) in graph.steps.iter().enumerate() {
            for node in nodes.outputs.iter().chain(&nodes.byproducts) {
                if graph.producers.insert(node.clone(), index).is_some() {
                    return Err(GenError::Invalid(
                        format!("multiple rules generate {node}").into(),
                    ));
                }
                if node.is_empty() || node == &graph.node(b"all") {
                    return Err(generation_error(
                        build_dir,
                        format!("Invalid or duplicate Ninja output: {node}"),
                    ));
                }
            }
        }
        for (&index, sym) in &graph.exe_symlinks {
            if graph
                .producers
                .insert(sym.symlink_output.clone(), index)
                .is_some()
            {
                return Err(GenError::Invalid(
                    format!("multiple rules generate {}", sym.symlink_output).into(),
                ));
            }
            graph
                .artifact_to_target
                .entry(sym.symlink_output.clone())
                .or_insert_with(|| graph.step_owner.get(&index).copied().unwrap_or(0));
        }
        graph.disambiguate_aliases();
        graph.compute_target_artifacts(artifacts, model);
        graph.collect_assumed_sources(model);
        graph.connect_targets()?;
        Ok(graph)
    }

    fn collect_assumed_sources(&mut self, model: Option<&ProjectModel>) {
        let Some(model) = model else { return };
        for target in &self.input.targets {
            let Some(target_id) = model.find_target(DirId::ROOT, &target.name) else {
                continue;
            };
            let Some(owner) = model.target(target_id) else {
                continue;
            };
            for &index in &target.steps {
                let Some(BuildStep::Compile { source, .. } | BuildStep::Scan { source, .. }) =
                    self.input.steps.get(index)
                else {
                    continue;
                };
                if self.has_target(&self.node(source))
                    || self.has_target(&self.commands.path(source))
                {
                    continue;
                }
                if model
                    .get_prop(
                        &PropScope::Source {
                            dir: owner.dir,
                            path: source.clone(),
                        },
                        b"GENERATED",
                    )
                    .is_some_and(|value| truth::is_on(&value))
                {
                    self.assumed_sources.insert(source.clone());
                }
            }
        }
    }

    fn compute_target_artifacts(
        &mut self,
        artifacts: &BTreeMap<&[u8], &ArtifactSet>,
        model: Option<&ProjectModel>,
    ) {
        for (index, target) in self.input.targets.iter().enumerate() {
            let Some(kind) = self.target_meta.get(index).map(|meta| meta.kind) else {
                continue;
            };
            let raw = model.and_then(|model| {
                self.input
                    .target_ids
                    .get(index)
                    .and_then(|id| model.target(*id))
                    .or_else(|| {
                        model
                            .find_target(DirId::ROOT, &target.name)
                            .and_then(|id| model.target(id))
                    })
            });
            let is_framework = model.is_some_and(|model| {
                raw.is_some_and(|raw| {
                    matches!(
                        rcmake_resolve::ide::platform_bundle_kind(model, raw),
                        rcmake_resolve::ide::BundleKind::Framework { .. }
                    )
                })
            });
            let set = artifacts.get(target.name.as_slice()).copied();
            let link_step = target.steps.iter().copied().find(|&step_idx| {
                self.steps.get(step_idx).is_some_and(|nodes| nodes.link)
                    || matches!(self.input.steps.get(step_idx), Some(BuildStep::Link { .. }))
            });
            let link_output = link_step
                .and_then(|step_idx| self.steps.get(step_idx))
                .and_then(|nodes| nodes.outputs.first().cloned());
            let exe_symlink_output = link_step
                .and_then(|step_idx| self.exe_symlinks.get(&step_idx))
                .map(|sym| sym.symlink_output.clone())
                .or_else(|| {
                    target.steps.iter().copied().find_map(|step_idx| {
                        let BuildStep::Custom(custom) = self.input.steps.get(step_idx)? else {
                            return None;
                        };
                        is_executable_symlink(custom).then(|| {
                            self.steps
                                .get(step_idx)
                                .and_then(|nodes| nodes.outputs.first().cloned())
                        })?
                    })
                });
            let mut outputs = Vec::new();
            let mut linker_path = None;
            match kind {
                TargetKind::SharedLibrary => {
                    if let Some(set) = set {
                        if let Some(import_file) = set
                            .import_file
                            .as_ref()
                            .filter(|file| file.ends_with(b".tbd"))
                        {
                            outputs.push(self.node(import_file));
                        }
                        let main = if is_framework
                            || set
                                .linker_file
                                .as_ref()
                                .is_some_and(|f| set.import_file.as_ref() == Some(f))
                        {
                            set.file.as_ref()
                        } else {
                            set.linker_file.as_ref().or(set.file.as_ref())
                        };
                        if let Some(main) = main {
                            let node = self.node(main);
                            linker_path = Some(node.clone());
                            if !outputs.contains(&node) {
                                outputs.push(node);
                            }
                        }
                    }
                    if outputs.is_empty()
                        && let Some(out) = link_output.clone()
                    {
                        linker_path = Some(out.clone());
                        outputs.push(out);
                    }
                }
                TargetKind::Executable => {
                    if let Some(sym) = exe_symlink_output {
                        outputs.push(sym);
                    } else if let Some(set) = set {
                        let main = if is_framework {
                            set.file.as_ref()
                        } else {
                            set.linker_file
                                .as_ref()
                                .filter(|f| set.import_file.as_ref() != Some(*f))
                                .or(set.file.as_ref())
                        };
                        if let Some(main) = main {
                            outputs.push(self.node(main));
                        }
                    }
                    if outputs.is_empty()
                        && let Some(out) = link_output
                    {
                        outputs.push(out);
                    }
                }
                TargetKind::StaticLibrary
                | TargetKind::ModuleLibrary
                | TargetKind::UnknownLibrary => {
                    if let Some(file) = set.and_then(|set| set.file.as_ref()) {
                        outputs.push(self.node(file));
                    } else if let Some(out) = link_output {
                        outputs.push(out);
                    }
                }
                TargetKind::ObjectLibrary
                | TargetKind::Utility
                | TargetKind::InterfaceLibrary
                | TargetKind::GlobalTarget => {
                    if let Some(alias) = self.aliases.get(index) {
                        outputs.push(alias.clone());
                    }
                }
            }
            if outputs.is_empty()
                && let Some(alias) = self.aliases.get(index)
            {
                outputs.push(alias.clone());
            }
            if let Some(meta) = self.target_meta.get_mut(index) {
                meta.artifact_outputs = outputs;
                meta.linker_path = linker_path;
            }
        }
    }

    pub(super) fn step_belongs_to_target_dir(
        &self,
        target_index: usize,
        step_index: usize,
    ) -> bool {
        let Some(target_dir) = self.target_meta.get(target_index).and_then(|meta| meta.dir) else {
            return true;
        };
        let Some(&command_dir) = self.step_command_dir.get(&step_index) else {
            return true;
        };
        command_dir == target_dir
    }

    fn disambiguate_aliases(&mut self) {
        let mut reserved: HashSet<_> = self.names.keys().chain(&self.aliases).cloned().collect();
        for (index, (target, alias)) in self.input.targets.iter().zip(&mut self.aliases).enumerate()
        {
            // Normal artifact targets already expose their real build artifact
            // path as `artifact_outputs`, so a colliding short phony alias is
            // omitted (matching CMake) rather than given a synthetic `.util` name.
            let has_concrete_artifact = self.target_meta.get(index).is_some_and(|meta| {
                matches!(
                    meta.kind,
                    TargetKind::Executable
                        | TargetKind::SharedLibrary
                        | TargetKind::StaticLibrary
                        | TargetKind::ModuleLibrary
                        | TargetKind::UnknownLibrary
                )
            }) && (self.artifact_targets.contains(&index)
                || target.steps.iter().any(|&step_idx| {
                    self.steps.get(step_idx).is_some_and(|nodes| nodes.link)
                        || matches!(self.input.steps.get(step_idx), Some(BuildStep::Link { .. }))
                }));
            if has_concrete_artifact {
                continue;
            }
            if let Some(producer) = self.producers.get(alias)
                && !target.steps.contains(producer)
            {
                for suffix in 0usize.. {
                    let name =
                        BString::from(format!("CMakeFiles/rcmake-target-{index}-{suffix}.util"));
                    let candidate = qualified(&name, self.config);
                    if !self.producers.contains_key(&candidate)
                        && reserved.insert(candidate.clone())
                    {
                        *alias = candidate;
                        break;
                    }
                }
            }
        }
    }

    fn add_link_byproducts(&mut self, artifacts: &BTreeMap<&[u8], &ArtifactSet>) {
        let explicit: HashSet<_> = self
            .steps
            .iter()
            .flat_map(|step| step.outputs.iter().chain(&step.byproducts).cloned())
            .collect();
        for target in &self.input.targets {
            let Some(artifacts) = artifacts.get(target.name.as_slice()) else {
                continue;
            };
            let paths: Vec<_> = [
                &artifacts.soname_file,
                &artifacts.linker_file,
                &artifacts.import_file,
            ]
            .into_iter()
            .filter_map(Option::as_ref)
            .map(|path| self.node(path))
            .collect();
            let main = artifacts.file.as_ref().map(|file| self.node(file));
            for &index in &target.steps {
                let exe_symlink_out = self
                    .exe_symlinks
                    .get(&index)
                    .map(|sym| sym.symlink_output.clone());
                if let Some(nodes) = self.steps.get_mut(index) {
                    nodes.link |=
                        matches!(self.input.steps.get(index), Some(BuildStep::Link { .. }))
                            || main
                                .as_ref()
                                .is_some_and(|file| nodes.outputs.first() == Some(file));
                    if !nodes.link {
                        continue;
                    }
                    for path in &paths {
                        if exe_symlink_out.as_ref() == Some(path) {
                            continue;
                        }
                        if !explicit.contains(path) && !nodes.byproducts.contains(path) {
                            nodes.byproducts.push(path.clone());
                        }
                    }
                }
            }
        }
    }

    pub(crate) fn node(&self, value: &[u8]) -> BString {
        let value = self.commands.path(value);
        let relative = self
            .commands
            .beneath(&value, &self.build_root)
            .map(BString::from);
        let relative = relative.unwrap_or(value);
        if self.prefix.is_empty()
            || self.commands.absolute(&relative)
            || relative == self.prefix
            || relative
                .strip_prefix(self.prefix)
                .is_some_and(|tail| tail.starts_with(b"/"))
        {
            relative
        } else {
            self.commands.path(&[self.prefix, b"/", &relative].concat())
        }
    }

    pub(crate) fn in_build_dir(&self, command: &[u8]) -> BString {
        if self.prefix.is_empty() {
            return command.into();
        }
        self.commands
            .in_directory(command, &path::from_native(self.build_dir))
            .into()
    }

    pub(crate) fn command_line(
        &self,
        invocations: &[rcmake_resolve::Invocation],
        custom: bool,
    ) -> Vec<u8> {
        self.commands.line(
            invocations,
            custom,
            &path::from_native(self.build_dir),
            !self.prefix.is_empty(),
        )
    }

    fn link_command_line(
        &self,
        invocations: &[rcmake_resolve::Invocation],
        custom: bool,
    ) -> Vec<u8> {
        if custom {
            return self.command_line(invocations, true);
        }
        self.commands.link_line(
            invocations,
            &path::from_native(self.build_dir),
            !self.prefix.is_empty(),
        )
    }

    pub(super) fn utility_command_line(
        &self,
        invocations: &[rcmake_resolve::Invocation],
    ) -> Vec<u8> {
        self.commands.utility_line(
            invocations,
            &path::from_native(self.build_dir),
            !self.prefix.is_empty(),
        )
    }

    fn is_orphan_custom_step(&self, index: usize) -> bool {
        self.input
            .custom_output_owners
            .get(&index)
            .is_some_and(|owners| owners.targets.is_empty())
            && !self.step_owner.contains_key(&index)
    }

    pub(crate) fn has_target(&self, name: &BString) -> bool {
        self.names.contains_key(name) || self.producers.contains_key(name)
    }

    pub(crate) fn add_aliases(
        &mut self,
        artifacts: &BTreeMap<&[u8], &ArtifactSet>,
        model: Option<&ProjectModel>,
        result: &GenerationResult,
    ) {
        let mut ambiguous = BTreeSet::new();
        for (name, artifact) in artifacts {
            let Some(file) = artifact.linker_file.as_ref().or(artifact.file.as_ref()) else {
                continue;
            };
            let file = self.node(file);
            let filename = self.node(Lexical::new(&file).filename());
            if name == &filename.as_slice()
                || self.names.contains_key(&filename)
                || self.producers.contains_key(&filename)
            {
                continue;
            }
            if self
                .extra_aliases
                .insert(filename.clone(), vec![file])
                .is_some()
            {
                ambiguous.insert(filename);
            }
        }
        for name in ambiguous {
            self.extra_aliases.remove(&name);
        }
        let Some(model) = model else { return };
        for build_target in &self.input.targets {
            let Some(target) = model
                .find_target(DirId::ROOT, &build_target.name)
                .and_then(|id| model.target(id))
            else {
                continue;
            };
            for step in build_target
                .steps
                .iter()
                .filter_map(|index| self.input.steps.get(*index))
            {
                let (property, output, source) = match step {
                    BuildStep::Compile { source, object, .. } => {
                        (b"JOB_POOL_COMPILE".as_slice(), object, Some(source))
                    }
                    BuildStep::Link { step, .. } => {
                        (b"JOB_POOL_LINK".as_slice(), &step.output, None)
                    }
                    BuildStep::DeviceLink { output, .. } => {
                        (b"JOB_POOL_LINK".as_slice(), output, None)
                    }
                    _ => continue,
                };
                let dir = model.dir(target.dir);
                let source = source.map(|source| {
                    let absolute = Lexical::new(&dir.source_dir).append(source);
                    Lexical::new(&absolute).normalize()
                });
                let file_set = source.as_ref().and_then(|source| {
                    target
                        .file_sets
                        .iter()
                        .filter_map(|id| model.file_set(*id))
                        .find(|set| set.files.iter().any(|entry| entry.value == *source))
                });
                let pool = file_set
                    .and_then(|set| set.props.get(property))
                    .or_else(|| {
                        source
                            .as_ref()
                            .and_then(|source| dir.source_props.get(source)?.get(property))
                    })
                    .or_else(|| target.props.get(property));
                if let Some(pool) = pool.filter(|value| !value.is_empty()) {
                    self.pools.insert(output.clone(), pool.clone());
                }
            }
        }
        let directories: BTreeMap<_, _> = model
            .dirs()
            .filter(|(id, _)| *id != DirId::ROOT)
            .map(|(id, dir)| {
                let directory = self.node(&dir.binary_dir);
                (id, Lexical::new(&directory).append(b"all"))
            })
            .collect();
        let mut dir_all: BTreeMap<DirId, Vec<BString>> =
            model.dirs().map(|(id, _)| (id, Vec::new())).collect();
        let excluded_targets: HashSet<_> = result
            .targets
            .iter()
            .filter(|target| target.config == self.input.config && target.metadata.exclude_from_all)
            .map(|target| target.metadata.name.as_slice())
            .collect();
        for (index, build_target) in self.input.targets.iter().enumerate() {
            let Some(target) = self
                .input
                .target_ids
                .get(index)
                .and_then(|id| model.target(*id))
                .or_else(|| {
                    model
                        .find_target(DirId::ROOT, &build_target.name)
                        .and_then(|id| model.target(id))
                })
            else {
                if build_target.in_all
                    && let Some(inputs) = dir_all.get_mut(&DirId::ROOT)
                    && let Some(meta) = self.target_meta.get(index)
                {
                    inputs.extend(meta.artifact_outputs.iter().cloned());
                }
                continue;
            };
            let excluded_prop = target.props.get(b"EXCLUDE_FROM_ALL");
            let in_dir_all = if model.dir_excluded_from_all(target.dir) {
                !build_target.steps.is_empty()
                    && target.kind != TargetKind::Utility
                    && !excluded_prop.is_some_and(|value| {
                        excluded_targets.contains(build_target.name.as_slice())
                            || truth::is_on(value)
                    })
            } else {
                build_target.in_all
                    && !excluded_prop.is_some_and(|value| {
                        excluded_targets.contains(build_target.name.as_slice())
                            || truth::is_on(value)
                    })
            };
            if !in_dir_all {
                continue;
            }
            if let Some(inputs) = dir_all.get_mut(&target.dir)
                && let Some(meta) = self.target_meta.get(index)
            {
                inputs.extend(meta.artifact_outputs.iter().cloned());
            }
        }
        for (id, dir) in model.dirs() {
            for &child in &dir.children {
                if !model.dir(child).exclude_from_all
                    && let Some(child_all) = directories.get(&child)
                    && let Some(inputs) = dir_all.get_mut(&id)
                {
                    inputs.push(qualified(child_all, self.config));
                }
            }
        }
        for (id, name) in &directories {
            let inputs = dir_all.remove(id).unwrap_or_default();
            self.extra_aliases.insert(name.clone(), unique_vec(inputs));
        }
        self.root_all_inputs = Some(unique_vec(dir_all.remove(&DirId::ROOT).unwrap_or_default()));
    }

    pub(crate) fn prep_output_is_reserved(&self, name: &BString) -> bool {
        self.has_target(name) || self.aliases.contains(name)
    }

    pub(crate) fn file_dependency(&self, path: &[u8]) -> Option<BString> {
        let node = self.node(path);
        self.producers.contains_key(&node).then_some(node)
    }

    pub(crate) fn install_file_dependencies(&self, path: &[u8]) -> Vec<BString> {
        let node = self.node(path);
        if let Some(&index) = self.artifact_to_target.get(&node)
            && let Some(meta) = self.target_meta.get(index)
        {
            return meta.artifact_outputs.clone();
        }
        self.producers
            .contains_key(&node)
            .then_some(node)
            .into_iter()
            .collect()
    }

    pub(crate) fn target_dependencies(&self, name: &[u8]) -> Vec<BString> {
        let Some(&index) = self.names.get(&self.node(name)) else {
            return Vec::new();
        };
        self.target_meta
            .get(index)
            .map(|meta| meta.artifact_outputs.clone())
            .unwrap_or_default()
    }

    pub(crate) fn resolve_custom_inputs(&mut self, model: Option<&ProjectModel>, host: &dyn Host) {
        let Some(model) = model else {
            return;
        };
        let mut directories: Vec<_> = model.dirs().map(|(_, directory)| directory).collect();
        directories.sort_by_key(|dir| std::cmp::Reverse(dir.source_dir.len()));
        for dir in directories {
            let source_dir = path::to_native(&dir.source_dir);
            let binary_dir = path::to_native(&dir.binary_dir);
            for source in dir.source_props.keys() {
                let source_path = path::to_native(source);
                let Ok(relative) = source_path.strip_prefix(&source_dir) else {
                    continue;
                };
                let input = self.node(source);
                let output = self.node(&path::from_native(&binary_dir.join(relative)));
                if !self.producers.contains_key(&input)
                    && self.producers.get(&output).is_some_and(|&index| {
                        matches!(self.input.steps.get(index), Some(BuildStep::Custom(_)))
                    })
                    && host.metadata(&source_path, Follow::Yes).is_err()
                {
                    self.generated_inputs.entry(input).or_insert(output);
                }
            }
        }
    }

    pub(crate) fn manifest(&self) -> Ninja {
        // Allow space for edge syntax and commands in addition to the alias paths.
        let capacity = self
            .input
            .steps
            .len()
            .saturating_mul(256)
            .saturating_add(self.order_only_alias_bytes);
        Ninja(Vec::with_capacity(capacity))
    }

    pub(crate) fn set_compile_metadata(&mut self, result: &'a GenerationResult) {
        self.compile_languages = progress::compile_languages(result, self.input);
        self.launcher_slots = crate::instrumented::collect_launchers(result).1;
        self.command_settings.result = Some(result);
        self.fortran_info.extend(
            result
                .targets
                .iter()
                .filter(|target| target.config == self.input.config)
                .filter_map(|target| Some((target, target.fortran.as_ref()?)))
                .flat_map(|(target, info)| {
                    target
                        .compile_units
                        .iter()
                        .map(move |unit| (unit.object.as_slice(), info))
                }),
        );

        self.compile_inputs = Some(&result.compile_inputs);
        self.scan_responses.extend(
            result
                .targets
                .iter()
                .filter(|target| target.config == self.input.config)
                .filter_map(|target| target.cxx_modules.as_ref())
                .flat_map(|modules| modules.scan_responses.iter())
                .map(|(file, response)| (file.as_slice(), response)),
        );
    }

    pub(crate) fn emit(
        &self,
        ninja: &mut Ninja,
        cmake: &[u8],
        modmap_flag: &[u8],
        exports: &[ModuleExportJob],
    ) -> Result<(), GenError> {
        let owners = CustomOwners::new(std::iter::once(self));
        let inputs = self.inherited_custom_inputs(self, &owners, false);
        self.emit_into(ninja, cmake, modmap_flag, exports, None, &inputs)
    }

    pub(crate) fn emit_into(
        &self,
        ninja: &mut Ninja,
        cmake: &[u8],
        modmap_flag: &[u8],
        exports: &[ModuleExportJob],
        emitted: Option<&mut HashSet<BString>>,
        custom_inputs: &BTreeMap<usize, Vec<BString>>,
    ) -> Result<(), GenError> {
        self.emit_into_with_workers(
            ninja,
            cmake,
            modmap_flag,
            exports,
            emitted,
            custom_inputs,
            None,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn emit_into_with_workers(
        &self,
        ninja: &mut Ninja,
        cmake: &[u8],
        modmap_flag: &[u8],
        exports: &[ModuleExportJob],
        mut emitted: Option<&mut HashSet<BString>>,
        custom_inputs: &BTreeMap<usize, Vec<BString>>,
        workers_override: Option<usize>,
    ) -> Result<(), GenError> {
        let mut planned_steps = Vec::with_capacity(self.input.steps.len());
        for (index, (step, nodes)) in self.input.steps.iter().zip(&self.steps).enumerate() {
            if self.input.native_custom_steps.contains(&index) || self.is_orphan_custom_step(index)
            {
                continue;
            }
            if let Some(emitted) = &mut emitted {
                if nodes.outputs.iter().any(|node| emitted.contains(node)) {
                    continue;
                }
                let duplicate = nodes.byproducts.iter().any(|node| emitted.contains(node));
                if duplicate {
                    if !nodes.link {
                        continue;
                    }
                    let mut unique = nodes.clone();
                    unique.byproducts.retain(|node| !emitted.contains(node));
                    emitted.extend(unique.outputs.iter().chain(&unique.byproducts).cloned());
                    planned_steps.push((index, step, PlannedStepNodes::Owned(unique)));
                    continue;
                }
                emitted.extend(nodes.outputs.iter().chain(&nodes.byproducts).cloned());
                if let Some(sym) = self.exe_symlinks.get(&index) {
                    emitted.insert(sym.symlink_output.clone());
                }
            }
            planned_steps.push((index, step, PlannedStepNodes::Borrowed(nodes)));
        }

        for meta in &self.target_meta {
            if let Some(order_target) = &meta.order_depends_target
                && emitted
                    .as_mut()
                    .is_none_or(|nodes| nodes.insert(order_target.clone()))
            {
                let order_only: Vec<&BString> = meta.order_depends_inputs.iter().collect();
                ninja.edge(
                    b"phony",
                    Edge {
                        outputs: std::slice::from_ref(order_target),
                        order_only: &order_only,
                        ..Edge::default()
                    },
                );
            }
            if let Some(private_target) = &meta.order_depends_private
                && emitted
                    .as_mut()
                    .is_none_or(|nodes| nodes.insert(private_target.clone()))
            {
                let order_only: Vec<&BString> = meta.order_depends_private_inputs.iter().collect();
                ninja.edge(
                    b"phony",
                    Edge {
                        outputs: std::slice::from_ref(private_target),
                        order_only: &order_only,
                        ..Edge::default()
                    },
                );
            }
        }

        let step_workers = workers_override.unwrap_or_else(|| {
            render_worker_count(
                planned_steps.len(),
                PARALLEL_STEP_THRESHOLD,
                MAX_RENDER_WORKERS,
            )
        });
        #[cfg(not(target_arch = "wasm32"))]
        if step_workers > 1 && planned_steps.len() > 1 {
            let rendered = map_render_chunks(&planned_steps, step_workers, |chunk| {
                let mut local = Ninja(Vec::with_capacity(chunk.len().saturating_mul(256)));
                for &(index, step, ref nodes) in chunk {
                    self.emit_step(
                        &mut local,
                        index,
                        step,
                        nodes.as_ref(),
                        cmake,
                        modmap_flag,
                        exports,
                        custom_inputs.get(&index),
                    )?;
                }
                Ok::<Ninja, GenError>(local)
            });
            for slot in rendered {
                let local = slot?;
                ninja.0.extend_from_slice(&local.0);
            }
        } else {
            for &(index, step, ref nodes) in &planned_steps {
                self.emit_step(
                    ninja,
                    index,
                    step,
                    nodes.as_ref(),
                    cmake,
                    modmap_flag,
                    exports,
                    custom_inputs.get(&index),
                )?;
            }
        }
        #[cfg(target_arch = "wasm32")]
        {
            let _ = step_workers;
            for &(index, step, ref nodes) in &planned_steps {
                self.emit_step(
                    ninja,
                    index,
                    step,
                    nodes.as_ref(),
                    cmake,
                    modmap_flag,
                    exports,
                    custom_inputs.get(&index),
                )?;
            }
        }

        let mut all = Vec::new();
        let mut phony_targets = Vec::new();
        for (index, (target, alias)) in self.input.targets.iter().zip(&self.aliases).enumerate() {
            if !self.producers.contains_key(alias)
                && emitted
                    .as_mut()
                    .is_none_or(|nodes| nodes.insert(alias.clone()))
            {
                phony_targets.push((index, target, alias));
            }
            if target.in_all
                && self.root_all_inputs.is_none()
                && let Some(meta) = self.target_meta.get(index)
            {
                all.extend(meta.artifact_outputs.iter().cloned());
            }
        }
        let emit_phony_target = |out: &mut Ninja,
                                 target_index: usize,
                                 target: &rcmake_resolve::GraphTarget,
                                 alias: &BString| {
            let Some(meta) = self.target_meta.get(target_index) else {
                return;
            };
            let inputs: Vec<BString> = match meta.kind {
                TargetKind::Executable
                | TargetKind::SharedLibrary
                | TargetKind::StaticLibrary
                | TargetKind::ModuleLibrary
                | TargetKind::UnknownLibrary => meta.artifact_outputs.clone(),
                TargetKind::ObjectLibrary => target
                    .steps
                    .iter()
                    .filter(|&&idx| {
                        self.input.swift_actions.contains_key(&idx)
                            || matches!(self.input.steps.get(idx), Some(BuildStep::Compile { .. }))
                    })
                    .filter_map(|&idx| self.steps.get(idx))
                    .flat_map(|nodes| nodes.outputs.iter().cloned())
                    .collect(),
                TargetKind::Utility | TargetKind::InterfaceLibrary | TargetKind::GlobalTarget => {
                    let utilities = &self.input.utility_steps;
                    let util_outputs: Vec<BString> = target
                        .steps
                        .iter()
                        .filter(|&&index| utilities.contains(&index))
                        .filter_map(|&index| self.steps.get(index))
                        .flat_map(|nodes| nodes.outputs.iter())
                        .filter(|output| output.ends_with(b".util"))
                        .cloned()
                        .collect();
                    if !util_outputs.is_empty() {
                        util_outputs
                    } else {
                        let utility_first = |index: &&usize| utilities.contains(*index);
                        target
                            .steps
                            .iter()
                            .filter(utility_first)
                            .chain(target.steps.iter().filter(|index| {
                                !utility_first(index)
                                    && self.step_belongs_to_target_dir(target_index, **index)
                            }))
                            .filter_map(|&index| self.steps.get(index))
                            .flat_map(|nodes| nodes.outputs.iter().chain(&nodes.byproducts))
                            .cloned()
                            .chain(target.depends.iter().flat_map(|&dep| {
                                self.target_meta
                                    .get(dep)
                                    .map(|m| m.artifact_outputs.clone())
                                    .unwrap_or_default()
                            }))
                            .collect()
                    }
                }
            };
            out.phony(alias, &unique_vec(inputs));
        };
        let target_workers = workers_override.unwrap_or_else(|| {
            render_worker_count(
                phony_targets.len(),
                PARALLEL_TARGET_THRESHOLD,
                MAX_RENDER_WORKERS,
            )
        });
        #[cfg(not(target_arch = "wasm32"))]
        if target_workers > 1 && phony_targets.len() > 1 {
            let rendered = map_render_chunks(&phony_targets, target_workers, |chunk| {
                let mut local = Ninja(Vec::with_capacity(chunk.len().saturating_mul(128)));
                for &(index, target, alias) in chunk {
                    emit_phony_target(&mut local, index, target, alias);
                }
                local
            });
            for local in rendered {
                ninja.0.extend_from_slice(&local.0);
            }
        } else {
            for &(index, target, alias) in &phony_targets {
                emit_phony_target(ninja, index, target, alias);
            }
        }
        #[cfg(target_arch = "wasm32")]
        {
            let _ = target_workers;
            for &(index, target, alias) in &phony_targets {
                emit_phony_target(ninja, index, target, alias);
            }
        }
        let all_alias = qualified(&self.node(b"all"), self.config);
        if let Some(nodes) = &mut emitted {
            nodes.insert(all_alias.clone());
        }
        let all_inputs = self
            .root_all_inputs
            .clone()
            .unwrap_or_else(|| unique_vec(all));
        ninja.phony(&all_alias, &all_inputs);
        for (name, inputs) in &self.extra_aliases {
            let alias = qualified(name, self.config);
            if !self.producers.contains_key(&alias)
                && emitted
                    .as_mut()
                    .is_none_or(|nodes| nodes.insert(alias.clone()))
            {
                ninja.phony(&alias, inputs);
            }
        }
        Ok(())
    }

    pub(crate) fn emit_clean_inputs(&self, ninja: &mut Ninja) {
        let inputs = self
            .steps
            .iter()
            .enumerate()
            .filter(|(index, _)| !self.is_orphan_custom_step(*index))
            .flat_map(|(_, nodes)| nodes.outputs.iter().chain(&nodes.byproducts))
            .chain(self.exe_symlinks.values().map(|sym| &sym.symlink_output))
            .cloned()
            .collect::<Vec<_>>();
        ninja.phony(
            &qualified(&self.node(b"CMakeFiles/rcmake-clean"), self.config),
            &inputs,
        );
    }

    /// Return the absolute path of a node in the build tree. Nodes outside the
    /// build tree, and nodes under an output path prefix, have no alias.
    fn absolute_alias(&self, node: &[u8]) -> Option<BString> {
        if !self.prefix.is_empty() || node.is_empty() || self.commands.absolute(node) {
            return None;
        }
        let mut alias = self.build_root.clone();
        if !alias.ends_with(b"/") {
            alias.push(b'/');
        }
        alias.extend_from_slice(node);
        Some(alias)
    }

    fn custom_input(&self, input: &[u8]) -> CustomInput {
        let node = self.node(input);
        let file = |node: BString| CustomInput {
            node,
            target_only: false,
        };
        if let Some(output) = self.generated_inputs.get(&node) {
            return file(output.clone());
        }
        let Some(&index) = self.names.get(&node) else {
            return file(node);
        };
        // The resolver converts target dependencies with artifacts to file paths.
        // Bare target names can still refer to targets without artifacts or to a
        // library target whose name collides with another target's output file.
        if let Some(producer) = self.producers.get(&node) {
            let owned = self
                .input
                .targets
                .get(index)
                .is_some_and(|target| target.steps.contains(producer));
            if input != node.as_slice() || owned || self.artifact_targets.contains(&index) {
                return file(node);
            }
            if let Some(artifact) = self
                .target_meta
                .get(index)
                .and_then(|meta| meta.artifact_outputs.first())
                .filter(|artifact| *artifact != &node)
            {
                return file(artifact.clone());
            }
        }
        if !self.artifact_targets.contains(&index)
            && let Some(alias) = self.aliases.get(index)
            && self.producers.get(alias).is_some_and(|producer| {
                !self
                    .input
                    .targets
                    .get(index)
                    .is_some_and(|target| target.steps.contains(producer))
            })
            && let Some(artifact) = self
                .target_meta
                .get(index)
                .and_then(|meta| meta.artifact_outputs.first())
        {
            return file(artifact.clone());
        }
        CustomInput {
            node: self.aliases.get(index).cloned().unwrap_or(node),
            target_only: true,
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn emit_step(
        &self,
        ninja: &mut Ninja,
        step_index: usize,
        step: &BuildStep,
        nodes: &StepNodes<'_>,
        cmake: &[u8],
        modmap_flag: &[u8],
        exports: &[ModuleExportJob],
        custom_inputs: Option<&Vec<BString>>,
    ) -> Result<(), GenError> {
        let mut language_inputs = Vec::new();
        let mut plan = match step {
            BuildStep::DeviceLink { .. } => None,
            BuildStep::Collate { lang, .. } if lang == "Fortran" => None,
            BuildStep::Scan { lang, object, .. } if lang == "Fortran" => {
                language_inputs.extend(
                    self.compile_inputs
                        .and_then(|inputs| inputs.get(object))
                        .into_iter()
                        .flatten()
                        .map(|path| self.node(path)),
                );
                None
            }
            BuildStep::Compile {
                object,
                dyndep: Some(_),
                modmap: None,
                ..
            } if self.compile_languages.get(object.as_slice()) == Some(&b"Fortran".as_slice()) => {
                language_inputs.extend(
                    self.compile_inputs
                        .and_then(|inputs| inputs.get(object))
                        .into_iter()
                        .flatten()
                        .map(|path| self.node(path)),
                );
                None
            }
            _ => Some(self.plan_step(step_index, step, nodes, cmake, modmap_flag, exports)?),
        };
        let inputs = plan.as_mut().map_or_else(Vec::new, |plan| {
            if matches!(step, BuildStep::Custom(_)) {
                std::mem::take(&mut plan.inputs)
            } else {
                unique_vec(std::mem::take(&mut plan.inputs))
            }
        });
        let implicit = plan.as_mut().map_or_else(
            || unique_vec(language_inputs),
            |plan| {
                if matches!(step, BuildStep::Link { .. }) {
                    std::mem::take(&mut plan.implicit_inputs)
                } else {
                    unique_vec(std::mem::take(&mut plan.implicit_inputs))
                }
            },
        );
        let dyndep = plan.as_ref().and_then(|plan| plan.dyndep.as_ref());
        let planned = plan
            .as_ref()
            .map_or(&[][..], |plan| plan.order_only.as_slice());
        let is_link_step = matches!(step, BuildStep::Link { .. }) || nodes.link;
        let is_utility_step = self.input.utility_steps.contains(&step_index);
        let mut order_only: Vec<&BString> = Vec::new();
        if is_link_step {
            let self_outputs: HashSet<&BString> =
                nodes.outputs.iter().chain(&nodes.byproducts).collect();
            let mut part1_seen = self_outputs.clone();
            let mut dep_artifacts: Vec<&BString> = Vec::new();
            for entry in &nodes.order_only {
                match entry {
                    OrderOnly::Targets(indices) => {
                        for &dep_index in indices.iter() {
                            if let Some(meta) = self.target_meta.get(dep_index) {
                                for output in &meta.artifact_outputs {
                                    if part1_seen.insert(output) {
                                        dep_artifacts.push(output);
                                    }
                                }
                            }
                        }
                    }
                    OrderOnly::TargetClosure(indices) => {
                        let mut visited = HashSet::new();
                        let mut stack: Vec<usize> = indices.iter().copied().collect();
                        while let Some(dep_index) = stack.pop() {
                            if !visited.insert(dep_index) {
                                continue;
                            }
                            if let Some(meta) = self.target_meta.get(dep_index) {
                                for output in &meta.artifact_outputs {
                                    if part1_seen.insert(output) {
                                        dep_artifacts.push(output);
                                    }
                                }
                            }
                            if let Some(dep_target) = self.input.targets.get(dep_index) {
                                stack.extend(dep_target.depends.iter().copied());
                            }
                        }
                    }
                    OrderOnly::StepOutputs(index) => {
                        if let Some(step_nodes) = self.steps.get(*index) {
                            for output in step_nodes.outputs.iter().chain(&step_nodes.byproducts) {
                                if part1_seen.insert(output) {
                                    dep_artifacts.push(output);
                                }
                            }
                        }
                    }
                    OrderOnly::ObjectOrder(_) => {}
                }
            }
            for output in planned {
                if part1_seen.insert(output) {
                    dep_artifacts.push(output);
                }
            }
            dep_artifacts.sort();
            order_only.extend(dep_artifacts);
            // Append versioning symlinks of linked non-framework shared libraries
            // on non-DLL platforms for runtime binaries, preserving duplicates across
            // the two parts (cmNinjaNormalTargetGenerator.cxx:1643-1658).
            if let BuildStep::Link {
                inputs: link_inputs,
                step: link_step,
                ..
            } = step
                && !self.commands.windows()
                && !self.target_is_windows()
                && matches!(
                    link_step.kind,
                    LinkKind::Executable | LinkKind::SharedLibrary | LinkKind::ModuleLibrary
                )
            {
                let mut seen_shared = HashSet::new();
                for dep_idx in self.linked_shared_targets(link_inputs, link_step) {
                    if !seen_shared.insert(dep_idx) {
                        continue;
                    }
                    if let Some(meta) = self.target_meta.get(dep_idx)
                        && meta.is_non_framework_shared
                        && let Some(lib) = &meta.linker_path
                        && !implicit.contains(lib)
                        && !self_outputs.contains(lib)
                    {
                        order_only.push(lib);
                    }
                }
            }
        } else if dyndep.is_some()
            || !nodes.order_only.is_empty()
            || !planned.is_empty()
            || custom_inputs.is_some()
        {
            let mut seen: HashSet<&BString> =
                if is_utility_step || matches!(step, BuildStep::Custom(_)) {
                    nodes.outputs.iter().chain(&nodes.byproducts).collect()
                } else {
                    inputs
                        .iter()
                        .chain(&implicit)
                        .chain(&nodes.outputs)
                        .chain(&nodes.byproducts)
                        .collect()
                };
            order_only.extend(dyndep.into_iter().filter(|node| seen.insert(node)));
            order_only.extend(planned.iter().filter(|node| seen.insert(node)));
            order_only.extend(
                custom_inputs
                    .into_iter()
                    .flatten()
                    .filter(|node| seen.insert(node)),
            );
            for entry in &nodes.order_only {
                match entry {
                    OrderOnly::Targets(indices) => {
                        let mut dep_artifacts: Vec<&BString> = Vec::new();
                        for &dep_index in indices.iter() {
                            if let Some(meta) = self.target_meta.get(dep_index) {
                                for output in &meta.artifact_outputs {
                                    if seen.insert(output) {
                                        dep_artifacts.push(output);
                                    }
                                }
                            }
                        }
                        dep_artifacts.sort();
                        order_only.extend(dep_artifacts);
                    }
                    OrderOnly::TargetClosure(indices) => {
                        let mut dep_artifacts: Vec<&BString> = Vec::new();
                        let mut visited = HashSet::new();
                        let mut stack: Vec<usize> = indices.iter().copied().collect();
                        while let Some(dep_index) = stack.pop() {
                            if !visited.insert(dep_index) {
                                continue;
                            }
                            if let Some(meta) = self.target_meta.get(dep_index) {
                                for output in &meta.artifact_outputs {
                                    if seen.insert(output) {
                                        dep_artifacts.push(output);
                                    }
                                }
                            }
                            if let Some(dep_target) = self.input.targets.get(dep_index) {
                                stack.extend(dep_target.depends.iter().copied());
                            }
                        }
                        dep_artifacts.sort();
                        order_only.extend(dep_artifacts);
                    }
                    OrderOnly::StepOutputs(index) => order_only.extend(
                        self.steps
                            .get(*index)
                            .into_iter()
                            .flat_map(|step| step.outputs.iter().chain(&step.byproducts))
                            .filter(|node| seen.insert(node)),
                    ),
                    OrderOnly::ObjectOrder(target_index) => {
                        if let Some(meta) = self.target_meta.get(*target_index)
                            && let Some(order_target) = meta
                                .order_depends_private
                                .as_ref()
                                .or(meta.order_depends_target.as_ref())
                            && seen.insert(order_target)
                        {
                            order_only.push(order_target);
                        }
                    }
                }
            }
        }
        let Some(plan) = plan.as_ref() else {
            match step {
                BuildStep::DeviceLink { .. } => {
                    crate::cuda::emit(ninja, self, step, &[], &order_only);
                }
                BuildStep::Collate { .. } => {
                    let raw = self
                        .command_settings
                        .result
                        .and_then(|result| {
                            result
                                .metadata
                                .cmake_command
                                .as_ref()
                                .map(|path| path.as_slice())
                        })
                        .filter(|path| !path.is_empty())
                        .unwrap_or(b"cmake");
                    crate::fortran::emit_collate(ninja, self, step, raw);
                }
                BuildStep::Scan { object, .. } => {
                    let info = self.fortran_info.get(object.as_slice()).ok_or_else(|| {
                        generation_error(self.build_dir, "Missing Fortran dependency information.")
                    })?;
                    crate::fortran::emit_scan(ninja, self, step, info, &implicit, &order_only);
                }
                BuildStep::Compile { .. } => {
                    crate::fortran::emit_compile(ninja, self, step, &implicit, &order_only);
                }
                _ => {
                    return Err(generation_error(
                        self.build_dir,
                        "Invalid language build step.",
                    ));
                }
            }
            return Ok(());
        };
        ninja.edge(
            &plan.rule,
            Edge {
                outputs: &nodes.outputs,
                implicit_outputs: &plan.implicit_outputs,
                inputs: &inputs,
                implicit_inputs: &implicit,
                order_only: &order_only,
            },
        );

        for (key, value) in &plan.bindings {
            ninja.binding(key, value);
        }

        if let Some(sym) = self.exe_symlinks.get(&step_index)
            && let BuildStep::Link { invocations, .. } = step
        {
            let sym_invocations = invocations.get(sym.split_at..).unwrap_or_default();
            let command = self.commands.symlink_executable_line(
                sym_invocations,
                &path::from_native(self.build_dir),
                !self.prefix.is_empty(),
            );
            ninja.edge(
                b"CMAKE_SYMLINK_EXECUTABLE",
                Edge {
                    outputs: std::slice::from_ref(&sym.symlink_output),
                    inputs: std::slice::from_ref(&sym.real_output),
                    ..Edge::default()
                },
            );
            ninja.binding(b"COMMAND", &command);
            let mut desc = b"Creating executable symlink ".to_vec();
            desc.extend_from_slice(&sym.symlink_output);
            ninja.binding(b"DESC", &desc);
        }
        Ok(())
    }

    pub(super) fn step_link_depends_no_shared(&self, step_index: usize) -> bool {
        self.step_owner
            .get(&step_index)
            .and_then(|&owner| self.target_meta.get(owner))
            .is_some_and(|meta| meta.link_depends_no_shared)
    }

    pub(super) fn is_shared_library_input(&self, node: &BString) -> bool {
        if let Some(&target_idx) = self.artifact_to_target.get(node) {
            return self
                .target_meta
                .get(target_idx)
                .is_some_and(|meta| meta.kind == TargetKind::SharedLibrary);
        }
        if let Some(&producer) = self.producers.get(node) {
            return matches!(
                self.input.steps.get(producer),
                Some(BuildStep::Link { step, .. }) if step.kind == LinkKind::SharedLibrary
            );
        }
        self.imported_shared_artifacts.contains(node)
    }

    pub(super) fn is_link_library_input(&self, node: &BString, step: &LinkStep) -> bool {
        if let Some(&target_idx) = self.artifact_to_target.get(node)
            && matches!(
                self.target_meta.get(target_idx).map(|m| m.kind),
                Some(
                    TargetKind::StaticLibrary
                        | TargetKind::SharedLibrary
                        | TargetKind::ModuleLibrary
                )
            )
        {
            return true;
        }
        if let Some(&producer) = self.producers.get(node) {
            if matches!(
                self.input.steps.get(producer),
                Some(BuildStep::Compile { .. } | BuildStep::DeviceLink { .. })
            ) {
                return false;
            }
            if let Some(BuildStep::Link { step: dep_step, .. }) = self.input.steps.get(producer)
                && matches!(
                    dep_step.kind,
                    LinkKind::StaticArchive | LinkKind::SharedLibrary | LinkKind::ModuleLibrary
                )
            {
                return true;
            }
        }
        if step.fragments.iter().any(|frag| {
            frag.role == LinkFragmentRole::Libraries && self.node(&frag.text.value) == *node
        }) {
            return true;
        }
        looks_like_library_file(node)
    }

    fn linked_shared_targets(&self, link_inputs: &[BString], step: &LinkStep) -> Vec<usize> {
        let mut out = Vec::new();
        for input in link_inputs {
            let node = self.node(input);
            if let Some(&idx) = self.artifact_to_target.get(&node) {
                out.push(idx);
            }
        }
        for frag in &step.fragments {
            if frag.role != LinkFragmentRole::Libraries {
                continue;
            }
            let node = self.node(&frag.text.value);
            if let Some(&idx) = self.artifact_to_target.get(&node) {
                out.push(idx);
            }
        }
        out
    }

    fn target_is_windows(&self) -> bool {
        self.command_settings
            .model
            .and_then(|model| model.definition(DirId::ROOT, b"CMAKE_SYSTEM_NAME"))
            .is_some_and(|sys| sys.eq_ignore_ascii_case(b"Windows"))
    }
}

fn looks_like_library_file(path: &[u8]) -> bool {
    let filename = Lexical::new(path).filename();
    if filename.ends_with(b".a")
        || filename.ends_with(b".lib")
        || filename.ends_with(b".dylib")
        || filename.ends_with(b".tbd")
        || filename.ends_with(b".dll")
        || filename.ends_with(b".so")
    {
        return true;
    }
    filename.windows(4).any(|w| w == b".so.")
}

/// A custom command input after target names map to Ninja nodes.
struct CustomInput {
    node: BString,
    /// The input names a target without a file, so it only orders the command.
    target_only: bool,
}

fn is_library_symlink(custom: &rcmake_resolve::CustomStep) -> bool {
    custom.invocations.first().is_some_and(|invocation| {
        invocation
            .argv
            .get(2)
            .is_some_and(|word| word == b"cmake_symlink_library")
    })
}

fn is_executable_symlink(custom: &rcmake_resolve::CustomStep) -> bool {
    custom.invocations.first().is_some_and(is_exe_symlink_call)
}

fn is_exe_symlink_call(invocation: &rcmake_resolve::Invocation) -> bool {
    invocation
        .argv
        .get(2)
        .is_some_and(|word| word == b"cmake_symlink_executable")
}

pub(crate) fn qualified(name: &BString, config: Option<&[u8]>) -> BString {
    match config {
        Some(config) => [name.as_slice(), b":", config].concat().into(),
        None => name.clone(),
    }
}

fn unique_vec(nodes: Vec<BString>) -> Vec<BString> {
    if nodes.len() <= 1 {
        return nodes;
    }
    let mut seen = HashSet::with_capacity(nodes.len());
    nodes
        .into_iter()
        .filter(|node| seen.insert(node.clone()))
        .collect()
}
