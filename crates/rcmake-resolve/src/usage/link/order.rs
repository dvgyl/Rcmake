use std::collections::{BTreeSet, HashMap, HashSet};
use std::fmt::Write;
use std::sync::Arc;

use indexmap::{IndexMap, IndexSet};
use rcmake_base::{BString, ByteSlice, list, truth};
use rcmake_diag::{PolicyId, PolicyStatus};
use rcmake_genex::EvalContext;
use rcmake_model::{ProjectModel, Target, TargetId, TargetKind};

use super::{Limits, LinkKey, ResolvedLinkItem, ResolvedLinkKind, UsageResolver, link_context};
use crate::{ResolveError, lower::definition, sources::SourcePaths};

mod diagnostics;
use diagnostics::{print_link_order, warn_policy};

impl UsageResolver<'_> {
    pub(crate) fn ordered_links(
        &mut self,
        target: TargetId,
        ctx: &EvalContext,
    ) -> Result<Vec<Arc<ResolvedLinkItem>>, ResolveError> {
        self.order_links(target, ctx, false, false)
    }

    pub(crate) fn ordered_links_for_result(
        &mut self,
        target: TargetId,
        ctx: &EvalContext,
    ) -> Result<Vec<Arc<ResolvedLinkItem>>, ResolveError> {
        self.order_links(target, ctx, self.provenance.is_some(), false)
    }

    pub(crate) fn ordered_runtime_links(
        &mut self,
        target: TargetId,
        ctx: &EvalContext,
    ) -> Result<Vec<Arc<ResolvedLinkItem>>, ResolveError> {
        self.order_links(target, ctx, false, true)
    }

    fn order_links(
        &mut self,
        target: TargetId,
        ctx: &EvalContext,
        record: bool,
        include_shared_deps: bool,
    ) -> Result<Vec<Arc<ResolvedLinkItem>>, ResolveError> {
        let ctx = link_context(target, ctx);
        let owner = self.target(target)?;
        let model = self.model;
        let strategy = owner.props.get(b"LINK_LIBRARIES_STRATEGY");
        let paths = self.source_paths();
        let error = |body: BString| {
            paths.map_or_else(
                || ResolveError::Message(body.clone()),
                |paths| paths.target_error(model, owner, body.clone()),
            )
        };
        let strategy = match strategy.map(|value| value.as_slice()) {
            None | Some(b"REORDER_MINIMALLY") => LinkOrder::Minimal,
            Some(b"REORDER_FREELY") => LinkOrder::Free,
            Some(value) => {
                let body = BString::from(format!(
                    "LINK_LIBRARIES_STRATEGY value '{}' is not recognized.",
                    BString::from(value)
                ));
                return Err(error(body));
            }
        };
        let (direct, _) = self.usage_links(target, false, &ctx)?;
        let language = ctx.link_language.as_ref().or(ctx.language.as_ref());
        if owner.kind != TargetKind::StaticLibrary
            && let (Some(paths), Some(language)) = (paths, language)
        {
            for item in direct.iter() {
                if let Some(depender) = item
                    .injected_by
                    .and_then(|id| model.target(id))
                    .filter(|target| target.is_imported())
                {
                    diagnostics::warn_imported_features(
                        model,
                        paths,
                        owner,
                        depender,
                        language,
                        std::slice::from_ref(item),
                    );
                }
            }
        }
        let mut graph = LinkGraph::default();
        let mut original: Vec<_> = direct
            .iter()
            .map(|item| (graph.insert(item), Arc::clone(item)))
            .collect();
        let mut chains = record.then(|| {
            direct
                .iter()
                .map(|item| (item.key(), Vec::new()))
                .collect::<HashMap<_, _>>()
        });
        let mut inferred = IndexMap::new();
        let mut edge_budget = Limits::default().max_value_bytes / 64;
        if original.iter().any(|(_, item)| raw_library(item)) {
            graph.infer_raw_edges(
                &mut inferred,
                &mut edge_budget,
                original.iter().map(|(id, _)| *id),
            )?;
        }
        let mut index = 0;
        let mut indices_buf = Vec::new();
        while let Some((_, (item, _))) = graph.nodes.get_index(index) {
            let item = Arc::clone(item);
            let mut edges = IndexSet::new();
            for (parent, edge) in item.edges() {
                let (items, _) = self.usage_links(parent, true, &ctx)?;
                if owner.kind != TargetKind::StaticLibrary
                    && let (Some(paths), Some(language), Some(depender)) =
                        (paths, language, model.target(parent))
                    && depender.is_imported()
                {
                    diagnostics::warn_imported_features(
                        model, paths, owner, depender, language, &items,
                    );
                }
                if let Some(chains) = &mut chains {
                    let mut path = chains.get(&item.key()).cloned().unwrap_or_default();
                    path.push(edge.hop(parent));
                    for child in items.iter() {
                        chains.entry(child.key()).or_insert_with(|| path.clone());
                    }
                }
                let need_infer = items.iter().any(|child| raw_library(child));
                if need_infer {
                    indices_buf.clear();
                    indices_buf.extend(items.iter().map(|item| graph.insert(item)));
                    graph.infer_raw_edges(
                        &mut inferred,
                        &mut edge_budget,
                        indices_buf.iter().copied(),
                    )?;
                    edges.extend(indices_buf.iter().copied());
                } else {
                    for child in items.iter() {
                        edges.insert(graph.insert(child));
                    }
                }
            }
            if let Some((_, (_, current_edges))) = graph.nodes.get_index_mut(index) {
                *current_edges = edges;
            }
            index += 1;
        }

        for (index, inferred) in inferred {
            if let Some((_, (_, edges))) = graph.nodes.get_index_mut(index) {
                edges.extend(inferred);
            }
        }
        let dependent_only = self.add_shared_dependencies(&mut graph, &ctx)?;
        let overrides = self.link_feature_overrides(target, owner, &ctx)?;
        let features =
            graph.apply_features(self.model, owner, &ctx, &overrides, paths, &mut original)?;
        graph.apply_groups(&mut original)?;
        let mut ordered = graph
            .order(self.model, owner, &ctx, original, strategy)
            .map_err(|error_value| match error_value {
                ResolveError::Message(body) => error(body),
                other => other,
            })?;
        if !include_shared_deps && let Some(paths) = paths {
            warn_policy(model, paths, owner, 156);
            if owner.policies.policies.get(PolicyId::new(156)) == PolicyStatus::New {
                warn_policy(model, paths, owner, 179);
            }
        }
        let debug = (!include_shared_deps)
            .then_some(paths)
            .flatten()
            .filter(|_| {
                owner
                    .props
                    .get(b"LINK_DEPENDS_DEBUG_MODE")
                    .is_some_and(|value| truth::is_on(value))
                    || definition(model, owner.dir, b"CMAKE_LINK_DEPENDS_DEBUG_MODE")
                        .is_some_and(|value| truth::is_on(value))
            });
        // A shared library that appears only as a dependency of another shared
        // library constrains the order (`LinkEntry::SharedDep`). It stays on the
        // link line only in the `Link` mode, whereas runtime DLL collection keeps
        // all shared dependencies in the final link order.
        if !include_shared_deps {
            let mode = crate::lower::shared_dependency_mode(
                model,
                owner.dir,
                ctx.link_language
                    .as_ref()
                    .map(|language| language.as_slice()),
            );
            ordered.retain(|item| {
                !dependent_only.contains(&item.key())
                    || (mode == crate::lower::SharedDependencyMode::Link
                        && crate::lower::links_shared_dependency(model, item))
            });
        }
        if let Some(paths) = debug {
            print_link_order(paths, model, owner, "link dependency ordering", &ordered);
        }
        deduplicate(self.model, owner, &ctx, &features, &mut ordered).map_err(error)?;
        if let Some(paths) = debug {
            print_link_order(paths, model, owner, "link line", &ordered);
        }
        // Explicit object inputs are placed before the libraries.
        ordered.retain(|item| !matches!(item.kind, ResolvedLinkKind::Interface(_)));
        ordered.sort_by_key(|item| {
            !matches!(
                item.kind,
                ResolvedLinkKind::Objects(_) | ResolvedLinkKind::ObjectFile
            )
        });
        if let (Some(provenance), Some(chains)) = (&mut self.provenance, chains) {
            for item in &ordered {
                let path = chains.get(&item.key()).cloned().unwrap_or_default();
                provenance.record(
                    target,
                    &ctx.config,
                    crate::ChainKind::LinkItem,
                    &item.text,
                    path,
                    item.targets().collect(),
                );
            }
        }
        Ok(ordered)
    }

    /// Add the private shared library dependencies of each linked target
    /// (`cmComputeLinkDepends::FollowSharedDeps` and `HandleSharedDependency`).
    /// Each dependency must follow the library that lists it. Return the keys of
    /// the entries that only these dependencies added.
    fn add_shared_dependencies(
        &mut self,
        graph: &mut LinkGraph,
        ctx: &EvalContext,
    ) -> Result<HashSet<LinkKey>, ResolveError> {
        if !self.has_shared_or_imported_targets {
            return Ok(HashSet::new());
        }
        let mut followed = HashSet::new();
        let mut queue = std::collections::VecDeque::new();
        for index in 0..graph.nodes.len() {
            if let Some((item, _)) = graph.node(index)
                && let ResolvedLinkKind::Library(id) = item.kind
                && followed.insert(index)
            {
                for dependency in self.shared_dependencies(id, ctx)? {
                    queue.push_back((index, dependency));
                }
            }
        }
        let mut dependent_only = HashSet::new();
        let mut budget = Limits::default().max_value_bytes / 64;
        while let Some((depender, mut dependency)) = queue.pop_front() {
            budget = budget.checked_sub(1).ok_or_else(|| {
                ResolveError::Message("The link dependency graph exceeds the size limit.".into())
            })?;
            if dependency.feature.is_some() {
                Arc::make_mut(&mut dependency).feature = None;
            }
            let key = dependency.key();
            let index = if let Some(index) = graph.nodes.get_index_of(&key) {
                index
            } else {
                dependent_only.insert(key);
                graph.insert(&dependency)
            };
            if let Some((_, (_, edges))) = graph.nodes.get_index_mut(depender) {
                edges.insert(index);
            }
            if let ResolvedLinkKind::Library(id) = dependency.kind
                && followed.insert(index)
            {
                let (interface, _) = self.usage_links(id, true, ctx)?;
                for item in interface.iter() {
                    if let ResolvedLinkKind::Group { members, .. } = &item.kind {
                        for member in members {
                            queue.push_back((index, Arc::new(member.clone())));
                        }
                    } else {
                        queue.push_back((index, Arc::clone(item)));
                    }
                }
                for item in self.shared_dependencies(id, ctx)? {
                    queue.push_back((index, item));
                }
            }
        }
        Ok(dependent_only)
    }

    /// The runtime dependencies of a library on shared libraries that are not in
    /// its link interface (`cmLinkInterface::SharedDeps`).
    pub(super) fn shared_dependencies(
        &mut self,
        id: TargetId,
        ctx: &EvalContext,
    ) -> Result<Vec<Arc<ResolvedLinkItem>>, ResolveError> {
        let target = self.target(id)?;
        if target.is_imported() {
            let Some((names, _)) = super::super::artifacts::imported_property(
                target,
                b"IMPORTED_LINK_DEPENDENT_LIBRARIES",
                &ctx.config,
            ) else {
                return Ok(Vec::new());
            };
            let dir = target.dir;
            return Ok(list::split(&names, list::Empty::Drop)
                .into_iter()
                .filter_map(|name| {
                    let dependency = self.model.find_target(dir, &name)?;
                    Some(Arc::new(ResolvedLinkItem {
                        text: rcmake_model::BT::new(name, rcmake_diag::Backtrace::default()),
                        kind: ResolvedLinkKind::Library(dependency),
                        feature: None,
                        direct: false,
                        scope: rcmake_model::UsageScope::Private,
                        injected_by: None,
                    }))
                })
                .collect());
        }
        if matches!(
            target.kind,
            TargetKind::StaticLibrary | TargetKind::InterfaceLibrary | TargetKind::ObjectLibrary
        ) {
            return Ok(Vec::new());
        }
        let mut own = ctx.clone();
        own.head = Some(super::handle(id));
        let (interface, _) = self.usage_links(id, true, ctx)?;
        let mut emitted: HashSet<_> = interface.iter().map(|item| item.key()).collect();
        let (implementation, _) = self.usage_links(id, false, &own)?;
        Ok(implementation
            .iter()
            .filter(|item| emitted.insert(item.key()))
            .filter(|item| {
                matches!(item.kind, ResolvedLinkKind::Library(dependency)
                    if self.model.target(dependency)
                        .is_some_and(|target| target.kind == TargetKind::SharedLibrary))
            })
            .cloned()
            .collect())
    }

    fn link_feature_overrides(
        &mut self,
        id: TargetId,
        target: &Target,
        ctx: &EvalContext,
    ) -> Result<HashMap<BString, BString>, ResolveError> {
        if !self.has_link_feature_overrides {
            return Ok(HashMap::new());
        }
        let mut overrides = HashMap::new();
        let mut ctx = ctx.clone();
        ctx.language.clone_from(&ctx.link_language);
        for key in target.props.names() {
            if let Some(item) = key
                .strip_prefix(b"LINK_LIBRARY_OVERRIDE_")
                .filter(|item| !item.is_empty())
                && let Some(value) = target.props.get(&key).filter(|value| !value.is_empty())
            {
                overrides.insert(item.into(), self.eval_ide_property(id, &key, value, &ctx)?);
            }
        }
        if let Some(value) = target.props.get(b"LINK_LIBRARY_OVERRIDE") {
            let value = self.eval_ide_property(id, b"LINK_LIBRARY_OVERRIDE", value, &ctx)?;
            let mut parts = value.split(|&byte| byte == b',');
            if let Some(feature) = parts.next() {
                for item in parts {
                    overrides
                        .entry(item.into())
                        .or_insert_with(|| feature.into());
                }
            }
        }
        Ok(overrides)
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
    feature_variants: Vec<Arc<ResolvedLinkItem>>,
}

