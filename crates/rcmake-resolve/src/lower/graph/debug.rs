use std::{collections::BTreeSet, fmt::Write};

use indexmap::{IndexMap, IndexSet};
use rcmake_base::{BString, truth};
use rcmake_host::{FileKind, Follow, Host, Stream, path::to_native};
use rcmake_model::{DirId, GeneratorKind, ProjectModel, Target, TargetId};

use crate::{BuildGraph, BuildStep, GraphTarget, ResolvedTarget};

type Edges = Vec<Vec<(usize, bool)>>;

pub(super) struct Trace<'a> {
    host: &'a dyn Host,
    rows: Vec<Row>,
    graph_to_row: Vec<Option<usize>>,
}

struct Row {
    name: BString,
    node: Option<usize>,
    custom: bool,
    languages: BTreeSet<BString>,
}

impl<'a> Trace<'a> {
    pub(super) fn new(
        host: &'a dyn Host,
        model: &ProjectModel,
        local: &[(&Target, &ResolvedTarget)],
        indices: &IndexMap<TargetId, usize>,
        graph: &BuildGraph,
    ) -> Self {
        let mut trace = Self {
            host,
            rows: Vec::new(),
            graph_to_row: vec![None; local.len()],
        };
        let utilities = utility_names(model, host);
        for (_, dir) in model.dirs() {
            for &id in &dir.targets {
                let Some(target) = model.target(id).filter(|target| !target.is_imported()) else {
                    continue;
                };
                let node = indices.get(&id).copied();
                let mut row = Row {
                    name: target.name.clone(),
                    node,
                    custom: false,
                    languages: BTreeSet::new(),
                };
                if let Some((slot, (target_node, (_, resolved)))) = node.and_then(|node| {
                    trace
                        .graph_to_row
                        .get_mut(node)
                        .zip(graph.targets.get(node).zip(local.get(node)))
                }) {
                    *slot = Some(trace.rows.len());
                    row.custom = !target.event_commands.is_empty()
                        || target_node.steps.iter().any(|&step| {
                            matches!(graph.steps.get(step), Some(BuildStep::Custom(_)))
                        });
                    row.languages
                        .extend(resolved.compile_groups.iter().filter_map(|group| {
                            (!matches!(
                                group.language.as_slice(),
                                b"C" | b"CXX" | b"OBJC" | b"OBJCXX" | b"ASM" | b"CUDA" | b"HIP"
                            ))
                            .then_some(group.language.clone())
                        }));
                }
                trace.rows.push(row);
            }
            trace.rows.extend(utilities.iter().map(|name| Row {
                name: (*name).into(),
                node: None,
                custom: true,
                languages: BTreeSet::new(),
            }));
        }
        trace
    }

    pub(super) fn snapshot(&self, graph: &[GraphTarget], strong: &[IndexSet<usize>]) -> Edges {
        self.rows
            .iter()
            .map(|row| {
                let Some((target, strong)) = row
                    .node
                    .and_then(|node| graph.get(node).zip(strong.get(node)))
                else {
                    return Vec::new();
                };
                let mut edges: Vec<_> = target
                    .depends
                    .iter()
                    .filter_map(|dep| {
                        let row = self.graph_to_row.get(*dep).copied().flatten()?;
                        Some((row, strong.contains(dep)))
                    })
                    .collect();
                edges.sort_by_key(|&(_, strong)| strong);
                edges
            })
            .collect()
    }

    pub(super) fn graph(&self, name: &str, graph: &Edges) {
        let mut text = format!("The {name} target dependency graph is:\n");
        for (index, (row, edges)) in self.rows.iter().zip(graph).enumerate() {
            let _ = writeln!(text, "target {index} is [{}]", row.name);
            for &(target, strong) in edges {
                if let Some(row) = self.rows.get(target) {
                    let kind = if strong { "strong" } else { "weak" };
                    let _ = writeln!(text, "  depends on target {target} [{}] ({kind})", row.name);
                }
            }
        }
        self.output(text);
    }

