//! Transitive usage requirements (ADR-007 item 6).
//!
//! Compile usage follows `INTERFACE_LINK_LIBRARIES` but skips `$<LINK_ONLY:...>` items.
//! Link usage includes them. Compile-usage deduplication (encounter order and `SYSTEM`
//! classification) stays separate from link-line ordering.

use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use indexmap::{IndexMap, IndexSet};
use rcmake_base::{BString, Limits, list, truth};
use rcmake_diag::{Backtrace, Frame};
use rcmake_genex::{
    EvalContext, EvalFlags, GenexAst, GenexError, ParseCache, TargetHandle, UsageTraversal,
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

pub(crate) mod artifacts;
mod compatible;
mod debug;
mod entries;
mod host;
mod includes;
mod link;
pub(crate) mod link_options;
pub(crate) mod paths;
pub(crate) mod provenance;
mod wrappers;

pub use host::{ModelGenexHost, ObjectPaths};
pub use link::{ResolvedLinkItem, ResolvedLinkKind, collect_link_items};

use crate::ResolveError;

pub(crate) type SourceFileSetEntry<'a> = (FileSetId, &'a FileSet, Option<Backtrace>);

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
    chains: Option<IndexMap<TargetId, Vec<crate::Hop>>>,
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
    device_link: bool,
    property_read: bool,
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
            device_link: self.device_link,
            property_read: self.property_read,
            command_config: self.command_config.map(owned),
            output_config: self.output_config.map(owned),
        }
    }
}

type UsagePaths = Vec<Vec<crate::Hop>>;

const MAX_WALK_DEPTH: usize = 128;

/// Evaluate usage requirements against one immutable project model.
pub struct UsageResolver<'a> {
    model: &'a ProjectModel,
    ide: Option<crate::ide::IdeFlavor>,
    pub(crate) pch_objects: IndexMap<(TargetId, rcmake_genex::ConfigId), Vec<BString>>,
    artifact_properties: Vec<(TargetId, rcmake_genex::ConfigId, BString)>,
    source_paths: Option<&'a SourcePaths<'a>>,
    ids: Arc<[TargetId]>,
    parsed: ParseCache,
    memo: HashMap<MemoKey<'static>, Arc<WalkValue>>,
    link_memo: HashMap<link::LinkMemoKey, link::SharedLinks>,
    debug_configs: Vec<BString>,
    debug_properties: bool,
    codemodel_backtraces: bool,
    active: Vec<(TargetId, BString)>,
    cycle_count: usize,
    has_injections: bool,
    compatible: compatible::Properties,
    pub(crate) provenance: Option<provenance::Provenance>,
    pub(crate) runtime_install_targets: IndexSet<TargetId>,
    /// Number of active `$<TARGET_PROPERTY>` reads. CMake checks for relative
    /// interface paths only when it collects the usage requirements of a
    /// consumer, not when an expression reads the property.
    property_reads: usize,
    /// Backtrace of the outermost link edge being traversed for policy warnings.
    current_link_bt: Option<Backtrace>,
}

impl<'a> UsageResolver<'a> {
    /// Start a resolution session. Reuse it across targets and configurations.
    #[must_use]
    pub fn new(model: &'a ProjectModel) -> Self {
        Self {
            model,
            ide: crate::ide::IdeFlavor::from_model(model),
            artifact_properties: Vec::new(),
            pch_objects: IndexMap::new(),
            runtime_install_targets: IndexSet::new(),
            source_paths: None,
            ids: model.targets().map(|(id, _)| id).collect(),
            parsed: ParseCache::default(),
            memo: HashMap::new(),
            link_memo: HashMap::new(),
            debug_configs: model
                .global
                .props
                .get(b"DEBUG_CONFIGURATIONS")
                .map_or_else(Vec::new, |value| list::split(value, list::Empty::Drop)),
            debug_properties: model.dirs().any(|(id, _)| {
                definition(model, id, b"CMAKE_DEBUG_TARGET_PROPERTIES")
                    .is_some_and(|value| !value.is_empty())
            }),
            codemodel_backtraces: false,
            active: Vec::new(),
            cycle_count: 0,
            compatible: compatible::Properties::default(),
            provenance: None,
            property_reads: 0,
            current_link_bt: None,
            has_injections: model.targets().any(|(_, target)| {
                target.props.get(link::DIRECT).is_some()
                    || target.props.get(link::EXCLUDE).is_some()
            }),
        }
    }

    /// Attribute inherited usage to the consumer's first link edge for the File API.
    #[must_use]
    pub fn for_codemodel(model: &'a ProjectModel) -> Self {
        Self {
            codemodel_backtraces: true,
            ..Self::new(model)
        }
    }