impl LinkGraph {
    fn node(&self, index: usize) -> Option<&LinkNode> {
        self.nodes.get_index(index).map(|(_, node)| node)
    }

    fn insert(&mut self, item: &Arc<ResolvedLinkItem>) -> usize {
        let entry = self.nodes.entry(item.key());
        let index = entry.index();
        let (first, _) = entry.or_insert_with(|| (Arc::clone(item), IndexSet::new()));
        if first.feature != item.feature
            || matches!(item.kind, ResolvedLinkKind::Group { .. }) && first.kind != item.kind
        {
            self.feature_variants.push(Arc::clone(item));
        }
        index
    }

    fn infer_raw_edges(
        &self,
        inferred: &mut IndexMap<usize, IndexSet<usize>>,
        budget: &mut usize,
        entries: impl Iterator<Item = usize>,
    ) -> Result<(), ResolveError> {
        let mut positions = IndexMap::new();
        for (position, id) in entries.enumerate() {
            if self.node(id).is_some_and(|(item, _)| raw_library(item)) {
                let (_, last) = positions.entry(id).or_insert((position, position));
                *last = position;
            }
        }
        // Infer an edge when the same item follows this library in each list.
        for (&id, &(first, _)) in &positions {
            let follows = |other: &usize| {
                *other != id && positions.get(other).is_some_and(|&(_, last)| last > first)
            };
            if let Some(edges) = inferred.get_mut(&id) {
                edges.retain(follows);
            } else {
                let edges = positions.keys().copied().filter(follows);
                *budget = budget.checked_sub(edges.clone().count()).ok_or_else(|| {
                    ResolveError::Message("The link graph exceeds the size limit.".into())
                })?;
                inferred.insert(id, edges.collect());
            }
        }
        Ok(())
    }

