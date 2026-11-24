use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write;
use std::path::PathBuf;
use std::sync::Arc;

use rcmake_base::{BString, list, truth};
use rcmake_eval::RunMode;
use rcmake_genex::ConfigId;
use rcmake_model::{DirId, ProjectModel, TargetKind};
use rcmake_resolve::{
    graphviz::{self, Node, Scope},
    usage::UsageResolver,
};

use super::{Exit, Host, Interp, Path, WriteMode, options};

const TYPES: [(TargetKind, &[u8], &str, &str); 8] = [
    (TargetKind::Executable, b"EXECUTABLES", "Executable", "egg"),
    (
        TargetKind::StaticLibrary,
        b"STATIC_LIBS",
        "Static Library",
        "octagon",
    ),
    (
        TargetKind::SharedLibrary,
        b"SHARED_LIBS",
        "Shared Library",
        "doubleoctagon",
    ),
    (
        TargetKind::ModuleLibrary,
        b"MODULE_LIBS",
        "Module Library",
        "tripleoctagon",
    ),
    (
        TargetKind::InterfaceLibrary,
        b"INTERFACE_LIBS",
        "Interface Library",
        "pentagon",
    ),
    (
        TargetKind::ObjectLibrary,
        b"OBJECT_LIBS",
        "Object Library",
        "hexagon",
    ),
    (
        TargetKind::UnknownLibrary,
        b"UNKNOWN_LIBS",
        "Unknown Library",
        "septagon",
    ),
    (
        TargetKind::Utility,
        b"CUSTOM_TARGETS",
        "Custom Target",
        "box",
    ),
];

type Edges = BTreeMap<Node, Vec<(Node, Scope)>>;

