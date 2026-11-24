//! Build edges and target aliases for one configuration.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use rcmake_base::{BString, path_lex::Lexical};
use rcmake_gen_common::GenError;
use rcmake_gen_common::export::ModuleExportJob;
use rcmake_host::{Follow, Host, path};
use rcmake_model::ProjectModel;
use rcmake_resolve::{
    ArtifactSet, BuildGraph, BuildStep, GenerationResult, Invocation, LinkKind, ScanResponse,
};

use crate::syntax::{Edge, Ninja, shell_word};
use crate::{generation_error, modules};

pub(crate) struct Graph<'a> {
    pub(crate) input: &'a BuildGraph,
    pub(crate) build_dir: &'a Path,
    pub(crate) config: Option<&'a [u8]>,
    names: HashMap<BString, usize>,
    scan_responses: HashMap<&'a [u8], &'a ScanResponse>,
    aliases: Vec<BString>,
    artifact_targets: HashSet<usize>,
    producers: HashMap<BString, usize>,
    generated_inputs: HashMap<BString, BString>,
    steps: Vec<StepNodes<'a>>,
    order_only_alias_bytes: usize,
}

#[derive(Default)]
struct StepNodes<'a> {
    outputs: Vec<BString>,
    byproducts: Vec<BString>,
    order_only: Vec<OrderOnly<'a>>,
}

enum OrderOnly<'a> {
    Targets(&'a [usize]),
    StepOutputs(usize),
}

