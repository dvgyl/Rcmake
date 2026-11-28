//! Target-level build order (`cmComputeTargetDepends`).
//!
//! The graph has one node for each target in the build system, in directory and
//! creation order. Its edges come from the link implementation, the transitive
//! link interfaces, object library references, and the utility items of each
//! target. Cycles among static libraries become one strongly connected component.
//! The final graph orders the members of each component as a linked list and
//! connects components from tail to head.

use indexmap::{IndexMap, IndexSet};
use rcmake_base::{BString, list, truth};
use rcmake_diag::Backtrace;
use rcmake_genex::{ConfigId, EvalContext, UsageTraversal};
use rcmake_model::{CustomCommandKind, ProjectModel, Target, TargetId, TargetKind};

use crate::sources::SourcePaths;
use crate::usage::{ResolvedLinkItem, ResolvedLinkKind, UsageResolver, handle};
use crate::{BuildGraph, ResolveError, ResolvedTarget};

/// One edge of the final target dependency graph (`cmTargetDepend`).
#[derive(Clone, Debug)]
pub struct TargetDepend {
    /// The target that must build first.
    pub target: TargetId,
    /// `true` for a utility edge, `false` for a link edge.
    pub strong: bool,
    /// `true` when the dependency uses the command configuration.
    pub cross: bool,
    /// Where the project created the edge (empty for a linearization edge).
    pub backtrace: Backtrace,
}

/// One utility item of a target (`cmGeneratorTarget::GetUtilityItems`).
#[derive(Clone, Debug)]
pub struct UtilityItem {
    /// The target that the utility names. It can be an imported target.
    pub target: TargetId,
    /// `true` when the dependency uses the command configuration.
    pub cross: bool,
    /// Where the project named the utility (empty when CMake records none).
    pub backtrace: Backtrace,
}

/// The target dependency graph of one generation.
#[derive(Clone, Debug, Default)]
pub struct TargetDependGraph {
    nodes: Vec<TargetId>,
    index: IndexMap<TargetId, usize>,
    direct: Vec<Vec<TargetDepend>>,
    utilities: IndexMap<TargetId, Vec<UtilityItem>>,
    traced_sources: IndexMap<TargetId, Vec<BString>>,
    source_paths: IndexMap<TargetId, IndexMap<BString, BString>>,
    generated_sources: IndexMap<TargetId, IndexSet<BString>>,
}

impl TargetDependGraph {
    /// Targets in the build system, in `cmComputeTargetDepends::CollectTargets` order.
    #[must_use]
    pub fn targets(&self) -> &[TargetId] {
        &self.nodes
    }

    /// Return the `cmComputeTargetDepends::CollectTargets` index of `target`,
    /// or `None` for targets outside the build system (such as imported targets).
    #[must_use]
    pub fn target_index(&self, target: TargetId) -> Option<usize> {
        self.index.get(&target).copied()
    }

    /// Direct dependencies of `target` (`GetTargetDirectDepends`), one entry per
    /// dependency, in final graph order. A repeated edge keeps the attributes of
    /// its last occurrence, as `cmTargetDependSet` does.
    #[must_use]
    pub fn direct_depends(&self, target: TargetId) -> &[TargetDepend] {
        self.index
            .get(&target)
            .and_then(|&node| self.direct.get(node))
            .map_or(&[], Vec::as_slice)
    }

    /// All utility items of `target`, in the order that the project created them.
    #[must_use]
    pub fn utility_items(&self, target: TargetId) -> &[UtilityItem] {
        self.utilities.get(&target).map_or(&[], Vec::as_slice)
    }

    /// Utility items for the codemodel `orderDependencies`
    /// (`cmFileAPICodemodel.cxx`): all utility items except reserved targets.
    /// The target itself and imported targets stay.
    pub fn order_dependencies<'a>(
        &'a self,
        model: &'a ProjectModel,
        target: TargetId,
    ) -> impl Iterator<Item = &'a UtilityItem> + 'a {
        self.utility_items(target).iter().filter(move |item| {
            model
                .target(item.target)
                .is_some_and(|dependency| !is_reserved_target(&dependency.name))
        })
    }

    /// The full path that a source name of `target` resolves to
    /// (`cmMakefile::GetSource`). A relative name resolves to an earlier
    /// generated output in the binary directory before a file in the source
    /// directory. `name` is one item of a `SOURCES` entry, as written.
    #[must_use]
    pub fn resolved_source(&self, target: TargetId, name: &[u8]) -> Option<&BString> {
        self.source_paths.get(&target)?.get(name)
    }

    /// Return whether `path` (normalized absolute path) is a generated source
    /// visible to `target`, including custom-command outputs/byproducts under
    /// CMP0118/CMP0163 and `file(GENERATE)` outputs.
    #[must_use]
    pub fn is_generated_source(&self, target: TargetId, path: &[u8]) -> bool {
        self.generated_sources
            .get(&target)
            .is_some_and(|set| set.contains(path))
    }

    /// Sources that the dependency trace appends to the sources of `target`
    /// (`cmGeneratorTarget::AddTracedSources`), in discovery order. Each entry is
    /// the `MAIN_DEPENDENCY` of a custom command or `<output>.rule`.
    #[must_use]
    pub fn traced_sources(&self, target: TargetId) -> &[BString] {
        self.traced_sources.get(&target).map_or(&[], Vec::as_slice)
    }
}

/// Return `true` for a name that a generator reserves
/// (`cmGlobalGenerator::IsReservedTarget`).
#[must_use]
pub fn is_reserved_target(name: &[u8]) -> bool {
    matches!(
        name,
        b"all"
            | b"ALL_BUILD"
            | b"help"
            | b"install"
            | b"INSTALL"
            | b"preinstall"
            | b"clean"
            | b"edit_cache"
            | b"rebuild_cache"
            | b"ZERO_CHECK"
    )
}

#[derive(Clone, Debug)]
struct Edge {
    to: usize,
    strong: bool,
    cross: bool,
    backtrace: Backtrace,
}

type Graph = Vec<Vec<Edge>>;