pub(super) fn write_graphviz(
    host: Arc<dyn Host>,
    path: &Path,
    interp: &mut Interp,
) -> Result<(), Exit> {
    let mut project = interp.cx();
    let project_name = project.var_or_empty(b"CMAKE_PROJECT_NAME");
    let model = project.project();
    let root = model.dir(DirId::ROOT);
    let mut settings = crate::new_interp(
        &[],
        &options::Options::default(),
        RunMode::Script,
        Arc::clone(&host),
    );
    if let Some(file) = [&root.binary_dir, &root.source_dir]
        .into_iter()
        .map(|dir| rcmake_host::path::to_native(dir).join("CMakeGraphVizOptions.cmake"))
        .find(|file| super::is_file(host.as_ref(), file))
    {
        options::out(
            host.as_ref(),
            &format!("Reading GraphViz options file: {}\n", file.display()),
        );
        if settings.run_script(&file).exit_code != 0 {
            return Err(Exit::Code(1));
        }
    }
    let mut cx = settings.cx();
    let mut get = |key: &[u8]| cx.var(&[b"GRAPHVIZ_", key].concat());
    let graph_name = get(b"GRAPH_NAME").unwrap_or(project_name);
    let graph_type = get(b"GRAPH_TYPE").unwrap_or_else(|| "digraph".into());
    let header = get(b"GRAPH_HEADER").unwrap_or_else(|| "node [\n  fontsize = \"12\"\n];".into());
    let prefix = get(b"NODE_PREFIX").unwrap_or_else(|| "node".into());
    let ignore = get(b"IGNORE_TARGETS").unwrap_or_default();
    let mut on = |key: &[u8], default: bool| get(key).map_or(default, |v| truth::is_on(&v));
    let externals = on(b"EXTERNAL_LIBS", true);
    let per_target = on(b"GENERATE_PER_TARGET", true);
    let dependers = on(b"GENERATE_DEPENDERS", true);
    let enabled = TYPES.map(|(kind, key, _, _)| on(key, kind != TargetKind::Utility));
    let ignore: Vec<_> = list::split(&ignore, list::Empty::Drop)
        .iter()
        .filter_map(|pattern| cx.regex(pattern).ok())
        .collect();
    let configs = if model.generator.is_some_and(|kind| kind.multi_config()) {
        let names = model.definition(DirId::ROOT, b"CMAKE_CONFIGURATION_TYPES");
        list::split(
            names.map_or(b"", |value| value.as_slice()),
            list::Empty::Drop,
        )
        .into_iter()
        .map(ConfigId)
        .collect()
    } else {
        vec![ConfigId(
            model
                .definition(DirId::ROOT, b"CMAKE_BUILD_TYPE")
                .cloned()
                .unwrap_or_default(),
        )]
    };
    let mut roots: Vec<_> = model
        .targets()
        .filter(|(_, target)| !target.is_imported() && !target.name.starts_with(b"__cmake_"))
        .map(|(id, _)| Node::Target(id))
        .collect();
    roots.sort_by(|left, right| name(model, left).cmp(name(model, right)));
    let mut usage = UsageResolver::new(model);
    let mut edges = Edges::new();
    let mut aliases = BTreeMap::<_, BTreeSet<_>>::new();
    let mut pending = roots.clone();
    while let Some(node) = pending.pop() {
        if edges.contains_key(&node) {
            continue;
        }
        let mut connections = Vec::new();
        if let Node::Target(id) = node {
            let deps =
                graphviz::dependencies(model, &mut usage, id, &configs).map_err(Exit::message)?;
            for (id, names) in deps.aliases {
                aliases.entry(id).or_default().extend(names);
            }
            connections.extend(deps.edges);
            connections.sort_by(|(a, _), (b, _)| name(model, a).cmp(name(model, b)));
            pending.extend(connections.iter().map(|(dep, _)| dep.clone()));
        }
        edges.insert(node, connections);
    }
    let visible = |node: &Node| {
        let name = name(model, node);
        if name == b">"
            || ignore.iter().any(|regex| {
                regex
                    .find_at(name, 0, Default::default(), &mut 1_000_000)
                    .is_ok_and(|m| m.is_some())
            })
        {
            return false;
        }
        match node {
            Node::External(_) => externals,
            Node::Target(id) => model.target(*id).is_some_and(|target| {
                (!target.is_imported() || externals)
                    && (target.kind != TargetKind::Utility
                        || ![b"Nightly".as_slice(), b"Continuous", b"Experimental"]
                            .iter()
                            .any(|p| name.starts_with(p)))
                    && kind_index(target.kind).and_then(|i| enabled.get(i)) == Some(&true)
            }),
        }
    };
    let label = |node: &Node| {
        let mut label = display_name(model, node);
        if let Node::Target(id) = node {
            let mut names = model.target_aliases(*id).collect::<BTreeSet<_>>();
            names.extend(aliases.get(id).into_iter().flatten());
            for alias in names {
                let _ = write!(label, "\n({alias})");
            }
        }
        label
    };
    let start = |title: &[u8]| format!("{graph_type} \"{}\" {{\n{header}\n", escape(title));
    let mut graph = start(&graph_name);
    legend(&mut graph);
    let mut numbers = BTreeMap::new();
    let mut reverse = Edges::new();
    let mut forward = Edges::new();
    let mut visited = BTreeSet::new();
    for root in &roots {
        walk(&edges, root, &mut visited, true, |visit| match visit {
            Visit::Node(node) if visible(node) => {
                let number = numbers.len();
                numbers.insert(node.clone(), number);
                write_node(&mut graph, &prefix, model, node, number, &label(node));
            }
            Visit::Edge(from, to, scope) if visible(from) && visible(to) => {
                write_edge(&mut graph, &prefix, model, &numbers, from, to, scope);
                reverse
                    .entry(to.clone())
                    .or_default()
                    .push((from.clone(), scope));
                forward
                    .entry(from.clone())
                    .or_default()
                    .push((to.clone(), scope));
            }
            _ => {}
        });
    }
    save(host.as_ref(), path, graph)?;
    for (enabled, connections, reversed) in
        [(per_target, &forward, false), (dependers, &reverse, true)]
    {
        if !enabled {
            continue;
        }
        for root in connections.keys() {
            let mut graph = start(name(model, root));
            let mut visited = BTreeSet::new();
            let mut written = BTreeSet::new();
            walk(
                connections,
                root,
                &mut visited,
                false,
                |visit| match visit {
                    Visit::Node(node) => {
                        if let Some(&number) = numbers.get(node)
                            && written.insert(node.clone())
                        {
                            write_node(&mut graph, &prefix, model, node, number, &label(node));
                        }
                    }
                    Visit::Edge(from, to, scope) => {
                        if let Some(&number) = numbers.get(to)
                            && written.insert(to.clone())
                        {
                            write_node(&mut graph, &prefix, model, to, number, &label(to));
                        }
                        let (from, to) = if reversed { (to, from) } else { (from, to) };
                        write_edge(&mut graph, &prefix, model, &numbers, from, to, scope);
                    }
                },
            );
            let suffix: String = display_name(model, root)
                .chars()
                .map(|c| {
                    if matches!(c, '/' | '\\' | ':' | '*') {
                        '_'
                    } else {
                        c
                    }
                })
                .collect();
            let mut file = path.as_os_str().to_os_string();
            file.push(format!(
                ".{suffix}{}",
                if reversed { ".dependers" } else { "" }
            ));
            save(host.as_ref(), &PathBuf::from(file), graph)?;
        }
    }
    options::out(
        host.as_ref(),
        &format!("Generate graphviz: {}\n", path.display()),
    );
    Ok(())
}

