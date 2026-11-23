//! Build edges and target aliases for one configuration.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use rcmake_base::{BString, path_lex::Lexical};
use rcmake_gen_common::GenError;
use rcmake_host::path;
use rcmake_resolve::{ArtifactSet, BuildGraph, BuildStep, Invocation, LinkKind};

use crate::generation_error;
use crate::syntax::{Edge, Ninja, shell_word};

pub(crate) struct Graph<'a> {
    input: &'a BuildGraph,
    build_dir: &'a Path,
    pub(crate) config: Option<&'a [u8]>,
    names: HashMap<BString, usize>,
    aliases: Vec<BString>,
    artifact_targets: HashSet<usize>,
    producers: HashMap<BString, usize>,
    steps: Vec<StepNodes>,
}

#[derive(Default)]
struct StepNodes {
    outputs: Vec<BString>,
    byproducts: Vec<BString>,
    order_only: Vec<BString>,
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
            names: HashMap::new(),
            aliases: Vec::new(),
            artifact_targets: HashSet::new(),
            producers: HashMap::new(),
            steps: Vec::new(),
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
                BuildStep::Scan { .. } | BuildStep::Collate { .. } => {
                    return Err(crate::modules::unsupported());
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

    fn connect_targets(&mut self) -> Result<(), GenError> {
        for (target, alias) in self.input.targets.iter().zip(&self.aliases) {
            let dependencies: Vec<_> = target
                .depends
                .iter()
                .map(|&index| self.aliases.get(index).cloned())
                .collect::<Option<_>>()
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
                if matches!(step, BuildStep::Compile { .. })
                    && let Some(nodes) = self.steps.get_mut(index)
                {
                    nodes.order_only.extend(dependencies.iter().cloned());
                }
            }
            if let Some(&producer) = self.producers.get(alias) {
                // A real output can also be the target name, such as a root executable.
                let extra: Vec<_> = dependencies
                    .into_iter()
                    .chain(
                        target
                            .steps
                            .iter()
                            .filter(|&&index| index != producer)
                            .filter_map(|&index| self.steps.get(index))
                            .flat_map(|nodes| nodes.outputs.iter().chain(&nodes.byproducts))
                            .cloned(),
                    )
                    .collect();
                if let Some(nodes) = self.steps.get_mut(producer) {
                    nodes.order_only.extend(extra);
                }
            }
        }
        Ok(())
    }

    pub(crate) fn emit(&self, ninja: &mut Ninja) {
        for (step, nodes) in self.input.steps.iter().zip(&self.steps) {
            self.emit_step(ninja, step, nodes);
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

    fn emit_step(&self, ninja: &mut Ninja, step: &BuildStep, nodes: &StepNodes) {
        let (rule, inputs, invocations, depfile) = match step {
            BuildStep::Compile {
                source,
                invocation,
                depfile,
                ..
            } => {
                let rule: &[u8] = if depfile.is_some() {
                    b"COMPILE_DEPFILE"
                } else {
                    b"COMPILE"
                };
                (
                    rule,
                    vec![self.node(source)],
                    std::slice::from_ref(invocation),
                    depfile.as_ref(),
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
            // `Graph::new` rejects module steps.
            BuildStep::Scan { .. } | BuildStep::Collate { .. } => return,
        };
        let inputs = unique(inputs);
        let order_only = unique(
            nodes
                .order_only
                .iter()
                .filter(|node| {
                    !inputs.contains(node)
                        && !nodes.outputs.contains(node)
                        && !nodes.byproducts.contains(node)
                })
                .cloned(),
        );
        ninja.edge(
            rule,
            Edge {
                outputs: &nodes.outputs,
                implicit_outputs: &nodes.byproducts,
                inputs: &inputs,
                order_only: &order_only,
                ..Edge::default()
            },
        );
        if rule == b"phony" {
            return;
        }
        ninja.binding(b"COMMAND", &command_line(invocations));
        if let Some(depfile) = depfile {
            let key: &[u8] = if matches!(step, BuildStep::Compile { .. }) {
                b"DEP_FILE"
            } else {
                b"depfile"
            };
            ninja.binding(key, &self.node(depfile));
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

fn command_line(invocations: &[Invocation]) -> Vec<u8> {
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
        if !invocation.shell.is_empty() {
            result.extend_from_slice(&invocation.shell);
        } else if let Some((first, rest)) = invocation.argv.split_first() {
            shell_word(first, &mut result);
            for arg in rest {
                result.push(b' ');
                shell_word(arg, &mut result);
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
