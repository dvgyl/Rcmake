mod site;

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

pub use site::{ExecutedSite, ExecutedSites, ExecutedSymbol, SourceRange};

use rcmake_base::{BString, fold_command_name};
use rcmake_diag::{Backtrace, SourceFile, SourceId, SourceMap};
use rcmake_eval::UserCommandKind;
use rcmake_model::{
    DirId, ProjectModel, PropScope, TargetBinding, TargetId, TargetKind, UsageField,
};
use rcmake_resolve::ResolveChains;

use crate::{EvaluationFacts, VarLedgerSnapshot};

/// Source location in CMake path spelling.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct SourceLocation {
    /// Source file path bytes.
    pub path: BString,
    /// One-based line number. Zero means that the line is unavailable.
    pub line: u32,
    /// Zero-based byte offset in the file, when recorded.
    pub byte_offset: Option<u32>,
    /// Identity of the exact evaluated buffer, when known.
    pub source_id: Option<SourceId>,
    /// Full range of the symbol or command, when recorded.
    pub range: Option<SourceRange>,
}

impl SourceLocation {
    /// Use the innermost frame of a call stack as a source location.
    #[must_use]
    pub fn from_backtrace(trace: &Backtrace) -> Option<Self> {
        let frame = trace.top()?;
        Some(Self {
            path: frame.file.as_ref().into(),
            line: frame.line,
            byte_offset: None,
            source_id: None,
            range: None,
        })
    }

    fn matches(&self, query: &Self) -> bool {
        self.matches_range(query, None)
    }

    fn matches_range(&self, query: &Self, argument: Option<SourceRange>) -> bool {
        if self
            .source_id
            .zip(query.source_id)
            .is_some_and(|(a, b)| a != b)
        {
            return false;
        }
        let offset = query.byte_offset.or(query.range.map(|range| range.start));
        if let Some(offset) = offset {
            if [self.range, argument]
                .into_iter()
                .flatten()
                .any(|range| range.start <= offset && offset < range.end)
            {
                return true;
            }
            return self.byte_offset == Some(offset);
        }
        self.line == query.line
    }
}

/// A function or macro definition.
#[derive(Clone, Debug)]
pub struct CallableDefinition {
    /// Name as written at the definition.
    pub name: BString,
    /// Whether the command is a function or macro.
    pub kind: UserCommandKind,
    /// Location of the definition.
    pub location: SourceLocation,
    /// Formal parameters in declaration order.
    pub parameters: Vec<BString>,
    /// Documentation from the source, when available.
    pub documentation: Option<BString>,
}

/// Summary of a target's configured usage requirements.
#[derive(Clone, Debug, Default)]
pub struct UsageSummary {
    /// Number of declared entries for each usage property.
    pub declared_entries: BTreeMap<UsageField, usize>,
    /// Number of evaluated entries that have recorded provenance chains.
    pub recorded_chains: usize,
    /// Target properties that have retained mutation histories.
    pub changed_properties: Vec<BString>,
}

/// A real target or an alias that names it.
#[derive(Clone, Debug)]
pub struct TargetDefinition {
    /// Name used for lookup.
    pub name: BString,
    /// Real target in the project model.
    pub id: TargetId,
    /// Kind of the real target.
    pub kind: TargetKind,
    /// Definition of this name, when available.
    pub location: Option<SourceLocation>,
    /// Real target name when this name is an alias.
    pub alias_of: Option<BString>,
    /// Link entries as written, including generator expressions.
    pub declared_links: Vec<BString>,
    /// Evaluated link targets across the recorded configurations.
    pub link_dependencies: Vec<TargetId>,
    /// Configured usage requirements and provenance counts.
    pub usage: UsageSummary,
}

/// A final value visible in a directory or through the cache.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VariableDefinition {
    /// Evaluated value.
    pub value: BString,
    /// Site of the write that supplied this value, when available.
    pub location: Option<SourceLocation>,
    /// Whether the value came from the cache.
    pub cached: bool,
    /// Whether the evaluation was partial.
    pub provisional: bool,
}

/// Command that caused a listfile to be read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FileEdgeKind {
    /// `include()` read a file.
    Include,
    /// `add_subdirectory()` read the child directory's listfile.
    AddSubdirectory,
}

