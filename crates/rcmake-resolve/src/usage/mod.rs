//! Transitive usage requirements (ADR-007 item 6).
//!
//! Compile usage follows `INTERFACE_LINK_LIBRARIES` but skips `$<LINK_ONLY:...>` items.
//! Link usage includes them. Compile-usage deduplication (encounter order and `SYSTEM`
//! classification) stays separate from link-line ordering.

use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use rcmake_base::{BString, Limits, list, truth};
use rcmake_diag::Frame;
use rcmake_genex::{
    EvalContext, EvalFlags, EvalSite, GenexError, GenexNode, TargetHandle, UsageTraversal,
    evaluate, parse,
};
use rcmake_model::{
    BT, DirId, FileSet, FileSetId, FileSetKind, ProjectModel, Target, TargetId, TargetKind,
    UsageField, UsageScope,
};

use crate::{
    InterfaceInclude,
    lower::{definition, without_trailing_slashes},
    sources::SourcePaths,
};

mod artifacts;
mod host;
mod link;
#[cfg(test)]
mod tests;

pub use host::ModelGenexHost;
pub use link::{ResolvedLinkItem, ResolvedLinkKind, collect_link_items};

use crate::ResolveError;

/// Memoization key of one transitive evaluation (ADR-006 item 4). `head` is only part
/// of the key when the cached value is head-sensitive.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct UsageKey {
    /// Target whose interface is walked.
    pub target: TargetId,
    /// Property.
    pub field: UsageField,
    /// Configuration name.
    pub config: BString,
    /// Compile language, if any.
    pub language: Option<BString>,
    /// Consuming target, when the value depends on it.
    pub head: Option<TargetId>,
    /// Walk kind.
    pub traversal: UsageTraversal,
}

/// Result of a transitive walk: entries in encounter order plus dependency flags.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct UsageValue {
    /// Evaluated entries.
    pub entries: Vec<BT<BString>>,
    /// What the value depends on.
    pub flags: EvalFlags,
}

#[derive(Default)]
struct WalkValue {
    contributions: Vec<(TargetId, Arc<[BT<BString>]>)>,
    flags: EvalFlags,
}

#[derive(Debug, PartialEq, Eq, Hash)]
struct MemoKey<'a> {
    target: TargetId,
    property: Cow<'a, [u8]>,
    config: Cow<'a, [u8]>,
    language: Option<Cow<'a, [u8]>>,
    head: Option<TargetId>,
    traversal: UsageTraversal,
    link_language: Option<Cow<'a, [u8]>>,
    command_config: Option<Cow<'a, [u8]>>,
    output_config: Option<Cow<'a, [u8]>>,
}

impl MemoKey<'_> {
    fn into_owned(self) -> MemoKey<'static> {
        let owned = |value: Cow<'_, [u8]>| Cow::Owned(value.into_owned());
        MemoKey {
            target: self.target,
            property: owned(self.property),
            config: owned(self.config),
            language: self.language.map(owned),
            head: self.head,
            traversal: self.traversal,
            link_language: self.link_language.map(owned),
            command_config: self.command_config.map(owned),
            output_config: self.output_config.map(owned),
        }
    }
}

const MAX_WALK_DEPTH: usize = 128;

/// Evaluate usage requirements against one immutable project model.
pub struct UsageResolver<'a> {
    model: &'a ProjectModel,
    ide: Option<crate::ide::IdeFlavor>,
    artifact_depth: usize,
    source_paths: Option<&'a SourcePaths<'a>>,
    ids: Vec<TargetId>,
    memo: HashMap<MemoKey<'static>, Arc<WalkValue>>,
    link_memo: HashMap<link::LinkMemoKey, link::SharedLinks>,
    debug_configs: Vec<BString>,
    active: Vec<(TargetId, BString)>,
    cycle_count: usize,
    has_injections: bool,
}

