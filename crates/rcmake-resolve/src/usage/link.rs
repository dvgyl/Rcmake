use std::collections::HashSet;
use std::sync::Arc;

use rcmake_base::{BString, list};
use rcmake_genex::{ConfigId, EvalContext, EvalFlags, GenexError, UsageTraversal};
use rcmake_model::{BT, LinkItem, ProjectModel, Target, TargetId, TargetKind, UsageScope};

use super::{Limits, UsageResolver, UsageValue, context, handle};
use crate::ResolveError;

mod language;
mod order;

/// A link entry with its evaluated text, provenance, and link semantics.
#[derive(Clone, Debug)]
pub struct ResolvedLinkItem {
    /// Evaluated link text with the backtrace of the original entry.
    pub text: BT<BString>,
    /// How the entry contributes to the link step.
    pub kind: ResolvedLinkKind,
    /// A named `LINK_LIBRARY` feature, when present.
    pub feature: Option<BString>,
    /// Whether the item appears in the direct dependencies after injections.
    pub direct: bool,
    /// Scope of the original dependency edge.
    pub scope: UsageScope,
    /// Target that injected this item through INTERFACE_LINK_LIBRARIES_DIRECT.
    pub injected_by: Option<TargetId>,
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

impl PartialEq for ResolvedLinkItem {
    fn eq(&self, other: &Self) -> bool {
        self.text == other.text
            && self.kind == other.kind
            && self.feature == other.feature
            && self.direct == other.direct
    }
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

    pub(super) fn edge_targets(&self) -> impl DoubleEndedIterator<Item = TargetId> + '_ {
        self.edges().map(|(target, _)| target)
    }

    pub(super) fn hop(&self, from: TargetId) -> crate::Hop {
        crate::Hop {
            from,
            via: self.scope,
            link_bt: self.text.backtrace.clone(),
        }
    }

    pub(super) fn edges(&self) -> impl DoubleEndedIterator<Item = (TargetId, &Self)> + '_ {
        // Decoded groups have only one level of members.
        let items = match &self.kind {
            ResolvedLinkKind::Group { members, .. } => members.as_slice(),
            _ => std::slice::from_ref(self),
        };
        items.iter().filter_map(|item| match item.kind {
            ResolvedLinkKind::Library(id)
            | ResolvedLinkKind::Objects(id)
            | ResolvedLinkKind::Interface(id) => Some((id, item)),
            _ => None,
        })
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

pub(super) type SharedLinks = (Arc<[Arc<ResolvedLinkItem>]>, EvalFlags);

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub(super) struct LinkMemoKey {
    owner: TargetId,
    interface: bool,
    debug: bool,
    traversal: UsageTraversal,
}

impl<'a> UsageResolver<'a> {
    /// Collect direct and transitive link items in link order.
    pub fn collect_link_items(
        &mut self,
        target: TargetId,
        ctx: &EvalContext,
    ) -> Result<Vec<ResolvedLinkItem>, ResolveError> {
        Ok(self
            .ordered_links(target, ctx)?
            .into_iter()
            .map(Arc::unwrap_or_clone)
            .collect())
    }

    /// Visit all implementation and interface link targets in breadth-first order.
    pub fn reachable_link_targets(
        &mut self,
        target: TargetId,
        ctx: &EvalContext,
    ) -> Result<Vec<TargetId>, ResolveError> {
        let ctx = link_context(target, ctx);
        let mut seen = HashSet::from([target]);
        let mut pending = vec![target];
        let mut i = 0;
        while let Some(&id) = pending.get(i) {
            for interface in [false, true] {
                let (items, _) = self.usage_links(id, interface, &ctx)?;
                pending.extend(
                    items
                        .iter()
                        .flat_map(|item| item.edge_targets())
                        .filter(|id| seen.insert(*id)),
                );
            }
            i += 1;
        }
        pending.remove(0);
        Ok(pending)
    }

    /// Read the direct implementation links for one consumer and configuration.
    pub(crate) fn direct_links(
        &mut self,
        target: TargetId,
        ctx: &EvalContext,
    ) -> Result<SharedLinks, ResolveError> {
        self.usage_links(target, false, &link_context(target, ctx))
    }