/// A resolved listfile read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileEdge {
    /// Kind of command that read the file.
    pub kind: FileEdgeKind,
    /// Directory whose evaluation caused the read.
    pub dir: DirId,
    /// Source location of the command.
    pub site: SourceLocation,
    /// Argument range, when the evaluator recorded the original syntax.
    pub argument_range: Option<SourceRange>,
    /// Exact path selected by evaluation.
    pub destination: BString,
}

/// Type of a workspace completion.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum CompletionKind {
    /// Real target or alias.
    Target,
    /// Function or macro.
    Callable,
    /// Normal or cache variable.
    Variable,
}

/// A name visible from a directory at the end of evaluation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Completion<'a> {
    /// Matching name.
    pub name: &'a BString,
    /// Type of symbol the name identifies.
    pub kind: CompletionKind,
}

/// Immutable workspace facts for evaluated editor queries.
///
/// Directory IDs distinguish repeated evaluations of the same source file in
/// different binary directories. The language server owns URI and position
/// conversion and can use lexical results when an evaluated site is unavailable.
#[derive(Clone, Debug, Default)]
pub struct WorkspaceIndex {
    targets: BTreeMap<TargetId, TargetDefinition>,
    global_targets: BTreeMap<BString, TargetId>,
    local_targets: BTreeMap<DirId, BTreeMap<BString, TargetBinding>>,
    aliases: BTreeMap<Option<DirId>, BTreeMap<BString, TargetDefinition>>,
    callables: BTreeMap<BString, CallableDefinition>,
    variables: BTreeMap<DirId, BTreeMap<BString, VariableDefinition>>,
    cache: BTreeMap<BString, VariableDefinition>,
    files: BTreeMap<BString, Vec<FileEdge>>,
    executed_sites: Arc<ExecutedSites>,
    source_map: Option<Arc<SourceMap>>,
    provisional: bool,
    file_scopes: BTreeMap<BString, Vec<DirId>>,
}

