use std::collections::{BTreeSet, HashMap, HashSet, VecDeque};

use indexmap::{IndexMap, IndexSet};
use rcmake_base::{BString, list};
use rcmake_diag::{PolicyId, PolicyStatus};
use rcmake_genex::{EvalContext, EvalFlags, GenexError, UsageTraversal};
use rcmake_model::{BT, LinkItem, ProjectModel, TargetId, TargetKind};

use super::{Limits, UsageResolver, UsageValue, context, handle, host::definition};
use crate::ResolveError;

/// A link entry with its evaluated text, provenance, and link semantics.
#[derive(Clone, Debug, PartialEq)]
pub struct ResolvedLinkItem {
    /// Evaluated link text with the backtrace of the original entry.
    pub text: BT<BString>,
    /// How the entry contributes to the link step.
    pub kind: ResolvedLinkKind,
    /// A named `LINK_LIBRARY` feature, when present.
    pub feature: Option<BString>,
    /// Whether the item appears in the direct dependencies after injections.
    pub direct: bool,
}

/// The role of one resolved link item.
#[derive(Clone, Debug, PartialEq)]
pub enum ResolvedLinkKind {
    /// Link to an artifact of the target.
    Library(TargetId),
    /// Add object files from a directly linked object library.
    Objects(TargetId),
    /// Collect usage only.
    Interface(TargetId),
    /// An explicit object file path.
    ObjectFile,
    /// A library name, path, or flag supplied by the project.
    Raw,
    /// A set of items that the linker must process as a group.
    Group {
        /// The `LINK_GROUP` feature name.
        feature: BString,
        /// Members in link order.
        members: Vec<ResolvedLinkItem>,
    },
}