/// Compute the target dependency graph from the resolved targets and the
/// lowered build graphs of all configurations.
pub(crate) fn compute(
    model: &ProjectModel,
    configs: &[ConfigId],
    (targets, graphs): (&[ResolvedTarget], &[BuildGraph]),
    paths: &SourcePaths<'_>,
    usage: &mut UsageResolver<'_>,
) -> Result<TargetDependGraph, ResolveError> {
    let mut graph = TargetDependGraph::default();
    let mut by_dir = IndexMap::<_, Vec<TargetId>>::new();
    for (id, target) in model.targets() {
        if !target.is_imported() {
            by_dir.entry(target.dir).or_default().push(id);
        }
    }
    for (dir, _) in model.dirs() {
        for &id in by_dir.get(&dir).into_iter().flatten() {
            graph.index.insert(id, graph.nodes.len());
            graph.nodes.push(id);
        }
    }
    let resolved: IndexMap<(TargetId, &ConfigId), &ResolvedTarget> = targets
        .iter()
        .map(|target| ((target.id, &target.config), target))
        .collect();
    let link_items = LinkItems::collect(model, configs, &resolved, &graph.nodes, paths, usage)?;
    let mut artifact_to_target = IndexMap::<BString, TargetId>::new();
    for target in targets {
        for artifact in target
            .artifacts
            .file
            .iter()
            .chain(target.artifacts.linker_file.iter())
        {
            artifact_to_target
                .entry(crate::lower::build_relative(model, artifact))
                .or_insert(target.id);
        }
    }
    let mut attached = IndexMap::<TargetId, Vec<rcmake_model::CommandId>>::new();
    for (command_id, command) in model.commands() {
        if let CustomCommandKind::Utility(owner)
        | CustomCommandKind::TargetEvent { target: owner, .. } = command.kind
        {
            attached.entry(owner).or_default().push(command_id);
        }
    }
    // An imported target is not a graph node, but a link to it follows its
    // utility items (`AddTargetDepend` for a target outside the build system).
    for (id, target) in model.targets() {
        if target.is_imported() {
            let utilities = utility_items(model, (id, target), &[], graphs, &artifact_to_target);
            if !utilities.is_empty() {
                graph.utilities.insert(id, utilities);
            }
        }
    }
    for &id in &graph.nodes {
        let Some(target) = model.target(id) else {
            continue;
        };
        let commands = attached.get(&id).map_or(&[][..], Vec::as_slice);
        let utilities = utility_items(model, (id, target), commands, graphs, &artifact_to_target);
        graph.utilities.insert(id, utilities);
        let traced = traced_sources(model, id, target, graphs);
        if !traced.is_empty() {
            graph.traced_sources.insert(id, traced);
        }
        let dir = model.dir(target.dir);
        let mut names = IndexMap::<BString, BString>::new();
        let mut generated = IndexSet::<BString>::new();
        let record_name = |name: BString,
                           names: &mut IndexMap<BString, BString>,
                           generated: &mut IndexSet<BString>| {
            let resolved_path = if let Some(existing) = names.get(&name) {
                Some(existing.clone())
            } else if let Some(path) = paths.find(dir, &name) {
                names.insert(name.clone(), path.clone());
                Some(path)
            } else {
                None
            };
            let check_path = resolved_path.unwrap_or_else(|| {
                rcmake_base::path_lex::Lexical::new(&crate::sources::absolute_path(
                    &dir.source_dir,
                    &name,
                ))
                .normalize()
            });
            if paths.is_generated(dir, &check_path) {
                generated.insert(check_path);
            }
        };
        for entry in &target.sources {
            for name in list::split(&entry.value, list::Empty::Drop) {
                if rcmake_genex::find_genex(&name).is_none() {
                    record_name(name, &mut names, &mut generated);
                }
            }
        }
        if !matches!(
            target.kind,
            TargetKind::InterfaceLibrary | TargetKind::GlobalTarget
        ) {
            for config in configs {
                let ctx = EvalContext {
                    config: config.clone(),
                    head: Some(handle(id)),
                    traversal: UsageTraversal::Compile,
                    ..EvalContext::default()
                };
                if let Ok(collected) = usage.collect_property(id, b"SOURCES", &ctx) {
                    for entry in collected.entries {
                        if paths.object_owner(&entry.value).is_none() {
                            record_name(entry.value, &mut names, &mut generated);
                        }
                    }
                }
                if let Some(resolved_target) = resolved.get(&(id, config)) {
                    for unit in &resolved_target.compile_units {
                        let abs = rcmake_base::path_lex::Lexical::new(
                            &crate::sources::absolute_path(&dir.source_dir, &unit.source.value),
                        )
                        .normalize();
                        if paths.is_generated(dir, &abs) {
                            generated.insert(abs);
                        }
                    }
                }
                for &set_id in &target.file_sets {
                    if let Some(set) = model.file_set(set_id)
                        && let Ok(files) =
                            usage.file_set_paths(set.target, b"SOURCES", &set.files, &ctx)
                    {
                        for file in files {
                            let abs = rcmake_base::path_lex::Lexical::new(
                                &crate::sources::absolute_path(&dir.source_dir, &file.value),
                            )
                            .normalize();
                            if paths.is_generated(dir, &abs) {
                                generated.insert(abs);
                            }
                        }
                    }
                }
            }
        }
        if !names.is_empty() {
            graph.source_paths.insert(id, names);
        }
        if !generated.is_empty() {
            graph.generated_sources.insert(id, generated);
        }
    }
    let mut collector = Collector {
        model,
        graph: &graph,
        paths,
        initial: vec![Vec::new(); graph.nodes.len()],
        object_utilities: IndexMap::new(),
        emitted: Emitted::new(graph.nodes.len()),
    };
    for (node, &id) in graph.nodes.iter().enumerate() {
        collector.collect_target(node, id, &link_items, usage)?;
    }
    let initial = collector.initial;
    for (id, owners) in collector.object_utilities {
        let utilities = graph.utilities.entry(id).or_default();
        for owner in owners {
            if !utilities
                .iter()
                .any(|item| item.target == owner && !item.cross)
            {
                utilities.push(UtilityItem {
                    target: owner,
                    cross: false,
                    backtrace: Backtrace::default(),
                });
            }
        }
    }
    // The initial graph is the union of all configurations, so a cycle can
    // exist here even when no single configuration has one.
    let initial_components = Components::compute(&initial);
    check_components(model, &graph.nodes, &initial, &initial_components)?;
    let (components, optimized) =
        if let Some(intermediate) = intermediate_graph(model, &graph, targets, graphs, &initial) {
            let components = Components::compute(&intermediate);
            check_components(model, &graph.nodes, &initial, &components)?;
            (components, Some(intermediate))
        } else {
            (initial_components, None)
        };
    let final_edges = if components.all_singletons {
        if let Some(mut intermediate) = optimized {
            for (node, edges) in intermediate.iter_mut().enumerate() {
                edges.retain(|edge| edge.to != node);
            }
            intermediate
        } else {
            initial
        }
    } else {
        final_graph(&initial, &components).map_err(|component| {
            cycle_error(
                model,
                &graph.nodes,
                &initial,
                &components,
                component,
                "The component contains at least one cycle consisting of strong dependencies (created by add_dependencies) that cannot be broken.",
            )
        })?
    };
    let mut pos = vec![usize::MAX; graph.nodes.len()];
    let mut touched = Vec::new();
    graph.direct = final_edges
        .into_iter()
        .map(|edges| {
            let mut out: Vec<TargetDepend> = Vec::with_capacity(edges.len());
            touched.clear();
            for edge in edges {
                let Some(&target) = graph.nodes.get(edge.to) else {
                    continue;
                };
                let depend = TargetDepend {
                    target,
                    strong: edge.strong,
                    cross: edge.cross,
                    backtrace: edge.backtrace,
                };
                if let Some(slot) = pos.get_mut(edge.to) {
                    if *slot == usize::MAX {
                        *slot = out.len();
                        touched.push(edge.to);
                        out.push(depend);
                    } else if let Some(existing) = out.get_mut(*slot) {
                        *existing = depend;
                    }
                }
            }
            for &idx in &touched {
                if let Some(slot) = pos.get_mut(idx) {
                    *slot = usize::MAX;
                }
            }
            out
        })
        .collect();
    Ok(graph)
}