#[derive(Clone, Copy)]
enum Visit<'a> {
    Node(&'a Node),
    Edge(&'a Node, &'a Node, Scope),
}

fn walk<'a>(
    edges: &'a Edges,
    root: &'a Node,
    visited: &mut BTreeSet<&'a Node>,
    postorder: bool,
    mut emit: impl FnMut(Visit<'a>),
) {
    let mut pending = vec![Visit::Node(root)];
    while let Some(visit) = pending.pop() {
        match visit {
            Visit::Node(node) if visited.insert(node) => {
                emit(visit);
                for (dep, scope) in edges.get(node).into_iter().flatten().rev() {
                    let link = Visit::Edge(node, dep, *scope);
                    let next = Visit::Node(dep);
                    if postorder {
                        pending.extend([link, next]);
                    } else {
                        pending.extend([next, link]);
                    }
                }
            }
            Visit::Node(_) => {}
            Visit::Edge(..) => emit(visit),
        }
    }
}

fn name<'a>(model: &'a ProjectModel, node: &'a Node) -> &'a [u8] {
    match node {
        Node::Target(id) => model.target(*id).map_or(b"", |t| t.name.as_slice()),
        Node::External(name) => name,
    }
}

fn display_name(model: &ProjectModel, node: &Node) -> String {
    let name = String::from_utf8_lossy(name(model, node));
    if matches!(node, Node::External(_)) && name.starts_with('-') {
        format!("\"{name}\"")
    } else {
        name.into_owned()
    }
}

fn kind_index(kind: TargetKind) -> Option<usize> {
    TYPES.iter().position(|(key, ..)| *key == kind)
}

fn write_node(
    out: &mut String,
    prefix: &BString,
    model: &ProjectModel,
    node: &Node,
    number: usize,
    label: &str,
) {
    let shape = match node {
        Node::Target(id) => model.target(*id).and_then(|target| kind_index(target.kind)),
        Node::External(_) => None,
    }
    .and_then(|index| TYPES.get(index))
    .map_or("septagon", |entry| entry.3);
    let _ = writeln!(
        out,
        "    \"{prefix}{number}\" [ label = \"{}\", shape = {shape} ];",
        escape(label.as_bytes())
    );
}

fn write_edge(
    out: &mut String,
    prefix: &BString,
    model: &ProjectModel,
    numbers: &BTreeMap<Node, usize>,
    from: &Node,
    to: &Node,
    scope: Scope,
) {
    let style = match scope {
        Scope::Public => "",
        Scope::Private => "[ style = dotted ]",
        Scope::Interface => "[ style = dashed ]",
    };
    let Some((a, b)) = numbers.get(from).zip(numbers.get(to)) else {
        return;
    };
    let _ = writeln!(
        out,
        "    \"{prefix}{a}\" -> \"{prefix}{b}\" {style} // {} -> {}",
        display_name(model, from),
        display_name(model, to)
    );
}

fn legend(out: &mut String) {
    out.push_str("subgraph clusterLegend {\n  label = \"Legend\";\n  color = black;\n  edge [ style = invis ];\n");
    for (i, (_, _, name, shape)) in TYPES.iter().enumerate() {
        let _ = writeln!(
            out,
            "  legendNode{i} [ label = \"{name}\", shape = {shape} ];"
        );
    }
    out.push_str(concat!(
        "  legendNode0 -> legendNode1 [ style = solid ];\n",
        "  legendNode0 -> legendNode2 [ style = solid ];\n",
        "  legendNode0 -> legendNode3;\n",
        "  legendNode1 -> legendNode4 [ label = \"Interface\", style = dashed ];\n",
        "  legendNode2 -> legendNode5 [ label = \"Private\", style = dotted ];\n",
        "  legendNode3 -> legendNode6 [ style = solid ];\n",
        "  legendNode0 -> legendNode7;\n}\n",
    ));
}

fn save(host: &dyn Host, path: &Path, mut graph: String) -> Result<(), Exit> {
    graph.push_str("}\n");
    host.write(path, graph.as_bytes(), WriteMode::Truncate)
        .map_err(|err| Exit::Message(format!("Could not write Graphviz output: {err}")))
}

fn escape(value: &[u8]) -> String {
    String::from_utf8_lossy(value)
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
}