impl WorkspaceIndex {
    /// Build an index from one evaluated model and its optional recorded data.
    ///
    /// Property histories come from the model's property ledger.
    #[must_use]
    pub fn build(
        model: &ProjectModel,
        chains: Option<&ResolveChains>,
        variables: Option<&VarLedgerSnapshot>,
        facts: &EvaluationFacts,
        provisional: bool,
    ) -> Self {
        let mut index = Self {
            executed_sites: Arc::clone(&facts.executed_sites),
            source_map: facts.source_map.clone(),
            provisional,
            ..Self::default()
        };
        for (id, target) in model.targets() {
            let declared_links = target
                .usage
                .link_libraries
                .iter()
                .chain(&target.usage.interface_link_libraries)
                .flatten()
                .map(|link| link.text.value.clone())
                .collect();
            let declared_entries = target
                .usage
                .fields
                .iter()
                .map(|(&field, entries)| (field, entries.len()))
                .collect();
            index.targets.insert(
                id,
                TargetDefinition {
                    name: target.name.clone(),
                    id,
                    kind: target.kind,
                    location: facts
                        .target_sites
                        .get(&id)
                        .cloned()
                        .or_else(|| SourceLocation::from_backtrace(&target.backtrace)),
                    alias_of: None,
                    declared_links,
                    link_dependencies: Vec::new(),
                    usage: UsageSummary {
                        declared_entries,
                        ..UsageSummary::default()
                    },
                },
            );
        }
        let mut dependencies = BTreeMap::<TargetId, BTreeSet<TargetId>>::new();
        for entry in &facts.resolved_entries {
            if let Some(target) = index.targets.get_mut(&entry.key.target) {
                let seen = dependencies.entry(target.id).or_default();
                target.link_dependencies.extend(
                    entry
                        .link_targets
                        .iter()
                        .copied()
                        .filter(|id| seen.insert(*id)),
                );
            }
        }
        for key in chains.into_iter().flat_map(ResolveChains::keys) {
            if let Some(target) = index.targets.get_mut(&key.target) {
                target.usage.recorded_chains += 1;
            }
        }
        if let Some(ledger) = model.prop_ledger() {
            for (scope, property, _) in ledger.iter() {
                if let PropScope::Target(id) = scope
                    && let Some(target) = index.targets.get_mut(id)
                {
                    target.usage.changed_properties.push(property.clone());
                }
            }
        }
        let mut alias_sites = BTreeMap::new();
        for alias in &facts.aliases {
            alias_sites.insert(
                ((!alias.global).then_some(alias.dir), &alias.name),
                &alias.location,
            );
        }
        for (name, binding) in model.global_target_bindings() {
            match binding {
                TargetBinding::Target(id) => {
                    index.global_targets.insert(name.clone(), id);
                }
                TargetBinding::Alias(id) => {
                    if let Some(target) = index.targets.get(&id) {
                        let location = alias_sites.get(&(None, name)).copied().cloned();
                        index
                            .aliases
                            .entry(None)
                            .or_default()
                            .insert(name.clone(), alias_definition(target, name, location));
                    }
                }
            }
        }
        for callable in &facts.callables {
            index
                .callables
                .insert(fold_command_name(&callable.name), callable.clone());
        }
        for (name, entry) in model.cache().iter() {
            let location = facts
                .cache_sites
                .get(name)
                .cloned()
                .or_else(|| crate::result::infer_cache_location(variables, name, &entry.value));
            index.cache.insert(
                name.clone(),
                VariableDefinition {
                    value: entry.value.clone(),
                    location,
                    cached: true,
                    provisional,
                },
            );
        }
        for (dir, directory) in model.dirs() {
            index.local_targets.insert(
                dir,
                directory
                    .local_targets
                    .iter()
                    .map(|(name, binding)| (name.clone(), *binding))
                    .collect(),
            );
            for (name, binding) in &directory.local_targets {
                if let TargetBinding::Alias(id) = binding
                    && let Some(target) = index.targets.get(id)
                {
                    let location = model.ancestors(dir).find_map(|owner| {
                        if model.dir(owner).local_targets.get(name) == Some(binding) {
                            alias_sites.get(&(Some(owner), name)).copied().cloned()
                        } else {
                            None
                        }
                    });
                    index
                        .aliases
                        .entry(Some(dir))
                        .or_default()
                        .insert(name.clone(), alias_definition(target, name, location));
                }
            }
            index.variables.insert(
                dir,
                directory
                    .vars
                    .iter()
                    .map(|(name, value)| {
                        let location = facts
                            .variable_sites
                            .get(&dir)
                            .and_then(|sites| sites.get(name))
                            .cloned();
                        (
                            name.clone(),
                            VariableDefinition {
                                value: value.clone(),
                                location,
                                cached: false,
                                provisional,
                            },
                        )
                    })
                    .collect(),
            );
            for file in &directory.listfiles {
                let scopes = index.file_scopes.entry(file.clone()).or_default();
                if scopes.last() != Some(&dir) {
                    scopes.push(dir);
                }
            }
        }
        for edge in &facts.file_edges {
            if index.local_targets.contains_key(&edge.dir) {
                index
                    .files
                    .entry(edge.site.path.clone())
                    .or_default()
                    .push(edge.clone());
                let scopes = index.file_scopes.entry(edge.site.path.clone()).or_default();
                if let Err(index) = scopes.binary_search(&edge.dir) {
                    scopes.insert(index, edge.dir);
                }
            }
        }
        index
    }

    /// Return whether the index came from a partial configure.
    #[must_use]
    pub fn is_provisional(&self) -> bool {
        self.provisional
    }

    /// Return whether the executed-site limit caused records to be discarded.
    #[must_use]
    pub fn executed_sites_truncated(&self) -> bool {
        self.executed_sites.is_truncated()
    }

    /// Get the buffer used for the specified source ID.
    ///
    /// File buffers may use normalized line endings. Map positions to the
    /// editor's document only after checking its version and line endings.
    #[must_use]
    pub fn source(&self, id: SourceId) -> Option<&SourceFile> {
        self.source_map.as_ref()?.get(id)
    }

    /// Find bindings observed at the site during evaluation, in execution order.
    ///
    /// Pass no directory to match all evaluations of the file.
    pub fn executed_at(
        &self,
        dir: Option<DirId>,
        site: &SourceLocation,
    ) -> impl Iterator<Item = &ExecutedSite> {
        self.executed_sites.matching(dir, site)
    }

