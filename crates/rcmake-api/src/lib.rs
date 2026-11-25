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

mod config;
mod explain;
mod explain_entries;
mod explain_json;
mod explain_render;
mod index;
mod overlay_diff;
mod result;
mod runner;
pub use runner::run_configure;
mod variable_history;

pub use config::{
    ApiError, ConfigureRequest, PreparedConfigure, ProcessPolicy, WriteMode, observe_reads,
};
pub use explain::{
    CompilePosition, ExplainCache, ExplainCompileGroup, ExplainConfig, ExplainEntry, ExplainHop,
    ExplainInput, ExplainItemKind, ExplainProperty, ExplainQuery, ExplainReport, ExplainRoots,
    ExplainSuggestion, ExplainTarget, ExplainVariable, ExplainWrite,
};
pub use explain_json::{EXPLAIN_SCHEMA, explain_json, render_explain_json};
pub use explain_render::{ExplainRenderOptions, render_explain_terminal};
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

pub use rcmake_eval::{CacheDefine, CancelToken, LedgerLevel, Services, VarWrite};

pub use variable_history::{VarLedgerSnapshot, VariableHistory};
