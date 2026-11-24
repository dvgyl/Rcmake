use std::collections::{BTreeMap, BTreeSet};

use indexmap::IndexMap;
use rcmake_base::BString;
use rcmake_diag::{
    Backtrace,
    suggest::{Namespace, suggest_candidates_in},
};
use rcmake_eval::{ValuePreview, VarOp};
use rcmake_model::{
    BT, CacheType, DirId, PropEvent, PropScope, Target, TargetBinding, TargetId, TargetKind,
    UsageScope,
};
use rcmake_resolve::{ChainKind, LinkFragmentRole};

use crate::explain_entries::{build_entries, join_entries, set_spelling};
use crate::explain_render::chain_kind;
use crate::{ConfigureResult, PreviousValue, SourceLocation, VariableHistory};

/// Select the name and kind of item to explain.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ExplainQuery {
    /// Find a target or an alias.
    Target(BString),
    /// Find a variable or its recorded history.
    Variable(BString),
    /// Find a target or variable. Report ambiguity if both exist.
    Auto(BString),
}

/// Result of an explanation query.
#[derive(Clone, Debug)]
pub enum ExplainReport {
    /// Target definition, resolved values, and property history.
    Target(Box<ExplainTarget>),
    /// Variable value and write history.
    Variable(Box<ExplainVariable>),
    /// The query matches both kinds.
    Ambiguous {
        /// Name that identifies both a target and a variable.
        name: BString,
        /// Kinds matched by the query.
        kinds: [ExplainItemKind; 2],
    },
    /// No item matched the query.
    NotFound {
        /// Requested name.
        name: BString,
        /// Similar names, closest first.
        suggestions: Vec<ExplainSuggestion>,
    },
    /// The requested directory does not belong to this snapshot.
    InvalidDirectory {
        /// Requested item name.
        name: BString,
        /// Invalid directory identifier.
        directory: DirId,
    },
}

/// Kind of item matched by an explanation query.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum ExplainItemKind {
    /// Target or alias.
    Target,
    /// Normal or cache variable.
    Variable,
}

impl ExplainItemKind {
    /// Stable name of this kind in the explanation schema.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Target => "target",
            Self::Variable => "variable",
        }
    }
}

/// One name and kind that the caller can query.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExplainSuggestion {
    /// Candidate name.
    pub name: BString,
    /// Kind of item with this name.
    pub kind: ExplainItemKind,
}

/// One target and the values that contribute to its build.
#[derive(Clone, Debug)]
pub struct ExplainTarget {
    /// Name used in the query, which can be an alias.
    pub name: BString,
    /// Name of the real target.
    pub target: BString,
    /// CMake target kind.
    pub kind: TargetKind,
    /// Call stack that created the real target.
    pub definition: Backtrace,
    /// Alias declaration, when the evaluator recorded it.
    pub alias_definition: Option<SourceLocation>,
    /// Sources declared during configure with their call stacks.
    pub sources: Vec<BT<BString>>,
    /// Source and build roots for relative location display.
    pub roots: ExplainRoots,
    /// Value or compiler spelling selected by the caller, if any.
    pub filter: Option<BString>,
    /// Resolved usage entries for each configuration.
    pub configurations: Vec<ExplainConfig>,
    /// Property writes on the target and contributors in its chains.
    pub properties: Vec<ExplainProperty>,
    /// Whether a property history reached its limit, if collection was enabled.
    pub property_history_incomplete: Option<bool>,
    /// Whether evaluation was partial.
    pub provisional: bool,
}

impl ExplainTarget {
    pub(crate) fn configurations_for(
        &self,
        requested: Option<&[u8]>,
    ) -> impl Iterator<Item = &ExplainConfig> {
        let requested = requested.map(BString::from);
        self.configurations.iter().filter(move |config| {
            requested
                .as_ref()
                .is_none_or(|name| name.eq_ignore_ascii_case(&config.name))
        })
    }