/// The target items of one configuration's link implementation.
struct ConfigLinks {
    config: ConfigId,
    /// `UseTo::Link` libraries with the backtrace of their entry.
    link: Vec<(TargetId, Backtrace)>,
    /// Object libraries of the `UseTo::Link` objects.
    objects: Vec<TargetId>,
    /// `UseTo::Compile` libraries.
    compile: Vec<(TargetId, Backtrace)>,
}

/// Direct link items of each node for every configuration.
#[derive(Default)]
struct LinkItems {
    implementation: IndexMap<TargetId, Vec<ConfigLinks>>,
    /// Object libraries named by `$<TARGET_OBJECTS>` in the sources
    /// (`GetExternalObjects`).
    external_objects: IndexMap<TargetId, Vec<TargetId>>,
}

impl LinkItems {
    fn collect(
        model: &ProjectModel,
        configs: &[ConfigId],
        resolved: &IndexMap<(TargetId, &ConfigId), &ResolvedTarget>,
        nodes: &[TargetId],
        paths: &SourcePaths<'_>,
        usage: &mut UsageResolver<'_>,
    ) -> Result<Self, ResolveError> {
        let mut items = Self::default();
        for &id in nodes {
            let Some(target) = model.target(id) else {
                continue;
            };
            if !model.is_in_build_system(target) {
                continue;
            }
            let mut external = IndexSet::new();
            for entry in &target.sources {
                for source in list::split(&entry.value, list::Empty::Drop) {
                    if let Some(name) = source
                        .strip_prefix(b"$<TARGET_OBJECTS:")
                        .and_then(|name| name.strip_suffix(b">"))
                        && let Some(owner) = model.find_target(target.dir, name)
                    {
                        external.insert(owner);
                    }
                }
            }
            items
                .external_objects
                .insert(id, external.into_iter().collect());
            let mut per_config = Vec::new();
            for config in configs {
                let link_language = resolved
                    .get(&(id, config))
                    .and_then(|target| target.link.as_ref())
                    .map(|link| link.language.clone());
                let mut links = ConfigLinks {
                    config: config.clone(),
                    link: Vec::new(),
                    objects: Vec::new(),
                    compile: Vec::new(),
                };
                for traversal in [UsageTraversal::Link, UsageTraversal::Compile] {
                    let ctx = EvalContext {
                        config: config.clone(),
                        traversal,
                        link_language: link_language.clone(),
                        ..EvalContext::default()
                    };
                    for item in usage.direct_link_items(id, false, &ctx)?.iter() {
                        let owner = object_owner(item, paths);
                        if traversal == UsageTraversal::Link
                            && let Some(owner) = owner
                        {
                            links.objects.push(owner);
                        }
                        if item.kind == ResolvedLinkKind::ObjectFile {
                            continue;
                        }
                        let found = item.targets().map(|dep| (dep, item.text.backtrace.clone()));
                        if traversal == UsageTraversal::Link {
                            links.link.extend(found);
                        } else {
                            links.compile.extend(found);
                        }
                    }
                }
                per_config.push(links);
            }
            items.implementation.insert(id, per_config);
        }
        Ok(items)
    }
}

/// Collect the utility items of one target.
fn utility_items(
    model: &ProjectModel,
    (id, target): (TargetId, &Target),
    commands: &[rcmake_model::CommandId],
    graphs: &[BuildGraph],
    artifact_to_target: &IndexMap<BString, TargetId>,
) -> Vec<UtilityItem> {
    // `cmTarget::Utilities` is keyed by name and cross flag. The first entry keeps its backtrace.
    let mut items = IndexMap::<(TargetId, bool), Backtrace>::new();
    let mut add = |dependency: TargetId, cross: bool, backtrace: &Backtrace| {
        items
            .entry((dependency, cross))
            .or_insert_with(|| backtrace.clone());
    };
    for entry in &target.utility_deps {
        if let Some(dependency) = model.find_target(target.dir, &entry.value) {
            add(dependency, false, &entry.backtrace);
        }
    }
    let mut attached_bare_depends = IndexSet::new();
    for &command_id in commands {
        let Some(command) = model.command(command_id) else {
            continue;
        };
        for graph in graphs {
            add_command_utilities(graph, command_id, &command.backtrace, &mut add);
            if let Some(utilities) = graph.command_utilities.get(&command_id) {
                attached_bare_depends.extend(utilities.depends.iter().copied());
            }
        }
    }
    for graph in graphs {
        if let Some(graph_idx) = graph.target_ids.iter().position(|&tid| tid == id)
            && let Some(graph_target) = graph.targets.get(graph_idx)
        {
            for &step_idx in &graph_target.steps {
                if !graph.custom_output_owners.contains_key(&step_idx)
                    && let Some(crate::BuildStep::Custom(custom)) = graph.steps.get(step_idx)
                {
                    for dep in &custom.depends {
                        if let Some(&owner) = artifact_to_target.get(dep)
                            && owner != id
                            && !attached_bare_depends.contains(&owner)
                        {
                            add(owner, false, &Backtrace::default());
                        }
                    }
                }
            }
        }
        for step in graph.custom_source_steps.get(&id).into_iter().flatten() {
            let Some(command_id) = graph
                .custom_output_owners
                .get(step)
                .map(|owners| owners.command)
            else {
                continue;
            };
            let Some(command) = model
                .command(command_id)
                .filter(|command| command.dir == target.dir)
            else {
                continue;
            };
            match command.kind {
                CustomCommandKind::Utility(owner)
                | CustomCommandKind::TargetEvent { target: owner, .. }
                    if owner != id =>
                {
                    // A source that another target's command writes (`FollowName`).
                    add(owner, false, &Backtrace::default());
                }
                _ => {}
            }
            add_command_utilities(graph, command_id, &command.backtrace, &mut add);
            // Match `cmTargetTraceDependencies.cxx`: an absolute artifact path in
            // `DEPENDS` matching another target's output records `cross = false`,
            // while a bare target name in `DEPENDS` records `cross = true`.
            if let Some(crate::BuildStep::Custom(custom)) = graph.steps.get(*step) {
                let bare_depends = graph
                    .command_utilities
                    .get(&command_id)
                    .map(|utilities| &utilities.depends);
                for dep in &custom.depends {
                    if let Some(&owner) = artifact_to_target.get(dep)
                        && owner != id
                        && !bare_depends.is_some_and(|set| set.contains(&owner))
                    {
                        add(owner, false, &Backtrace::default());
                    }
                }
            }
        }
    }
    if let Some(reuse) = target
        .props
        .get(b"PRECOMPILE_HEADERS_REUSE_FROM")
        .and_then(|name| model.find_target(target.dir, name))
    {
        add(reuse, false, &Backtrace::default());
    }
    items
        .into_iter()
        .map(|((target, cross), backtrace)| UtilityItem {
            target,
            cross,
            backtrace,
        })
        .collect()
}