impl ResolvedLinkItem {
    /// Targets named by this entry, including members of a link group.
    pub fn targets(&self) -> Box<dyn Iterator<Item = TargetId> + '_> {
        match &self.kind {
            ResolvedLinkKind::Library(id)
            | ResolvedLinkKind::Objects(id)
            | ResolvedLinkKind::Interface(id) => Box::new(std::iter::once(*id)),
            ResolvedLinkKind::Raw | ResolvedLinkKind::ObjectFile => Box::new(std::iter::empty()),
            ResolvedLinkKind::Group { members, .. } => {
                Box::new(members.iter().flat_map(Self::targets))
            }
        }
    }

    fn key(&self) -> LinkKey {
        match &self.kind {
            ResolvedLinkKind::Library(id)
            | ResolvedLinkKind::Objects(id)
            | ResolvedLinkKind::Interface(id) => LinkKey::Target(*id),
            ResolvedLinkKind::Raw | ResolvedLinkKind::ObjectFile => {
                LinkKey::Raw(self.text.value.clone())
            }
            ResolvedLinkKind::Group { feature, members } => {
                LinkKey::Group(feature.clone(), members.iter().map(Self::key).collect())
            }
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum LinkKey {
    Target(TargetId),
    Raw(BString),
    Group(BString, Vec<Self>),
}

/// Collect direct and transitive link items in link order.
pub fn collect_link_items(
    model: &ProjectModel,
    target: TargetId,
    ctx: &EvalContext,
) -> Result<Vec<ResolvedLinkItem>, ResolveError> {
    UsageResolver::new(model).collect_link_items(target, ctx)
}

#[derive(Default)]
struct EvaluatedLinks {
    items: Vec<ResolvedLinkItem>,
    value: UsageValue,
}

impl UsageResolver<'_> {
    /// Collect direct and transitive link items in link order.
    pub fn collect_link_items(
        &mut self,
        target: TargetId,
        ctx: &EvalContext,
    ) -> Result<Vec<ResolvedLinkItem>, ResolveError> {
        let ctx = link_context(target, ctx);
        let (direct, _) = self.usage_links(target, false, &ctx)?;
        let mut graph = LinkGraph::default();
        let original: Vec<_> = direct
            .into_iter()
            .map(|item| (graph.insert(item.clone()), item))
            .collect();
        let mut index = 0;
        while let Some((_, (item, _))) = graph.nodes.get_index(index) {
            let parents: Vec<_> = item.targets().collect();
            let mut edges = IndexSet::new();
            for parent in parents {
                let (items, _) = self.usage_links(parent, true, &ctx)?;
                edges.extend(items.into_iter().map(|item| graph.insert(item)));
            }
            if let Some((_, (_, current_edges))) = graph.nodes.get_index_mut(index) {
                *current_edges = edges;
            }
            index += 1;
        }

        let original = graph.apply_groups(original);
        let mut ordered = graph.order(self.model, &ctx, original)?;
        deduplicate(self.model, target, &ctx, &mut ordered);
        // Explicit object inputs are placed before the libraries.
        let (objects, libraries): (Vec<_>, Vec<_>) = ordered
            .into_iter()
            .filter(|item| !matches!(item.kind, ResolvedLinkKind::Interface(_)))
            .partition(|item| {
                matches!(
                    item.kind,
                    ResolvedLinkKind::Objects(_) | ResolvedLinkKind::ObjectFile
                )
            });
        Ok(objects.into_iter().chain(libraries).collect())
    }

    pub(super) fn link_targets(
        &mut self,
        target: TargetId,
        ctx: &EvalContext,
    ) -> Result<Vec<TargetId>, ResolveError> {
        let ctx = link_context(target, ctx);
        let (direct, _) = self.usage_links(target, false, &ctx)?;
        let mut todo: VecDeque<_> = direct.iter().flat_map(ResolvedLinkItem::targets).collect();
        let mut seen = IndexSet::new();
        while let Some(id) = todo.pop_front() {
            if seen.insert(id) {
                let (links, _) = self.usage_links(id, true, &ctx)?;
                todo.extend(links.iter().flat_map(ResolvedLinkItem::targets));
            }
        }
        Ok(seen.into_iter().collect())
    }

    pub(super) fn usage_links(
        &mut self,
        owner: TargetId,
        interface: bool,
        ctx: &EvalContext,
    ) -> Result<(Vec<ResolvedLinkItem>, EvalFlags), ResolveError> {
        let explicit = self.explicit_links(owner, interface, ctx)?;
        self.effective_links(owner, interface, ctx, explicit.items, explicit.value.flags)
    }

    fn explicit_links(
        &mut self,
        owner: TargetId,
        interface: bool,
        ctx: &EvalContext,
    ) -> Result<EvaluatedLinks, ResolveError> {
        let target = self.target(owner)?;
        let (name, entries) = if interface {
            (
                b"INTERFACE_LINK_LIBRARIES".as_slice(),
                &target.usage.interface_link_libraries,
            )
        } else {
            (b"LINK_LIBRARIES".as_slice(), &target.usage.link_libraries)
        };
        self.evaluate_links(
            owner,
            name,
            entries.as_deref().unwrap_or(&[]),
            ctx,
            !interface,
        )
    }

    fn effective_links(
        &mut self,
        owner: TargetId,
        interface: bool,
        ctx: &EvalContext,
        mut links: Vec<ResolvedLinkItem>,
        mut flags: EvalFlags,
    ) -> Result<(Vec<ResolvedLinkItem>, EvalFlags), ResolveError> {
        if !interface && self.has_injections {
            let mut injection = Injection {
                emitted: links.iter().map(ResolvedLinkItem::key).collect(),
                flags,
                ..Injection::default()
            };
            for link in links {
                injection.follow(self, &link, ctx, 0)?;
                injection.output.push(link);
            }
            links = injection.output;
            links.retain(|item| !injection.excluded.contains(&item.key()));
            flags = injection.flags;
        }
        if links
            .iter()
            .flat_map(ResolvedLinkItem::targets)
            .any(|id| id == owner)
        {
            if interface {
                // Interface-only self edges do not contribute another usage value.
                links.retain(|item| !item.targets().any(|id| id == owner));
            } else {
                return Err(GenexError::SelfReference {
                    target: self.target(owner)?.name.clone(),
                }
                .into());
            }
        }
        Ok((links, flags))
    }

    fn evaluate_links(
        &mut self,
        owner: TargetId,
        name: &[u8],
        entries: &[LinkItem],
        ctx: &EvalContext,
        direct: bool,
    ) -> Result<EvaluatedLinks, ResolveError> {
        let mut output = EvaluatedLinks::default();
        let debug_configs = self
            .model
            .global
            .props
            .get(b"DEBUG_CONFIGURATIONS")
            .map(|value| list::split(value, list::Empty::Drop))
            .unwrap_or_default();
        let debug = if debug_configs.is_empty() {
            ctx.config.matches(b"Debug")
        } else {
            debug_configs.iter().any(|name| ctx.config.matches(name))
        };
        let mut take_next = true;
        for entry in entries {
            match entry.text.value.as_slice() {
                b"debug" => take_next = debug,
                b"optimized" => take_next = !debug,
                b"general" => take_next = true,
                _ => {
                    if take_next {
                        let (value, deps) =
                            self.eval_entry(owner, entry.lookup_dir, name, &entry.text, ctx, true)?;
                        output.value.flags = output.value.flags.union(deps);
                        let values = list::split(&value, list::Empty::Drop);
                        output
                            .items
                            .extend(self.decode_links(entry, &values, direct)?);
                        output.value.entries.extend(
                            values
                                .into_iter()
                                .map(|value| BT::new(value, entry.text.backtrace.clone())),
                        );
                    }
                    take_next = true;
                }
            }
        }
        Ok(output)
    }

    fn decode_links(
        &self,
        origin: &LinkItem,
        values: &[BString],
        direct: bool,
    ) -> Result<Vec<ResolvedLinkItem>, ResolveError> {
        let mut feature = None;
        let mut group: Option<ResolvedLinkItem> = None;
        let mut items = Vec::new();
        for value in values {
            let value = value.trim_ascii();
            if value.is_empty() {
                continue;
            }
            if let Some(name) = marker(value, b"<LINK_LIBRARY:") {
                feature = Some(name.into());
                continue;
            }
            if marker(value, b"</LINK_LIBRARY:").is_some() {
                feature = None;
                continue;
            }
            if let Some(spec) = marker(value, b"<LINK_GROUP:") {
                let name = spec.split(|&byte| byte == b':').next().unwrap_or_default();
                if group.is_some() {
                    return Err(ResolveError::Message(
                        "A link group cannot contain another link group.".into(),
                    ));
                }
                group = Some(ResolvedLinkItem {
                    text: BT::new(value.into(), origin.text.backtrace.clone()),
                    kind: ResolvedLinkKind::Group {
                        feature: name.into(),
                        members: Vec::new(),
                    },
                    feature: None,
                    direct,
                });
                continue;
            }
            if marker(value, b"</LINK_GROUP:").is_some() {
                if let Some(item) = group.take() {
                    items.push(item);
                }
                continue;
            }
            let kind = if let Some(id) = self.model.find_target(origin.lookup_dir, value) {
                match self.target(id)?.kind {
                    TargetKind::ObjectLibrary if direct => ResolvedLinkKind::Objects(id),
                    TargetKind::InterfaceLibrary | TargetKind::ObjectLibrary => {
                        ResolvedLinkKind::Interface(id)
                    }
                    _ => ResolvedLinkKind::Library(id),
                }
            } else if rcmake_base::path_lex::Lexical::new(value).absolute()
                && (value.ends_with(b".o") || value.ends_with(b".obj"))
            {
                ResolvedLinkKind::ObjectFile
            } else {
                ResolvedLinkKind::Raw
            };
            let item = ResolvedLinkItem {
                text: BT::new(value.into(), origin.text.backtrace.clone()),
                kind,
                feature: feature.clone(),
                direct,
            };
            if let Some(ResolvedLinkItem {
                kind: ResolvedLinkKind::Group { members, .. },
                ..
            }) = &mut group
            {
                members.push(item);
            } else {
                items.push(item);
            }
        }
        if group.is_some() {
            return Err(ResolveError::Message(
                "A link group is missing its end marker.".into(),
            ));
        }
        Ok(items)
    }

    pub(super) fn link_property(
        &mut self,
        owner: TargetId,
        name: &[u8],
        ctx: &EvalContext,
    ) -> Result<UsageValue, ResolveError> {
        self.flatten_links(
            owner,
            name == b"INTERFACE_LINK_LIBRARIES",
            ctx,
            &mut HashSet::new(),
            &mut 0,
        )
    }

    fn flatten_links(
        &mut self,
        owner: TargetId,
        interface: bool,
        ctx: &EvalContext,
        visited: &mut HashSet<TargetId>,
        work: &mut usize,
    ) -> Result<UsageValue, ResolveError> {
        if !visited.insert(owner) {
            return Ok(UsageValue::default());
        }
        let name = if interface {
            b"INTERFACE_LINK_LIBRARIES".as_slice()
        } else {
            b"LINK_LIBRARIES".as_slice()
        };
        self.with_property(owner, name, |this| {
            let explicit = this.explicit_links(owner, interface, ctx)?;
            let mut result = explicit.value;
            let (items, flags) =
                this.effective_links(owner, interface, ctx, explicit.items, result.flags)?;
            result.flags = flags;
            for item in &result.entries {
                *work = work.saturating_add(item.value.len() + 1);
            }
            if *work > Limits::default().max_value_bytes {
                return Err(ResolveError::Message(
                    "The link property exceeds the size limit.".into(),
                ));
            }
            for id in items.iter().flat_map(ResolvedLinkItem::targets) {
                let value = this.flatten_links(id, true, ctx, visited, work)?;
                result.flags = result.flags.union(value.flags);
                result.entries.extend(value.entries);
            }
            Ok(result)
        })
    }
}

fn link_context(owner: TargetId, ctx: &EvalContext) -> EvalContext {
    let mut ctx = context(owner, ctx, UsageTraversal::Link);
    ctx.head.get_or_insert(handle(owner));
    ctx
}

fn marker<'a>(value: &'a [u8], prefix: &[u8]) -> Option<&'a [u8]> {
    value.strip_prefix(prefix)?.strip_suffix(b">")
}

