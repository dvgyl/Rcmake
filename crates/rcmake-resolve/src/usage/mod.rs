//! Transitive usage requirements (ADR-007 item 6).
//!
//! Compile usage follows `INTERFACE_LINK_LIBRARIES` but skips `$<LINK_ONLY:...>` items.
//! Link usage includes them. Compile-usage deduplication (encounter order and `SYSTEM`
//! classification) stays separate from link-line ordering.

use std::borrow::Cow;
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
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
pub(crate) mod includes;
mod link;
pub(crate) mod link_options;
pub(crate) mod paths;
pub(crate) mod provenance;
mod wrappers;

pub use host::{ModelGenexHost, ObjectPaths};
pub use link::{ResolvedLinkItem, ResolvedLinkKind, collect_link_items};
pub(crate) use link_options::traversal as link_property_traversal;

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
    /// Range-encoded direct consumer link backtrace for each inherited contribution slice
    /// (`(start, end, link_bt)` covering `contributions[start..end]`).
    first_links: Vec<(usize, usize, Backtrace)>,
    flags: EvalFlags,
}

impl WalkValue {
    fn first_link_bt(&self, contrib_index: usize) -> Option<&Backtrace> {
        if self.first_links.is_empty() {
            return None;
        }
        let pos = self
            .first_links
            .partition_point(|&(_, end, _)| end <= contrib_index);
        let (start, end, bt) = self.first_links.get(pos)?;
        (*start..*end).contains(&contrib_index).then_some(bt)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
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

const MAX_WALK_DEPTH: usize = 256;

/// Evaluate usage requirements against one immutable project model.
pub struct UsageResolver<'a> {
    model: &'a ProjectModel,
    ide: Option<crate::ide::IdeFlavor>,
    pub(crate) pch_objects: IndexMap<(TargetId, rcmake_genex::ConfigId), Vec<BString>>,
    artifact_properties: Vec<(TargetId, rcmake_genex::ConfigId, BString)>,
    source_paths: Option<&'a SourcePaths<'a>>,
    pub(crate) source_reads: Option<Rc<RefCell<IndexSet<TargetId>>>>,
    traced_sources: Option<&'a IndexMap<TargetId, IndexSet<BString>>>,
    ids: Arc<[TargetId]>,
    parsed: ParseCache,
    memo: HashMap<MemoKey<'static>, Arc<WalkValue>>,
    shared_memo: Option<Arc<HashMap<MemoKey<'static>, Arc<WalkValue>>>>,
    link_memo: HashMap<link::LinkMemoKey, link::SharedLinks>,
    shared_link_memo: Option<Arc<HashMap<link::LinkMemoKey, link::SharedLinks>>>,
    target_languages: HashMap<(TargetId, rcmake_genex::ConfigId), Arc<IndexSet<BString>>>,
    active_compile_language: Option<(TargetId, BString)>,
    debug_configs: Vec<BString>,
    debug_properties: bool,
    codemodel_backtraces: bool,
    active: Vec<(TargetId, BString)>,
    cycle_count: usize,
    has_injections: bool,
    has_system_interface_includes: bool,
    has_compatible_decls: bool,
    has_interface_pic: bool,
    has_language_runtime_libraries: bool,
    has_shared_or_imported_targets: bool,
    has_link_feature_overrides: bool,
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

/// Read-only snapshot of memoized usage walks and link lists that can be shared
/// across worker threads and resolution phases for the same [`ProjectModel`].
#[derive(Clone, Default)]
pub(crate) struct MemoSnapshot {
    memo: Option<Arc<HashMap<MemoKey<'static>, Arc<WalkValue>>>>,
    link_memo: Option<Arc<HashMap<link::LinkMemoKey, link::SharedLinks>>>,
}

impl MemoSnapshot {
    pub(crate) fn merge_from(&mut self, other: Self) {
        if let Some(other_memo) = other.memo {
            match &mut self.memo {
                None => self.memo = Some(other_memo),
                Some(existing) => {
                    let mut merged = existing.as_ref().clone();
                    merged.extend(other_memo.iter().map(|(k, v)| (k.clone(), Arc::clone(v))));
                    *existing = Arc::new(merged);
                }
            }
        }
        if let Some(other_links) = other.link_memo {
            match &mut self.link_memo {
                None => self.link_memo = Some(other_links),
                Some(existing) => {
                    let mut merged = existing.as_ref().clone();
                    merged.extend(
                        other_links
                            .iter()
                            .map(|(&k, (v, f))| (k, (Arc::clone(v), *f))),
                    );
                    *existing = Arc::new(merged);
                }
            }
        }
    }
}

/// Thread-safe snapshot of immutable per-model metadata used to instantiate
/// per-worker [`UsageResolver`] sessions without rescanning the project model.
pub(crate) struct WorkerUsageTemplate<'a> {
    model: &'a ProjectModel,
    ide: Option<crate::ide::IdeFlavor>,
    pch_objects: IndexMap<(TargetId, rcmake_genex::ConfigId), Vec<BString>>,
    runtime_install_targets: IndexSet<TargetId>,
    traced_sources: Option<&'a IndexMap<TargetId, IndexSet<BString>>>,
    ids: Arc<[TargetId]>,
    debug_configs: Vec<BString>,
    debug_properties: bool,
    has_injections: bool,
    has_system_interface_includes: bool,
    has_compatible_decls: bool,
    has_interface_pic: bool,
    has_language_runtime_libraries: bool,
    has_shared_or_imported_targets: bool,
    has_link_feature_overrides: bool,
    shared_memo: Option<Arc<HashMap<MemoKey<'static>, Arc<WalkValue>>>>,
    shared_link_memo: Option<Arc<HashMap<link::LinkMemoKey, link::SharedLinks>>>,
    target_languages: HashMap<(TargetId, rcmake_genex::ConfigId), Arc<IndexSet<BString>>>,
    compatible_implied: HashMap<(TargetId, rcmake_genex::ConfigId), HashSet<BString>>,
}

impl<'a> WorkerUsageTemplate<'a> {
    pub(crate) fn build<'w>(
        &'w self,
        source_paths: Option<&'w SourcePaths<'w>>,
        provenance: bool,
    ) -> UsageResolver<'w> {
        UsageResolver {
            model: self.model,
            ide: self.ide,
            pch_objects: self.pch_objects.clone(),
            artifact_properties: Vec::new(),
            source_paths,
            source_reads: None,
            traced_sources: self.traced_sources,
            ids: Arc::clone(&self.ids),
            parsed: ParseCache::default(),
            memo: HashMap::new(),
            shared_memo: self.shared_memo.clone(),
            link_memo: HashMap::new(),
            shared_link_memo: self.shared_link_memo.clone(),
            target_languages: self.target_languages.clone(),
            active_compile_language: None,
            debug_configs: self.debug_configs.clone(),
            debug_properties: self.debug_properties,
            codemodel_backtraces: false,
            active: Vec::new(),
            cycle_count: 0,
            has_injections: self.has_injections,
            has_system_interface_includes: self.has_system_interface_includes,
            has_compatible_decls: self.has_compatible_decls,
            has_interface_pic: self.has_interface_pic,
            has_language_runtime_libraries: self.has_language_runtime_libraries,
            has_shared_or_imported_targets: self.has_shared_or_imported_targets,
            has_link_feature_overrides: self.has_link_feature_overrides,
            compatible: compatible::Properties::with_implied(self.compatible_implied.clone()),
            provenance: provenance.then(provenance::Provenance::default),
            runtime_install_targets: self.runtime_install_targets.clone(),
            property_reads: 0,
            current_link_bt: None,
        }
    }
}