impl<'a> Graph<'a> {
    pub(crate) fn new(
        input: &'a BuildGraph,
        artifacts: &HashMap<&[u8], &ArtifactSet>,
        build_dir: &'a Path,
        config: Option<&'a [u8]>,
    ) -> Result<Self, GenError> {
        let mut graph = Self {
            input,
            build_dir,
            config,
            names: HashMap::with_capacity(input.targets.len()),
            scan_responses: HashMap::new(),
            aliases: Vec::with_capacity(input.targets.len()),
            artifact_targets: HashSet::new(),
            producers: HashMap::with_capacity(input.steps.len()),
            generated_inputs: HashMap::new(),
            steps: Vec::with_capacity(input.steps.len()),
            order_only_alias_bytes: 0,
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
                BuildStep::Compile { object, .. } => nodes.outputs.push(graph.node(object)),
                BuildStep::Link { step, .. } => nodes.outputs.push(graph.node(&step.output)),
                BuildStep::Scan { ddi, .. } => nodes.outputs.push(graph.node(ddi)),
                BuildStep::Collate { dd, modmaps, .. } => {
                    nodes.outputs.push(graph.node(dd));
                    nodes
                        .byproducts
                        .push(graph.node(&modules::modules_json(dd)));
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
                        nodes.outputs.push(qualified(&name, config));
                    }
                }
            }
            graph.steps.push(nodes);
        }
        graph.add_link_byproducts(artifacts);
        for (index, nodes) in graph.steps.iter().enumerate() {
            for node in nodes.outputs.iter().chain(&nodes.byproducts) {
                if node.is_empty()
                    || node == "all"
                    || graph.producers.insert(node.clone(), index).is_some()
                {
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

    fn add_link_byproducts(&mut self, artifacts: &HashMap<&[u8], &ArtifactSet>) {
        for target in &self.input.targets {
            let Some(artifacts) = artifacts.get(target.name.as_slice()) else {
                continue;
            };
            let paths: Vec<_> = [
                &artifacts.soname_file,
                &artifacts.linker_file,
                &artifacts.import_file,
                &artifacts.pdb_file,
            ]
            .into_iter()
            .filter_map(Option::as_ref)
            .map(|path| self.node(path))
            .collect();
            for &index in &target.steps {
                if matches!(self.input.steps.get(index), Some(BuildStep::Link { .. }))
                    && let Some(nodes) = self.steps.get_mut(index)
                {
                    for path in &paths {
                        if !nodes.outputs.contains(path) && !nodes.byproducts.contains(path) {
                            nodes.byproducts.push(path.clone());
                        }
                    }
                }
            }
        }
    }

    pub(crate) fn node(&self, value: &[u8]) -> BString {
        let value = Lexical::new(value).normalize();
        let native = path::to_native(&value);
        match native.strip_prefix(self.build_dir) {
            Ok(relative) if relative.as_os_str().is_empty() => ".".into(),
            Ok(relative) => path::from_native(relative),
            Err(_) => value,
        }
    }

    pub(crate) fn has_target(&self, name: &BString) -> bool {
        self.names.contains_key(name) || self.producers.contains_key(name)
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

    fn connect_targets(&mut self) -> Result<(), GenError> {
        for (target, alias) in self.input.targets.iter().zip(&self.aliases) {
            let dependencies = target.depends.as_slice();
            let alias_bytes = dependencies
                .iter()
                .try_fold(0usize, |bytes, &index| {
                    self.aliases
                        .get(index)
                        .map(|name| bytes.saturating_add(name.len()).saturating_add(1))
                })
                .ok_or_else(|| {
                    generation_error(self.build_dir, "Invalid Ninja target dependency index.")
                })?;
            for &index in &target.steps {
                let Some(step) = self.input.steps.get(index) else {
                    return Err(generation_error(
                        self.build_dir,
                        "Invalid Ninja build step index.",
                    ));
                };
                if matches!(step, BuildStep::Compile { .. } | BuildStep::Scan { .. })
                    && let Some(nodes) = self.steps.get_mut(index)
                    && !dependencies.is_empty()
                {
                    nodes.order_only.push(OrderOnly::Targets(dependencies));
                    self.order_only_alias_bytes =
                        self.order_only_alias_bytes.saturating_add(alias_bytes);
                }
            }
            if let Some(&producer) = self.producers.get(alias)
                && let Some(nodes) = self.steps.get_mut(producer)
            {
                // A real output can also be the target name, such as a root executable.
                if !dependencies.is_empty() {
                    nodes.order_only.push(OrderOnly::Targets(dependencies));
                    self.order_only_alias_bytes =
                        self.order_only_alias_bytes.saturating_add(alias_bytes);
                }
                nodes.order_only.extend(
                    target
                        .steps
                        .iter()
                        .copied()
                        .filter(|&index| index != producer)
                        .map(OrderOnly::StepOutputs),
                );
            }
        }
        Ok(())
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

    pub(crate) fn set_scan_responses(&mut self, result: &'a GenerationResult) {
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
    ) {
        for (step, nodes) in self.input.steps.iter().zip(&self.steps) {
            self.emit_step(ninja, step, nodes, cmake, modmap_flag, exports);
        }

        let mut all = Vec::new();
        for (target, alias) in self.input.targets.iter().zip(&self.aliases) {
            if !self.producers.contains_key(alias) {
                let inputs: Vec<_> = target
                    .steps
                    .iter()
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
        ninja.phony(&qualified(&"all".into(), self.config), &all);
    }

    pub(crate) fn emit_entry_aliases(&self, ninja: &mut Ninja) {
        let aliases: HashSet<_> = self.aliases.iter().collect();
        for (target, alias) in self.input.targets.iter().zip(&self.aliases) {
            let name = self.node(&target.name);
            if !self.producers.contains_key(&name) && !aliases.contains(&name) {
                ninja.phony(&name, std::slice::from_ref(alias));
            }
        }
        ninja.phony(&"all".into(), &[qualified(&"all".into(), self.config)]);
    }

    fn custom_input(&self, input: &[u8]) -> BString {
        let node = self.node(input);
        if let Some(output) = self.generated_inputs.get(&node) {
            return output.clone();
        }
        let Some(&index) = self.names.get(&node) else {
            return node;
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
            return node;
        }
        self.aliases.get(index).cloned().unwrap_or(node)
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
    ) {
        let mut implicit = Vec::new();
        let mut implicit_outputs = nodes.byproducts.clone();
        let mut export_commands = Vec::new();
        let mut dyndep_node = None;
        let mut scan_response = None;
        let (rule, inputs, invocations, depfile) = match step {
            BuildStep::Compile {
                source,
                invocation,
                depfile,
                modmap,
                dyndep,
                ..
            } => {
                implicit.extend(modmap.iter().map(|map| self.node(map)));
                dyndep_node = dyndep.as_ref().map(|dd| self.node(dd));
                let rule: &[u8] = match (dyndep.is_some() || modmap.is_some(), depfile.is_some()) {
                    (false, false) => b"COMPILE",
                    (false, true) => b"COMPILE_DEPFILE",
                    (true, false) => b"CXX_COMPILE_SCANNED",
                    (true, true) => b"CXX_COMPILE_SCANNED_DEPFILE",
                };
                (
                    rule,
                    vec![self.node(source)],
                    std::slice::from_ref(invocation),
                    depfile.as_ref(),
                )
            }
            BuildStep::Scan {
                source,
                ddi,
                invocation,
                depfile,
                ..
            } => {
                scan_response = self
                    .scan_responses
                    .get(ddi.as_slice())
                    .map(|&response| (ddi, response));
                let rule: &[u8] = match (depfile.is_some(), scan_response.is_some()) {
                    (true, true) => b"CXX_SCAN_RSP",
                    (true, false) => b"CXX_SCAN",
                    (false, true) => b"CXX_SCAN_RSP_NO_DEPFILE",
                    (false, false) => b"CXX_SCAN_NO_DEPFILE",
                };
                (
                    rule,
                    vec![self.node(source)],
                    std::slice::from_ref(
                        scan_response.map_or(invocation, |(_, response)| &response.invocation),
                    ),
                    depfile.as_ref(),
                )
            }
            BuildStep::Collate {
                dd,
                ddis,
                tdi,
                implicit_inputs,
                depend_info,
                ..
            } => {
                implicit_outputs.extend(
                    depend_info
                        .bmi_installation
                        .iter()
                        .filter(|install| !install.script_location.is_empty())
                        .map(|install| self.node(&install.script_location)),
                );
                implicit.push(self.node(tdi));
                implicit.extend(implicit_inputs.iter().map(|input| self.node(input)));
                for export in exports.iter().filter(|export| export.dyndep == *dd) {
                    implicit.push(self.node(&export.script));
                    implicit_outputs.push(self.node(&export.output));
                    export_commands.extend_from_slice(b" && ");
                    export_commands.extend_from_slice(cmake);
                    export_commands.extend_from_slice(b" -P ");
                    shell_word(&self.node(&export.script), &mut export_commands);
                }
                (
                    b"CXX_DYNDEP".as_slice(),
                    ddis.iter().map(|ddi| self.node(ddi)).collect(),
                    &[][..],
                    None,
                )
            }
            BuildStep::Link {
                inputs,
                step,
                invocations,
            } => {
                let rule: &[u8] = match step.kind {
                    LinkKind::Executable => b"LINK_EXECUTABLE",
                    LinkKind::SharedLibrary => b"LINK_SHARED_LIBRARY",
                    LinkKind::ModuleLibrary => b"LINK_MODULE_LIBRARY",
                    LinkKind::StaticArchive => b"STATIC_ARCHIVE",
                };
                (
                    rule,
                    inputs.iter().map(|input| self.node(input)).collect(),
                    invocations.as_slice(),
                    None,
                )
            }
            BuildStep::Custom(custom) => {
                let rule: &[u8] = if custom.invocations.is_empty() {
                    b"phony"
                } else {
                    b"CUSTOM_COMMAND"
                };
                let inputs = custom
                    .depends
                    .iter()
                    .map(|input| self.custom_input(input))
                    .collect();
                (
                    rule,
                    inputs,
                    custom.invocations.as_slice(),
                    custom.depfile.as_ref(),
                )
            }
        };
        let inputs = unique(inputs);
        let implicit = unique(implicit);
        let mut order_only = Vec::new();
        if dyndep_node.is_some() || !nodes.order_only.is_empty() {
            let mut seen: HashSet<_> = inputs
                .iter()
                .chain(&implicit)
                .chain(&nodes.outputs)
                .chain(&nodes.byproducts)
                .collect();
            order_only.extend(dyndep_node.iter().filter(|node| seen.insert(node)));
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
                }
            }
        }
        ninja.edge(
            rule,
            Edge {
                outputs: &nodes.outputs,
                implicit_outputs: &implicit_outputs,
                inputs: &inputs,
                implicit_inputs: &implicit,
                order_only: &order_only,
            },
        );
        if rule == b"phony" {
            return;
        }
        if let BuildStep::Collate { tdi, .. } = step {
            let mut tdi_arg = Vec::new();
            shell_word(&self.node(tdi), &mut tdi_arg);
            ninja.binding(b"CMAKE_COMMAND", cmake);
            ninja.binding(b"in_tdi", &tdi_arg);
            ninja.binding(b"MODMAP_FLAG", modmap_flag);
            if !export_commands.is_empty() {
                ninja.binding(b"MODULE_EXPORT_COMMANDS", &export_commands);
            }
            return;
        }
        ninja.binding(
            b"COMMAND",
            &command_line(invocations, matches!(step, BuildStep::Custom(_))),
        );
        if let Some((ddi, response)) = scan_response {
            let rsp = [self.node(ddi).as_slice(), b".rsp"].concat();
            ninja.binding(b"RSP_FILE", &rsp);
            ninja.binding(b"RSP_ARGS", &response.arguments);
        }
        if let Some(depfile) = depfile {
            let key: &[u8] = if matches!(step, BuildStep::Compile { .. } | BuildStep::Scan { .. }) {
                b"DEP_FILE"
            } else {
                b"depfile"
            };
            ninja.binding(key, &self.node(depfile));
        }
        if let BuildStep::Compile {
            modmap: Some(map), ..
        } = step
        {
            ninja.binding(b"DYNDEP_MODULE_MAP_FILE", &self.node(map));
        }
        if let Some(dd) = dyndep_node {
            ninja.binding(b"dyndep", &dd);
        }
        if let BuildStep::Custom(custom) = step {
            ninja.binding(
                b"DESC",
                custom
                    .comment
                    .as_deref()
                    .map(Vec::as_slice)
                    .unwrap_or(b"Running custom command"),
            );
            if custom.uses_terminal {
                ninja.binding(b"pool", b"console");
            } else if let Some(pool) = custom.job_pool.as_ref().filter(|pool| !pool.is_empty()) {
                ninja.binding(b"pool", pool);
            }
        }
    }
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

fn command_line(invocations: &[Invocation], custom: bool) -> Vec<u8> {
    let mut result = Vec::new();
    for invocation in invocations {
        if !result.is_empty() {
            result.extend_from_slice(b" && ");
        }
        let group = invocations.len() > 1 || invocation.working_dir.is_some();
        if group {
            result.push(b'(');
        }
        if let Some(directory) = &invocation.working_dir {
            result.extend_from_slice(b"cd -- ");
            shell_word(directory, &mut result);
            result.extend_from_slice(b" && ");
        }
        if !invocation.shell.is_empty() && (!custom || invocation.argv.is_empty()) {
            result.extend_from_slice(&invocation.shell);
        } else if let Some((first, rest)) = invocation.argv.split_first() {
            shell_word(first, &mut result);
            for arg in rest {
                result.push(b' ');
                if custom
                    && matches!(
                        arg.as_slice(),
                        b"<" | b">"
                            | b"<<"
                            | b">>"
                            | b"2>"
                            | b"2>>"
                            | b"2>&1"
                            | b"|"
                            | b"||"
                            | b"&&"
                    )
                {
                    result.extend_from_slice(arg);
                } else {
                    shell_word(arg, &mut result);
                }
            }
        } else {
            result.push(b':');
        }
        if group {
            result.push(b')');
        }
    }
    if result.is_empty() {
        result.push(b':');
    }
    result
}