/// Add the utilities of one command: programs and generator expression targets
/// first, then bare `DEPENDS` targets without a backtrace (`CheckCustomCommand`).
fn add_command_utilities(
    graph: &BuildGraph,
    command: rcmake_model::CommandId,
    backtrace: &Backtrace,
    add: &mut impl FnMut(TargetId, bool, &Backtrace),
) {
    let Some(utilities) = graph.command_utilities.get(&command) else {
        return;
    };
    for &dependency in &utilities.commands {
        add(dependency, true, backtrace);
    }
    for &dependency in &utilities.depends {
        add(dependency, true, &Backtrace::default());
    }
}

/// Sources that the trace appends to a target (`cmTargetTraceDependencies`).
/// The trace looks up outputs only in the directory of the target.
fn traced_sources(
    model: &ProjectModel,
    id: TargetId,
    target: &Target,
    graphs: &[BuildGraph],
) -> Vec<BString> {
    let mut sources = IndexSet::new();
    for graph in graphs {
        for step in graph.custom_source_steps.get(&id).into_iter().flatten() {
            let Some(owners) = graph.custom_output_owners.get(step) else {
                continue;
            };
            let Some(command) = model
                .command(owners.command)
                .filter(|command| command.dir == target.dir)
            else {
                continue;
            };
            if let Some(main) = graph.custom_main_sources.get(step) {
                sources.insert(main.clone());
                continue;
            }
            let Some(crate::BuildStep::Custom(custom)) = graph.steps.get(*step) else {
                continue;
            };
            let (Some(output), false) = (custom.outputs.first(), command.outputs.is_empty()) else {
                continue;
            };
            let root = &model.dir(rcmake_model::DirId::ROOT).binary_dir;
            let mut rule = crate::sources::absolute_path(root, output);
            rule.extend_from_slice(b".rule");
            sources.insert(rule);
        }
    }
    sources.into_iter().collect()
}

/// Return the object library of an `Objects` item or of an object file path.
fn object_owner(item: &ResolvedLinkItem, paths: &SourcePaths<'_>) -> Option<TargetId> {
    match item.kind {
        ResolvedLinkKind::Objects(owner) => Some(owner),
        ResolvedLinkKind::ObjectFile => paths.object_owner(&item.text.value),
        _ => None,
    }
}

struct Collector<'a> {
    model: &'a ProjectModel,
    graph: &'a TargetDependGraph,
    paths: &'a SourcePaths<'a>,
    initial: Graph,
    /// Object libraries that `AddObjectDepends` adds as utilities of the depender.
    object_utilities: IndexMap<TargetId, Vec<TargetId>>,
    emitted: Emitted,
}

/// Epoch-stamped dense visited set for build-system nodes with a sparse fallback
/// for targets outside the build system (such as imported targets).
struct Emitted {
    normal_marks: Vec<u32>,
    cross_marks: Vec<u32>,
    epoch: u32,
    extra: IndexSet<(TargetId, bool)>,
}

impl Emitted {
    fn new(nodes: usize) -> Self {
        Self {
            normal_marks: vec![0; nodes],
            cross_marks: vec![0; nodes],
            epoch: 0,
            extra: IndexSet::new(),
        }
    }

    fn reset(&mut self) {
        self.epoch = self.epoch.wrapping_add(1);
        if self.epoch == 0 {
            self.normal_marks.fill(0);
            self.cross_marks.fill(0);
            self.epoch = 1;
        }
        self.extra.clear();
    }

    fn insert(&mut self, graph: &TargetDependGraph, id: TargetId, cross: bool) -> bool {
        if let Some(&node) = graph.index.get(&id) {
            let marks = if cross {
                &mut self.cross_marks
            } else {
                &mut self.normal_marks
            };
            if let Some(slot) = marks.get_mut(node) {
                if *slot == self.epoch {
                    return false;
                }
                *slot = self.epoch;
                return true;
            }
        }
        self.extra.insert((id, cross))
    }
}