impl<'a> UsageResolver<'a> {
    /// Start a resolution session. Reuse it across targets and configurations.
    #[must_use]
    pub fn new(model: &'a ProjectModel) -> Self {
        Self {
            model,
            ide: crate::ide::IdeFlavor::from_model(model),
            artifact_depth: 0,
            source_paths: None,
            ids: model.targets().map(|(id, _)| id).collect(),
            memo: HashMap::new(),
            link_memo: HashMap::new(),
            debug_configs: model
                .global
                .props
                .get(b"DEBUG_CONFIGURATIONS")
                .map_or_else(Vec::new, |value| list::split(value, list::Empty::Drop)),
            active: Vec::new(),
            cycle_count: 0,
            has_injections: model.targets().any(|(_, target)| {
                target.props.get(link::DIRECT).is_some()
                    || target.props.get(link::EXCLUDE).is_some()
            }),
        }
    }

    pub(crate) fn with_source_paths(model: &'a ProjectModel, paths: &'a SourcePaths<'a>) -> Self {
        Self {
            ide: paths.ide,
            source_paths: Some(paths),
            ..Self::new(model)
        }
    }

    pub(crate) fn with_ide(mut self, ide: Option<crate::ide::IdeFlavor>) -> Self {
        self.ide = ide;
        self
    }

    pub(crate) fn ide(&self) -> Option<crate::ide::IdeFlavor> {
        self.ide
    }

    pub(crate) fn eval_ide_property(
        &mut self,
        id: TargetId,
        property: &[u8],
        value: &BString,
        ctx: &EvalContext,
    ) -> Result<BString, ResolveError> {
        self.eval_ide_property_with_flags(id, property, value, ctx)
            .map(|(value, _)| value)
    }

    pub(crate) fn eval_ide_property_with_flags(
        &mut self,
        id: TargetId,
        property: &[u8],
        value: &BString,
        ctx: &EvalContext,
    ) -> Result<(BString, EvalFlags), ResolveError> {
        let target = self.target(id)?;
        let entry = BT::new(value.clone(), target.backtrace.clone());
        self.eval_entry(id, target.dir, property, &entry, ctx, false)
    }

    pub(crate) fn source_paths(&self) -> Option<&'a SourcePaths<'a>> {
        self.source_paths
    }

    /// Borrow a generator expression host with the given default lookup directory.
    pub fn host(&mut self, lookup_dir: DirId) -> ModelGenexHost<'a, '_> {
        ModelGenexHost::new(self, lookup_dir)
    }

    /// Collect a typed usage property in encounter order.
    pub fn collect(
        &mut self,
        target: TargetId,
        field: UsageField,
        ctx: &EvalContext,
    ) -> Result<UsageValue, ResolveError> {
        let name = field.name().as_bytes();
        let ctx = self.property_context(target, name, ctx)?;
        let result = self.walk(target, name, &ctx)?;
        let mut seen = HashSet::new();
        Ok(UsageValue {
            entries: result
                .contributions
                .iter()
                .flat_map(|(_, entries)| entries.iter())
                .filter(|entry| seen.insert(entry.value.as_slice()))
                .cloned()
                .collect(),
            flags: result.flags,
        })
    }

    /// Collect a usage property by name, including sources and custom transitive properties.
    pub fn collect_property(
        &mut self,
        target: TargetId,
        name: &[u8],
        ctx: &EvalContext,
    ) -> Result<UsageValue, ResolveError> {
        let ctx = self.property_context(target, name, ctx)?;
        if matches!(name, b"LINK_LIBRARIES" | b"INTERFACE_LINK_LIBRARIES") {
            return self.link_property(target, name, &ctx);
        }
        let result = self.walk(target, name, &ctx)?;
        Ok(UsageValue {
            entries: result
                .contributions
                .iter()
                .flat_map(|(_, entries)| entries.iter().cloned())
                .collect(),
            flags: result.flags,
        })
    }

    fn property_context(
        &self,
        target: TargetId,
        name: &[u8],
        ctx: &EvalContext,
    ) -> Result<EvalContext, ResolveError> {
        let mut ctx = context(target, ctx, self.traversal(target, name));
        ctx.head.get_or_insert(handle(target));
        self.check_reentry(target, name)?;
        Ok(ctx)
    }

    fn traversal(&self, target: TargetId, name: &[u8]) -> UsageTraversal {
        let base = interface_base(name);
        if base.starts_with(b"LINK_")
            || self
                .model
                .target(target)
                .and_then(|t| t.props.get(b"TRANSITIVE_LINK_PROPERTIES"))
                .is_some_and(|props| {
                    list::split(props, list::Empty::Drop)
                        .iter()
                        .any(|p| p == base)
                })
        {
            UsageTraversal::Link
        } else {
            UsageTraversal::Compile
        }
    }