    pub(crate) fn properties_for(
        &self,
        requested: Option<&[u8]>,
    ) -> impl Iterator<Item = &ExplainProperty> {
        let requested = requested.map(BString::from);
        self.properties.iter().filter(move |property| {
            requested.as_ref().is_none_or(|name| {
                property
                    .configurations
                    .iter()
                    .any(|config| name.eq_ignore_ascii_case(config))
            })
        })
    }
}

/// Project paths used for relative location display.
#[derive(Clone, Debug, Default)]
pub struct ExplainRoots {
    /// Project source directory.
    pub source: BString,
    /// Project build directory.
    pub build: BString,
}

/// Resolved usage for one configuration.
#[derive(Clone, Debug, Default)]
pub struct ExplainConfig {
    /// Configuration name. An empty name means the default configuration.
    pub name: BString,
    /// Resolved sources with call stacks, when available.
    pub sources: Vec<BT<BString>>,
    /// Sources and language of each compile group, in build order.
    pub compile_groups: Vec<ExplainCompileGroup>,
    /// Usage entries in build order, followed by unmatched recorded entries.
    pub entries: Vec<ExplainEntry>,
}

/// Translation units that share a set of compile settings.
#[derive(Clone, Debug)]
pub struct ExplainCompileGroup {
    /// Compile language.
    pub language: BString,
    /// Source paths that use this group.
    pub sources: Vec<BString>,
}

/// Position of an entry in a compile group.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CompilePosition {
    /// Zero-based index into [ExplainConfig::compile_groups].
    pub group: usize,
    /// Zero-based position in that group's explanation entries.
    pub ordinal: usize,
}

/// Variable that supplied a compiler flag.
#[derive(Clone, Debug)]
pub struct ExplainInput {
    /// Name of the variable evaluated by the lowerer.
    pub name: BString,
    /// Location of the effective binding, when recorded.
    pub location: Option<SourceLocation>,
}

/// One resolved usage requirement or link item.
#[derive(Clone, Debug)]
pub struct ExplainEntry {
    /// Type of usage requirement or link item.
    pub kind: ChainKind,
    /// Evaluated value.
    pub value: BString,
    /// Compiler or linker spelling when available.
    pub spelling: Option<BString>,
    /// Positions in the compile groups that use this entry.
    pub compile_positions: Vec<CompilePosition>,
    /// Whether an include directory is a system include, when known.
    pub system_include: Option<bool>,
    /// Role of a lowered link fragment, when known.
    pub link_role: Option<LinkFragmentRole>,
    /// Targets referenced by the recorded link item, in link order.
    pub link_targets: Vec<TargetId>,
    /// Variable that supplied the value, when known.
    pub input: Option<ExplainInput>,
    /// Whether the entry came from the build settings or lowerer inputs.
    pub build_entry: bool,
    /// Call stack for the original entry.
    pub origin: Backtrace,
    /// Ordered hops. An empty chain identifies a direct value.
    /// An absent chain means that no matching chain was recorded.
    pub chain: Option<Vec<ExplainHop>>,
}

impl ExplainEntry {
    pub(crate) fn new(kind: ChainKind, value: &BT<BString>) -> Self {
        Self {
            kind,
            value: value.value.clone(),
            spelling: None,
            compile_positions: Vec::new(),
            system_include: None,
            link_role: None,
            link_targets: Vec::new(),
            input: None,
            build_entry: false,
            origin: value.backtrace.clone(),
            chain: None,
        }
    }

    fn matches_filter(&self, filter: &[u8]) -> bool {
        self.value.as_slice() == filter
            || self.spelling.as_ref().map(|value| value.as_slice()) == Some(filter)
    }
}

/// One link hop in a provenance chain.
#[derive(Clone, Debug)]
pub struct ExplainHop {
    /// Name of the target identified by the resolver hop.
    pub target: Option<BString>,
    /// Target ID even if the target is absent from the configure model.
    pub id: TargetId,
    /// Scope of the link edge.
    pub via: UsageScope,
    /// Call stack for the link edge.
    pub origin: Backtrace,
}