    fn apply_features(
        &mut self,
        model: &ProjectModel,
        target: &Target,
        ctx: &EvalContext,
        overrides: &HashMap<BString, BString>,
        paths: Option<&SourcePaths<'_>>,
        direct: &mut [(usize, Arc<ResolvedLinkItem>)],
    ) -> Result<HashMap<BString, FeatureAttributes>, ResolveError> {
        if target.kind == TargetKind::StaticLibrary
            || (overrides.is_empty()
                && self.feature_variants.is_empty()
                && !self.nodes.values().any(|(item, _)| {
                    item.feature.is_some() || matches!(item.kind, ResolvedLinkKind::Group { .. })
                }))
        {
            return Ok(HashMap::new());
        }
        let mut attributes = HashMap::new();
        let mut features = HashMap::<LinkKey, BString>::new();
        let mut groups = HashMap::<LinkKey, BString>::new();
        let mut warned = HashSet::new();
        let error = |body: BString| {
            if let Some(paths) = paths {
                paths.target_error(model, target, body)
            } else {
                ResolveError::Message(body)
            }
        };
        let language = ctx.link_language.as_ref().or(ctx.language.as_ref());
        for entry in self
            .nodes
            .values()
            .map(|(item, _)| item)
            .chain(&self.feature_variants)
        {
            let (group, items) = match &entry.kind {
                ResolvedLinkKind::Group { feature, members } => (Some(feature), members.as_slice()),
                _ => (None, std::slice::from_ref(entry.as_ref())),
            };
            for item in items {
                let key = item.key();
                if let Some(group) = group {
                    let prior = groups.entry(key.clone()).or_insert_with(|| group.clone());
                    if group.as_slice() != prior.as_slice() {
                        return Err(error(format!(
                            "Impossible to link target '{}' because the link item '{}', specified with the group feature '{group}', has already occurred with the feature '{prior}', which is not allowed.",
                            target.name, item.text.value
                        ).into()));
                    }
                }
                if let (Some(group), Some(paths)) = (group, paths)
                    && let Some(member) = match item.kind {
                        ResolvedLinkKind::Interface(id) | ResolvedLinkKind::Objects(id) => {
                            model.target(id)
                        }
                        _ => None,
                    }
                    && matches!(
                        member.kind,
                        TargetKind::InterfaceLibrary | TargetKind::ObjectLibrary
                    )
                    && warned.insert((
                        key.clone(),
                        [b"<LINK_GROUP>", group.as_slice()].concat().into(),
                    ))
                    && !definition(model, target.dir, b"CMAKE_SUPPRESS_DEVELOPER_WARNINGS")
                        .is_some_and(|value| truth::is_on(value))
                {
                    paths.emit(
                        model,
                        rcmake_diag::MessageKind::AuthorWarning,
                        rcmake_diag::Location::Backtrace(target.backtrace.clone()),
                        format!(
                            "The feature '{group}', specified as part of a generator-expression '$<LINK_GROUP:{group}>', will not be applied to the {} library '{}'.",
                            if member.kind == TargetKind::InterfaceLibrary { "INTERFACE" } else { "OBJECT" },
                            item.text.value
                        )
                        .into(),
                    );
                }
                let requested = overrides.get(&item.text.value).or(item.feature.as_ref());
                let feature = requested.map_or(b"DEFAULT".as_slice(), |value| value.as_slice());
                if !attributes.contains_key(feature) {
                    attributes.insert(
                        feature.into(),
                        FeatureAttributes::read(model, target, language, feature).map_err(error)?,
                    );
                }
                let kind = match item.kind {
                    ResolvedLinkKind::Library(id)
                    | ResolvedLinkKind::Interface(id)
                    | ResolvedLinkKind::Objects(id) => model.target(id).map(|target| target.kind),
                    _ => None,
                };
                let applicable = feature == b"DEFAULT"
                    || attributes
                        .get(feature)
                        .is_none_or(|attribute| attribute.applies(kind));
                if !applicable {
                    if let (Some(paths), Some(kind)) = (paths, kind)
                        && warned.insert((key.clone(), BString::from(feature)))
                        && !definition(model, target.dir, b"CMAKE_SUPPRESS_DEVELOPER_WARNINGS")
                            .is_some_and(|value| truth::is_on(value))
                    {
                        paths.emit(
                            model,
                            rcmake_diag::MessageKind::AuthorWarning,
                            rcmake_diag::Location::Backtrace(target.backtrace.clone()),
                            format!(
                                "The feature '{}', specified as part of a generator-expression '$<LINK_LIBRARY:{}>', will not be applied to the {} '{}'.",
                                BString::from(feature), BString::from(feature), kind.as_str(), item.text.value
                            ).into(),
                        );
                    }
                    continue;
                }
                let prior = features.entry(key).or_insert_with(|| feature.into());
                if prior.as_slice() == feature {
                    continue;
                }
                let replaces = |feature: &[u8], old: &[u8]| {
                    attributes
                        .get(feature)
                        .is_some_and(|attribute| attribute.overrides.contains(old))
                };
                match (replaces(feature, prior), replaces(prior, feature)) {
                    (true, false) => *prior = feature.into(),
                    (false, true) => {}
                    (true, true) => return Err(error(format!(
                        "Impossible to link target '{}' because the link item '{}' is specified with the features '{}' and '{prior}', and both have an 'OVERRIDE' attribute that overrides the other. Such cycles are not allowed.",
                        target.name, item.text.value, BString::from(feature)
                    ).into())),
                    (false, false) => return Err(error(format!(
                        "Impossible to link target '{}' because the link item '{}', specified {}, has already occurred {}, which is not allowed.",
                        target.name, item.text.value, describe_feature(feature), describe_feature(prior)
                    ).into())),
                }
            }
        }
        features.retain(|_, feature| feature.as_slice() != b"DEFAULT");
        fn apply(item: &mut ResolvedLinkItem, features: &HashMap<LinkKey, BString>) {
            let selected = features.get(&item.key());
            if let ResolvedLinkKind::Group { members, .. } = &mut item.kind {
                for member in members {
                    member.feature = features.get(&member.key()).cloned();
                }
            } else {
                item.feature = selected.cloned();
            }
        }
        for (item, _) in self.nodes.values_mut() {
            if item.feature.is_some()
                || features.contains_key(&item.key())
                || matches!(item.kind, ResolvedLinkKind::Group { .. })
            {
                apply(Arc::make_mut(item), &features);
            }
        }
        for (_, item) in direct {
            if item.feature.is_some()
                || features.contains_key(&item.key())
                || matches!(item.kind, ResolvedLinkKind::Group { .. })
            {
                apply(Arc::make_mut(item), &features);
            }
        }
        Ok(attributes)
    }