impl Collector<'_> {
    fn in_build_system(&self, id: TargetId) -> bool {
        self.model
            .target(id)
            .is_some_and(|target| self.model.is_in_build_system(target))
    }

    fn executable_without_exports(&self, id: TargetId) -> bool {
        self.model.target(id).is_some_and(|target| {
            target.kind == TargetKind::Executable
                && !target
                    .props
                    .get(b"ENABLE_EXPORTS")
                    .is_some_and(|value| truth::is_on(value))
        })
    }

    fn links_objects(&self, id: TargetId) -> bool {
        self.model.target(id).is_some_and(|target| {
            matches!(
                target.kind,
                TargetKind::Executable
                    | TargetKind::StaticLibrary
                    | TargetKind::SharedLibrary
                    | TargetKind::ModuleLibrary
                    | TargetKind::ObjectLibrary
            )
        })
    }

    fn collect_target(
        &mut self,
        node: usize,
        id: TargetId,
        link_items: &LinkItems,
        usage: &mut UsageResolver<'_>,
    ) -> Result<(), ResolveError> {
        if !self.in_build_system(id) {
            return Ok(());
        }
        self.emitted.reset();
        self.emitted.insert(self.graph, id, false);
        self.emitted.insert(self.graph, id, true);
        for links in link_items.implementation.get(&id).into_iter().flatten() {
            for (dependency, backtrace) in &links.link {
                let dependency = (*dependency, backtrace);
                self.add_link_depend(node, id, dependency, &links.config, usage)?;
            }
            for &owner in &links.objects {
                self.add_object_depends(id, owner);
            }
            for (dependency, backtrace) in &links.compile {
                let dependency = (*dependency, backtrace);
                self.add_link_depend(node, id, dependency, &links.config, usage)?;
            }
            for &owner in link_items.external_objects.get(&id).into_iter().flatten() {
                self.add_object_depends(id, owner);
            }
        }
        self.emitted.reset();
        self.emitted.insert(self.graph, id, false);
        self.emitted.insert(self.graph, id, true);
        let objects = self.object_utilities.get(&id).cloned().unwrap_or_default();
        let extra = objects.into_iter().map(|target| UtilityItem {
            target,
            cross: false,
            backtrace: Backtrace::default(),
        });
        let items: Vec<_> = self
            .graph
            .utility_items(id)
            .iter()
            .cloned()
            .chain(extra)
            .collect();
        for item in &items {
            if self.emitted.insert(self.graph, item.target, item.cross) {
                self.add_target_depend(node, item.target, &item.backtrace, false, item.cross);
            }
        }
        Ok(())
    }

    fn add_link_depend(
        &mut self,
        node: usize,
        depender: TargetId,
        (dependency, backtrace): (TargetId, &Backtrace),
        config: &ConfigId,
        usage: &mut UsageResolver<'_>,
    ) -> Result<(), ResolveError> {
        if self.emitted.insert(self.graph, dependency, false) {
            self.add_target_depend(node, dependency, backtrace, true, false);
            self.add_interface_depends(node, depender, dependency, backtrace, config, usage)?;
        }
        Ok(())
    }

    /// `AddObjectDepends`: an object library that is not already a link item
    /// becomes a utility of the depender.
    fn add_object_depends(&mut self, depender: TargetId, owner: TargetId) {
        if self.links_objects(depender) && self.emitted.insert(self.graph, owner, false) {
            self.object_utilities
                .entry(depender)
                .or_default()
                .push(owner);
        }
    }

    fn add_interface_depends(
        &mut self,
        node: usize,
        depender: TargetId,
        dependee: TargetId,
        backtrace: &Backtrace,
        config: &ConfigId,
        usage: &mut UsageResolver<'_>,
    ) -> Result<(), ResolveError> {
        let ctx = EvalContext {
            config: config.clone(),
            traversal: UsageTraversal::Link,
            head: Some(handle(depender)),
            ..EvalContext::default()
        };
        if self.executable_without_exports(dependee) {
            return Ok(());
        }
        // Each frame is one link interface and the position of its next item.
        let mut stack = vec![(usage.direct_link_items(dependee, true, &ctx)?, 0usize)];
        let mut interface_objects = Vec::new();
        while let Some((items, position)) = stack.last_mut() {
            let Some(item) = items.get(*position).cloned() else {
                stack.pop();
                continue;
            };
            *position += 1;
            // `$<TARGET_OBJECTS>` in a link interface is an object item
            // (`cmLinkInterface::Objects`); a linked object library is a library.
            // Defer `AddObjectDepends` until after the library items so an object
            // library already visited as a link dependency does not become a utility.
            if item.kind == ResolvedLinkKind::ObjectFile {
                if let Some(owner) = object_owner(&item, self.paths) {
                    interface_objects.push(owner);
                }
                continue;
            }
            for dependency in item.targets() {
                if !self.emitted.insert(self.graph, dependency, false) {
                    continue;
                }
                self.add_target_depend(node, dependency, backtrace, true, false);
                if !self.executable_without_exports(dependency)
                    && self.model.target(dependency).is_some()
                {
                    stack.push((usage.direct_link_items(dependency, true, &ctx)?, 0));
                }
            }
        }
        for owner in interface_objects {
            self.add_object_depends(depender, owner);
        }
        Ok(())
    }

    fn add_target_depend(
        &mut self,
        node: usize,
        dependee: TargetId,
        backtrace: &Backtrace,
        linking: bool,
        cross: bool,
    ) {
        if linking && self.executable_without_exports(dependee) {
            return;
        }
        if !self.in_build_system(dependee) {
            // Follow the utilities of a target that is not in the build system.
            let mut pending: Vec<_> = self.graph.utility_items(dependee).iter().rev().collect();
            while let Some(item) = pending.pop() {
                if !self.emitted.insert(self.graph, item.target, item.cross) {
                    continue;
                }
                if self.in_build_system(item.target) {
                    self.push_edge(node, item.target, &item.backtrace, false, item.cross);
                } else {
                    pending.extend(self.graph.utility_items(item.target).iter().rev());
                }
            }
            return;
        }
        self.push_edge(node, dependee, backtrace, linking, cross);
    }

    fn push_edge(
        &mut self,
        node: usize,
        dependee: TargetId,
        backtrace: &Backtrace,
        linking: bool,
        cross: bool,
    ) {
        if let (Some(&to), Some(edges)) =
            (self.graph.index.get(&dependee), self.initial.get_mut(node))
        {
            edges.push(Edge {
                to,
                strong: !linking,
                cross,
                backtrace: backtrace.clone(),
            });
        }
    }
}

/// Apply `OPTIMIZE_DEPENDENCIES` to static and object libraries
/// (`ComputeIntermediateGraph`).
fn intermediate_graph(
    model: &ProjectModel,
    graph: &TargetDependGraph,
    targets: &[ResolvedTarget],
    graphs: &[BuildGraph],
    initial: &Graph,
) -> Option<Graph> {
    let optimized: Vec<bool> = graph
        .nodes
        .iter()
        .map(|&id| {
            model.target(id).is_some_and(|target| {
                matches!(
                    target.kind,
                    TargetKind::StaticLibrary | TargetKind::ObjectLibrary
                ) && target
                    .props
                    .get(b"OPTIMIZE_DEPENDENCIES")
                    .is_some_and(|value| truth::is_on(value))
            })
        })
        .collect();
    if !optimized.contains(&true) {
        return None;
    }
    let languages: Vec<IndexSet<BString>> = graph
        .nodes
        .iter()
        .map(|&id| {
            targets
                .iter()
                .filter(|target| target.id == id)
                .flat_map(|target| {
                    target
                        .compile_groups
                        .iter()
                        .map(|group| group.language.clone())
                })
                .collect()
        })
        .collect();
    let side_effects = SideEffects::collect(model, graph, graphs, initial, &languages);
    Some(
        initial
            .iter()
            .enumerate()
            .map(|(node, edges)| {
                if !optimized.get(node).copied().unwrap_or(false) {
                    return edges.clone();
                }
                let mut output = Vec::new();
                let mut emitted = IndexSet::new();
                for edge in edges {
                    if edge.strong {
                        output.push(edge.clone());
                        continue;
                    }
                    let Some(effects) = side_effects.get(edge.to) else {
                        continue;
                    };
                    let language_effects = languages
                        .get(node)
                        .into_iter()
                        .flatten()
                        .flat_map(|language| effects.languages.get(language).into_iter().flatten());
                    for &dependency in effects.custom.iter().chain(language_effects) {
                        if emitted.insert(dependency) {
                            output.push(Edge {
                                to: dependency,
                                strong: false,
                                cross: edge.cross,
                                backtrace: edge.backtrace.clone(),
                            });
                        }
                    }
                }
                output
            })
            .collect(),
    )
}

#[derive(Clone, Default)]
struct SideEffects {
    custom: IndexSet<usize>,
    languages: IndexMap<BString, IndexSet<usize>>,
}

