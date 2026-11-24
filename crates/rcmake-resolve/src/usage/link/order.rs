use std::collections::{BTreeSet, HashMap, HashSet};
use std::sync::Arc;

use indexmap::{IndexMap, IndexSet};
use rcmake_base::{BString, list};
use rcmake_diag::{PolicyId, PolicyStatus};
use rcmake_genex::EvalContext;
use rcmake_model::{ProjectModel, TargetId, TargetKind};

use super::{Limits, LinkKey, ResolvedLinkItem, ResolvedLinkKind, UsageResolver, link_context};
use crate::{ResolveError, lower::definition};

impl UsageResolver<'_> {
    pub(crate) fn ordered_links(
        &mut self,
        target: TargetId,
        ctx: &EvalContext,
    ) -> Result<Vec<Arc<ResolvedLinkItem>>, ResolveError> {
        let ctx = link_context(target, ctx);
        let strategy = self
            .model
            .target(target)
            .and_then(|target| target.props.get(b"LINK_LIBRARIES_STRATEGY"));
        let strategy = match strategy.map(|value| value.as_slice()) {
            None | Some(b"REORDER_MINIMALLY") => LinkOrder::Minimal,
            Some(b"REORDER_FREELY") => LinkOrder::Free,
            Some(value) => {
                let body = BString::from(format!(
                    "LINK_LIBRARIES_STRATEGY value '{}' is not recognized.",
                    BString::from(value)
                ));
                if let Some(paths) = self.source_paths()
                    && let Some(target) = self.model.target(target)
                {
                    return Err(paths.target_error(self.model, target, body));
                }
                return Err(ResolveError::Message(body));
            }
        };
        let (direct, _) = self.usage_links(target, false, &ctx)?;
        let mut graph = LinkGraph::default();
        let mut original: Vec<_> = direct
            .iter()
            .map(|item| (graph.insert(item), Arc::clone(item)))
            .collect();
        let mut index = 0;
        while let Some((_, (item, _))) = graph.nodes.get_index(index) {
            let item = Arc::clone(item);
            let mut edges = IndexSet::new();
            for parent in item.edge_targets() {
                let (items, _) = self.usage_links(parent, true, &ctx)?;
                edges.extend(items.iter().map(|item| graph.insert(item)));
            }
            if let Some((_, (_, current_edges))) = graph.nodes.get_index_mut(index) {
                *current_edges = edges;
            }
            index += 1;
        }

        graph.apply_groups(&mut original);
        let mut ordered = graph.order(self.model, &ctx, original, strategy)?;
        deduplicate(self.model, target, &ctx, &mut ordered);
        // Explicit object inputs are placed before the libraries.
        ordered.retain(|item| !matches!(item.kind, ResolvedLinkKind::Interface(_)));
        ordered.sort_by_key(|item| {
            !matches!(
                item.kind,
                ResolvedLinkKind::Objects(_) | ResolvedLinkKind::ObjectFile
            )
        });
        Ok(ordered)
    }
}

type LinkNode = (Arc<ResolvedLinkItem>, IndexSet<usize>);

enum LinkOrder {
    Minimal,
    Free,
}

#[derive(Default)]
struct LinkGraph {
    nodes: IndexMap<LinkKey, LinkNode>,
}

#[expect(
    clippy::indexing_slicing,
    reason = "Node and component indices come from the graph and its component table."
)]
impl LinkGraph {
    fn insert(&mut self, item: &Arc<ResolvedLinkItem>) -> usize {
        let entry = self.nodes.entry(item.key());
        let index = entry.index();
        entry.or_insert_with(|| (Arc::clone(item), IndexSet::new()));
        index
    }

    fn apply_groups(&mut self, direct: &mut [(usize, Arc<ResolvedLinkItem>)]) {
        let mut replacements = HashMap::new();
        for (index, (_, (item, _))) in self.nodes.iter().enumerate() {
            if let ResolvedLinkKind::Group { members, .. } = &item.kind {
                for member in members {
                    if let Some(member) = self.nodes.get_index_of(&member.key()) {
                        replacements.entry(member).or_insert(index);
                    }
                }
            }
        }
        if replacements.is_empty() {
            for (index, (_, edges)) in self.nodes.values_mut().enumerate() {
                edges.shift_remove(&index);
            }
            return;
        }
        for (index, item) in direct {
            if let Some(group) = replacements.get(index) {
                *index = *group;
                *item = Arc::clone(&self.nodes[*group].0);
                Arc::make_mut(item).direct = true;
            }
        }
        for (index, (_, edges)) in self.nodes.values_mut().enumerate() {
            let original = std::mem::take(edges);
            for next in original {
                let next = replacements.get(&next).copied().unwrap_or(next);
                if index != next {
                    edges.insert(next);
                }
            }
        }
    }