    /// Return inherited target-usage `(original_entry, consumer_link_backtrace)` pairs
    /// for File API codemodel backtrace attribution.
    ///
    /// # Errors
    ///
    /// Returns an error if the target or its usage generator expressions are invalid.
    pub fn codemodel_inherited_backtraces(
        &mut self,
        target: TargetId,
        field: UsageField,
        ctx: &EvalContext,
    ) -> Result<Vec<(BT<BString>, Backtrace)>, ResolveError> {
        let name = field.name().as_bytes();
        let mut ctx = self.property_context(target, name, ctx)?;
        if matches!(field, UsageField::LinkOptions | UsageField::LinkDirectories) {
            ctx.traversal = link_options::traversal(self.target(target)?);
        }
        let result = self.walk(target, name, &ctx)?;
        let mut pairs = Vec::new();
        for (owner, entries) in &result.contributions {
            if *owner == target {
                continue;
            }
            let Some(edge) = result
                .chains
                .as_ref()
                .and_then(|paths| paths.get(owner))
                .and_then(|path| path.first())
            else {
                continue;
            };
            for entry in &**entries {
                let value = if field == UsageField::CompileDefinitions {
                    entry
                        .value
                        .strip_prefix(b"-D")
                        .map_or_else(|| entry.value.clone(), BString::from)
                } else {
                    entry.value.clone()
                };
                pairs.push((
                    BT::new(value, entry.backtrace.clone()),
                    edge.link_bt.clone(),
                ));
            }
        }
        Ok(pairs)
    }

    pub(crate) fn with_source_paths(model: &'a ProjectModel, paths: &'a SourcePaths<'a>) -> Self {
        Self {
            ide: paths.ide,
            source_paths: Some(paths),
            ..Self::new(model)
        }
    }