pub(super) const DIRECT: &[u8] = b"INTERFACE_LINK_LIBRARIES_DIRECT";
pub(super) const EXCLUDE: &[u8] = b"INTERFACE_LINK_LIBRARIES_DIRECT_EXCLUDE";

#[derive(Default)]
struct Injection {
    visited: HashSet<TargetId>,
    emitted: HashSet<LinkKey>,
    excluded: HashSet<LinkKey>,
    output: Vec<ResolvedLinkItem>,
    flags: EvalFlags,
}

impl Injection {
    fn follow(
        &mut self,
        resolver: &mut UsageResolver<'_>,
        item: &ResolvedLinkItem,
        ctx: &EvalContext,
        depth: usize,
    ) -> Result<(), ResolveError> {
        if depth >= super::MAX_WALK_DEPTH {
            return Err(GenexError::TooDeep.into());
        }
        for id in item.targets() {
            if !self.visited.insert(id) {
                continue;
            }
            let target = resolver.target(id)?;
            for property in [DIRECT, EXCLUDE] {
                let Some(raw) = target.props.get(property) else {
                    continue;
                };
                let raw = LinkItem {
                    text: BT::new(raw.clone(), target.backtrace.clone()),
                    lookup_dir: target.dir,
                };
                let links = resolver.evaluate_links(id, property, &[raw], ctx, true)?;
                self.flags = self.flags.union(links.value.flags);
                for link in links.items {
                    if property == EXCLUDE {
                        self.excluded.insert(link.key());
                    } else if self.emitted.insert(link.key()) {
                        self.follow(resolver, &link, ctx, depth + 1)?;
                        self.output.push(link);
                    }
                }
            }
            let (links, flags) = resolver.usage_links(id, true, ctx)?;
            self.flags = self.flags.union(flags);
            for link in links {
                self.follow(resolver, &link, ctx, depth + 1)?;
            }
        }
        Ok(())
    }
}