impl<'a> UsageResolver<'a> {
    /// Start a resolution session. Reuse it across targets and configurations.
    #[must_use]
    pub fn new(model: &'a ProjectModel) -> Self {
        let mut ids = Vec::new();
        let mut has_injections = false;
        let mut has_system_interface_includes = false;
        let mut has_compatible_decls = false;
        let mut has_interface_pic = false;
        let mut has_shared_or_imported_targets = false;
        let mut has_link_feature_overrides = false;
        for (id, target) in model.targets() {
            ids.push(id);
            has_injections |= target.props.get(link::DIRECT).is_some()
                || target.props.get(link::EXCLUDE).is_some();
            has_system_interface_includes |= !target
                .usage
                .entries(UsageField::InterfaceSystemIncludeDirectories)
                .is_empty()
                || (!target
                    .usage
                    .entries(UsageField::InterfaceIncludeDirectories)
                    .is_empty()
                    && target
                        .props
                        .get(b"SYSTEM")
                        .map_or(target.is_imported(), |value| truth::is_on(value)));
            has_compatible_decls |= compatible::Properties::has_declarations(target);
            has_interface_pic |= target
                .props
                .get(b"INTERFACE_POSITION_INDEPENDENT_CODE")
                .is_some();
            has_shared_or_imported_targets |=
                target.is_imported() || target.kind == TargetKind::SharedLibrary;
            if !has_link_feature_overrides {
                has_link_feature_overrides = target.props.get(b"LINK_LIBRARY_OVERRIDE").is_some()
                    || target
                        .props
                        .names()
                        .iter()
                        .any(|key| key.starts_with(b"LINK_LIBRARY_OVERRIDE_"));
            }
        }
        let has_language_runtime_libraries = model.dirs().any(|(id, _)| {
            definition(model, id, b"CMAKE_HIP_RUNTIME_LIBRARY_DEFAULT")
                .is_some_and(|value| !value.is_empty())
                || definition(model, id, b"CMAKE_CUDA_RUNTIME_LIBRARY_DEFAULT")
                    .is_some_and(|value| !value.is_empty())
        });
        Self {
            model,
            ide: crate::ide::IdeFlavor::from_model(model),
            artifact_properties: Vec::new(),
            pch_objects: IndexMap::new(),
            runtime_install_targets: IndexSet::new(),
            source_paths: None,
            source_reads: None,
            traced_sources: None,
            ids: ids.into(),
            parsed: ParseCache::default(),
            memo: HashMap::new(),
            shared_memo: None,
            link_memo: HashMap::new(),
            shared_link_memo: None,
            target_languages: HashMap::new(),
            active_compile_language: None,
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
            has_injections,
            has_system_interface_includes,
            has_compatible_decls,
            has_interface_pic,
            has_language_runtime_libraries,
            has_shared_or_imported_targets,
            has_link_feature_overrides,
        }
    }