    fn fork_for_artifacts(&self) -> Self {
        Self {
            model: self.model,
            ide: self.ide,
            artifact_properties: self.artifact_properties.clone(),
            pch_objects: self.pch_objects.clone(),
            runtime_install_targets: IndexSet::new(),
            source_paths: None,
            ids: Arc::clone(&self.ids),
            parsed: ParseCache::default(),
            memo: HashMap::new(),
            link_memo: HashMap::new(),
            debug_configs: self.debug_configs.clone(),
            debug_properties: self.debug_properties,
            codemodel_backtraces: false,
            active: Vec::new(),
            cycle_count: 0,
            compatible: compatible::Properties::default(),
            provenance: None,
            property_reads: 0,
            has_injections: self.has_injections,
            current_link_bt: None,
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
        // CMake reports Xcode attribute errors without a call stack.
        if property.starts_with(b"XCODE_ATTRIBUTE_") {
            let entry = BT::new(value.clone(), Default::default());
            return self
                .eval_entry(id, target.dir, property, &entry, ctx, false)
                .map_err(|error| match error {
                    ResolveError::Genex(error) => ResolveError::Genex(error.without_location()),
                    error => error,
                });
        }
        let entry = BT::new(value.clone(), target.backtrace.clone());
        self.eval_entry(id, target.dir, property, &entry, ctx, false)
    }

    pub(crate) fn source_paths(&self) -> Option<&'a SourcePaths<'a>> {
        self.source_paths
    }

    pub(crate) fn parse_genex(&mut self, input: &[u8]) -> Result<Arc<GenexAst>, GenexError> {
        self.parsed.parse(input)
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
        self.collect_usage(target, field, ctx, false)
            .map(|(value, _)| value)
    }

    pub(crate) fn collect_for_result(
        &mut self,
        target: TargetId,
        field: UsageField,
        ctx: &EvalContext,
    ) -> Result<UsageValue, ResolveError> {
        let (value, chains) = self.collect_usage(target, field, ctx, self.provenance.is_some())?;
        if let (Some(provenance), Some(chains)) = (&mut self.provenance, chains) {
            for (entry, hops) in value.entries.iter().zip(chains) {
                provenance.record(
                    target,
                    &ctx.config,
                    crate::ChainKind::Usage(field),
                    entry,
                    hops,
                    Vec::new(),
                );
            }
        }
        Ok(value)
    }

    fn collect_usage(
        &mut self,
        target: TargetId,
        field: UsageField,
        ctx: &EvalContext,
        record: bool,
    ) -> Result<(UsageValue, Option<UsagePaths>), ResolveError> {
        let name = field.name().as_bytes();
        let mut ctx = self.property_context(target, name, ctx)?;
        let mut chains = record.then(Vec::new);
        let mut value = if field == UsageField::LinkOptions
            && self.target(target)?.kind == TargetKind::StaticLibrary
        {
            let mut value = self.archive_options(target, &ctx)?;
            let mut seen = IndexMap::new();
            for entry in std::mem::take(&mut value.entries) {
                seen.entry(entry.value).or_insert(entry.backtrace);
            }
            value.entries.extend(
                seen.into_iter()
                    .map(|(value, backtrace)| BT::new(value, backtrace)),
            );
            if let Some(chains) = &mut chains {
                chains.resize_with(value.entries.len(), Vec::new);
            }
            value
        } else {
            // CMP0099 makes link properties transitive over private static dependencies.
            if matches!(field, UsageField::LinkOptions | UsageField::LinkDirectories) {
                ctx.traversal = link_options::traversal(self.target(target)?);
            }
            let result = self.walk(target, name, &ctx)?;
            self.log_usage(target, name, &result);
            let mut value = UsageValue {
                entries: Vec::new(),
                flags: result.flags,
            };
            let mut seen = HashSet::new();
            for (owner, entries) in &result.contributions {
                if field == UsageField::LinkDirectories
                    && self.property_reads == 0
                    && let Some(paths) = self.source_paths()
                {
                    crate::resolve_link::directories::check_relative(
                        self.model,
                        paths,
                        self.target(target)?,
                        self.target(*owner)?,
                        entries,
                    )?;
                }
                for entry in &**entries {
                    if seen.insert(entry.value.as_slice()) {
                        let mut entry = entry.clone();
                        if self.codemodel_backtraces
                            && let Some(edge) = result
                                .chains
                                .as_ref()
                                .and_then(|paths| paths.get(owner))
                                .and_then(|path| path.first())
                        {
                            entry.backtrace.clone_from(&edge.link_bt);
                        }
                        value.entries.push(entry);
                        if let Some(chains) = &mut chains {
                            chains.push(
                                result
                                    .chains
                                    .as_ref()
                                    .and_then(|paths| paths.get(owner))
                                    .cloned()
                                    .unwrap_or_default(),
                            );
                        }
                    }
                }
            }
            value
        };
        if field == UsageField::LinkOptions {
            self.prepare_link_options(target, &ctx, &mut value, chains.as_mut())?;
        }
        Ok((value, chains))
    }

    /// Collect a usage property by name, including sources and custom transitive properties.
    pub fn collect_property(
        &mut self,
        target: TargetId,
        name: &[u8],
        ctx: &EvalContext,
    ) -> Result<UsageValue, ResolveError> {
        self.collect_property_with(target, name, ctx, None)
    }

    /// Collect a usage property with an optional traversal in place of the
    /// traversal that the property name selects.
    pub(crate) fn collect_property_with(
        &mut self,
        target: TargetId,
        name: &[u8],
        ctx: &EvalContext,
        traversal: Option<UsageTraversal>,
    ) -> Result<UsageValue, ResolveError> {
        if self
            .active
            .last()
            .is_some_and(|(id, prop)| *id != target || prop != name)
            && self
                .active
                .iter()
                .any(|(id, prop)| *id == target && prop == name)
        {
            self.cycle_count += 1;
            return Ok(UsageValue::default());
        }
        let mut ctx = self.property_context(target, name, ctx)?;
        if let Some(traversal) = traversal {
            ctx.traversal = traversal;
        }
        if matches!(name, b"LINK_LIBRARIES" | b"INTERFACE_LINK_LIBRARIES") {
            return self.link_property(target, name, &ctx);
        }
        let result = self.walk(target, name, &ctx)?;
        if self.property_reads == 0 {
            self.log_usage(target, name, &result);
        }
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
            device_link: ctx.device_link,
            property_read: self.property_reads > 0,
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
        } else if let Some(entries) = target.props.tracked_entries(name) {
            self.eval_entries(id, name, entries, ctx, &mut own)?;
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
        for (dependency, edge) in links.iter().flat_map(|item| item.edges()) {
            let previous_link_bt = if self.current_link_bt.is_none() {
                self.current_link_bt.replace(edge.text.backtrace.clone())
            } else {
                None
            };
            let restore_link_bt = previous_link_bt.is_none() && self.current_link_bt.is_some();
            let value = self.walk(dependency, &dependency_name, ctx);
            if restore_link_bt {
                self.current_link_bt = previous_link_bt;
            }
            let value = value?;
            result.flags = result.flags.union(value.flags);
            for (owner, entries) in &value.contributions {
                if !seen.insert(*owner) {
                    continue;
                }
                result.contributions.push((*owner, Arc::clone(entries)));
                if self.provenance.is_some()
                    || self.debug_properties
                    || self.codemodel_backtraces
                    || name == b"SOURCES"
                {
                    let mut chain = vec![edge.hop(dependency)];
                    if let Some(path) = value.chains.as_ref().and_then(|chains| chains.get(owner)) {
                        chain.extend_from_slice(path);
                    }
                    result
                        .chains
                        .get_or_insert_with(IndexMap::new)
                        .insert(*owner, chain);
                }
            }
        }
        Ok(result)
    }

    pub(crate) fn source_file_sets(
        &mut self,
        target: TargetId,
        ctx: &EvalContext,
    ) -> Result<Vec<SourceFileSetEntry<'a>>, ResolveError> {
        let mut ctx = context(target, ctx, UsageTraversal::Compile);
        ctx.head.get_or_insert(handle(target));
        let owners = self.walk(target, b"SOURCES", &ctx)?;
        let model = self.model;
        Ok(owners
            .contributions
            .iter()
            .filter_map(|(owner, _)| {
                let link_bt = (*owner != target)
                    .then(|| {
                        owners
                            .chains
                            .as_ref()
                            .and_then(|chains| chains.get(owner))
                            .and_then(|chain| chain.first())
                            .map(|hop| hop.link_bt.clone())
                    })
                    .flatten();
                Some((model.target(*owner)?, link_bt))
            })
            .flat_map(|(owner, link_bt)| {
                owner.file_sets.iter().map(move |id| (*id, link_bt.clone()))
            })
            .filter_map(|(id, link_bt)| Some((id, model.file_set(id)?, link_bt)))
            .filter(|(_, set, _)| {
                let own = set.target == target;
                match set.kind {
                    FileSetKind::Headers => own,
                    FileSetKind::Sources if !own => set.scope != UsageScope::Private,
                    _ => {
                        own && (set.scope != UsageScope::Interface
                            || model
                                .target(target)
                                .is_some_and(crate::cxx_modules::is_std_provider))
                    }
                }
            })
            .collect())
    }