    fn target(&self, target: TargetId) -> Result<&'a Target, ResolveError> {
        self.model
            .target(target)
            .ok_or_else(|| ResolveError::Message("Unknown target.".into()))
    }

    fn check_reentry(&self, target: TargetId, name: &[u8]) -> Result<(), ResolveError> {
        if self
            .active
            .iter()
            .any(|(id, prop)| *id == target && prop == name)
        {
            return Err(GenexError::SelfReference {
                target: self.target(target)?.name.clone(),
            }
            .into());
        }
        Ok(())
    }

    fn with_property<T>(
        &mut self,
        target: TargetId,
        name: &[u8],
        evaluate: impl FnOnce(&mut Self) -> Result<T, ResolveError>,
    ) -> Result<T, ResolveError> {
        if self.active.len() >= MAX_WALK_DEPTH {
            return Err(GenexError::TooDeep.into());
        }
        self.active.push((target, name.into()));
        let result = evaluate(self);
        self.active.pop();
        result
    }

    fn walk(
        &mut self,
        target: TargetId,
        name: &[u8],
        ctx: &EvalContext,
    ) -> Result<Arc<WalkValue>, ResolveError> {
        // A link graph may contain a cycle. Stop this branch and avoid caching its
        // partial result. A generator expression that re-enters a property is an error.
        let in_cycle = self
            .active
            .iter()
            .any(|(id, prop)| *id == target && prop == name);
        if in_cycle {
            self.cycle_count += 1;
            return Ok(Arc::default());
        }
        let mut key = MemoKey {
            target,
            property: Cow::Borrowed(name),
            config: Cow::Borrowed(&ctx.config.0),
            language: ctx
                .language
                .as_ref()
                .map(|value| Cow::Borrowed(value.as_slice())),
            head: None,
            traversal: ctx.traversal,
            link_language: ctx
                .link_language
                .as_ref()
                .map(|value| Cow::Borrowed(value.as_slice())),
            command_config: ctx
                .command_config
                .as_ref()
                .map(|value| Cow::Borrowed(value.0.as_slice())),
            output_config: ctx
                .output_config
                .as_ref()
                .map(|value| Cow::Borrowed(value.0.as_slice())),
        };
        if let Some(value) = self.memo.get(&key) {
            return Ok(Arc::clone(value));
        }
        key.head = ctx.head.and_then(|id| self.ids.get(id.0 as usize).copied());
        if let Some(value) = self.memo.get(&key) {
            return Ok(Arc::clone(value));
        }
        let cycle_count = self.cycle_count;
        let value = Arc::new(
            self.with_property(target, name, |this| this.walk_uncached(target, name, ctx))?,
        );
        if self.cycle_count == cycle_count {
            if !value.flags.head_sensitive {
                key.head = None;
            }
            self.memo.insert(key.into_owned(), Arc::clone(&value));
        }
        Ok(value)
    }

    fn walk_uncached(
        &mut self,
        id: TargetId,
        name: &[u8],
        ctx: &EvalContext,
    ) -> Result<WalkValue, ResolveError> {
        let target = self.target(id)?;
        let mut own = UsageValue::default();
        if let Some(field) = UsageField::from_name(name) {
            let directory = &self.model.dir(target.dir).usage;
            let entries = target.usage.entries(field);
            if field == UsageField::CompileDefinitions
                && !target.is_imported()
                && target.kind != TargetKind::InterfaceLibrary
            {
                // Replace the directory snapshot with the directory's current definitions.
                let definitions = directory.entries(field);
                let inherited: HashSet<_> = definitions.iter().map(entry_origin).collect();
                let target_definitions = entries.iter().filter(|entry| {
                    !is_directory_definition(entry) && !inherited.contains(&entry_origin(entry))
                });
                self.eval_entries(
                    id,
                    name,
                    target_definitions.chain(definitions),
                    ctx,
                    &mut own,
                )?;
            } else {
                // include_directories() updates existing targets in search order.
                let entries = if field == UsageField::IncludeDirectories
                    && !target.is_imported()
                    && !target.usage.fields.contains_key(&field)
                {
                    directory.entries(field)
                } else {
                    entries
                };
                self.eval_entries(id, name, entries, ctx, &mut own)?;
            }
        } else if name == b"SOURCES" {
            self.eval_entries(id, name, &target.sources, ctx, &mut own)?;
        } else if let Some(raw) = target.props.get(name) {
            let entry = BT::new(raw.clone(), target.backtrace.clone());
            self.eval_entries(id, name, &[entry], ctx, &mut own)?;
        }

        let interface = name.starts_with(b"INTERFACE_");
        let dependency_name = if interface {
            Cow::Borrowed(name)
        } else {
            Cow::Owned([b"INTERFACE_", name].concat())
        };
        let (links, flags) = self.usage_links(id, interface, ctx)?;
        let mut result = WalkValue {
            flags: own.flags.union(flags),
            ..WalkValue::default()
        };
        if !own.entries.is_empty()
            || (matches!(name, b"SOURCES" | b"INTERFACE_SOURCES") && !target.file_sets.is_empty())
        {
            result.contributions.push((id, own.entries.into()));
        }
        let mut seen = HashSet::from([id]);
        for dependency in links.iter().flat_map(|item| item.edge_targets()) {
            let value = self.walk(dependency, &dependency_name, ctx)?;
            result.flags = result.flags.union(value.flags);
            result.contributions.extend(
                value
                    .contributions
                    .iter()
                    .filter(|(owner, _)| seen.insert(*owner))
                    .cloned(),
            );
        }
        Ok(result)
    }

    pub(crate) fn source_file_sets(
        &mut self,
        target: TargetId,
        ctx: &EvalContext,
    ) -> Result<Vec<(FileSetId, &'a FileSet)>, ResolveError> {
        let mut ctx = context(target, ctx, UsageTraversal::Compile);
        ctx.head.get_or_insert(handle(target));
        let owners = self.walk(target, b"SOURCES", &ctx)?;
        let model = self.model;
        Ok(owners
            .contributions
            .iter()
            .filter_map(|(owner, _)| model.target(*owner))
            .flat_map(|owner| owner.file_sets.iter())
            .filter_map(|id| Some((*id, model.file_set(*id)?)))
            .filter(|(_, set)| {
                let own = set.target == target;
                match set.kind {
                    FileSetKind::Headers => own && self.ide.is_some(),
                    FileSetKind::Sources if !own => set.scope != UsageScope::Private,
                    _ => {
                        own && (set.scope != UsageScope::Interface
                            || model
                                .target(target)
                                .is_some_and(|target| target.name == crate::STD_MODULE_TARGET))
                    }
                }
            })
            .collect())
    }

    pub(crate) fn file_set_paths(
        &mut self,
        owner: TargetId,
        property: &[u8],
        entries: &[BT<BString>],
        ctx: &EvalContext,
    ) -> Result<Vec<BT<BString>>, ResolveError> {
        let mut result = UsageValue::default();
        self.eval_entries(owner, property, entries, ctx, &mut result)?;
        Ok(result.entries)
    }

    pub(crate) fn eval_source_compile_flags(
        &mut self,
        target: TargetId,
        entry: &BT<BString>,
        ctx: &EvalContext,
    ) -> Result<BString, ResolveError> {
        let dir = self.target(target)?.dir;
        let (value, _) = self.eval_entry(target, dir, b"COMPILE_FLAGS", entry, ctx, false)?;
        Ok(value)
    }

    fn eval_entries<'entry>(
        &mut self,
        target: TargetId,
        property: &[u8],
        entries: impl IntoIterator<Item = &'entry BT<BString>>,
        ctx: &EvalContext,
        result: &mut UsageValue,
    ) -> Result<(), ResolveError> {
        let dir = self.target(target)?.dir;
        for entry in entries {
            let (value, flags) = self.eval_entry(target, dir, property, entry, ctx, false)?;
            result.flags = result.flags.union(flags);
            result.entries.extend(
                list::split(&value, list::Empty::Drop)
                    .into_iter()
                    .map(|value| BT::new(value, entry.backtrace.clone())),
            );
        }
        Ok(())
    }

    fn eval_entry(
        &mut self,
        target: TargetId,
        dir: DirId,
        property: &[u8],
        entry: &BT<BString>,
        ctx: &EvalContext,
        link_edges: bool,
    ) -> Result<(BString, EvalFlags), ResolveError> {
        if !entry.value.windows(2).any(|window| window == b"$<") {
            return Ok((entry.value.clone(), EvalFlags::default()));
        }
        let mut ctx = context(target, ctx, ctx.traversal);
        ctx.site = EvalSite {
            backtrace: entry.backtrace.clone(),
            property: Some(property.into()),
        };
        let mut ast = parse(&entry.value)?;
        if link_edges || ctx.traversal == UsageTraversal::Compile {
            prepare_nodes(&mut ast.nodes, ctx.traversal, link_edges);
        }
        let mut host = self.host(dir);
        host.current = ctx.current;
        Ok(evaluate(&ast, &ctx, &mut host)?)
    }

    /// Collect includes in search order and mark system paths.
    pub fn collect_includes(
        &mut self,
        target: TargetId,
        ctx: &EvalContext,
    ) -> Result<Vec<InterfaceInclude>, ResolveError> {
        use UsageField::*;
        let entries = self.collect(target, IncludeDirectories, ctx)?.entries;
        let mut ctx = context(target, ctx, UsageTraversal::Compile);
        let head = ctx.head.get_or_insert(handle(target));
        let no_imported_system = self
            .ids
            .get(head.0 as usize)
            .and_then(|id| self.model.target(*id))
            .is_some_and(|head| on(head, b"NO_SYSTEM_FROM_IMPORTED"));
        let owner = self.target(target)?;
        let mut system = UsageValue::default();
        self.eval_entries(
            target,
            b"SYSTEM_INCLUDE_DIRECTORIES",
            self.model
                .dir(owner.dir)
                .usage
                .entries(InterfaceSystemIncludeDirectories),
            &ctx,
            &mut system,
        )?;
        // A SYSTEM PUBLIC include has matching entries from the same command.
        let own_includes: HashSet<_> = owner
            .usage
            .entries(IncludeDirectories)
            .iter()
            .map(entry_origin)
            .collect();
        self.eval_entries(
            target,
            InterfaceSystemIncludeDirectories.name().as_bytes(),
            owner
                .usage
                .entries(InterfaceSystemIncludeDirectories)
                .iter()
                .filter(|entry| own_includes.contains(&entry_origin(entry))),
            &ctx,
            &mut system,
        )?;
        if let Some(raw) = owner.props.get(b"SYSTEM_INCLUDE_DIRECTORIES") {
            self.eval_entries(
                target,
                b"SYSTEM_INCLUDE_DIRECTORIES",
                &[BT::new(raw.clone(), owner.backtrace.clone())],
                &ctx,
                &mut system,
            )?;
        }

        let (links, _) = self.usage_links(target, false, &ctx)?;
        let mut todo: Vec<_> = links.iter().flat_map(|item| item.edge_targets()).collect();
        let mut visited = HashSet::new();
        while let Some(id) = todo.pop() {
            if !visited.insert(id) {
                continue;
            }
            let dependency = self.target(id)?;
            let default_system = dependency.props.get(b"SYSTEM").map_or(
                dependency.is_imported() || self.model.dir(dependency.dir).system,
                |value| truth::is_on(value),
            );
            self.eval_entries(
                id,
                InterfaceSystemIncludeDirectories.name().as_bytes(),
                dependency.usage.entries(InterfaceSystemIncludeDirectories),
                &ctx,
                &mut system,
            )?;
            if default_system && !(dependency.is_imported() && no_imported_system) {
                self.eval_entries(
                    id,
                    InterfaceIncludeDirectories.name().as_bytes(),
                    dependency.usage.entries(InterfaceIncludeDirectories),
                    &ctx,
                    &mut system,
                )?;
            }
            let (links, _) = self.usage_links(id, true, &ctx)?;
            todo.extend(links.iter().flat_map(|item| item.edge_targets()));
        }

        let system: HashSet<_> = system
            .entries
            .iter()
            .map(|entry| without_trailing_slashes(&entry.value))
            .collect();
        let directory = self.model.dir(owner.dir);
        let current_dirs = definition(self.model, owner.dir, b"CMAKE_INCLUDE_CURRENT_DIR")
            .filter(|value| truth::is_on(value))
            .into_iter()
            .flat_map(|_| [&directory.binary_dir, &directory.source_dir])
            .filter(|path| !path.is_empty())
            .map(|path| BT::new(path.clone(), Default::default()));
        let mut seen = HashSet::new();
        let mut includes: Vec<_> = current_dirs
            .chain(entries)
            .map(|mut path| {
                let len = without_trailing_slashes(&path.value).len();
                path.value.truncate(len);
                path
            })
            .filter(|path| seen.insert(path.value.clone()))
            .map(|path| InterfaceInclude {
                system: system.contains(path.value.as_slice()),
                path,
            })
            .collect();
        includes.sort_by_key(|include| include.system);
        Ok(includes)
    }
}

