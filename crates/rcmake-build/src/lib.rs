//! In-process executor for a lowered [`BuildGraph`] (ADR-008 item 4).
//!
//! `try_compile()` and `try_run()` run their build graph here instead of starting
//! Ninja or Make. This keeps compiler checks fast and deterministic, and lets compiler
//! bootstrap work before the generators exist.
//!
//! # Status
//!
//! Wave 0 freezes the contract. [`execute`] succeeds for an empty selection and returns
//! [`BuildError::Unimplemented`] otherwise until Wave 2.

#![forbid(unsafe_code)]

use rcmake_base::BString;
use rcmake_host::Host;
use rcmake_resolve::BuildGraph;

/// Options of one build run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BuildOptions {
    /// Targets to build (empty means every target that is part of `all`).
    pub targets: Vec<BString>,
    /// Maximum number of parallel steps (at least 1).
    pub jobs: usize,
    /// Continue after a failed step.
    pub keep_going: bool,
    /// Print each command line.
    pub verbose: bool,
}

impl Default for BuildOptions {
    fn default() -> Self {
        Self {
            targets: Vec::new(),
            jobs: 1,
            keep_going: false,
            verbose: false,
        }
    }
}

/// Result of a build run.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BuildOutcome {
    /// `true` when every selected step succeeded.
    pub success: bool,
    /// Number of steps that ran.
    pub steps_run: usize,
    /// Combined command output, in step order.
    pub output: BString,
}

/// Error that stops a build before or between steps.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum BuildError {
    /// A requested target does not exist in the graph.
    #[error("unknown target")]
    UnknownTarget(BString),
    /// The graph has a dependency cycle.
    #[error("dependency cycle")]
    Cycle,
    /// The executor is not implemented yet.
    #[error("{0} is not implemented yet")]
    Unimplemented(&'static str),
}

/// Run the steps of `graph` that `options` selects, through `host`.
pub fn execute(
    graph: &BuildGraph,
    _host: &dyn Host,
    options: &BuildOptions,
) -> Result<BuildOutcome, BuildError> {
    for name in &options.targets {
        if !graph.targets.iter().any(|t| &t.name == name) {
            return Err(BuildError::UnknownTarget(name.clone()));
        }
    }
    if graph.steps.is_empty() {
        return Ok(BuildOutcome {
            success: true,
            ..BuildOutcome::default()
        });
    }
    Err(BuildError::Unimplemented("the in-process build executor"))
}