    /// Read the direct entries of a target's link implementation or interface.
    pub fn direct_link_items(
        &mut self,
        target: TargetId,
        interface: bool,
        ctx: &EvalContext,
    ) -> Result<Arc<[Arc<ResolvedLinkItem>]>, ResolveError> {
        let mut ctx = context(target, ctx, ctx.traversal);
        ctx.head.get_or_insert(handle(target));
        let (items, _) = self.usage_links(target, interface, &ctx)?;
        Ok(items)
    }

    pub(crate) fn graphviz_interface_links(
        &mut self,
        target: TargetId,
        ctx: &EvalContext,
    ) -> Result<SharedLinks, ResolveError> {
        self.usage_links(target, true, ctx)
    }

    /// Return the configuration requested by a computed target LOCATION property.
    #[must_use]
    pub fn location_property_config(kind: TargetKind, property: &[u8]) -> Option<ConfigId> {
        if !matches!(
            kind,
            TargetKind::Executable
                | TargetKind::StaticLibrary
                | TargetKind::SharedLibrary
                | TargetKind::ModuleLibrary
                | TargetKind::UnknownLibrary
        ) || property == b"IMPORTED_LOCATION"
            || property.starts_with(b"XCODE_ATTRIBUTE_")
        {
            return None;
        }
        let config = if property == b"LOCATION" {
            b"".as_slice()
        } else {
            property
                .strip_prefix(b"LOCATION_")
                .or_else(|| property.strip_suffix(b"_LOCATION"))?
        };
        Some(ConfigId(config.into()))
    }

    /// Select the imported runtime location for a computed LOCATION property.
    pub fn imported_runtime_location(&self, target: &Target, config: &ConfigId) -> Option<BString> {
        let selected = super::artifacts::selected_imported_config(target, config)?;
        let use_generic = target
            .policies
            .policies
            .get(rcmake_diag::PolicyId::new(200))
            == rcmake_diag::PolicyStatus::New
            || super::artifacts::imported_from_cps(target);
        let lookup = |property: &[u8]| {
            let configured = [property, b"_", &selected].concat();
            let key = if selected.is_empty() {
                property
            } else {
                &configured
            };
            target
                .props
                .get(key)
                .filter(|path| !path.is_empty())
                .or_else(|| {
                    use_generic
                        .then(|| target.props.get(property))
                        .flatten()
                        .filter(|path| !path.is_empty())
                })
        };
        lookup(b"IMPORTED_LOCATION")
            .or_else(|| {
                (target.kind == TargetKind::SharedLibrary
                    || (target.kind == TargetKind::Executable
                        && super::on(target, b"ENABLE_EXPORTS")))
                .then(|| lookup(b"IMPORTED_IMPLIB"))
                .flatten()
            })
            .cloned()
    }

    pub(crate) fn link_targets(
        &mut self,
        target: TargetId,
        ctx: &EvalContext,
    ) -> Result<Vec<TargetId>, ResolveError> {
        let ctx = link_context(target, ctx);
        let (direct, _) = self.direct_links(target, &ctx)?;
        let mut seen = HashSet::new();
        let mut targets: Vec<_> = direct
            .iter()
            .flat_map(|item| item.edge_targets())
            .filter(|id| seen.insert(*id))
            .collect();
        let mut index = 0;
        while let Some(&id) = targets.get(index) {
            let (links, _) = self.usage_links(id, true, &ctx)?;
            targets.extend(
                links
                    .iter()
                    .flat_map(|item| item.edge_targets())
                    .filter(|id| seen.insert(*id)),
            );
            index += 1;
        }
        Ok(targets)
    }

    pub(crate) fn compile_targets(
        &mut self,
        target: TargetId,
        ctx: &EvalContext,
    ) -> Result<Vec<TargetId>, ResolveError> {
        let mut ctx = context(target, ctx, UsageTraversal::Compile);
        ctx.head.get_or_insert(handle(target));
        let (direct, _) = self.usage_links(target, false, &ctx)?;
        let mut seen = HashSet::new();
        let mut targets: Vec<_> = direct
            .iter()
            .flat_map(|item| item.edge_targets())
            .filter(|id| seen.insert(*id))
            .collect();
        let mut index = 0;
        while let Some(&id) = targets.get(index) {
            let (links, _) = self.usage_links(id, true, &ctx)?;
            targets.extend(
                links
                    .iter()
                    .flat_map(|item| item.edge_targets())
                    .filter(|id| seen.insert(*id)),
            );
            index += 1;
        }
        Ok(targets)
    }

