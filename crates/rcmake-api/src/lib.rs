//! Types and queries for embedding Rcmake in a command-line tool or editor.
//!
//! Use [ConfigureRequest::prepare] to apply host policies before evaluation.
//! Pass the evaluated model and recorded facts to [ConfigureResult]. Use that
//! result to query the workspace or explain how an item got its value.
//!
//! ```
//! use std::{path::Path, sync::Arc};
//!
//! use rcmake_api::{ConfigureRequest, ProcessPolicy, WriteMode};
//! use rcmake_host::PureHost;
//!
//! # fn main() -> Result<(), rcmake_api::ApiError> {
//! let root = Path::new(if cfg!(windows) { "C:/" } else { "/" });
//! let host = Arc::new(PureHost::new(root.to_owned(), root.join("bin/rcmake")));
//! let request = ConfigureRequest::new("project", "build", "cmake", host)
//!     .process_policy(ProcessPolicy::Deny)
//!     .write_mode(WriteMode::Overlay)
//!     .collect_index(true);
//! let session = request.prepare()?;
//! let _host = session.host();
//! # Ok(())
//! # }
//! ```

#![forbid(unsafe_code)]

#[cfg(doctest)]
#[doc = include_str!("../README.md")]
pub struct ReadmeDoctests;

mod config;
mod eval_explain;
mod explain;
mod explain_deps;
mod explain_entries;
mod explain_filter;
mod explain_json;
mod explain_render;
mod find_explain;
mod find_trace_json;
mod index;
mod overlay_diff;
mod result;
mod runner;
pub use runner::run_configure;
mod variable_history;

pub use config::{
    ApiError, ConfigureRequest, PreparedConfigure, ProcessPolicy, WriteMode, observe_reads,
};
pub use eval_explain::{
    EXPLAIN_EVAL_SCHEMA, EvalConfigResult, EvalReport, EvalRequest, render_eval_json,
    render_eval_terminal,
};
pub use explain::{
    CompilePosition, ExplainCache, ExplainCompileGroup, ExplainConfig, ExplainEntry, ExplainHop,
    ExplainInput, ExplainItemKind, ExplainProperty, ExplainQuery, ExplainReport, ExplainRoots,
    ExplainSuggestion, ExplainTarget, ExplainVariable, ExplainWrite,
};
pub use explain_deps::{DependencyEdge, DependencyHop, DependencyPath, DependencyReport};
pub use explain_filter::ExplainFilter;
pub use explain_json::{EXPLAIN_SCHEMA, explain_json, render_explain_json};
pub use explain_render::{ExplainRenderOptions, render_explain_terminal};
pub use find_explain::{
    EXPLAIN_FIND_SCHEMA, FindReport, FindSource, FindSuggestion, NearMissProbe, SuggestionKind,
    render_find_json, render_find_terminal, suggest,
};
pub use find_trace_json::{
    FIND_TRACE_MAX_BYTES, FIND_TRACE_PATH, FIND_TRACE_SCHEMA, FileStamp, FindTraceDocument,
    Freshness,
};
pub use index::{
    CallableDefinition, Completion, CompletionKind, ExecutedSite, ExecutedSites, ExecutedSymbol,
    FileEdge, FileEdgeKind, SourceLocation, SourceRange, TargetDefinition, UsageSummary,
    VariableDefinition, WorkspaceIndex,
};
pub use overlay_diff::{OverlayBytes, OverlayChange, OverlayDiff, OverlayEntryKind};
pub use result::{
    AliasSite, CompletionState, ConfigureResult, EvaluationFacts, PreviousValue, ResolvedEntry,
    VariableWriteDetails,
};

/// Set a native stack budget around [`PreparedConfigure::evaluate`] or
/// [`run_configure`] on a thread with a known larger stack.
pub use rcmake_eval::stack_guard::with_budget as with_stack_budget;
pub use rcmake_eval::{CacheDefine, CancelToken, LedgerLevel, Services, VarWrite, stack_guard};

pub use variable_history::{VarLedgerSnapshot, VariableHistory};