impl SideEffects {
    fn collect(
        model: &ProjectModel,
        graph: &TargetDependGraph,
        graphs: &[BuildGraph],
        initial: &Graph,
        languages: &[IndexSet<BString>],
    ) -> Vec<Self> {
        let own: Vec<Self> = graph
            .nodes
            .iter()
            .enumerate()
            .map(|(node, &id)| {
                let mut effects = Self::default();
                let commands = model.commands().any(|(_, command)| match command.kind {
                    CustomCommandKind::TargetEvent { target, .. }
                    | CustomCommandKind::Utility(target) => target == id,
                    CustomCommandKind::Output => false,
                }) || graphs.iter().any(|graph| {
                    graph
                        .custom_source_steps
                        .get(&id)
                        .is_some_and(|steps| !steps.is_empty())
                });
                if commands {
                    effects.custom.insert(node);
                }
                for language in languages.get(node).into_iter().flatten() {
                    if !matches!(
                        language.as_slice(),
                        b"ASM"
                            | b"ASM-ATT"
                            | b"ASM_MASM"
                            | b"ASM_NASM"
                            | b"ASM_MARMASM"
                            | b"C"
                            | b"CSharp"
                            | b"CUDA"
                            | b"CXX"
                            | b"HIP"
                            | b"ISPC"
                            | b"OBJC"
                            | b"OBJCXX"
                            | b"RC"
                            | b"Swift"
                    ) {
                        effects
                            .languages
                            .entry(language.clone())
                            .or_default()
                            .insert(node);
                    }
                }
                effects
            })
            .collect();
        // Propagate along the initial graph in post order (`CollectSideEffectsForTarget`).
        let mut result: Vec<Option<Self>> = vec![None; own.len()];
        for start in 0..own.len() {
            let mut stack = vec![(start, 0usize)];
            let mut on_stack = IndexSet::from([start]);
            while let Some(&mut (node, ref mut next)) = stack.last_mut() {
                if result.get(node).is_some_and(Option::is_some) {
                    stack.pop();
                    continue;
                }
                let edges = initial.get(node).map_or(&[][..], Vec::as_slice);
                if let Some(edge) = edges.get(*next) {
                    *next += 1;
                    if result.get(edge.to).is_some_and(Option::is_none) && on_stack.insert(edge.to)
                    {
                        stack.push((edge.to, 0));
                    }
                    continue;
                }
                let mut effects = own.get(node).cloned().unwrap_or_default();
                for edge in edges {
                    if let Some(Some(dependency)) = result.get(edge.to) {
                        effects.custom.extend(dependency.custom.iter().copied());
                        for (language, targets) in &dependency.languages {
                            effects
                                .languages
                                .entry(language.clone())
                                .or_default()
                                .extend(targets.iter().copied());
                        }
                    }
                }
                if let Some(slot) = result.get_mut(node) {
                    *slot = Some(effects);
                }
                stack.pop();
            }
        }
        result.into_iter().map(Option::unwrap_or_default).collect()
    }
}

/// Strongly connected components (`cmComputeComponentGraph`, Tarjan).
struct Components {
    /// Members of each component, sorted by node index.
    members: Vec<Vec<usize>>,
    /// Component of each node.
    map: Vec<usize>,
    /// Inter-component edges with multiplicity (empty when `all_singletons` is true).
    edges: Vec<Vec<Edge>>,
    /// True when every strongly connected component has exactly one node (DAG).
    all_singletons: bool,
}

impl Components {
    fn compute(graph: &Graph) -> Self {
        const UNSET: usize = usize::MAX;
        let n = graph.len();
        let mut root = vec![0usize; n];
        let mut visit_index = vec![0usize; n];
        let mut component = vec![UNSET; n];
        let mut visited = vec![0usize; n];
        let mut members: Vec<Vec<usize>> = Vec::new();
        let mut walk = 0usize;
        for start in 0..n {
            if visited.get(start).copied().unwrap_or(1) != 0 {
                continue;
            }
            walk += 1;
            let mut counter = 0usize;
            let mut stack = Vec::new();
            let mut frames = vec![(start, 0usize)];
            enter(
                start,
                walk,
                &mut counter,
                &mut visited,
                &mut root,
                &mut visit_index,
                &mut component,
                &mut stack,
            );
            while let Some(&mut (node, ref mut next)) = frames.last_mut() {
                let edges = graph.get(node).map_or(&[][..], Vec::as_slice);
                if let Some(edge) = edges.get(*next) {
                    let j = edge.to;
                    let state = visited.get(j).copied().unwrap_or(walk);
                    if state > 0 && state < walk {
                        *next += 1;
                        continue;
                    }
                    if state == 0 {
                        enter(
                            j,
                            walk,
                            &mut counter,
                            &mut visited,
                            &mut root,
                            &mut visit_index,
                            &mut component,
                            &mut stack,
                        );
                        frames.push((j, 0));
                        continue;
                    }
                    *next += 1;
                    improve_root(node, j, &mut root, &visit_index, &component);
                    continue;
                }
                frames.pop();
                if root.get(node) == Some(&node) {
                    let index = members.len();
                    let mut list = Vec::new();
                    while let Some(member) = stack.pop() {
                        if let Some(slot) = component.get_mut(member) {
                            *slot = index;
                        }
                        if let Some(slot) = root.get_mut(member) {
                            *slot = node;
                        }
                        list.push(member);
                        if member == node {
                            break;
                        }
                    }
                    list.sort_unstable();
                    members.push(list);
                }
                if let Some(&mut (parent, ref mut next)) = frames.last_mut() {
                    *next += 1;
                    improve_root(parent, node, &mut root, &visit_index, &component);
                }
            }
        }
        let all_singletons = members.len() == n && members.iter().all(|list| list.len() == 1);
        let mut edges = vec![Vec::new(); members.len()];
        if !all_singletons {
            for (i, list) in graph.iter().enumerate() {
                let from = component.get(i).copied().unwrap_or(UNSET);
                for edge in list {
                    let to = component.get(edge.to).copied().unwrap_or(UNSET);
                    if from != to
                        && let Some(out) = edges.get_mut(from)
                    {
                        out.push(Edge { to, ..edge.clone() });
                    }
                }
            }
        }
        Self {
            members,
            map: component,
            edges,
            all_singletons,
        }
    }
}

#[expect(
    clippy::too_many_arguments,
    reason = "Tarjan state is split into parallel vectors."
)]
fn enter(
    node: usize,
    walk: usize,
    counter: &mut usize,
    visited: &mut [usize],
    root: &mut [usize],
    visit_index: &mut [usize],
    component: &mut [usize],
    stack: &mut Vec<usize>,
) {
    *counter += 1;
    if let Some(slot) = visited.get_mut(node) {
        *slot = walk;
    }
    if let Some(slot) = root.get_mut(node) {
        *slot = node;
    }
    if let Some(slot) = component.get_mut(node) {
        *slot = usize::MAX;
    }
    if let Some(slot) = visit_index.get_mut(node) {
        *slot = *counter;
    }
    stack.push(node);
}

fn improve_root(
    node: usize,
    dependee: usize,
    root: &mut [usize],
    visit_index: &[usize],
    component: &[usize],
) {
    if component.get(dependee) != Some(&usize::MAX) {
        return;
    }
    let (Some(&dependee_root), Some(&node_root)) = (root.get(dependee), root.get(node)) else {
        return;
    };
    if visit_index.get(dependee_root) < visit_index.get(node_root)
        && let Some(slot) = root.get_mut(node)
    {
        *slot = dependee_root;
    }
}

/// Reject a non-trivial component that is not only static libraries, or any
/// non-trivial component when `GLOBAL_DEPENDS_NO_CYCLES` is on
/// (`CheckComponents`).
fn check_components(
    model: &ProjectModel,
    nodes: &[TargetId],
    initial: &Graph,
    components: &Components,
) -> Result<(), ResolveError> {
    let no_cycles = model
        .global
        .props
        .get(b"GLOBAL_DEPENDS_NO_CYCLES")
        .is_some_and(|value| truth::is_on(value));
    for (index, members) in components.members.iter().enumerate() {
        if members.len() < 2 {
            continue;
        }
        let reason = if no_cycles {
            "The GLOBAL_DEPENDS_NO_CYCLES global property is enabled, so cyclic dependencies are not allowed even among static libraries."
        } else if members.iter().any(|&member| {
            nodes
                .get(member)
                .and_then(|&id| model.target(id))
                .is_none_or(|target| target.kind != TargetKind::StaticLibrary)
        }) {
            "At least one of these targets is not a STATIC_LIBRARY.  Cyclic dependencies are allowed only among static libraries."
        } else {
            continue;
        };
        return Err(cycle_error(
            model, nodes, initial, components, index, reason,
        ));
    }
    Ok(())
}