    pub(crate) fn worker_template(&self) -> WorkerUsageTemplate<'a> {
        let shared_memo = if !self.memo.is_empty() {
            let mut merged = self
                .shared_memo
                .as_deref()
                .cloned()
                .unwrap_or_else(|| HashMap::with_capacity(self.memo.len()));
            merged.extend(self.memo.iter().map(|(k, v)| (k.clone(), Arc::clone(v))));
            Some(Arc::new(merged))
        } else {
            self.shared_memo.clone()
        };
        let shared_link_memo = if !self.link_memo.is_empty() {
            let mut merged = self
                .shared_link_memo
                .as_deref()
                .cloned()
                .unwrap_or_else(|| HashMap::with_capacity(self.link_memo.len()));
            merged.extend(
                self.link_memo
                    .iter()
                    .map(|(&k, (v, f))| (k, (Arc::clone(v), *f))),
            );
            Some(Arc::new(merged))
        } else {
            self.shared_link_memo.clone()
        };
        WorkerUsageTemplate {
            model: self.model,
            ide: self.ide,
            pch_objects: self.pch_objects.clone(),
            runtime_install_targets: self.runtime_install_targets.clone(),
            traced_sources: self.traced_sources,
            ids: Arc::clone(&self.ids),
            debug_configs: self.debug_configs.clone(),
            debug_properties: self.debug_properties,
            has_injections: self.has_injections,
            has_system_interface_includes: self.has_system_interface_includes,
            has_compatible_decls: self.has_compatible_decls,
            has_interface_pic: self.has_interface_pic,
            has_language_runtime_libraries: self.has_language_runtime_libraries,
            has_shared_or_imported_targets: self.has_shared_or_imported_targets,
            has_link_feature_overrides: self.has_link_feature_overrides,
            shared_memo,
            shared_link_memo,
            target_languages: self.target_languages.clone(),
            compatible_implied: self.compatible.implied.clone(),
        }
    }

    /// Extract a read-only [`MemoSnapshot`] combining local and shared memos.
    pub(crate) fn memo_snapshot(&self) -> MemoSnapshot {
        let memo = if !self.memo.is_empty() {
            let mut merged = self
                .shared_memo
                .as_deref()
                .cloned()
                .unwrap_or_else(|| HashMap::with_capacity(self.memo.len()));
            merged.extend(self.memo.iter().map(|(k, v)| (k.clone(), Arc::clone(v))));
            Some(Arc::new(merged))
        } else {
            self.shared_memo.clone()
        };
        let link_memo = if !self.link_memo.is_empty() {
            let mut merged = self
                .shared_link_memo
                .as_deref()
                .cloned()
                .unwrap_or_else(|| HashMap::with_capacity(self.link_memo.len()));
            merged.extend(
                self.link_memo
                    .iter()
                    .map(|(&k, (v, f))| (k, (Arc::clone(v), *f))),
            );
            Some(Arc::new(merged))
        } else {
            self.shared_link_memo.clone()
        };
        MemoSnapshot { memo, link_memo }
    }

    /// Install a read-only [`MemoSnapshot`] from an earlier phase on the same model.
    pub(crate) fn with_memos(mut self, memos: &MemoSnapshot) -> Self {
        self.shared_memo = memos.memo.clone();
        self.shared_link_memo = memos.link_memo.clone();
        self
    }

    /// Extract the worker's locally learned memos into an owned [`MemoSnapshot`].
    pub(crate) fn take_local_memos(&mut self) -> MemoSnapshot {
        MemoSnapshot {
            memo: (!self.memo.is_empty()).then(|| Arc::new(std::mem::take(&mut self.memo))),
            link_memo: (!self.link_memo.is_empty())
                .then(|| Arc::new(std::mem::take(&mut self.link_memo))),
        }
    }

    /// Merge read-only memo entries learned by a worker back into this resolver.
    pub(crate) fn absorb_memos(&mut self, other: MemoSnapshot) {
        if let Some(memo) = other.memo {
            match Arc::try_unwrap(memo) {
                Ok(map) => {
                    if self.memo.is_empty() {
                        self.memo = map;
                    } else {
                        self.memo.extend(map);
                    }
                }
                Err(shared) => {
                    self.memo
                        .extend(shared.iter().map(|(k, v)| (k.clone(), Arc::clone(v))));
                }
            }
        }
        if let Some(link_memo) = other.link_memo {
            match Arc::try_unwrap(link_memo) {
                Ok(map) => {
                    if self.link_memo.is_empty() {
                        self.link_memo = map;
                    } else {
                        self.link_memo.extend(map);
                    }
                }
                Err(shared) => {
                    self.link_memo
                        .extend(shared.iter().map(|(&k, (v, f))| (k, (Arc::clone(v), *f))));
                }
            }
        }
    }

    pub(crate) fn seed_target_languages(
        &mut self,
        config: &rcmake_genex::ConfigId,
        resolved: &IndexMap<TargetId, crate::ResolvedTarget>,
    ) {
        for (&id, facts) in resolved {
            let languages: IndexSet<BString> = facts
                .compile_groups
                .iter()
                .map(|group| group.language.clone())
                .collect();
            self.target_languages
                .insert((id, config.clone()), Arc::new(languages));
        }
    }

    pub(crate) fn with_compile_group_language<T>(
        &mut self,
        target: TargetId,
        language: &[u8],
        f: impl FnOnce(&mut Self) -> T,
    ) -> T {
        let previous = self
            .active_compile_language
            .replace((target, language.into()));
        let output = f(self);
        self.active_compile_language = previous;
        output
    }

    /// Return the resolved compile languages of `owner` in `config`, evaluating
    /// generator expressions in `SOURCES` and `FILE_SET` entries when not yet seeded.
    pub(crate) fn target_compile_languages(
        &mut self,
        owner: TargetId,
        config: &rcmake_genex::ConfigId,
    ) -> Result<Arc<IndexSet<BString>>, ResolveError> {
        let target = self.target(owner)?;
        if target.is_imported() || !target.kind.compiles_objects() {
            return Ok(Arc::default());
        }
        if let Some(cached) = self.target_languages.get(&(owner, config.clone())) {
            return Ok(Arc::clone(cached));
        }
        if self.active.iter().any(|(id, prop)| {
            *id == owner
                && matches!(
                    prop.as_slice(),
                    b"SOURCES" | b"INTERFACE_SOURCES" | b"\0rcmake-target-languages"
                )
        }) {
            return Ok(Arc::default());
        }
        let languages = self.with_property(owner, b"\0rcmake-target-languages", |this| {
            let src_ctx = EvalContext {
                config: config.clone(),
                head: Some(handle(owner)),
                traversal: UsageTraversal::Compile,
                ..EvalContext::default()
            };
            let sources = this.collect_property(owner, b"SOURCES", &src_ctx)?.entries;
            let file_sets = this.source_file_sets(owner, &src_ctx)?;
            let target = this.target(owner)?;
            let dir = this.model.dir(target.dir);
            let source_paths = this.source_paths;
            let resolve_abs = |raw: &[u8]| {
                source_paths
                    .and_then(|paths| paths.find(dir, raw))
                    .unwrap_or_else(|| crate::sources::absolute_path(&dir.source_dir, raw))
            };
            let mut header_set_files = HashSet::new();
            let mut file_set_langs: HashMap<BString, Option<BString>> = HashMap::new();
            let mut candidates: Vec<(BString, Option<BString>)> = Vec::new();
            for entry in &sources {
                candidates.push((resolve_abs(&entry.value), None));
            }
            for (_, set, _) in file_sets {
                let files = this.file_set_target_sources(set, &src_ctx)?;
                if set.kind == FileSetKind::Headers {
                    for entry in files {
                        header_set_files.insert(resolve_abs(&entry.value));
                    }
                } else {
                    let set_lang = set
                        .props
                        .get(b"LANGUAGE")
                        .filter(|value| !value.is_empty())
                        .cloned();
                    for entry in files {
                        let abs = resolve_abs(&entry.value);
                        file_set_langs
                            .entry(abs.clone())
                            .or_insert_with(|| set_lang.clone());
                        candidates.push((abs, set_lang.clone()));
                    }
                }
            }
            if let Some(traced) = this.traced_sources.and_then(|map| map.get(&owner)) {
                for path in traced {
                    candidates.push((path.clone(), None));
                }
            }
            let mut seen = HashSet::new();
            let mut langs = IndexSet::new();
            for (abs, set_lang) in candidates {
                if !seen.insert(abs.clone()) {
                    continue;
                }
                if let Some(obj_owner) = this.source_paths.and_then(|p| p.object_owner(&abs)) {
                    if obj_owner != owner {
                        let nested = this.target_compile_languages(obj_owner, config)?;
                        langs.extend(nested.iter().cloned());
                    }
                    continue;
                }
                if crate::sources::external_object(dir, &abs)
                    || this.source_paths.is_some_and(|p| p.is_directory(&abs))
                    || header_set_files.contains(&abs)
                {
                    continue;
                }
                let source_props = this.source_paths.map_or_else(
                    || crate::sources::properties(dir, &abs),
                    |paths| paths.properties(dir, &abs),
                );
                if source_props.get(b"MACOSX_PACKAGE_LOCATION").is_some()
                    && crate::lower::is_darwin(this.model, target.dir)
                    && crate::ide::bundle_kind(target) != crate::ide::BundleKind::None
                {
                    continue;
                }
                let header_only = source_props
                    .get(b"HEADER_FILE_ONLY")
                    .is_some_and(|value| truth::is_on(value))
                    || this
                        .source_paths
                        .is_some_and(|p| p.is_header_only(target.dir, &abs));
                if header_only {
                    continue;
                }
                let lang = set_lang
                    .or_else(|| file_set_langs.get(&abs).and_then(Clone::clone))
                    .or_else(|| {
                        source_props
                            .get(b"LANGUAGE")
                            .filter(|value| !value.is_empty())
                            .cloned()
                    })
                    .or_else(|| crate::lower::source_language(this.model, target, &abs));
                if let Some(lang) = lang {
                    langs.insert(lang);
                }
            }
            Ok(langs)
        })?;
        let languages = Arc::new(languages);
        if self.source_reads.is_none() {
            self.target_languages
                .insert((owner, config.clone()), Arc::clone(&languages));
        }
        Ok(languages)
    }

    /// Use the generated rule sources when evaluating target source properties.
    #[must_use]
    pub fn with_traced_sources(
        mut self,
        sources: &'a IndexMap<TargetId, IndexSet<BString>>,
    ) -> Self {
        self.traced_sources = Some(sources);
        self
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
        for (contrib_idx, (owner, entries)) in result.contributions.iter().enumerate() {
            if *owner == target {
                continue;
            }
            let Some(link_bt) = result.first_link_bt(contrib_idx) else {
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
                pairs.push((BT::new(value, entry.backtrace.clone()), link_bt.clone()));
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
            source_reads: self.source_reads.clone(),
            traced_sources: self.traced_sources,
            ids: Arc::clone(&self.ids),
            parsed: ParseCache::default(),
            memo: HashMap::new(),
            shared_memo: self.shared_memo.clone(),
            link_memo: HashMap::new(),
            shared_link_memo: self.shared_link_memo.clone(),
            target_languages: self.target_languages.clone(),
            active_compile_language: None,
            debug_configs: self.debug_configs.clone(),
            debug_properties: self.debug_properties,
            codemodel_backtraces: false,
            active: Vec::new(),
            cycle_count: 0,
            compatible: compatible::Properties::default(),
            provenance: None,
            property_reads: 0,
            has_injections: self.has_injections,
            has_system_interface_includes: self.has_system_interface_includes,
            has_compatible_decls: self.has_compatible_decls,
            has_interface_pic: self.has_interface_pic,
            has_language_runtime_libraries: self.has_language_runtime_libraries,
            has_shared_or_imported_targets: self.has_shared_or_imported_targets,
            has_link_feature_overrides: self.has_link_feature_overrides,
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
            let total_entries: usize = result
                .contributions
                .iter()
                .map(|(_, entries)| entries.len())
                .sum();
            let mut value = UsageValue {
                entries: Vec::with_capacity(total_entries),
                flags: result.flags,
            };
            let mut seen = HashSet::with_capacity(total_entries);
            for (contrib_idx, (owner, entries)) in result.contributions.iter().enumerate() {
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
                let codemodel_bt = if self.codemodel_backtraces {
                    result.first_link_bt(contrib_idx)
                } else {
                    None
                };
                for entry in &**entries {
                    if seen.insert(entry.value.as_slice()) {
                        let mut entry = entry.clone();
                        if let Some(link_bt) = codemodel_bt {
                            entry.backtrace.clone_from(link_bt);
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
        if self.active.is_empty() && self.property_reads == 0 && self.source_reads.is_none() {
            self.warm_dependencies(target, name, ctx)?;
        }
        let mut visited = HashMap::new();
        let (value, _) = self.walk_with(target, name, ctx, &mut visited)?;
        Ok(value)
    }

    /// Build the memo key of one walk with no head.
    fn memo_key<'k>(&self, target: TargetId, name: &'k [u8], ctx: &'k EvalContext) -> MemoKey<'k> {
        MemoKey {
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
        }
    }

    fn memo_get(&self, key: &MemoKey<'_>) -> Option<Arc<WalkValue>> {
        self.memo
            .get(key)
            .or_else(|| {
                self.shared_memo
                    .as_deref()
                    .and_then(|shared| shared.get(key))
            })
            .cloned()
    }

    fn is_memoized(&self, target: TargetId, name: &[u8], ctx: &EvalContext) -> bool {
        let mut key = self.memo_key(target, name, ctx);
        if self.memo_get(&key).is_some() {
            return true;
        }
        key.head = ctx.head.and_then(|id| self.ids.get(id.0 as usize).copied());
        key.head.is_some() && self.memo_get(&key).is_some()
    }

    /// Targets that a walk of `name` on `target` visits next.
    fn edge_targets(
        &mut self,
        target: TargetId,
        name: &[u8],
        ctx: &EvalContext,
    ) -> Result<Vec<TargetId>, ResolveError> {
        let (links, _) = self.usage_links(target, name.starts_with(b"INTERFACE_"), ctx)?;
        Ok(links.iter().flat_map(|item| item.edge_targets()).collect())
    }

    /// Memoize the interface walks of a deep dependency chain bottom-up.
    ///
    /// A walk recurses once per link hop. The memo keeps the recursion shallow
    /// only when the dependencies were walked first, and this depends on the
    /// declaration order and on how the targets are split between workers.
    /// This pass finds the dependencies that are not in the memo with an
    /// iterative depth-first search. When the longest chain of such
    /// dependencies is deep, it walks them in post-order, so that each walk
    /// finds the walks of its dependencies in the memo. Shallow graphs keep
    /// their usual evaluation order. A cycle or an expression that prevents
    /// memoization keeps the walk deep, and the real walk then reports
    /// [`GenexError::TooDeep`] as before.
    fn warm_dependencies(
        &mut self,
        target: TargetId,
        name: &[u8],
        ctx: &EvalContext,
    ) -> Result<(), ResolveError> {
        const WARM_DEPTH: usize = MAX_WALK_DEPTH / 8;
        let dependency_name = if name.starts_with(b"INTERFACE_") {
            Cow::Borrowed(name)
        } else {
            interface_property_name(name)
        };
        let (root_links, _) = self.usage_links(target, name.starts_with(b"INTERFACE_"), ctx)?;
        if root_links
            .iter()
            .flat_map(|item| item.edge_targets())
            .all(|dep| dep == target || self.is_memoized(dep, &dependency_name, ctx))
        {
            return Ok(());
        }
        // Depth of each finished node; `None` marks a node on the search path.
        let mut depth: HashMap<TargetId, Option<usize>> = HashMap::from([(target, None)]);
        let mut order = Vec::new();
        let root_edges: Vec<TargetId> = root_links
            .iter()
            .flat_map(|item| item.edge_targets())
            .collect();
        let mut stack = vec![(target, root_edges, 0usize)];
        while let Some((node, edges, next)) = stack.last_mut() {
            let Some(&dependency) = edges.get(*next) else {
                let node = *node;
                let deepest = edges
                    .iter()
                    .filter_map(|child| depth.get(child).copied().flatten())
                    .max()
                    .unwrap_or(0);
                stack.pop();
                depth.insert(node, Some(deepest + 1));
                if node != target {
                    order.push(node);
                }
                continue;
            };
            *next += 1;
            if depth.contains_key(&dependency)
                || self.is_memoized(dependency, &dependency_name, ctx)
            {
                continue;
            }
            depth.insert(dependency, None);
            let edges = self.edge_targets(dependency, &dependency_name, ctx)?;
            stack.push((dependency, edges, 0));
        }
        if depth.get(&target).copied().flatten().unwrap_or(0) < WARM_DEPTH {
            return Ok(());
        }
        for dependency in order {
            let mut visited = HashMap::new();
            if self
                .walk_with(dependency, &dependency_name, ctx, &mut visited)
                .is_err()
            {
                break;
            }
        }
        Ok(())
    }

    fn walk_with(
        &mut self,
        target: TargetId,
        name: &[u8],
        ctx: &EvalContext,
        visited: &mut HashMap<(TargetId, bool), usize>,
    ) -> Result<(Arc<WalkValue>, usize), ResolveError> {
        // A link graph may contain a cycle. Stop this branch and avoid caching its
        // partial result. A generator expression that re-enters a property is an error.
        let in_cycle = self
            .active
            .iter()
            .any(|(id, prop)| *id == target && prop == name);
        if in_cycle {
            self.cycle_count += 1;
            return Ok((Arc::default(), 0));
        }
        let is_iface = name.starts_with(b"INTERFACE_");
        let start_seq = visited.len();
        let mut key = self.memo_key(target, name, ctx);
        let lookup_cached = |this: &Self,
                             key: &MemoKey<'_>,
                             visited: &mut HashMap<(TargetId, bool), usize>|
         -> Option<Arc<WalkValue>> {
            let value = this.memo_get(key)?;
            if ctx.traversal == UsageTraversal::Link && !visited.is_empty() {
                if visited.contains_key(&(target, is_iface))
                    || value
                        .contributions
                        .iter()
                        .any(|(owner, _)| visited.contains_key(&(*owner, true)))
                {
                    return None;
                }
                let seq = visited.len();
                visited.entry((target, is_iface)).or_insert(seq);
                for (owner, _) in &value.contributions {
                    let seq = visited.len();
                    visited.entry((*owner, true)).or_insert(seq);
                }
            }
            Some(value)
        };
        if self.source_reads.is_none()
            && let Some(value) = lookup_cached(self, &key, visited)
        {
            return Ok((value, usize::MAX));
        }
        key.head = ctx.head.and_then(|id| self.ids.get(id.0 as usize).copied());
        if self.source_reads.is_none()
            && let Some(value) = lookup_cached(self, &key, visited)
        {
            return Ok((value, usize::MAX));
        }
        let cycle_count = self.cycle_count;
        let (walk_value, min_dep_seq) = self.walk_once(target, name, ctx, visited)?;
        let value = Arc::new(walk_value);
        let self_contained = ctx.traversal == UsageTraversal::Compile || min_dep_seq >= start_seq;
        if self.source_reads.is_none() && self.cycle_count == cycle_count && self_contained {
            if !value.flags.head_sensitive {
                key.head = None;
            }
            self.memo.insert(key.into_owned(), Arc::clone(&value));
        }
        Ok((value, min_dep_seq))
    }

    fn walk_once(
        &mut self,
        target: TargetId,
        name: &[u8],
        ctx: &EvalContext,
        visited: &mut HashMap<(TargetId, bool), usize>,
    ) -> Result<(WalkValue, usize), ResolveError> {
        // A query on a direct property can reach the root's interface.
        let key = (target, name.starts_with(b"INTERFACE_"));
        if let Some(&seq) = visited.get(&key) {
            self.cycle_count += usize::from(ctx.traversal == UsageTraversal::Compile);
            return Ok((WalkValue::default(), seq));
        }
        let seq = visited.len();
        visited.insert(key, seq);
        self.with_property(target, name, |this| {
            this.walk_uncached(target, name, ctx, visited)
        })
    }

    fn walk_uncached(
        &mut self,
        id: TargetId,
        name: &[u8],
        ctx: &EvalContext,
        visited: &mut HashMap<(TargetId, bool), usize>,
    ) -> Result<(WalkValue, usize), ResolveError> {
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
            if let Some(sources) = self.traced_sources.and_then(|sources| sources.get(&id)) {
                let dir = self.model.dir(target.dir);
                for source in sources {
                    if !own.entries.iter().any(|entry| {
                        [&dir.source_dir, &dir.binary_dir].into_iter().any(|base| {
                            crate::sources::absolute_path(base, &entry.value) == *source
                        })
                    }) {
                        own.entries
                            .push(BT::new(source.clone(), target.backtrace.clone()));
                    }
                }
            }
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
            interface_property_name(name)
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
        let direct_edge_count = links.iter().flat_map(|item| item.edges()).take(2).count();
        let mut seen =
            (ctx.traversal == UsageTraversal::Compile && direct_edge_count > 1).then(HashSet::new);
        let track_full_chains = self.provenance.is_some() || self.debug_properties;
        let track_first_links = self.codemodel_backtraces || name == b"SOURCES";
        let mut min_dep_seq = usize::MAX;
        for (dependency, edge) in links.iter().flat_map(|item| item.edges()) {
            if ctx.traversal == UsageTraversal::Link
                && let Some(&dep_seq) = visited.get(&(dependency, true))
            {
                min_dep_seq = min_dep_seq.min(dep_seq);
                if self
                    .active
                    .iter()
                    .any(|(active_id, _)| *active_id == dependency)
                {
                    self.cycle_count += 1;
                }
                continue;
            }
            let previous_link_bt = if self.current_link_bt.is_none() {
                self.current_link_bt.replace(edge.text.backtrace.clone())
            } else {
                None
            };
            let restore_link_bt = previous_link_bt.is_none() && self.current_link_bt.is_some();
            let value = self.walk_with(dependency, &dependency_name, ctx, visited);
            if restore_link_bt {
                self.current_link_bt = previous_link_bt;
            }
            let (value, child_min_dep) = value?;
            min_dep_seq = min_dep_seq.min(child_min_dep);
            result.flags = result.flags.union(value.flags);
            let start = result.contributions.len();
            result.contributions.reserve(value.contributions.len());
            for (owner, entries) in &value.contributions {
                if *owner == id || seen.as_mut().is_some_and(|seen| !seen.insert(*owner)) {
                    continue;
                }
                result.contributions.push((*owner, Arc::clone(entries)));
                if track_full_chains {
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
            let end = result.contributions.len();
            if track_first_links && end > start {
                result
                    .first_links
                    .push((start, end, edge.text.backtrace.clone()));
            }
        }
        Ok((result, min_dep_seq))
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
            .enumerate()
            .filter_map(|(contrib_idx, (owner, _))| {
                let link_bt = (*owner != target)
                    .then(|| owners.first_link_bt(contrib_idx).cloned())
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
                            || model.target(target).is_some_and(|target| {
                                crate::cxx_modules::is_std_provider(target)
                                    || crate::cxx_modules::is_synthetic_provider(target)
                            }))
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

fn interface_property_name(name: &[u8]) -> Cow<'static, [u8]> {
    match name {
        b"INCLUDE_DIRECTORIES" => Cow::Borrowed(b"INTERFACE_INCLUDE_DIRECTORIES"),
        b"COMPILE_DEFINITIONS" => Cow::Borrowed(b"INTERFACE_COMPILE_DEFINITIONS"),
        b"COMPILE_OPTIONS" => Cow::Borrowed(b"INTERFACE_COMPILE_OPTIONS"),
        b"COMPILE_FEATURES" => Cow::Borrowed(b"INTERFACE_COMPILE_FEATURES"),
        b"LINK_OPTIONS" => Cow::Borrowed(b"INTERFACE_LINK_OPTIONS"),
        b"LINK_DIRECTORIES" => Cow::Borrowed(b"INTERFACE_LINK_DIRECTORIES"),
        b"LINK_DEPENDS" => Cow::Borrowed(b"INTERFACE_LINK_DEPENDS"),
        b"SOURCES" => Cow::Borrowed(b"INTERFACE_SOURCES"),
        b"PRECOMPILE_HEADERS" => Cow::Borrowed(b"INTERFACE_PRECOMPILE_HEADERS"),
        _ => Cow::Owned([b"INTERFACE_", name].concat()),
    }
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