    pub(super) fn compatibility_targets(
        &mut self,
        target: TargetId,
        ctx: &EvalContext,
    ) -> Result<Vec<TargetId>, ResolveError> {
        let mut ctx = context(target, ctx, UsageTraversal::Compile);
        ctx.head = Some(handle(target));
        let (direct, _) = self.usage_links(target, false, &ctx)?;
        let mut seen = HashSet::from([target]);
        let mut targets = Vec::new();
        let mut pending: Vec<_> = direct
            .iter()
            .flat_map(|item| item.edge_targets())
            .rev()
            .collect();
        while let Some(id) = pending.pop() {
            if seen.insert(id) {
                targets.push(id);
                let (links, _) = self.usage_links(id, true, &ctx)?;
                pending.extend(links.iter().flat_map(|item| item.edge_targets()).rev());
            }
        }
        Ok(targets)
    }

    pub(super) fn usage_links(
        &mut self,
        owner: TargetId,
        interface: bool,
        ctx: &EvalContext,
    ) -> Result<SharedLinks, ResolveError> {
        let key = LinkMemoKey {
            owner,
            interface,
            debug: self.debug_config(ctx),
            traversal: ctx.traversal,
        };
        if self.source_reads.is_none()
            && let Some(links) = self.link_memo.get(&key)
            && (self.active.is_empty() || !links.1.context_sensitive)
        {
            return Ok(links.clone());
        }
        let (name, entries) = self.link_entries(owner, interface)?;
        let explicit = self.evaluate_links(owner, name, entries, ctx, !interface, false)?;
        let (items, flags) =
            self.effective_links(owner, interface, ctx, explicit.items, explicit.value.flags)?;
        let links: SharedLinks = (items.into_iter().map(Arc::new).collect(), flags);
        // Other expressions can read target properties and enter a usage walk.
        // During a property walk, evaluate any remaining expression to check its depth.
        if self.source_reads.is_none()
            && (interface || !self.has_injections)
            && (self.active.is_empty() || !flags.context_sensitive)
            && entries.iter().all(|entry| literal_link(&entry.text.value))
        {
            self.link_memo.insert(key, links.clone());
        }
        Ok(links)
    }