    fn apply_groups(
        &mut self,
        direct: &mut [(usize, Arc<ResolvedLinkItem>)],
    ) -> Result<(), ResolveError> {
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
                edges.sort_unstable();
            }
            return Ok(());
        }
        for (index, item) in direct {
            if let Some(&group) = replacements.get(index) {
                let (group_item, _) = self.node(group).ok_or_else(invalid_link_graph)?;
                *index = group;
                *item = Arc::clone(group_item);
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
            edges.sort_unstable();
        }
        Ok(())
    }

    fn order(
        &self,
        model: &ProjectModel,
        owner: &Target,
        ctx: &EvalContext,
        direct: Vec<(usize, Arc<ResolvedLinkItem>)>,
        strategy: LinkOrder,
    ) -> Result<Vec<Arc<ResolvedLinkItem>>, ResolveError> {
        self.order_with_limit(
            model,
            owner,
            ctx,
            direct,
            strategy,
            Limits::default().max_value_bytes / 64,
        )
    }

    fn order_with_limit(
        &self,
        model: &ProjectModel,
        owner: &Target,
        ctx: &EvalContext,
        direct: Vec<(usize, Arc<ResolvedLinkItem>)>,
        strategy: LinkOrder,
        max_len: usize,
    ) -> Result<Vec<Arc<ResolvedLinkItem>>, ResolveError> {
        // Fast path for a strict linear chain (`0 -> 1 -> ... -> n-1`, single direct item 0):
        // every node is a singleton SCC in topological order 0..n, identical under Minimal and Free.
        if direct.len() == 1
            && direct.first().is_some_and(|(idx, _)| *idx == 0)
            && !self.nodes.is_empty()
            && self.nodes.values().enumerate().all(|(i, (_, edges))| {
                if i + 1 < self.nodes.len() {
                    edges.len() == 1 && edges.first() == Some(&(i + 1))
                } else {
                    edges.is_empty()
                }
            })
        {
            if self.nodes.len() > max_len {
                return Err(ResolveError::Message(
                    "The link line exceeds the size limit.".into(),
                ));
            }
            let mut order = Vec::with_capacity(self.nodes.len());
            if let Some((_, first)) = direct.into_iter().next() {
                order.push(first);
            }
            for (_, (item, _)) in self.nodes.iter().skip(1) {
                order.push(Arc::clone(item));
            }
            return Ok(order);
        }
        let (component_ids, mut components) = self.components().ok_or_else(invalid_link_graph)?;
        for (&id, (_, edges)) in component_ids.iter().zip(self.nodes.values()) {
            let component = components.get_mut(id).ok_or_else(invalid_link_graph)?;
            for &next in edges {
                let &next = component_ids.get(next).ok_or_else(invalid_link_graph)?;
                if id != next {
                    component.edges.push(next);
                }
            }
        }
        set_component_order(&mut components).ok_or_else(invalid_link_graph)?;
        for component in &mut components {
            if component.members.len() > 1 {
                component.passes = 2;
                for &index in &component.members {
                    let (item, _) = self.node(index).ok_or_else(invalid_link_graph)?;
                    match item.kind {
                        ResolvedLinkKind::Group { .. } => {
                            return Err(self.group_cycle(owner, component));
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
        // Use the component order to choose the next pending library.
        let mut order = Vec::with_capacity(self.nodes.len().max(direct.len()));
        let mut pending = BTreeSet::new();
        match strategy {
            LinkOrder::Minimal => {
                for (index, item) in direct {
                    visit(index, &component_ids, &mut components, &mut pending)
                        .ok_or_else(invalid_link_graph)?;
                    order.push(item);
                }
            }
            LinkOrder::Free => {
                for (index, _) in direct {
                    let &id = component_ids.get(index).ok_or_else(invalid_link_graph)?;
                    let component = components.get_mut(id).ok_or_else(invalid_link_graph)?;
                    if pending.insert((component.order, id)) {
                        component.reset();
                    }
                }
            }
        }
        while let Some((_, id)) = pending.first().copied() {
            let component = components.get(id).ok_or_else(invalid_link_graph)?;
            let &index = component.remaining.first().ok_or_else(invalid_link_graph)?;
            visit(index, &component_ids, &mut components, &mut pending)
                .ok_or_else(invalid_link_graph)?;
            let (item, _) = self.node(index).ok_or_else(invalid_link_graph)?;
            order.push(Arc::clone(item));
            if order.len() > max_len {
                return Err(ResolveError::Message(
                    "The link line exceeds the size limit.".into(),
                ));
            }
        }
        Ok(order)
    }

    fn group_cycle(&self, owner: &Target, component: &Component) -> ResolveError {
        let item_name = |item: &ResolvedLinkItem| -> String {
            if let ResolvedLinkKind::Group { feature, members } = &item.kind {
                let names: Vec<_> = members
                    .iter()
                    .map(|member| member.text.value.to_string())
                    .collect();
                format!("group \"{feature}:{{{}}}\"", names.join(","))
            } else {
                format!("\"{}\"", item.text.value)
            }
        };
        let mut body = format!(
            "The inter-target dependency graph, for the target \"{}\", contains the following strongly connected component (cycle):\n",
            owner.name
        );
        for (item, edges) in component
            .members
            .iter()
            .filter_map(|&index| self.node(index))
        {
            let _ = writeln!(body, "  {}", item_name(item));
            for &edge in edges {
                if component.members.binary_search(&edge).is_ok()
                    && let Some((dependency, _)) = self.node(edge)
                {
                    let _ = writeln!(body, "    depends on {}", item_name(dependency));
                }
            }
        }
        ResolveError::Message(body.into())
    }

    fn components(&self) -> Option<(Vec<usize>, Vec<Component>)> {
        let count = self.nodes.len();
        let mut reverse = vec![Vec::new(); count];
        for (id, (_, (_, edges))) in self.nodes.iter().enumerate() {
            for &next in edges {
                reverse.get_mut(next)?.push(id);
            }
        }

        // Find strongly connected components with two graph passes. Use
        // explicit stacks so large acyclic link graphs do not use the call stack.
        let mut visited = vec![false; count];
        let mut finished = Vec::with_capacity(count);
        let mut stack = Vec::new();
        for index in 0..count {
            stack.push((index, false));
            while let Some((node, done)) = stack.pop() {
                if done {
                    finished.push(node);
                } else if !std::mem::replace(visited.get_mut(node)?, true) {
                    stack.push((node, true));
                    let (_, edges) = self.node(node)?;
                    stack.extend(edges.iter().rev().map(|&edge| (edge, false)));
                }
            }
        }
        let mut ids = vec![usize::MAX; count];
        let mut components = Vec::new();
        let mut stack = Vec::new();
        for index in finished.into_iter().rev() {
            if *ids.get(index)? != usize::MAX {
                continue;
            }
            let id = components.len();
            let mut members = Vec::new();
            stack.push(index);
            while let Some(node) = stack.pop() {
                let component = ids.get_mut(node)?;
                if *component == usize::MAX {
                    *component = id;
                    members.push(node);
                    stack.extend(reverse.get(node)?.iter().copied());
                }
            }
            members.sort_unstable();
            components.push(Component {
                members,
                order: 0,
                edges: Vec::new(),
                passes: 1,
                left: 0,
                remaining: IndexSet::new(),
            });
        }
        Some((ids, components))
    }
}

fn invalid_link_graph() -> ResolveError {
    ResolveError::Message("The link graph contains an invalid node or component.".into())
}

struct Component {
    members: Vec<usize>,
    order: usize,
    edges: Vec<usize>,
    passes: usize,
    left: usize,
    remaining: IndexSet<usize>,
}

fn set_component_order(components: &mut [Component]) -> Option<()> {
    let mut visited = vec![false; components.len()];
    let mut next = components.len();
    let mut stack = Vec::new();
    for id in 0..components.len() {
        stack.push((id, false));
        while let Some((id, done)) = stack.pop() {
            let component = components.get_mut(id)?;
            if done {
                next -= 1;
                component.order = next;
            } else if !std::mem::replace(visited.get_mut(id)?, true) {
                stack.push((id, true));
                stack.extend(component.edges.iter().map(|&edge| (edge, false)));
            }
        }
    }
    Some(())
}

impl Component {
    fn reset(&mut self) {
        self.left = self.passes;
        self.remaining.clear();
        self.remaining.extend(self.members.iter().copied());
    }
}

fn visit(
    item: usize,
    ids: &[usize],
    components: &mut [Component],
    pending: &mut BTreeSet<(usize, usize)>,
) -> Option<()> {
    let &id = ids.get(item)?;
    let component = components.get_mut(id)?;
    let key = (component.order, id);
    if pending.insert(key) {
        component.reset();
    }
    component.remaining.shift_remove(&item);
    if !component.remaining.is_empty() {
        return Some(());
    }
    component.left -= 1;
    if component.left > 0 {
        component
            .remaining
            .extend(component.members.iter().copied());
        return Some(());
    }
    pending.remove(&key);
    let next_components: IndexSet<_> = component.edges.iter().copied().collect();
    for next in next_components {
        let component = components.get_mut(next)?;
        pending.insert((component.order, next));
        component.reset();
    }
    Some(())
}

fn describe_feature(feature: &[u8]) -> String {
    if feature == b"DEFAULT" {
        "without any feature or 'DEFAULT' feature".to_owned()
    } else {
        format!("with the feature '{}'", BString::from(feature))
    }
}

#[derive(Default)]
struct FeatureAttributes {
    types: Option<Vec<BString>>,
    deduplicate: Option<bool>,
    overrides: HashSet<BString>,
}

impl FeatureAttributes {
    fn read(
        model: &ProjectModel,
        target: &Target,
        language: Option<&BString>,
        feature: &[u8],
    ) -> Result<Self, BString> {
        let mut attributes = Self::default();
        if feature == b"DEFAULT" {
            return Ok(attributes);
        }
        let feature = BString::from(feature);
        let mut variable = format!("CMAKE_LINK_LIBRARY_{feature}_ATTRIBUTES");
        if let Some(language) = language {
            let specific = format!("CMAKE_{language}_LINK_LIBRARY_{feature}_ATTRIBUTES");
            if definition(model, target.dir, specific.as_bytes())
                .is_some_and(|value| !value.is_empty())
            {
                variable = specific;
            }
        }
        let mut invalid = Vec::new();
        if let Some(value) = definition(model, target.dir, variable.as_bytes()) {
            for entry in list::split(value, list::Empty::Drop) {
                match entry.split_once_str(b"=") {
                    Some((b"LIBRARY_TYPE", value)) => {
                        let types: Vec<_> = value
                            .split(|&byte| byte == b',')
                            .map(BString::from)
                            .collect();
                        if types.iter().all(|kind| {
                            matches!(
                                kind.as_slice(),
                                b"STATIC" | b"SHARED" | b"MODULE" | b"EXECUTABLE"
                            )
                        }) {
                            attributes.types = Some(types);
                        } else {
                            invalid.push(entry);
                        }
                    }
                    Some((b"DEDUPLICATION", b"YES")) => attributes.deduplicate = Some(true),
                    Some((b"DEDUPLICATION", b"NO")) => attributes.deduplicate = Some(false),
                    Some((b"DEDUPLICATION", b"DEFAULT")) => attributes.deduplicate = None,
                    Some((b"OVERRIDE", value))
                        if !value.is_empty()
                            && value.split(|&byte| byte == b',').all(|name| {
                                !name.is_empty()
                                    && name
                                        .iter()
                                        .all(|byte| byte.is_ascii_alphanumeric() || *byte == b'_')
                            }) =>
                    {
                        attributes.overrides = value
                            .split(|&byte| byte == b',')
                            .map(BString::from)
                            .collect();
                    }
                    _ => invalid.push(entry),
                }
            }
        }
        if invalid.is_empty() {
            Ok(attributes)
        } else {
            Err(invalid_options(&variable, &invalid))
        }
    }

    fn applies(&self, kind: Option<TargetKind>) -> bool {
        let name: &[u8] = match kind {
            None | Some(TargetKind::UnknownLibrary) => return true,
            Some(TargetKind::StaticLibrary) => b"STATIC",
            Some(TargetKind::SharedLibrary) => b"SHARED",
            Some(TargetKind::ModuleLibrary) => b"MODULE",
            Some(TargetKind::Executable) => b"EXECUTABLE",
            _ => return false,
        };
        self.types
            .as_ref()
            .is_none_or(|types| types.iter().any(|kind| kind == name))
    }
}

fn invalid_options(variable: &str, invalid: &[BString]) -> BString {
    let mut body = BString::from(format!("Erroneous option(s) for '{variable}':\n"));
    for entry in invalid {
        body.extend_from_slice(b"  ");
        body.extend_from_slice(entry);
        body.push(b'\n');
    }
    body
}

fn raw_library(item: &ResolvedLinkItem) -> bool {
    let text = &item.text.value;
    matches!(item.kind, ResolvedLinkKind::Raw)
        && (!text.starts_with(b"-") || text.starts_with(b"-l") || text.starts_with(b"-framework "))
        && !text.starts_with(b"LINKER:")
        && !text.starts_with(b"SHELL:")
}

fn deduplicate(
    model: &ProjectModel,
    target: &Target,
    ctx: &EvalContext,
    attributes: &HashMap<BString, FeatureAttributes>,
    items: &mut Vec<Arc<ResolvedLinkItem>>,
) -> Result<(), BString> {
    let policy = |number| target.policies.policies.get(PolicyId::new(number)) == PolicyStatus::New;
    let language = ctx.link_language.as_ref().or(ctx.language.as_ref());
    let mut all = false;
    let mut shared = true;
    let mut forward = false;
    // A static library has no link step, so CMake does not read the
    // processing options for it.
    if policy(156)
        && target.kind != TargetKind::StaticLibrary
        && let Some(language) = language
    {
        let variable = format!("CMAKE_{language}_LINK_LIBRARIES_PROCESSING");
        if let Some(value) = definition(model, target.dir, variable.as_bytes()) {
            let mut invalid = Vec::new();
            for item in list::split(value, list::Empty::Drop) {
                match item.split_once_str(b"=") {
                    Some((b"ORDER", b"FORWARD")) => forward = true,
                    Some((b"ORDER", b"REVERSE")) => forward = false,
                    Some((b"DEDUPLICATION" | b"UNICITY", b"ALL")) => {
                        all = true;
                        shared = true;
                    }
                    Some((b"DEDUPLICATION" | b"UNICITY", b"SHARED")) => {
                        all = false;
                        shared = true;
                    }
                    Some((b"DEDUPLICATION" | b"UNICITY", b"NONE")) => {
                        all = false;
                        shared = false;
                    }
                    _ => invalid.push(item),
                }
            }
            if !invalid.is_empty() {
                return Err(invalid_options(&variable, &invalid));
            }
            if all
                && !policy(179)
                && definition(model, target.dir, b"CMAKE_EXECUTABLE_FORMAT")
                    .is_some_and(|value| value == "ELF")
                && definition(
                    model,
                    target.dir,
                    format!("CMAKE_{language}_PLATFORM_LINKER_ID").as_bytes(),
                )
                .is_some_and(|value| value == "LLD")
            {
                all = false;
            }
        }
    }
    let keep_static_first = forward || all && policy(179);
    let feature_dedup = |item: &ResolvedLinkItem| {
        item.feature
            .as_ref()
            .and_then(|feature| attributes.get(feature))
            .and_then(|attribute| attribute.deduplicate)
    };
    let keep_first = |item: &ResolvedLinkItem| {
        if let Some(deduplicate) = feature_dedup(item) {
            return deduplicate.then_some(match item.kind {
                ResolvedLinkKind::Library(id)
                    if model
                        .target(id)
                        .is_some_and(|target| target.kind == TargetKind::StaticLibrary) =>
                {
                    keep_static_first
                }
                _ => forward,
            });
        }
        match item.kind {
            ResolvedLinkKind::Objects(_) | ResolvedLinkKind::ObjectFile => Some(true),
            ResolvedLinkKind::Library(id) => match model.target(id)?.kind {
                TargetKind::StaticLibrary if all => Some(keep_static_first),
                TargetKind::SharedLibrary if shared => Some(forward),
                _ if all => Some(forward),
                _ => None,
            },
            ResolvedLinkKind::Raw if all && raw_library(item) => Some(forward),
            _ => None,
        }
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
    Ok(())
}

#[cfg(test)]
mod tests {
    use rcmake_diag::{Backtrace, PolicyMap};
    use rcmake_model::{BT, DirId, LinkItem, TargetOrigin, UsageScope};

    use super::*;

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    fn add_target(
        model: &mut ProjectModel,
        name: &str,
        kind: TargetKind,
    ) -> Result<TargetId, Box<dyn std::error::Error>> {
        Ok(model.add_target(Target::new(
            name.into(),
            kind,
            TargetOrigin::Normal,
            DirId::ROOT,
            Backtrace::default(),
            PolicyMap::default(),
        ))?)
    }

    fn link_items(model: &mut ProjectModel, id: TargetId, interface: bool, items: &[&str]) {
        let Some(target) = model.target_mut(id) else {
            return;
        };
        let entries = if interface {
            &mut target.usage.interface_link_libraries
        } else {
            &mut target.usage.link_libraries
        };
        entries
            .get_or_insert_default()
            .extend(items.iter().map(|name| LinkItem {
                opens_lookup_scope: false,
                closes_lookup_scope: false,
                scope: None,
                text: BT::new((*name).into(), Backtrace::default()),
                lookup_dir: DirId::ROOT,
            }));
    }

    #[test]
    fn singleton_raw_library_clears_prior_inferred_edge() -> TestResult {
        let mut model = ProjectModel::default();
        model.set_root_dirs("/src".into(), "/build".into());
        let app = add_target(&mut model, "app", TargetKind::Executable)?;
        let x = add_target(&mut model, "x", TargetKind::StaticLibrary)?;
        // `app` lists `alpha;beta;x`, suggesting `alpha -> beta`, while `x` lists only `[alpha]`.
        // The singleton raw library list `[alpha]` must record its empty successor set and clear
        // the `alpha -> beta` suggestion so `beta` is not appended again after `x;alpha`.
        link_items(&mut model, app, false, &["alpha", "beta", "x"]);
        link_items(&mut model, x, true, &["alpha"]);

        let mut resolver = UsageResolver::new(&model);
        let ordered = resolver.ordered_links(app, &EvalContext::default())?;
        let names: Vec<_> = ordered
            .iter()
            .map(|item| item.text.value.to_string())
            .collect();
        assert_eq!(names, vec!["alpha", "beta", "x", "alpha"]);
        Ok(())
    }

    #[test]
    fn singleton_direct_raw_library_blocks_later_inferred_edge() -> TestResult {
        let mut model = ProjectModel::default();
        model.set_root_dirs("/src".into(), "/build".into());
        let app = add_target(&mut model, "app", TargetKind::Executable)?;
        let x = add_target(&mut model, "x", TargetKind::StaticLibrary)?;
        let z = add_target(&mut model, "z", TargetKind::StaticLibrary)?;
        // `app` lists only one raw library (`alpha`) alongside `x`, while `z` lists `alpha;beta`.
        // Recording `alpha`'s empty successor set from `app` prevents inferring `alpha -> beta`.
        link_items(&mut model, app, false, &["alpha", "x"]);
        link_items(&mut model, x, true, &["beta", "z"]);
        link_items(&mut model, z, true, &["alpha", "beta"]);

        let mut resolver = UsageResolver::new(&model);
        let ordered = resolver.ordered_links(app, &EvalContext::default())?;
        let names: Vec<_> = ordered
            .iter()
            .map(|item| item.text.value.to_string())
            .collect();
        // Verified against CMake 4.5.1: `-lalpha libx.a libz.a -lalpha -lbeta`.
        assert_eq!(names, vec!["alpha", "x", "z", "alpha", "beta"]);
        Ok(())
    }

    #[test]
    fn linear_chain_fast_path_enforces_size_limit() -> TestResult {
        let mut model = ProjectModel::default();
        model.set_root_dirs("/src".into(), "/build".into());
        let app_id = add_target(&mut model, "app", TargetKind::Executable)?;
        let owner = model.target(app_id).ok_or("missing app target")?;

        let mut graph = LinkGraph::default();
        let mut items = Vec::new();
        for idx in 0..5usize {
            let item = Arc::new(ResolvedLinkItem {
                text: BT::new(format!("lib{idx}").into(), Backtrace::default()),
                kind: ResolvedLinkKind::Raw,
                feature: None,
                direct: idx == 0,
                scope: UsageScope::Private,
                injected_by: None,
            });
            let node = graph.insert(&item);
            items.push((node, item));
        }
        for idx in 0..4usize {
            if let Some((_, (_, edges))) = graph.nodes.get_index_mut(idx) {
                edges.insert(idx + 1);
            }
        }
        let first = vec![items.first().ok_or("missing first item")?.clone()];
        let err = graph.order_with_limit(
            &model,
            owner,
            &EvalContext::default(),
            first.clone(),
            LinkOrder::Minimal,
            4,
        );
        assert!(
            matches!(err, Err(ResolveError::Message(ref msg)) if msg == "The link line exceeds the size limit."),
            "5-node linear chain must fail when max_len is 4: {err:?}"
        );
        let ok = graph.order_with_limit(
            &model,
            owner,
            &EvalContext::default(),
            first,
            LinkOrder::Minimal,
            5,
        )?;
        assert_eq!(ok.len(), 5);
        Ok(())
    }
}