    fn order(
        &self,
        model: &ProjectModel,
        ctx: &EvalContext,
        direct: Vec<(usize, Arc<ResolvedLinkItem>)>,
        strategy: LinkOrder,
    ) -> Result<Vec<Arc<ResolvedLinkItem>>, ResolveError> {
        let (component_ids, mut components) =
            self.components(direct.iter().rev().map(|(index, _)| *index));
        for (index, (_, (_, edges))) in self.nodes.iter().enumerate() {
            let id = component_ids[index];
            for &next in edges {
                let next = component_ids[next];
                if id != next {
                    components[id].edges.insert(next);
                }
            }
        }
        for component in &mut components {
            if component.members.len() > 1 {
                component.passes = 2;
                for &index in &component.members {
                    let (item, _) = &self.nodes[index];
                    match item.kind {
                        ResolvedLinkKind::Group { .. } => {
                            return Err(ResolveError::Message(
                                "A link group has a circular dependency.".into(),
                            ));
                        }
                        ResolvedLinkKind::Library(id) => {
                            if let Some(target) = model.target(id) {
                                let property = if target.is_imported() {
                                    "IMPORTED_LINK_INTERFACE_MULTIPLICITY"
                                } else {
                                    "LINK_INTERFACE_MULTIPLICITY"
                                };
                                let configuration =
                                    BString::from(ctx.config.0.to_ascii_uppercase());
                                let count = target
                                    .props
                                    .get(format!("{property}_{configuration}").as_bytes())
                                    .or_else(|| target.props.get(property.as_bytes()))
                                    .and_then(|value| std::str::from_utf8(value).ok())
                                    .and_then(|value| value.parse::<usize>().ok())
                                    .unwrap_or_default();
                                component.passes = component.passes.max(count);
                            }
                        }
                        _ => {}
                    }
                }
            }
        }

        // Edges point from a library to the libraries that must follow it.
        // The component identifiers are in topological order.
        let mut order = Vec::with_capacity(self.nodes.len().max(direct.len()));
        let mut pending = BTreeSet::new();
        match strategy {
            LinkOrder::Minimal => {
                for (index, item) in direct {
                    visit(index, &component_ids, &mut components, &mut pending);
                    order.push(item);
                }
            }
            LinkOrder::Free => {
                for (index, _) in direct {
                    let id = component_ids[index];
                    if pending.insert(id) {
                        components[id].reset();
                    }
                }
            }
        }
        while let Some(id) = pending.first().copied() {
            let Some(index) = components[id].remaining.first().copied() else {
                return Err(ResolveError::Message(
                    "A pending link component has no members.".into(),
                ));
            };
            visit(index, &component_ids, &mut components, &mut pending);
            order.push(Arc::clone(&self.nodes[index].0));
            if order.len() > Limits::default().max_value_bytes / 64 {
                return Err(ResolveError::Message(
                    "The link line exceeds the size limit.".into(),
                ));
            }
        }
        Ok(order)
    }

    fn components(&self, roots: impl Iterator<Item = usize>) -> (Vec<usize>, Vec<Component>) {
        let count = self.nodes.len();
        let mut reverse = vec![Vec::new(); count];
        for (id, (_, (_, edges))) in self.nodes.iter().enumerate() {
            for &next in edges {
                reverse[next].push(id);
            }
        }

        // Find strongly connected components with two graph passes. Use
        // explicit stacks so large acyclic link graphs do not use the call stack.
        let mut visited = vec![false; count];
        let mut finished = Vec::with_capacity(count);
        let mut stack = Vec::new();
        for index in roots.chain((0..count).rev()) {
            stack.push((index, false));
            while let Some((node, done)) = stack.pop() {
                if done {
                    finished.push(node);
                } else if !std::mem::replace(&mut visited[node], true) {
                    stack.push((node, true));
                    stack.extend(self.nodes[node].1.iter().map(|&edge| (edge, false)));
                }
            }
        }
        let mut ids = vec![usize::MAX; count];
        let mut components = Vec::new();
        let mut stack = Vec::new();
        for index in finished.into_iter().rev() {
            if ids[index] != usize::MAX {
                continue;
            }
            let id = components.len();
            let mut members = Vec::new();
            stack.push(index);
            while let Some(node) = stack.pop() {
                if ids[node] == usize::MAX {
                    ids[node] = id;
                    members.push(node);
                    stack.extend(&reverse[node]);
                }
            }
            members.sort_unstable();
            components.push(Component {
                members,
                edges: IndexSet::new(),
                passes: 1,
                left: 0,
                remaining: IndexSet::new(),
            });
        }
        (ids, components)
    }
}