/// Retained writes to one property on a target.
#[derive(Clone, Debug)]
pub struct ExplainProperty {
    /// Target that owns the property.
    pub target: BString,
    /// Configurations that use this contributor. The writes themselves are shared.
    pub configurations: Vec<BString>,
    /// CMake property name.
    pub name: BString,
    /// Writes in evaluation order.
    pub writes: Vec<PropEvent>,
    /// Number of writes discarded for this property.
    pub dropped_writes: u32,
}

/// A variable as seen in the requested directory.
#[derive(Clone, Debug)]
pub struct ExplainVariable {
    /// Variable name.
    pub name: BString,
    /// Effective value after evaluation. An absent value means undefined.
    pub value: Option<BString>,
    /// Source of the effective value, when recorded.
    pub location: Option<SourceLocation>,
    /// Whether the effective value is a normal variable.
    pub normal_binding: bool,
    /// Cache entry even when a normal variable hides it.
    pub cache: Option<ExplainCache>,
    /// Retained writes to this name, oldest first.
    pub writes: Vec<ExplainWrite>,
    /// Recording coverage for this name.
    pub history: VariableHistory,
    /// Whether the session-wide retention limit was reached, when known.
    pub session_history_truncated: Option<bool>,
    /// Source directory that the user queried.
    pub directory: BString,
    /// Source and build roots for relative location display.
    pub roots: ExplainRoots,
    /// Whether evaluation was partial.
    pub provisional: bool,
}

/// Current cache entry for an explained variable.
#[derive(Clone, Debug)]
pub struct ExplainCache {
    /// Cache value.
    pub value: BString,
    /// CMake cache type.
    pub kind: CacheType,
    /// Cache help string.
    pub help: BString,
    /// Location of the write that supplied the cache value, when recorded.
    pub location: Option<SourceLocation>,
    /// Whether the write used `FORCE`, when recorded.
    pub force: Option<bool>,
}

/// A variable write and its effect in the written scope.
#[derive(Clone, Debug)]
pub struct ExplainWrite {
    /// Evaluation sequence number.
    pub sequence: u64,
    /// Type of write.
    pub operation: VarOp,
    /// Command with its operation, when recorded.
    pub command: Option<BString>,
    /// Directory whose evaluation caused the write, when recorded.
    pub directory: Option<DirId>,
    /// Source directory in which the command ran, when known.
    pub directory_path: Option<BString>,
    /// Directory binding changed by the write, if recorded.
    pub binding_directory: Option<DirId>,
    /// Whether this event affected the queried directory's normal binding.
    pub in_queried_scope: bool,
    /// Depth of the written scope.
    pub scope_depth: u32,
    /// Identity of the written scope within this evaluation.
    pub scope_id: u64,
    /// Previous value in the written scope.
    pub previous: PreviousValue,
    /// New value. An absent value means the write removed the value.
    pub value: Option<ValuePreview>,
    /// Call stack for the write.
    pub origin: Backtrace,
}

impl ConfigureResult {
    /// Explain an item in the top-level directory.
    #[must_use]
    pub fn explain(&self, query: &ExplainQuery) -> ExplainReport {
        self.explain_in(DirId::ROOT, query)
    }

    /// Explain an item and select one exact usage value or compiler spelling.
    #[must_use]
    pub fn explain_filtered(&self, query: &ExplainQuery, filter: Option<BString>) -> ExplainReport {
        self.explain_in_filtered(DirId::ROOT, query, filter)
    }

    /// Explain an item as seen from the specified evaluated directory.
    #[must_use]
    pub fn explain_in(&self, dir: DirId, query: &ExplainQuery) -> ExplainReport {
        self.explain_in_filtered(dir, query, None)
    }