/// The message of `ComplainAboutBadComponent`. It lists the initial edges of
/// each member that stay inside the component.
fn cycle_error(
    model: &ProjectModel,
    nodes: &[TargetId],
    initial: &Graph,
    components: &Components,
    component: usize,
    reason: &str,
) -> ResolveError {
    use std::fmt::Write as _;
    let mut message = String::from(
        "The inter-target dependency graph contains the following strongly connected component (cycle):\n",
    );
    let name = |node: usize| nodes.get(node).and_then(|&id| model.target(id));
    for &member in components.members.get(component).into_iter().flatten() {
        let Some(depender) = name(member) else {
            continue;
        };
        let _ = writeln!(
            message,
            "  \"{}\" of type {}",
            depender.name,
            depender.kind.as_str()
        );
        for edge in initial.get(member).into_iter().flatten() {
            if components.map.get(edge.to) == Some(&component)
                && let Some(dependee) = name(edge.to)
            {
                let strength = if edge.strong { "strong" } else { "weak" };
                let _ = writeln!(message, "    depends on \"{}\" ({strength})", dependee.name);
            }
        }
    }
    message.push_str(reason);
    ResolveError::Message(BString::from(message))
}

/// Linearize each component and connect components (`ComputeFinalDepends`).
/// The error is the index of a component whose strong edges form a cycle.
fn final_graph(initial: &Graph, components: &Components) -> Result<Graph, usize> {
    let mut graph: Graph = vec![Vec::new(); initial.len()];
    let mut heads = vec![usize::MAX; components.members.len()];
    let mut tails = vec![usize::MAX; components.members.len()];
    for (index, members) in components.members.iter().enumerate() {
        let mut head = usize::MAX;
        let mut emitted = IndexSet::new();
        for &member in members.iter().rev() {
            let linked = intra_component(
                (initial, components, index),
                member,
                &mut head,
                &mut emitted,
                &mut graph,
                tails.get_mut(index),
            );
            if !linked {
                return Err(index);
            }
        }
        if let Some(slot) = heads.get_mut(index) {
            *slot = head;
        }
    }
    for (component, edges) in components.edges.iter().enumerate() {
        let Some(&tail) = tails.get(component) else {
            continue;
        };
        for edge in edges {
            if let (Some(&head), Some(out)) = (heads.get(edge.to), graph.get_mut(tail)) {
                out.push(Edge {
                    to: head,
                    ..edge.clone()
                });
            }
        }
    }
    Ok(graph)
}