    pub(super) fn side_effects(&self, graph: &Edges) {
        let mut text = String::from("The side effects are:\n");
        for (index, row) in self.rows.iter().enumerate() {
            let _ = writeln!(text, "target {index} is [{}]", row.name);
            let mut pending = vec![index];
            let mut reachable = BTreeSet::new();
            while let Some(node) = pending.pop() {
                if reachable.insert(node) {
                    pending.extend(
                        graph
                            .get(node)
                            .into_iter()
                            .flatten()
                            .map(|&(target, _)| target),
                    );
                }
            }
            let custom: Vec<_> = reachable
                .iter()
                .copied()
                .filter(|&node| self.rows.get(node).is_some_and(|row| row.custom))
                .collect();
            if !custom.is_empty() {
                text.push_str("  custom commands\n");
                self.sources(&mut text, custom);
            }
            let languages: BTreeSet<_> = reachable
                .iter()
                .filter_map(|&node| self.rows.get(node))
                .flat_map(|row| &row.languages)
                .collect();
            for language in languages {
                let _ = writeln!(text, "  language {language}");
                self.sources(
                    &mut text,
                    reachable.iter().copied().filter(|&node| {
                        self.rows
                            .get(node)
                            .is_some_and(|row| row.languages.contains(language))
                    }),
                );
            }
        }
        self.output(text);
    }

    pub(super) fn components(&self, graph: &Edges, components: &[IndexSet<usize>]) {
        let mut members: Vec<_> = (0..self.rows.len()).map(|row| vec![row]).collect();
        let mut owner: Vec<_> = (0..self.rows.len()).collect();
        for component in components {
            let nodes: Vec<_> = component
                .iter()
                .filter_map(|&node| self.graph_to_row.get(node).copied().flatten())
                .collect();
            if let Some(&leader) = nodes.iter().min() {
                for &row in &nodes {
                    if let Some(owner) = owner.get_mut(row) {
                        *owner = leader;
                    }
                }
                if let Some(members) = members.get_mut(leader) {
                    *members = nodes;
                }
            }
        }
        let mut text =
            String::from("The strongly connected components for the intermediate graph are:\n");
        let mut visited = BTreeSet::new();
        let mut index = 0;
        for &root in &owner {
            let mut pending = vec![(root, false)];
            while let Some((node, done)) = pending.pop() {
                let Some(members) = members.get(node) else {
                    continue;
                };
                if done {
                    let _ = writeln!(text, "Component ({index}):");
                    for &row in members {
                        if let Some(member) = self.rows.get(row) {
                            let _ = writeln!(text, "  contains target {row} [{}]", member.name);
                        }
                    }
                    index += 1;
                } else if visited.insert(node) {
                    pending.push((node, true));
                    pending.extend(
                        members
                            .iter()
                            .rev()
                            .filter_map(|&row| graph.get(row))
                            .flat_map(|edges| {
                                edges
                                    .iter()
                                    .rev()
                                    .filter_map(|&(target, _)| Some((*owner.get(target)?, false)))
                            }),
                    );
                }
            }
        }
        self.output(text);
    }

    fn sources(&self, text: &mut String, nodes: impl IntoIterator<Item = usize>) {
        for node in nodes {
            if let Some(row) = self.rows.get(node) {
                let _ = writeln!(text, "    from target {node} [{}]", row.name);
            }
        }
    }

    fn output(&self, mut text: String) {
        text.push('\n');
        self.host.write_out(Stream::Stderr, text.as_bytes());
    }
}

fn utility_names(model: &ProjectModel, host: &dyn Host) -> Vec<&'static str> {
    let dir = model.dir(DirId::ROOT);
    let is_on = |key: &[u8]| {
        model
            .definition(DirId::ROOT, key)
            .is_some_and(|v| truth::is_on(v))
    };
    let has_file = |name| {
        host.metadata(&to_native(&dir.binary_dir).join(name), Follow::Yes)
            .is_ok_and(|metadata| metadata.kind == FileKind::File)
    };
    let mut utilities = Vec::new();
    let ide = model.generator.is_some_and(GeneratorKind::is_ide);
    let vs = matches!(model.generator, Some(GeneratorKind::VisualStudio(_)));
    if has_file("CPackConfig.cmake") {
        utilities.push(if vs { "PACKAGE" } else { "package" });
    }
    if !vs && has_file("CPackSourceConfig.cmake") {
        utilities.push("package_source");
    }
    if is_on(b"CMAKE_TESTING_ENABLED") {
        utilities.push(if ide { "RUN_TESTS" } else { "test" });
    }
    if !ide {
        utilities.extend(["edit_cache", "rebuild_cache"]);
    }
    if !model.installs.is_empty() && !is_on(b"CMAKE_SKIP_INSTALL_RULES") {
        if ide {
            utilities.push(if vs { "INSTALL" } else { "install" });
        } else {
            utilities.extend([
                "list_install_components",
                "install",
                "install/local",
                "install/strip",
            ]);
        }
    }
    utilities
}