type LinkNode = (ResolvedLinkItem, IndexSet<usize>);

#[derive(Default)]
struct LinkGraph {
    nodes: IndexMap<LinkKey, LinkNode>,
}

#[expect(
    clippy::indexing_slicing,
    reason = "Node and component indices come from the graph and its component table."
)]
impl LinkGraph {
    fn insert(&mut self, item: ResolvedLinkItem) -> usize {
        let entry = self.nodes.entry(item.key());
        let index = entry.index();
        entry.or_insert((item, IndexSet::new()));
        index
    }

    fn apply_groups(
        &mut self,
        mut direct: Vec<(usize, ResolvedLinkItem)>,
    ) -> Vec<(usize, ResolvedLinkItem)> {
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
        for (index, item) in &mut direct {
            if let Some(group) = replacements.get(index) {
                *index = *group;
                *item = self.nodes[*group].0.clone();
                item.direct = true;
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
        direct
    }

    fn order(
        &self,
        model: &ProjectModel,
        ctx: &EvalContext,
        direct: Vec<(usize, ResolvedLinkItem)>,
    ) -> Result<Vec<ResolvedLinkItem>, ResolveError> {
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
        let mut order = Vec::new();
        let mut pending = BTreeSet::new();
        for (index, item) in direct {
            visit(index, &component_ids, &mut components, &mut pending);
            order.push(item);
        }
        while let Some(id) = pending.first().copied() {
            let Some(index) = components[id].remaining.first().copied() else {
                return Err(ResolveError::Message(
                    "A pending link component has no members.".into(),
                ));
            };
            visit(index, &component_ids, &mut components, &mut pending);
            order.push(self.nodes[index].0.clone());
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
        let mut visited = HashSet::new();
        let mut finished = Vec::new();
        for index in roots.chain((0..count).rev()) {
            let mut stack = vec![(index, false)];
            while let Some((node, done)) = stack.pop() {
                if done {
                    finished.push(node);
                } else if visited.insert(node) {
                    stack.push((node, true));
                    stack.extend(self.nodes[node].1.iter().map(|&edge| (edge, false)));
                }
            }
        }
        let mut ids = vec![usize::MAX; count];
        let mut components = Vec::new();
        for index in finished.into_iter().rev() {
            if ids[index] != usize::MAX {
                continue;
            }
            let id = components.len();
            let mut members = Vec::new();
            let mut stack = vec![index];
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
        self.remaining = self.members.iter().copied().collect();
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
    items: &mut Vec<ResolvedLinkItem>,
) {
    let Some(target) = model.target(id) else {
        return;
    };
    let policy = |number| target.policies.policies.get(PolicyId::new(number)) == PolicyStatus::New;
    let platform = definition(model, target.dir, b"CMAKE_SYSTEM_NAME").unwrap_or_default();
    let windows = platform.starts_with(b"Windows");
    let apple = matches!(
        platform.as_slice(),
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
    let settings = processing
        .as_ref()
        .map(|value| list::split(value, list::Empty::Drop));
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
            TargetKind::StaticLibrary if all => Some(forward || policy(179)),
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
