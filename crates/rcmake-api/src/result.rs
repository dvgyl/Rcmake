use std::collections::BTreeMap;
use std::sync::Arc;

use rcmake_base::BString;
use rcmake_diag::{Diagnostic, SourceMap};
use rcmake_eval::{ValuePreview, VarOp};
use rcmake_host::no_process::DeniedCommand;
use rcmake_model::{BT, DirId, ProjectModel, TargetId};
use rcmake_resolve::{ChainKey, GenerationResult};

use crate::{
    CallableDefinition, ExecutedSites, FileEdge, OverlayDiff, SourceLocation, VarLedgerSnapshot,
    WorkspaceIndex,
};

/// Whether evaluation reached a complete result.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum CompletionState {
    /// Configure completed and no subprocess request was denied.
    #[default]
    Complete,
    /// Configure stopped early or ran with unavailable subprocess results.
    Partial {
        /// Retained subprocess requests that the host denied.
        denied_processes: Vec<DeniedCommand>,
        /// Number of complete denial records that the host evicted.
        omitted_denials: usize,
        /// Total request bytes in the evicted records.
        omitted_denial_bytes: usize,
        /// Whether the evaluator stopped because the caller cancelled it.
        cancelled: bool,
    },
}

impl CompletionState {
    /// Return whether the result is partial.
    #[must_use]
    pub fn is_partial(&self) -> bool {
        matches!(self, Self::Partial { .. })
    }
}

/// An alias declaration recorded when the evaluator registered its name.
#[derive(Clone, Debug)]
pub struct AliasSite {
    /// Name that refers to the target.
    pub name: BString,
    /// Directory in which the alias was declared.
    pub dir: DirId,
    /// Whether the alias was registered with global visibility.
    pub global: bool,
    /// Source site of the alias declaration.
    pub location: SourceLocation,
}

/// The previous value at a recorded write.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum PreviousValue {
    /// The evaluator did not record the previous value.
    #[default]
    Unknown,
    /// The variable had no value in the written scope.
    Unset,
    /// A preview of the value in the written scope.
    Value(ValuePreview),
}

/// Information captured at a variable write before its scope can change.
#[derive(Clone, Debug, Default)]
pub struct VariableWriteDetails {
    /// Directory in which the write occurred, when recorded.
    pub dir: Option<DirId>,
    /// Directory whose persistent normal binding changed, if applicable.
    /// A function or block local write has no directory binding.
    pub binding_dir: Option<DirId>,
    /// Command and operation, such as `list(APPEND)`, when recorded.
    pub command: Option<BString>,
    /// Value in the written scope immediately before the write.
    pub previous: PreviousValue,
}

/// One resolved value in the same output order used by the chain recorder.
#[derive(Clone, Debug)]
pub struct ResolvedEntry {
    /// Exact key assigned to this entry by the resolver.
    pub key: ChainKey,
    /// Value and original property or link call stack.
    pub value: BT<BString>,
    /// Targets referenced by a link item, in link order.
    pub link_targets: Vec<TargetId>,
}

/// Evaluation data that requires a source-site or resolver hook.
///
/// Supply these records from the same evaluation as the model. Index and
/// explain queries use available records when a hook was disabled.
#[derive(Clone, Debug, Default)]
pub struct EvaluationFacts {
    /// Alias declaration sites in evaluation order.
    pub aliases: Vec<AliasSite>,
    /// Definition locations for real targets, when recorded.
    pub target_sites: BTreeMap<TargetId, SourceLocation>,
    /// Registered buffers used during evaluation, if available.
    pub source_map: Option<Arc<SourceMap>>,
    /// Bounded executed symbol references for position-sensitive queries.
    pub executed_sites: Arc<ExecutedSites>,
    /// Evaluated command definitions in registration order.
    pub callables: Vec<CallableDefinition>,
    /// Resolved include and subdirectory edges in evaluation order.
    pub file_edges: Vec<FileEdge>,
    /// Source of each final normal variable value, grouped by directory.
    pub variable_sites: BTreeMap<DirId, BTreeMap<BString, SourceLocation>>,
    /// Source of each final cache entry, when recorded.
    pub cache_sites: BTreeMap<BString, SourceLocation>,
    /// Whether the write that produced each current cache entry used `FORCE`.
    pub cache_force: BTreeMap<BString, bool>,
    /// Details for each retained variable write, keyed by the ledger sequence number.
    pub variable_writes: BTreeMap<u64, VariableWriteDetails>,
    /// Ordered entries from the resolver stage that recorded provenance chains.
    pub resolved_entries: Vec<ResolvedEntry>,
}

/// Data produced by one configure session.
#[derive(Clone, Debug, Default)]
pub struct ConfigureResult {
    /// Diagnostics in emission order, after CMake warning policy was applied.
    pub diagnostics: Vec<Diagnostic>,
    /// Whether all steps completed with the required capabilities.
    pub completion: CompletionState,
    /// Evaluated project model.
    pub model: ProjectModel,
    /// Resolved targets and build graphs, when resolution completed.
    pub generation: Option<GenerationResult>,
    /// Evaluated index, when requested.
    pub index: Option<WorkspaceIndex>,
    /// Retained variable history, when requested.
    pub var_ledger: Option<VarLedgerSnapshot>,
    /// Filesystem changes, when the configure used overlay writes.
    pub overlay_diff: Option<OverlayDiff>,
    /// Source-site information and resolved values collected by the session.
    pub facts: EvaluationFacts,
}

impl ConfigureResult {
    pub(crate) fn cache_location(&self, name: &[u8], value: &[u8]) -> Option<SourceLocation> {
        self.facts
            .cache_sites
            .get(name)
            .cloned()
            .or_else(|| infer_cache_location(self.var_ledger.as_ref(), name, value))
    }

    pub(crate) fn binding_location(&self, dir: DirId, name: &[u8]) -> Option<SourceLocation> {
        if self.model.dir(dir).vars.contains_key(name) {
            return self.facts.variable_sites.get(&dir)?.get(name).cloned();
        }
        let entry = self.model.cache().get(name)?;
        self.cache_location(name, &entry.value)
    }

    /// Build an index from the result's current model and recorded data.
    pub fn build_index(&mut self) {
        self.index = Some(WorkspaceIndex::build(
            &self.model,
            self.generation
                .as_ref()
                .and_then(|result| result.chains.as_deref()),
            self.var_ledger.as_ref(),
            &self.facts,
            self.completion.is_partial(),
        ));
    }
}

pub(crate) fn infer_cache_location(
    ledger: Option<&VarLedgerSnapshot>,
    name: &[u8],
    value: &[u8],
) -> Option<SourceLocation> {
    let ledger = ledger.filter(|ledger| {
        ledger.session_truncated() == Some(false)
            || ledger.coverage(name) == crate::VariableHistory::Complete
    })?;
    let event = ledger
        .history(name)
        .rfind(|event| matches!(event.op, VarOp::CacheSet | VarOp::CacheUnset))?;
    let preview = event.new_value.as_ref()?;
    (event.op == VarOp::CacheSet && preview.len == value.len() && preview.prefix == value)
        .then(|| SourceLocation::from_backtrace(&event.backtrace))
        .flatten()
}