    /// Explain an item in one directory with an optional exact target entry filter.
    #[must_use]
    pub fn explain_in_filtered(
        &self,
        dir: DirId,
        query: &ExplainQuery,
        filter: Option<BString>,
    ) -> ExplainReport {
        let (name, targets, variables) = match query {
            ExplainQuery::Auto(name) => (name, true, true),
            ExplainQuery::Target(name) => (name, true, false),
            ExplainQuery::Variable(name) => (name, false, true),
        };
        let Some((_, directory)) = self.model.dirs().nth(dir.index()) else {
            return ExplainReport::InvalidDirectory {
                name: name.clone(),
                directory: dir,
            };
        };
        let target = targets
            .then(|| self.model.find_target(dir, name))
            .flatten()
            .and_then(|id| self.model.target(id).map(|target| (id, target)));
        let value = variables
            .then(|| self.model.definition(dir, name))
            .flatten();
        match (target, value) {
            (Some(_), Some(_)) => ExplainReport::Ambiguous {
                name: name.clone(),
                kinds: [ExplainItemKind::Target, ExplainItemKind::Variable],
            },
            (Some((id, target)), None) => {
                ExplainReport::Target(Box::new(self.explain_target(dir, name, id, target, filter)))
            }
            (None, Some(_)) => ExplainReport::Variable(Box::new(self.explain_variable(dir, name))),
            (None, None) if variables && self.has_visible_history(dir, name) => {
                ExplainReport::Variable(Box::new(self.explain_variable(dir, name)))
            }
            (None, None) => ExplainReport::NotFound {
                name: name.clone(),
                suggestions: self.suggestions(directory, name, targets, variables),
            },
        }
    }

    fn has_visible_history(&self, dir: DirId, name: &[u8]) -> bool {
        self.var_ledger
            .iter()
            .flat_map(|ledger| ledger.history(name))
            .any(|event| {
                matches!(event.op, VarOp::CacheSet | VarOp::CacheUnset)
                    || self
                        .facts
                        .variable_writes
                        .get(&event.seq)
                        .is_some_and(|details| details.binding_dir == Some(dir))
            })
    }

    fn suggestions(
        &self,
        directory: &rcmake_model::Directory,
        name: &[u8],
        targets: bool,
        variables: bool,
    ) -> Vec<ExplainSuggestion> {
        let mut names = BTreeMap::<&[u8], BTreeSet<ExplainItemKind>>::new();
        if targets {
            let target_names = directory
                .local_targets
                .keys()
                .chain(self.model.global_target_bindings().map(|(name, _)| name));
            for candidate in target_names {
                names
                    .entry(candidate.as_slice())
                    .or_default()
                    .insert(ExplainItemKind::Target);
            }
        }
        if variables {
            let variable_names = directory
                .vars
                .keys()
                .chain(self.model.cache().iter().map(|(name, _)| name));
            for candidate in variable_names {
                names
                    .entry(candidate.as_slice())
                    .or_default()
                    .insert(ExplainItemKind::Variable);
            }
        }
        let mut suggestions = Vec::new();
        for (name, _) in suggest_candidates_in(Namespace::Name, name, names.keys().copied()) {
            if let Some(kinds) = names.get(name.as_slice()) {
                suggestions.extend(kinds.iter().map(|&kind| ExplainSuggestion {
                    name: name.clone(),
                    kind,
                }));
            }
        }
        suggestions
    }

    fn roots(&self) -> ExplainRoots {
        let root = self.model.dir(DirId::ROOT);
        ExplainRoots {
            source: root.source_dir.clone(),
            build: root.binary_dir.clone(),
        }
    }