fn entry_origin(entry: &BT<BString>) -> (&[u8], Option<*const Frame>) {
    // Copies of an entry share the same backtrace frame.
    (
        entry.value.as_slice(),
        entry.backtrace.top().map(std::ptr::from_ref),
    )
}

fn is_directory_definition(entry: &BT<BString>) -> bool {
    entry
        .backtrace
        .top()
        .and_then(|frame| frame.name.as_deref())
        .is_some_and(|name| {
            [
                b"add_definitions".as_slice(),
                b"add_compile_definitions",
                b"set_directory_properties",
            ]
            .iter()
            .any(|command| name.eq_ignore_ascii_case(command))
        })
}

/// Collect `field` for `target` and its transitive interface in `ctx`.
pub fn collect(
    model: &ProjectModel,
    target: TargetId,
    field: UsageField,
    ctx: &EvalContext,
) -> Result<UsageValue, ResolveError> {
    UsageResolver::new(model).collect(target, field, ctx)
}

/// Collect includes in search order and mark system paths.
pub fn collect_includes(
    model: &ProjectModel,
    target: TargetId,
    ctx: &EvalContext,
) -> Result<Vec<InterfaceInclude>, ResolveError> {
    UsageResolver::new(model).collect_includes(target, ctx)
}

fn context(target: TargetId, ctx: &EvalContext, traversal: UsageTraversal) -> EvalContext {
    EvalContext {
        current: Some(handle(target)),
        traversal,
        ..ctx.clone()
    }
}

