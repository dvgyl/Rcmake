//! Build edges and target aliases for one configuration.

use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::Path;

use rcmake_base::{BString, path_lex::Lexical, truth};
use rcmake_gen_common::GenError;
use rcmake_gen_common::export::ModuleExportJob;
use rcmake_host::{Follow, Host, path};
use rcmake_model::{DirId, ProjectModel};
use rcmake_resolve::{
    ArtifactSet, BuildGraph, BuildStep, GenerationResult, ScanResponse, progress,
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
    compile_inputs: Option<&'a std::collections::BTreeMap<BString, Vec<BString>>>,
    aliases: Vec<BString>,
    extra_aliases: BTreeMap<BString, Vec<BString>>,
    artifact_targets: HashSet<usize>,
    producers: HashMap<BString, usize>,
    generated_inputs: HashMap<BString, BString>,
    pub(crate) pools: HashMap<BString, BString>,
    steps: Vec<StepNodes<'a>>,
    order_only_alias_bytes: usize,
    compile_prerequisites: Vec<Vec<order::Prerequisite>>,
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
    StepOutputs(usize),
    CompilePrerequisites(usize),
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

    pub(crate) fn with_prefix(
        input: &'a BuildGraph,
        artifacts: &BTreeMap<&[u8], &ArtifactSet>,
        build_dir: &'a Path,
        config: Option<&'a [u8]>,
        prefix: &'a [u8],
        commands: Commands,
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
            compile_inputs: None,
            aliases: Vec::with_capacity(input.targets.len()),
            extra_aliases: BTreeMap::new(),
            artifact_targets: HashSet::new(),
            producers: HashMap::with_capacity(input.steps.len()),
            generated_inputs: HashMap::new(),
            pools: HashMap::new(),
            steps: Vec::with_capacity(input.steps.len()),
            order_only_alias_bytes: 0,
            compile_prerequisites: Vec::new(),
        };
        for (index, target) in input.targets.iter().enumerate() {
            let name = graph.node(&target.name);
            if name.is_empty() || name == "all" || graph.names.insert(name.clone(), index).is_some()
            {
                return Err(generation_error(
                    build_dir,
                    "Invalid or duplicate Ninja target name.",
                ));
            }
            graph.aliases.push(qualified(&name, config));
            if artifacts
                .get(target.name.as_slice())
                .is_some_and(|artifacts| artifacts.file.is_some())
            {
                graph.artifact_targets.insert(index);
            }
        }
        for (index, step) in input.steps.iter().enumerate() {
            let mut nodes = StepNodes::default();
            match step {
                BuildStep::Compile {
                    object, byproducts, ..
                } => {
                    nodes.outputs.push(graph.node(object));
                    nodes
                        .byproducts
                        .extend(byproducts.iter().map(|output| graph.node(output)));
                }
                BuildStep::Link { step, .. } => nodes.outputs.push(graph.node(&step.output)),
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
                    nodes
                        .byproducts
                        .extend(custom.byproducts.iter().map(|name| graph.node(name)));
                    if nodes.outputs.is_empty() {
                        let name = BString::from(format!("CMakeFiles/rcmake-custom-{index}.util"));
                        nodes.outputs.push(qualified(&graph.node(&name), config));
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
        graph.disambiguate_aliases();
        graph.connect_targets()?;
        Ok(graph)
    }

    fn disambiguate_aliases(&mut self) {
        let mut reserved: HashSet<_> = self.names.keys().chain(&self.aliases).cloned().collect();
        for (index, (target, alias)) in self.input.targets.iter().zip(&mut self.aliases).enumerate()
        {
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
            if name == &filename.as_slice() || self.names.contains_key(&filename) {
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
        for (index, build_target) in self.input.targets.iter().enumerate() {
            let Some(target) = model
                .find_target(DirId::ROOT, &build_target.name)
                .and_then(|id| model.target(id))
            else {
                continue;
            };
            let directory = self.node(&model.dir(target.dir).binary_dir);
            if target.kind == rcmake_model::TargetKind::Utility && target.dir != DirId::ROOT {
                let name = Lexical::new(&directory).append(&build_target.name);
                let alias = qualified(&name, self.config);
                if !self.producers.contains_key(&alias)
                    && let Some(primary) = self.aliases.get_mut(index)
                {
                    *primary = alias;
                }
            }
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
        for name in directories.values() {
            self.extra_aliases.entry(name.clone()).or_default();
        }
        let excluded_targets: HashSet<_> = result
            .targets
            .iter()
            .filter(|target| target.config == self.input.config && target.metadata.exclude_from_all)
            .map(|target| target.metadata.name.as_slice())
            .collect();
        for (build_target, alias) in self.input.targets.iter().zip(&self.aliases) {
            let Some(target) = model
                .find_target(DirId::ROOT, &build_target.name)
                .and_then(|id| model.target(id))
            else {
                continue;
            };
            let excluded = target.props.get(b"EXCLUDE_FROM_ALL");
            if build_target.steps.is_empty()
                || excluded.is_some_and(|value| {
                    excluded_targets.contains(build_target.name.as_slice()) || truth::is_on(value)
                })
            {
                continue;
            }
            for ancestor in model.ancestors(target.dir) {
                if let Some(name) = directories.get(&ancestor)
                    && let Some(inputs) = self.extra_aliases.get_mut(name)
                {
                    inputs.push(alias.clone());
                }
                if excluded.is_none() && model.dir(ancestor).exclude_from_all {
                    break;
                }
            }
        }
    }

    pub(crate) fn prep_output_is_reserved(&self, name: &BString) -> bool {
        self.has_target(name) || self.aliases.contains(name)
    }

    pub(crate) fn file_dependency(&self, path: &[u8]) -> Option<BString> {
        let node = self.node(path);
        self.producers.contains_key(&node).then_some(node)
    }

    pub(crate) fn target_dependency(&self, name: &[u8]) -> Option<BString> {
        let index = self.names.get(&self.node(name))?;
        self.aliases.get(*index).cloned()
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
        mut emitted: Option<&mut HashSet<BString>>,
        custom_inputs: &BTreeMap<usize, Vec<BString>>,
    ) -> Result<(), GenError> {
        for (index, (step, nodes)) in self.input.steps.iter().zip(&self.steps).enumerate() {
            if self.input.native_custom_steps.contains(&index) {
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
                    self.emit_step(
                        ninja,
                        step,
                        &unique,
                        cmake,
                        modmap_flag,
                        exports,
                        custom_inputs.get(&index),
                    )?;
                    continue;
                }
                emitted.extend(nodes.outputs.iter().chain(&nodes.byproducts).cloned());
            }
            self.emit_step(
                ninja,
                step,
                nodes,
                cmake,
                modmap_flag,
                exports,
                custom_inputs.get(&index),
            )?;
        }

        let mut all = Vec::new();
        for (target, alias) in self.input.targets.iter().zip(&self.aliases) {
            if !self.producers.contains_key(alias)
                && emitted
                    .as_mut()
                    .is_none_or(|nodes| nodes.insert(alias.clone()))
            {
                let utilities = &self.input.utility_steps;
                let utility_first = |index: &&usize| utilities.contains(*index);
                let inputs: Vec<_> = target
                    .steps
                    .iter()
                    .filter(utility_first)
                    .chain(target.steps.iter().filter(|index| !utility_first(index)))
                    .filter_map(|&index| self.steps.get(index))
                    .flat_map(|nodes| nodes.outputs.iter().chain(&nodes.byproducts))
                    .cloned()
                    .chain(
                        target
                            .depends
                            .iter()
                            .filter_map(|&index| self.aliases.get(index).cloned()),
                    )
                    .collect();
                ninja.phony(alias, &unique(inputs));
            }
            if target.in_all {
                all.push(alias.clone());
            }
        }
        let all_alias = qualified(&self.node(b"all"), self.config);
        if let Some(nodes) = &mut emitted {
            nodes.insert(all_alias.clone());
        }
        ninja.phony(&all_alias, &all);
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
            .flat_map(|nodes| nodes.outputs.iter().chain(&nodes.byproducts))
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
        // Bare target names can still refer to targets without artifacts.
        if let Some(producer) = self.producers.get(&node)
            && (input != node.as_slice()
                || self.artifact_targets.contains(&index)
                || self
                    .input
                    .targets
                    .get(index)
                    .is_some_and(|target| target.steps.contains(producer)))
        {
            return file(node);
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
            _ => Some(self.plan_step(step, nodes, cmake, modmap_flag, exports)?),
        };
        let inputs = plan
            .as_mut()
            .map_or_else(Vec::new, |plan| unique(std::mem::take(&mut plan.inputs)));
        let implicit = plan.as_mut().map_or_else(
            || unique(language_inputs),
            |plan| unique(std::mem::take(&mut plan.implicit_inputs)),
        );
        let dyndep = plan.as_ref().and_then(|plan| plan.dyndep.as_ref());
        let planned = plan
            .as_ref()
            .map_or(&[][..], |plan| plan.order_only.as_slice());
        let mut order_only = Vec::new();
        if dyndep.is_some()
            || !nodes.order_only.is_empty()
            || !planned.is_empty()
            || custom_inputs.is_some()
        {
            let mut seen: HashSet<_> = inputs
                .iter()
                .chain(&implicit)
                .chain(&nodes.outputs)
                .chain(&nodes.byproducts)
                .collect();
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
                    OrderOnly::Targets(indices) => order_only.extend(
                        indices
                            .iter()
                            .filter_map(|&index| self.aliases.get(index))
                            .filter(|node| seen.insert(node)),
                    ),
                    OrderOnly::StepOutputs(index) => order_only.extend(
                        self.steps
                            .get(*index)
                            .into_iter()
                            .flat_map(|step| step.outputs.iter().chain(&step.byproducts))
                            .filter(|node| seen.insert(node)),
                    ),
                    OrderOnly::CompilePrerequisites(index) => order_only.extend(
                        self.compile_prerequisite_paths(*index)
                            .filter(|node| seen.insert(node)),
                    ),
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
            plan.rule,
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
        Ok(())
    }
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

pub(crate) fn qualified(name: &BString, config: Option<&[u8]>) -> BString {
    match config {
        Some(config) => [name.as_slice(), b":", config].concat().into(),
        None => name.clone(),
    }
}

fn unique(nodes: impl IntoIterator<Item = BString>) -> Vec<BString> {
    let mut seen = HashSet::new();
    nodes
        .into_iter()
        .filter(|node| seen.insert(node.clone()))
        .collect()
}