    fn explain_variable(&self, dir: DirId, name: &BString) -> ExplainVariable {
        let directory = self.model.dir(dir);
        let normal = directory.vars.get(name);
        let cache = self.model.cache().get(name).map(|entry| ExplainCache {
            value: entry.value.clone(),
            kind: entry.ty,
            help: entry.help.clone(),
            location: self.cache_location(name, &entry.value),
            force: self.facts.cache_force.get(name).copied(),
        });
        let writes = self
            .var_ledger
            .iter()
            .flat_map(|ledger| ledger.history(name))
            .map(|event| {
                let details = self.facts.variable_writes.get(&event.seq);
                let directory = details.and_then(|details| details.dir);
                let binding_directory = details.and_then(|details| details.binding_dir);
                ExplainWrite {
                    sequence: event.seq,
                    operation: event.op,
                    command: details.and_then(|details| details.command.clone()),
                    directory,
                    directory_path: directory
                        .and_then(|dir| {
                            self.model
                                .dirs()
                                .nth(dir.index())
                                .map(|(_, directory)| directory.source_dir.clone())
                        })
                        .filter(|path| !path.is_empty()),
                    binding_directory,
                    in_queried_scope: binding_directory == Some(dir)
                        && !matches!(event.op, VarOp::CacheSet | VarOp::CacheUnset),
                    scope_depth: event.scope_depth,
                    scope_id: event.scope_id,
                    previous: match details.map(|details| &details.previous) {
                        Some(PreviousValue::Unset) => PreviousValue::Unset,
                        Some(PreviousValue::Value(value)) => PreviousValue::Value(value.clone()),
                        Some(PreviousValue::Unknown) | None => event
                            .old_value
                            .clone()
                            .map_or(PreviousValue::Unset, PreviousValue::Value),
                    },
                    value: event.new_value.clone(),
                    origin: event.backtrace.clone(),
                }
            })
            .collect();
        ExplainVariable {
            name: name.clone(),
            value: normal
                .cloned()
                .or_else(|| cache.as_ref().map(|entry| entry.value.clone())),
            location: self.binding_location(dir, name),
            normal_binding: normal.is_some(),
            cache,
            writes,
            history: self
                .var_ledger
                .as_ref()
                .map_or(VariableHistory::Unavailable, |ledger| ledger.coverage(name)),
            session_history_truncated: self
                .var_ledger
                .as_ref()
                .and_then(|ledger| ledger.session_truncated()),
            directory: directory.source_dir.clone(),
            roots: self.roots(),
            provisional: self.completion.is_partial(),
        }
    }