struct Component {
    members: Vec<usize>,
    edges: IndexSet<usize>,
    passes: usize,
    left: usize,
    remaining: IndexSet<usize>,
}

impl Component {
    fn reset(&mut self) {
        self.left = self.passes;
        self.remaining.clear();
        self.remaining.extend(self.members.iter().copied());
    }
}

#[expect(
    clippy::indexing_slicing,
    reason = "The pending set and edges contain component indices from the component table."
)]
fn visit(item: usize, ids: &[usize], components: &mut [Component], pending: &mut BTreeSet<usize>) {
    let id = ids[item];
    let component = &mut components[id];
    if pending.insert(id) {
        component.reset();
    }
    component.remaining.shift_remove(&item);
    if !component.remaining.is_empty() {
        return;
    }
    component.left -= 1;
    if component.left > 0 {
        component
            .remaining
            .extend(component.members.iter().copied());
        return;
    }
    pending.remove(&id);
    let edges = component.edges.clone();
    for next in edges {
        pending.insert(next);
        components[next].reset();
    }
}

fn deduplicate(
    model: &ProjectModel,
    id: TargetId,
    ctx: &EvalContext,
    items: &mut Vec<Arc<ResolvedLinkItem>>,
) {
    let Some(target) = model.target(id) else {
        return;
    };
    let policy = |number| target.policies.policies.get(PolicyId::new(number)) == PolicyStatus::New;
    let platform = definition(model, target.dir, b"CMAKE_SYSTEM_NAME")
        .map_or(b"".as_slice(), |value| value.as_slice());
    let windows = platform.starts_with(b"Windows");
    let apple = matches!(
        platform,
        b"Darwin" | b"iOS" | b"tvOS" | b"watchOS" | b"visionOS"
    ) || (platform.is_empty() && cfg!(target_os = "macos"));
    let processing = ctx
        .link_language
        .as_ref()
        .or(ctx.language.as_ref())
        .and_then(|language| {
            definition(
                model,
                target.dir,
                format!("CMAKE_{language}_LINK_LIBRARIES_PROCESSING").as_bytes(),
            )
        });
    let settings = processing.map(|value| list::split(value, list::Empty::Drop));
    let setting = |name: &[u8]| {
        settings
            .as_ref()
            .and_then(|items| items.iter().find_map(|item| item.strip_prefix(name)))
    };
    let all =
        policy(156) && setting(b"DEDUPLICATION=").map_or(apple || windows, |value| value == b"ALL");
    let forward = policy(156) && setting(b"ORDER=").map_or(windows, |value| value == b"FORWARD");
    let keep_first = |item: &ResolvedLinkItem| match item.kind {
        ResolvedLinkKind::Objects(_)
        | ResolvedLinkKind::ObjectFile
        | ResolvedLinkKind::Group { .. } => Some(true),
        ResolvedLinkKind::Library(id) => match model.target(id)?.kind {
            TargetKind::StaticLibrary if all => Some(policy(179)),
            TargetKind::SharedLibrary
                if !policy(156) || setting(b"DEDUPLICATION=") != Some(b"NONE") =>
            {
                Some(forward)
            }
            _ => None,
        },
        ResolvedLinkKind::Raw if all => {
            let text = &item.text.value;
            (!text.starts_with(b"-") || text.starts_with(b"-l") || text.starts_with(b"-framework "))
                .then_some(forward)
        }
        _ => None,
    };

    let mut first = HashSet::new();
    let mut last = HashSet::new();
    let mut keep = vec![true; items.len()];
    for (keep, item) in keep.iter_mut().zip(items.iter()) {
        if keep_first(item) == Some(true) {
            *keep = first.insert(item.key());
        }
    }
    for (keep, item) in keep.iter_mut().zip(items.iter()).rev() {
        if keep_first(item) == Some(false) {
            *keep = last.insert(item.key());
        }
    }
    let mut keep = keep.into_iter();
    items.retain(|_| keep.next().unwrap_or_default());
}