pub(crate) fn handle(id: TargetId) -> TargetHandle {
    TargetHandle(id.index() as u32)
}

fn interface_base(name: &[u8]) -> &[u8] {
    name.strip_prefix(b"INTERFACE_").unwrap_or(name)
}

fn on(target: &Target, property: &[u8]) -> bool {
    target
        .props
        .get(property)
        .is_some_and(|value| truth::is_on(value))
}

fn prepare_nodes(nodes: &mut Vec<GenexNode>, traversal: UsageTraversal, link_edges: bool) {
    let skipped = match traversal {
        UsageTraversal::Compile => b"LINK_ONLY".as_slice(),
        UsageTraversal::Link => b"COMPILE_ONLY".as_slice(),
    };
    let mut expanded = Vec::with_capacity(nodes.len());
    for mut node in std::mem::take(nodes) {
        if let GenexNode::Expr(expr) = &mut node {
            if let [GenexNode::Text(name)] = expr.identifier.as_slice() {
                if name == skipped && expr.params.as_ref().is_some_and(|args| args.len() == 1) {
                    continue;
                }
                // The libraries inside a link feature also supply compile usage.
                if link_edges
                    && traversal == UsageTraversal::Compile
                    && matches!(name.as_slice(), b"LINK_LIBRARY" | b"LINK_GROUP")
                    && let Some(params) = &mut expr.params
                    && params.len() > 1
                {
                    for (index, mut param) in params.drain(1..).enumerate() {
                        if index > 0 {
                            expanded.push(GenexNode::Text(";".into()));
                        }
                        prepare_nodes(&mut param, traversal, link_edges);
                        expanded.extend(param);
                    }
                    continue;
                }
            }
            prepare_nodes(&mut expr.identifier, traversal, link_edges);
            if let Some(params) = &mut expr.params {
                for param in params {
                    prepare_nodes(param, traversal, link_edges);
                }
            }
        }
        expanded.push(node);
    }
    *nodes = expanded;
}