    fn link_entries(
        &self,
        owner: TargetId,
        interface: bool,
    ) -> Result<(&'static [u8], &'a [LinkItem]), ResolveError> {
        let target = self.target(owner)?;
        let (name, entries) = if interface {
            (
                b"INTERFACE_LINK_LIBRARIES".as_slice(),
                &target.usage.interface_link_libraries,
            )
        } else {
            (b"LINK_LIBRARIES".as_slice(), &target.usage.link_libraries)
        };
        Ok((name, entries.as_deref().unwrap_or(&[])))
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
            .flat_map(ResolvedLinkItem::edge_targets)
            .any(|id| id == owner)
        {
            // CMP0108 OLD lets a target link to itself through an ALIAS.
            let alias_self_link = self
                .target(owner)?
                .policies
                .policies
                .get(rcmake_diag::PolicyId::new(108))
                == rcmake_diag::PolicyStatus::Old;
            if interface || alias_self_link {
                // Interface-only self edges do not contribute another usage value.
                links.retain(|item| !item.edge_targets().any(|id| id == owner));
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
        collect_text: bool,
    ) -> Result<EvaluatedLinks, ResolveError> {
        let mut output = EvaluatedLinks::default();
        let debug = self.debug_config(ctx);
        let mut take_next = true;
        // CMake reports a located error for each entry that fails.
        let mut failures = Vec::new();
        for entry in entries {
            if entry.text.value.is_empty()
                && (entry.opens_lookup_scope || entry.closes_lookup_scope)
            {
                continue;
            }
            match entry.text.value.as_slice() {
                b"debug" => take_next = debug,
                b"optimized" => take_next = !debug,
                b"general" => take_next = true,
                _ => {
                    if take_next {
                        let evaluated =
                            self.eval_entry(owner, entry.lookup_dir, name, &entry.text, ctx, true);
                        let (value, deps) = match evaluated {
                            Ok(evaluated) => evaluated,
                            Err(error) if located(&error) => {
                                failures.push(error);
                                take_next = true;
                                continue;
                            }
                            Err(error) => return Err(error),
                        };
                        output.value.flags = output.value.flags.union(deps);
                        let values = list::split(&value, list::Empty::Drop);
                        let scope = entry.scope.unwrap_or(if name.starts_with(b"INTERFACE_") {
                            UsageScope::Interface
                        } else {
                            UsageScope::Private
                        });
                        output
                            .items
                            .extend(self.decode_links(entry, &values, direct, scope)?);
                        if collect_text {
                            output.value.entries.extend(
                                values
                                    .into_iter()
                                    .map(|value| BT::new(value, entry.text.backtrace.clone())),
                            );
                        }
                    }
                    take_next = true;
                }
            }
        }
        combine_failures(failures).map_or(Ok(output), Err)
    }

    fn debug_config(&self, ctx: &EvalContext) -> bool {
        if self.debug_configs.is_empty() {
            ctx.config.matches(b"Debug")
        } else {
            self.debug_configs
                .iter()
                .any(|name| ctx.config.matches(name))
        }
    }

    fn decode_links(
        &self,
        origin: &LinkItem,
        values: &[BString],
        direct: bool,
        scope: UsageScope,
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
                    injected_by: None,
                    scope,
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
                injected_by: None,
                scope,
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
        let (name, entries) = self.link_entries(owner, interface)?;
        self.with_property(owner, name, |this| {
            let explicit = this.evaluate_links(owner, name, entries, ctx, !interface, true)?;
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
            for id in items.iter().flat_map(ResolvedLinkItem::edge_targets) {
                let value = this.flatten_links(id, true, ctx, visited, work)?;
                result.flags = result.flags.union(value.flags);
                result.entries.extend(value.entries);
            }
            Ok(result)
        })
    }
}

/// Return whether an error has a source location in its text.
fn located(error: &ResolveError) -> bool {
    matches!(error, ResolveError::Genex(_)) && error.to_string().starts_with("CMake Error at ")
}

/// Join the located errors of several link entries into one error.
fn combine_failures(mut failures: Vec<ResolveError>) -> Option<ResolveError> {
    if failures.len() < 2 {
        return failures.pop();
    }
    let mut text = String::new();
    for failure in &failures {
        let message = failure.to_string();
        text.push_str(message.trim_end_matches('\n'));
        text.push_str("\n\n\n");
    }
    Some(ResolveError::Message(text.into()))
}

fn link_context(owner: TargetId, ctx: &EvalContext) -> EvalContext {
    let mut ctx = context(owner, ctx, UsageTraversal::Link);
    ctx.head.get_or_insert(handle(owner));
    ctx
}

fn marker<'a>(value: &'a [u8], prefix: &[u8]) -> Option<&'a [u8]> {
    value.strip_prefix(prefix)?.strip_suffix(b">")
}

fn literal_link(value: &[u8]) -> bool {
    !value.windows(2).any(|window| window == b"$<")
        || [b"$<LINK_ONLY:".as_slice(), b"$<COMPILE_ONLY:"]
            .into_iter()
            .filter_map(|prefix| marker(value, prefix))
            .any(|inner| !inner.windows(2).any(|window| window == b"$<") && !inner.contains(&b'>'))
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
        for id in item.edge_targets() {
            if !self.visited.insert(id) {
                continue;
            }
            let target = resolver.target(id)?;
            for property in [DIRECT, EXCLUDE] {
                let Some(raw) = target.props.get(property) else {
                    continue;
                };
                let fallback = [BT::new(raw.clone(), target.backtrace.clone())];
                let entries = target.direct_link_entries(property).unwrap_or(&fallback);
                let items: Vec<_> = entries
                    .iter()
                    .map(|entry| LinkItem {
                        opens_lookup_scope: false,
                        closes_lookup_scope: false,
                        scope: None,
                        text: entry.clone(),
                        lookup_dir: target.dir,
                    })
                    .collect();
                let links = resolver.evaluate_links(id, property, &items, ctx, true, false)?;
                self.flags = self.flags.union(links.value.flags);
                for mut link in links.items {
                    if property == EXCLUDE {
                        self.excluded.insert(link.key());
                    } else if self.emitted.insert(link.key()) {
                        self.follow(resolver, &link, ctx, depth + 1)?;
                        link.injected_by = Some(id);
                        self.output.push(link);
                    }
                }
            }
            let (links, flags) = resolver.usage_links(id, true, ctx)?;
            self.flags = self.flags.union(flags);
            for link in links.iter() {
                self.follow(resolver, link, ctx, depth + 1)?;
            }
        }
        Ok(())
    }
}