    /// Find a target or alias visible in `dir`.
    #[must_use]
    pub fn lookup_target(&self, dir: DirId, name: &[u8]) -> Option<&TargetDefinition> {
        if let Some(binding) = self.local_targets.get(&dir)?.get(name) {
            match binding {
                TargetBinding::Alias(_) => return self.aliases.get(&Some(dir))?.get(name),
                TargetBinding::Target(id) => return self.targets.get(id),
            }
        }
        self.aliases
            .get(&None)
            .and_then(|aliases| aliases.get(name))
            .or_else(|| self.targets.get(self.global_targets.get(name)?))
    }

    /// Find the last registered function or macro with this name, ignoring ASCII case.
    #[must_use]
    pub fn lookup_callable(&self, name: &[u8]) -> Option<&CallableDefinition> {
        self.callables.get(&fold_command_name(name))
    }

    /// Find the final normal variable value, then the cache value if no normal value exists.
    #[must_use]
    pub fn lookup_variable(&self, dir: DirId, name: &[u8]) -> Option<&VariableDefinition> {
        self.variables
            .get(&dir)?
            .get(name)
            .or_else(|| self.cache.get(name))
    }

    /// Iterate over the destinations read at a command site, in evaluation order.
    ///
    /// A query with a byte offset matches a recorded offset or range. A query
    /// with only a line number can return several calls on that line.
    pub fn resolve_include(
        &self,
        dir: DirId,
        site: &SourceLocation,
    ) -> impl Iterator<Item = &FileEdge> {
        self.resolve_file_edges(Some(dir), site)
    }

    /// Find the files read at this site, in evaluation order.
    ///
    /// Pass no directory to see results from all directory scopes. A range can
    /// match a cursor inside the command or its argument. Only successful
    /// evaluated reads appear here. A caller can use lexical fallback on a miss.
    pub fn resolve_file_edges(
        &self,
        dir: Option<DirId>,
        site: &SourceLocation,
    ) -> impl Iterator<Item = &FileEdge> {
        let query = site.clone();
        let edges = self.files.get(&query.path).into_iter().flatten();
        let at_site = move |edge: &&FileEdge| {
            dir.is_none_or(|dir| dir == edge.dir)
                && edge.site.matches_range(&query, edge.argument_range)
        };
        edges.filter(at_site)
    }

    /// Get directories that evaluated this listfile, in model order.
    #[must_use]
    pub fn scopes_for_file(&self, path: &[u8]) -> &[DirId] {
        self.file_scopes.get(path).map_or(&[], Vec::as_slice)
    }

    /// Return visible names matching a prefix, in byte order.
    ///
    /// Callable names use ASCII case folding. Target and variable names are case-sensitive.
    #[must_use]
    pub fn completions_for_scope(&self, dir: DirId, prefix: &[u8]) -> Vec<Completion<'_>> {
        let Some(local) = self.local_targets.get(&dir) else {
            return Vec::new();
        };
        let mut matches = BTreeMap::new();
        for name in self
            .global_targets
            .keys()
            .chain(self.aliases.get(&None).into_iter().flat_map(BTreeMap::keys))
            .chain(local.keys())
            .filter(|name| name.starts_with(prefix))
        {
            matches.insert((name, CompletionKind::Target), name);
        }
        for name in self
            .cache
            .keys()
            .chain(
                self.variables
                    .get(&dir)
                    .into_iter()
                    .flat_map(BTreeMap::keys),
            )
            .filter(|name| name.starts_with(prefix))
        {
            matches.insert((name, CompletionKind::Variable), name);
        }
        let folded = fold_command_name(prefix);
        for (key, callable) in self
            .callables
            .iter()
            .filter(|(key, _)| key.starts_with(&folded))
        {
            matches.insert((key, CompletionKind::Callable), &callable.name);
        }
        matches
            .into_iter()
            .map(|((_, kind), name)| Completion { name, kind })
            .collect()
    }
}

fn alias_definition(
    target: &TargetDefinition,
    name: &BString,
    location: Option<SourceLocation>,
) -> TargetDefinition {
    TargetDefinition {
        name: name.clone(),
        location,
        alias_of: Some(target.name.clone()),
        ..target.clone()
    }
}

#[cfg(test)]
mod tests;
