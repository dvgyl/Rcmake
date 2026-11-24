use std::collections::BTreeSet;

use indexmap::IndexMap;
use rcmake_base::BString;
use rcmake_diag::{Backtrace, suggest::suggest_candidates};
use rcmake_eval::{ValuePreview, VarOp};
use rcmake_model::{
    BT, CacheType, DirId, PropEvent, PropScope, Target, TargetBinding, TargetId, TargetKind,
    UsageField, UsageScope,
};
use rcmake_resolve::{ChainKind, LinkFragmentRole, ResolvedTarget};

use crate::{ConfigureResult, PreviousValue, SourceLocation};

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
    },
    /// No item matched the query.
    NotFound {
        /// Requested name.
        name: BString,
        /// Similar names, closest first.
        suggestions: Vec<BString>,
    },
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
    /// Resolved usage entries for each configuration.
    pub configurations: Vec<ExplainConfig>,
    /// Property writes on the target and contributors in its chains.
    pub properties: Vec<ExplainProperty>,
    /// Whether a property history reached its limit, if collection was enabled.
    pub property_history_incomplete: Option<bool>,
    /// Whether evaluation was partial.
    pub provisional: bool,
}

/// Resolved usage for one configuration.
#[derive(Clone, Debug, Default)]
pub struct ExplainConfig {
    /// Configuration name. An empty name means the default configuration.
    pub name: BString,
    /// Resolved sources with call stacks, when available.
    pub sources: Vec<BT<BString>>,
    /// Usage entries in recorder order or build order.
    pub entries: Vec<ExplainEntry>,
}

/// One resolved usage requirement or link item.
#[derive(Clone, Debug)]
pub struct ExplainEntry {
    /// Type of usage requirement or link item.
    pub kind: ChainKind,
    /// Evaluated value.
    pub value: BString,
    /// Call stack for the original entry.
    pub origin: Backtrace,
    /// Ordered hops. An empty chain identifies a direct value.
    /// An absent chain means that no matching chain was recorded.
    pub chain: Option<Vec<ExplainHop>>,
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
    /// CMake property name.
    pub name: BString,
    /// Writes in evaluation order.
    pub writes: Vec<PropEvent>,
}

/// A variable as seen in the requested directory.
#[derive(Clone, Debug)]
pub struct ExplainVariable {
    /// Variable name.
    pub name: BString,
    /// Effective value after evaluation. An absent value means undefined.
    pub value: Option<BString>,
    /// Cache entry even when a normal variable hides it.
    pub cache: Option<ExplainCache>,
    /// Retained writes to this name, oldest first.
    pub writes: Vec<ExplainWrite>,
    /// Whether recording lost events. An absent value means history was unavailable.
    pub history_incomplete: Option<bool>,
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
    /// Depth of the written scope.
    pub scope_depth: u32,
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

    /// Explain an item as seen from the specified evaluated directory.
    #[must_use]
    pub fn explain_in(&self, dir: DirId, query: &ExplainQuery) -> ExplainReport {
        let (name, targets, variables) = match query {
            ExplainQuery::Auto(name) => (name, true, true),
            ExplainQuery::Target(name) => (name, true, false),
            ExplainQuery::Variable(name) => (name, false, true),
        };
        let target = targets
            .then(|| self.model.find_target(dir, name))
            .flatten()
            .and_then(|id| self.model.target(id).map(|target| (id, target)));
        let value = variables
            .then(|| self.model.definition(dir, name))
            .flatten();
        let has_history = variables
            && self
                .var_ledger
                .as_ref()
                .is_some_and(|ledger| ledger.history(name).next().is_some());
        match (target, value.is_some() || has_history) {
            (Some(_), true) => ExplainReport::Ambiguous { name: name.clone() },
            (Some((id, target)), false) => {
                ExplainReport::Target(Box::new(self.explain_target(dir, name, id, target)))
            }
            (None, true) => ExplainReport::Variable(Box::new(self.explain_variable(name, value))),
            (None, false) => ExplainReport::NotFound {
                name: name.clone(),
                suggestions: self.suggestions(dir, name, targets, variables),
            },
        }
    }

    fn suggestions(&self, dir: DirId, name: &[u8], targets: bool, variables: bool) -> Vec<BString> {
        let directory = self.model.dir(dir);
        let target_names = self
            .model
            .targets()
            .filter(|(id, target)| self.model.find_target(dir, &target.name) == Some(*id))
            .map(|(_, target)| &target.name)
            .chain(directory.local_targets.keys())
            .chain(self.facts.aliases.iter().filter_map(|alias| {
                self.model
                    .lookup_target(dir, &alias.name)
                    .map(|_| &alias.name)
            }));
        let variable_names = directory
            .vars
            .keys()
            .chain(self.model.cache().iter().map(|(name, _)| name))
            .chain(self.var_ledger.iter().flat_map(|ledger| ledger.names()));
        suggest_candidates(
            name,
            targets
                .then_some(target_names)
                .into_iter()
                .flatten()
                .chain(variables.then_some(variable_names).into_iter().flatten())
                .map(|name| name.as_slice()),
        )
        .into_iter()
        .map(|(name, _)| name)
        .collect()
    }