    fn explain_target(
        &self,
        dir: DirId,
        name: &BString,
        id: TargetId,
        target: &Target,
        filter: Option<BString>,
    ) -> ExplainTarget {
        let mut configs = IndexMap::<BString, ExplainConfig>::new();
        let resolved = self
            .generation
            .iter()
            .flat_map(|result| &result.targets)
            .filter(|resolved| resolved.id == id);
        for resolved in resolved.clone() {
            let config = config_entry(&mut configs, &resolved.config.0);
            config.sources = resolved
                .compile_units
                .iter()
                .map(|unit| unit.source.clone())
                .collect();
        }
        let chains = self
            .generation
            .as_ref()
            .and_then(|result| result.chains.as_deref());
        for record in self
            .facts
            .resolved_entries
            .iter()
            .filter(|entry| entry.key.target == id)
        {
            let mut entry = ExplainEntry::new(record.key.kind, &record.value);
            entry.link_targets.clone_from(&record.link_targets);
            entry.chain = chains
                .and_then(|chains| chains.get(&record.key))
                .map(|hops| {
                    hops.iter()
                        .map(|hop| ExplainHop {
                            id: hop.from,
                            target: self
                                .model
                                .target(hop.from)
                                .map(|target| target.name.clone()),
                            via: hop.via,
                            origin: hop.link_bt.clone(),
                        })
                        .collect()
                });
            set_spelling(&mut entry);
            config_entry(&mut configs, &record.key.config.0)
                .entries
                .push(entry);
        }
        for resolved in resolved {
            let config = config_entry(&mut configs, &resolved.config.0);
            let (groups, build) = build_entries(self, target, resolved);
            config.compile_groups = groups;
            config.entries = join_entries(build, core::mem::take(&mut config.entries));
        }
        if let Some(filter) = &filter {
            for config in configs.values_mut() {
                config.entries.retain(|entry| entry.matches_filter(filter));
            }
        }
        let mut contributors = BTreeMap::<TargetId, Vec<&BString>>::new();
        let mut matched_properties = BTreeSet::new();
        for config in configs.values() {
            let mut owners = BTreeSet::new();
            if filter.is_none() || !config.entries.is_empty() {
                owners.insert(id);
            }
            for entry in &config.entries {
                matched_properties.insert(chain_kind(entry.kind).as_bytes());
                owners.extend(entry.chain.iter().flatten().map(|hop| hop.id));
                owners.extend(entry.link_targets.iter().copied());
            }
            for owner in owners {
                contributors.entry(owner).or_default().push(&config.name);
            }
        }
        let ledger = self.model.prop_ledger();
        let properties = ledger
            .into_iter()
            .flat_map(|ledger| ledger.iter())
            .filter_map(|(scope, property, events)| {
                let PropScope::Target(owner) = scope else {
                    return None;
                };
                let configurations = match contributors.get(owner) {
                    Some(names) => names.iter().map(|name| (*name).clone()).collect(),
                    None if configs.is_empty() && *owner == id && filter.is_none() => Vec::new(),
                    None => return None,
                };
                if filter.is_some()
                    && !matched_properties.contains(
                        property
                            .strip_prefix(b"INTERFACE_")
                            .unwrap_or(property.as_slice()),
                    )
                    && !matched_properties.contains(property.as_slice())
                    && !matches!(
                        property.as_slice(),
                        b"LINK_LIBRARIES" | b"INTERFACE_LINK_LIBRARIES"
                    )
                {
                    return None;
                }
                Some(ExplainProperty {
                    target: self.model.target(*owner)?.name.clone(),
                    name: property.clone(),
                    configurations,
                    writes: events.to_vec(),
                    dropped_writes: ledger.map_or(0, |ledger| ledger.dropped(scope, property)),
                })
            })
            .collect();
        let alias_definition = self.alias_location(dir, name, id);
        let sources = target
            .sources
            .iter()
            .chain(
                target
                    .file_sets
                    .iter()
                    .filter_map(|id| self.model.file_set(*id))
                    .flat_map(|set| &set.files),
            )
            .cloned()
            .collect();
        ExplainTarget {
            name: name.clone(),
            target: target.name.clone(),
            kind: target.kind,
            definition: target.backtrace.clone(),
            alias_definition,
            sources,
            roots: self.roots(),
            filter,
            configurations: configs.into_values().collect(),
            properties,
            property_history_incomplete: ledger.map(|ledger| ledger.is_incomplete()),
            provisional: self.completion.is_partial(),
        }
    }

    fn alias_location(&self, dir: DirId, name: &[u8], id: TargetId) -> Option<SourceLocation> {
        let binding = TargetBinding::Alias(id);
        if self.model.lookup_target(dir, name) != Some(binding) {
            return None;
        }
        let local = self.model.dir(dir).local_targets.contains_key(name);
        self.facts
            .aliases
            .iter()
            .rev()
            .find(|alias| {
                alias.name.as_slice() == name
                    && alias.global != local
                    && (alias.global || self.model.ancestors(dir).any(|owner| owner == alias.dir))
                    && self.model.lookup_target(alias.dir, name) == Some(binding)
            })
            .map(|alias| alias.location.clone())
    }
}

fn config_entry<'a>(
    configs: &'a mut IndexMap<BString, ExplainConfig>,
    name: &BString,
) -> &'a mut ExplainConfig {
    configs
        .entry(name.clone())
        .or_insert_with(|| ExplainConfig {
            name: name.clone(),
            ..ExplainConfig::default()
        })
}

#[cfg(test)]
mod tests;