    /// Evaluate file set paths in the property owner and consumer contexts.
    ///
    /// For file-set `BASE_DIRS` and `SOURCES` entries, mirrors CMake's
    /// `cmGeneratorFileSet::CompileFileEntries` and `EvaluateFileEntry` (two list
    /// splits across generator-expression evaluation) without the extra list
    /// serialization step that only target-source expansion performs.
    pub fn file_set_paths(
        &mut self,
        owner: TargetId,
        property: &[u8],
        entries: &[BT<BString>],
        ctx: &EvalContext,
    ) -> Result<Vec<BT<BString>>, ResolveError> {
        let mut result = UsageValue::default();
        let dir = self.target(owner)?.dir;
        let is_file_set_entries = matches!(property, b"BASE_DIRS" | b"SOURCES");
        for entry in entries {
            if is_file_set_entries {
                for raw in list::split(&entry.value, list::Empty::Drop) {
                    let item = BT::new(raw, entry.backtrace.clone());
                    let (value, _) = self.eval_entry(owner, dir, property, &item, ctx, false)?;
                    for evaluated in list::split(&value, list::Empty::Drop) {
                        result.entries.push(BT::new(
                            self.usage_path(dir, property, evaluated),
                            entry.backtrace.clone(),
                        ));
                    }
                }
            } else {
                let (value, _) = self.eval_entry(owner, dir, property, entry, ctx, false)?;
                result
                    .entries
                    .extend(
                        list::split(&value, list::Empty::Drop)
                            .into_iter()
                            .map(|value| {
                                BT::new(
                                    self.usage_path(dir, property, value),
                                    entry.backtrace.clone(),
                                )
                            }),
                    );
            }
        }
        Ok(result.entries)
    }

    /// Evaluate a file set's `SOURCES` entries at the target-source boundary.
    ///
    /// `PRIVATE`/`PUBLIC` file sets (and propagated `SOURCES` file sets) enter a
    /// target's `SOURCES` through `FileSetPropertyEntry::Evaluate`
    /// (`cmList::to_string` followed by target `SOURCES` list expansion).
    /// `INTERFACE` `HEADERS` file sets are never added to `SOURCES`, so they
    /// stay on the two-split `EvaluateFileEntry` path.
    pub(crate) fn file_set_target_sources(
        &mut self,
        set: &rcmake_model::FileSet,
        ctx: &EvalContext,
    ) -> Result<Vec<BT<BString>>, ResolveError> {
        let evaluated = self.file_set_paths(set.target, b"SOURCES", &set.files, ctx)?;
        if set.kind == FileSetKind::Headers && set.scope == UsageScope::Interface {
            return Ok(evaluated);
        }
        let mut out = Vec::with_capacity(evaluated.len());
        for entry in evaluated {
            for item in list::split(&entry.value, list::Empty::Drop) {
                out.push(BT::new(item, entry.backtrace.clone()));
            }
        }
        Ok(out)
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

#[cfg(test)]
mod tests;