/// Walk the strong edges inside one component from `root` in post order and
/// prepend each new node to the linked list that starts at `head`
/// (`IntraComponent`). The walk uses an explicit stack. Return `false` when the
/// walk reaches a node a second time, as CMake does.
fn intra_component(
    (initial, components, component): (&Graph, &Components, usize),
    root: usize,
    head: &mut usize,
    emitted: &mut IndexSet<usize>,
    graph: &mut Graph,
    mut tail: Option<&mut usize>,
) -> bool {
    let mut visited = IndexSet::new();
    let mut enter = |node: usize, emitted: &mut IndexSet<usize>| -> Option<bool> {
        if !visited.insert(node) {
            return None;
        }
        Some(emitted.insert(node))
    };
    // Each frame is a node and the position of its next initial edge.
    let mut frames = Vec::new();
    match enter(root, emitted) {
        None => return false,
        Some(true) => frames.push((root, 0usize)),
        Some(false) => {}
    }
    while let Some(&mut (node, ref mut next)) = frames.last_mut() {
        let edges = initial.get(node).map_or(&[][..], Vec::as_slice);
        if let Some(edge) = edges.get(*next) {
            *next += 1;
            if !edge.strong || components.map.get(edge.to) != Some(&component) {
                continue;
            }
            if let Some(out) = graph.get_mut(node) {
                out.push(edge.clone());
            }
            match enter(edge.to, emitted) {
                None => return false,
                Some(true) => frames.push((edge.to, 0)),
                Some(false) => {}
            }
            continue;
        }
        frames.pop();
        if *head == usize::MAX {
            if let Some(tail) = tail.as_deref_mut() {
                *tail = node;
            }
        } else if let Some(out) = graph.get_mut(node) {
            out.push(Edge {
                to: *head,
                strong: false,
                cross: false,
                backtrace: Backtrace::default(),
            });
        }
        *head = node;
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn edge(to: usize, strong: bool) -> Edge {
        Edge {
            to,
            strong,
            cross: false,
            backtrace: Backtrace::default(),
        }
    }

    #[test]
    fn linearization_of_a_very_deep_strong_chain_uses_no_recursion() {
        const COUNT: usize = 200_000;
        // Node `i` depends strongly on `i - 1`; node 0 closes the cycle weakly.
        let mut initial: Graph = (0..COUNT)
            .map(|node| {
                node.checked_sub(1)
                    .map(|dep| edge(dep, true))
                    .into_iter()
                    .collect()
            })
            .collect();
        if let Some(first) = initial.first_mut() {
            first.push(edge(COUNT - 1, false));
        }
        let components = Components::compute(&initial);
        assert_eq!(components.members.len(), 1);
        let Ok(graph) = final_graph(&initial, &components) else {
            panic!("the chain has no strong cycle");
        };
        assert!(graph.first().is_some_and(Vec::is_empty));
        for (node, edges) in graph.iter().enumerate().skip(1) {
            let targets: Vec<_> = edges.iter().map(|edge| (edge.to, edge.strong)).collect();
            assert_eq!(targets, [(node - 1, true), (node - 1, false)]);
        }
    }

    #[test]
    fn strong_diamond_inside_a_component_fails_as_in_cmake() {
        // `IntraComponent` keeps `visited` for the whole walk from one root.
        // The walk starts at the last member: 3 -> 2 -> 0, then 3 -> 1 -> 0.
        let initial: Graph = vec![
            vec![edge(3, false)],
            vec![edge(0, true)],
            vec![edge(0, true)],
            vec![edge(2, true), edge(1, true)],
        ];
        let components = Components::compute(&initial);
        assert_eq!(final_graph(&initial, &components).err(), Some(0));
    }

    fn make_target(
        model: &mut ProjectModel,
        name: &str,
        kind: TargetKind,
    ) -> Result<TargetId, Box<dyn std::error::Error>> {
        Ok(model.add_target(Target::new(
            name.into(),
            kind,
            rcmake_model::TargetOrigin::Normal,
            rcmake_model::DirId::ROOT,
            Backtrace::default(),
            rcmake_diag::PolicyMap::default(),
        ))?)
    }

    fn link_target(model: &mut ProjectModel, id: TargetId, dep: &str) {
        let Some(dir) = model.target(id).map(|target| target.dir) else {
            return;
        };
        if let Some(target) = model.target_mut(id) {
            target
                .usage
                .link_libraries
                .get_or_insert_default()
                .push(rcmake_model::LinkItem {
                    opens_lookup_scope: false,
                    closes_lookup_scope: false,
                    scope: None,
                    text: rcmake_model::BT::new(dep.into(), Backtrace::default()),
                    lookup_dir: dir,
                });
        }
    }

    fn add_utility_command(model: &mut ProjectModel, owner: TargetId) {
        assert!(
            model
                .add_command(rcmake_model::CustomCommand::new(
                    rcmake_model::DirId::ROOT,
                    CustomCommandKind::Utility(owner),
                    Backtrace::default(),
                    rcmake_diag::PolicyMap::default(),
                ))
                .is_ok()
        );
    }

    #[test]
    fn optimize_dependencies_preserves_strong_and_side_effect_edges_on_acyclic_graph()
    -> Result<(), Box<dyn std::error::Error>> {
        let mut model = ProjectModel::default();
        model.set_root_dirs("/src".into(), "/build".into());
        let codegen = make_target(&mut model, "codegen", TargetKind::Utility)?;
        let plain = make_target(&mut model, "plain", TargetKind::StaticLibrary)?;
        let gen_lib = make_target(&mut model, "gen_lib", TargetKind::StaticLibrary)?;
        add_utility_command(&mut model, gen_lib);
        let a = make_target(&mut model, "a", TargetKind::StaticLibrary)?;
        if let Some(target) = model.target_mut(a) {
            target
                .props
                .set("OPTIMIZE_DEPENDENCIES".into(), "ON".into());
            target.utility_deps.push(rcmake_model::BT::new(
                "codegen".into(),
                Backtrace::default(),
            ));
        }
        link_target(&mut model, a, "plain");
        link_target(&mut model, a, "gen_lib");

        let host = rcmake_host::PureHost::default();
        let configs = vec![ConfigId::default()];
        let paths = SourcePaths::new(&host);
        let mut usage = UsageResolver::new(&model);
        let depends = compute(&model, &configs, (&[], &[]), &paths, &mut usage)?;
        let a_deps: Vec<(TargetId, bool)> = depends
            .direct_depends(a)
            .iter()
            .map(|dep| (dep.target, dep.strong))
            .collect();
        assert!(
            a_deps.contains(&(codegen, true)),
            "strong add_dependencies edge a -> codegen must be preserved: {a_deps:?}"
        );
        assert!(
            a_deps.contains(&(gen_lib, false)),
            "side-effect link edge a -> gen_lib must be preserved: {a_deps:?}"
        );
        assert!(
            !a_deps.iter().any(|(target, _)| *target == plain),
            "side-effect-free static lib edge a -> plain should be optimized away: {a_deps:?}"
        );
        Ok(())
    }

    #[test]
    fn optimize_dependencies_drops_self_loops_on_singleton_components()
    -> Result<(), Box<dyn std::error::Error>> {
        let mut model = ProjectModel::default();
        model.set_root_dirs("/src".into(), "/build".into());
        let a = make_target(&mut model, "a", TargetKind::StaticLibrary)?;
        let b = make_target(&mut model, "b", TargetKind::StaticLibrary)?;
        add_utility_command(&mut model, a);
        for id in [a, b] {
            if let Some(target) = model.target_mut(id) {
                target
                    .props
                    .set("OPTIMIZE_DEPENDENCIES".into(), "ON".into());
            }
        }
        link_target(&mut model, a, "b");
        link_target(&mut model, b, "a");

        let host = rcmake_host::PureHost::default();
        let configs = vec![ConfigId::default()];
        let paths = SourcePaths::new(&host);
        let mut usage = UsageResolver::new(&model);
        let depends = compute(&model, &configs, (&[], &[]), &paths, &mut usage)?;
        let a_deps: Vec<TargetId> = depends
            .direct_depends(a)
            .iter()
            .map(|dep| dep.target)
            .collect();
        let b_deps: Vec<TargetId> = depends
            .direct_depends(b)
            .iter()
            .map(|dep| dep.target)
            .collect();
        assert!(
            !a_deps.contains(&a),
            "singleton optimized component must not retain self-loop a -> a: {a_deps:?}"
        );
        assert_eq!(b_deps, vec![a]);
        Ok(())
    }

    #[test]
    fn command_utilities_preserve_cross_flag_distinction() -> Result<(), Box<dyn std::error::Error>>
    {
        use rcmake_diag::PolicyMap;
        use rcmake_model::{CustomCommand, DirId, TargetOrigin};

        let mut model = ProjectModel::default();
        model.set_root_dirs("/src".into(), "/build".into());
        let tool = model.add_target(Target::new(
            "tool".into(),
            TargetKind::Executable,
            TargetOrigin::Normal,
            DirId::ROOT,
            Backtrace::default(),
            PolicyMap::default(),
        ))?;
        let bare = model.add_target(Target::new(
            "bare".into(),
            TargetKind::Executable,
            TargetOrigin::Normal,
            DirId::ROOT,
            Backtrace::default(),
            PolicyMap::default(),
        ))?;
        let app = model.add_target(Target::new(
            "app".into(),
            TargetKind::Utility,
            TargetOrigin::Normal,
            DirId::ROOT,
            Backtrace::default(),
            PolicyMap::default(),
        ))?;
        let cmd = model.add_command(CustomCommand::new(
            DirId::ROOT,
            CustomCommandKind::Utility(app),
            Backtrace::default(),
            PolicyMap::default(),
        ))?;
        let mut graph = BuildGraph::default();
        graph
            .steps
            .push(crate::BuildStep::Custom(crate::CustomStep {
                depends: vec!["bin/tool".into(), "bin/bare".into()],
                ..crate::CustomStep::default()
            }));
        graph.target_ids.push(app);
        graph.targets.push(crate::GraphTarget {
            name: "app".into(),
            steps: vec![0],
            depends: Vec::new(),
            in_all: false,
        });
        graph.command_utilities.insert(
            cmd,
            crate::CommandUtilities {
                commands: IndexSet::from([tool]),
                depends: IndexSet::from([bare]),
            },
        );
        let artifact_to_target = IndexMap::from([
            (BString::from("bin/tool"), tool),
            (BString::from("bin/bare"), bare),
        ]);
        let app_target = model.target(app).ok_or("missing app")?;
        let items = utility_items(
            &model,
            (app, app_target),
            &[cmd],
            &[graph],
            &artifact_to_target,
        );
        let flags: Vec<(TargetId, bool)> =
            items.iter().map(|item| (item.target, item.cross)).collect();
        // `COMMAND tool` -> cross = true; bare `DEPENDS bare` -> cross = true;
        // artifact path `bin/tool` in `DEPENDS` -> cross = false.
        assert_eq!(flags, vec![(tool, true), (bare, true), (tool, false)]);
        Ok(())
    }
}