    fn explain_variable(&self, name: &BString, value: Option<&BString>) -> ExplainVariable {
        let cache = self.model.cache().get(name).map(|entry| ExplainCache {
            value: entry.value.clone(),
            kind: entry.ty,
            help: entry.help.clone(),
            force: self.facts.cache_force.get(name).copied(),
        });
        let writes: Vec<_> = self
            .var_ledger
            .iter()
            .flat_map(|ledger| ledger.history(name))
            .map(|event| {
                let details = self.facts.variable_writes.get(&event.seq);
                ExplainWrite {
                    sequence: event.seq,
                    operation: event.op,
                    command: details.and_then(|details| details.command.clone()),
                    directory: details.and_then(|details| details.dir),
                    scope_depth: event.scope_depth,
                    previous: details
                        .map_or(PreviousValue::Unknown, |details| details.previous.clone()),
                    value: event.value_preview.clone(),
                    origin: event.backtrace.clone(),
                }
            })
            .collect();
        let history_incomplete = self.var_ledger.as_ref().and_then(|ledger| {
            let incomplete = ledger.is_incomplete();
            (incomplete || !writes.is_empty()).then_some(incomplete)
        });
        ExplainVariable {
            name: name.clone(),
            value: value.cloned(),
            cache,
            writes,
            history_incomplete,
            provisional: self.completion.is_partial(),
        }
    }

    fn explain_target(
        &self,
        dir: DirId,
        name: &BString,
        id: TargetId,
        target: &Target,
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
        let mut contributors = BTreeSet::from([id]);
        let chain_map = self
            .generation
            .as_ref()
            .and_then(|result| result.chains.as_deref());
        for entry in self
            .facts
            .resolved_entries
            .iter()
            .filter(|entry| entry.key.target == id)
        {
            let chain = chain_map
                .and_then(|chains| chains.get(&entry.key))
                .map(|hops| {
                    hops.iter()
                        .map(|hop| {
                            contributors.insert(hop.from);
                            ExplainHop {
                                id: hop.from,
                                target: self
                                    .model
                                    .target(hop.from)
                                    .map(|target| target.name.clone()),
                                via: hop.via,
                                origin: hop.link_bt.clone(),
                            }
                        })
                        .collect()
                });
            config_entry(&mut configs, &entry.key.config.0)
                .entries
                .push(ExplainEntry {
                    kind: entry.key.kind,
                    value: entry.value.value.clone(),
                    origin: entry.value.backtrace.clone(),
                    chain,
                });
        }
        for resolved in resolved {
            let config = config_entry(&mut configs, &resolved.config.0);
            if config.entries.is_empty() {
                config.entries = build_entries(resolved);
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
                if !contributors.contains(owner) {
                    return None;
                }
                Some(ExplainProperty {
                    target: self.model.target(*owner)?.name.clone(),
                    name: property.clone(),
                    writes: events.to_vec(),
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

// The build graph contains synthesized flags and can split or remove link items.
// A lowered entry's position cannot identify an entry in the chain recorder.
fn build_entries(target: &ResolvedTarget) -> Vec<ExplainEntry> {
    let mut entries = Vec::new();
    let mut push = |kind, entry: &BT<BString>| {
        entries.push(ExplainEntry {
            kind,
            value: entry.value.clone(),
            origin: entry.backtrace.clone(),
            chain: None,
        });
    };
    for group in &target.compile_groups {
        for flag in &group.flags {
            push(ChainKind::Usage(UsageField::CompileOptions), flag);
        }
        for define in &group.defines {
            push(ChainKind::Usage(UsageField::CompileDefinitions), define);
        }
        for include in &group.includes {
            push(
                ChainKind::Usage(UsageField::IncludeDirectories),
                &include.path,
            );
        }
    }
    for fragment in target.link.iter().flat_map(|link| &link.fragments) {
        match fragment.role {
            LinkFragmentRole::Flags => {
                push(ChainKind::Usage(UsageField::LinkOptions), &fragment.text)
            }
            LinkFragmentRole::Libraries => push(ChainKind::LinkItem, &fragment.text),
            LinkFragmentRole::LibraryPath | LinkFragmentRole::FrameworkPath => {}
        }
    }
    entries
}

#[cfg(test)]
mod tests;
