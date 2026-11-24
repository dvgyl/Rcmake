//! Optional settings for an evaluator session.

#![forbid(unsafe_code)]

use core::num::NonZeroUsize;
use std::collections::BTreeSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use rcmake_base::BString;
use rcmake_diag::SourceId;

/// Access mode for the probe cache.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ProbeCacheMode {
    /// Disable the cache.
    #[default]
    Off,
    /// Read and record cache entries.
    ReadWrite,
    /// Read existing cache entries.
    ReadOnly,
    /// Read entries and verify their referenced data.
    Verify,
}

/// Access mode for the dependency fetch lockfile.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum FetchLockMode {
    /// Ignore the lockfile.
    #[default]
    Off,
    /// Use locked revisions and record newly resolved dependencies.
    Auto,
    /// Resolve dependencies again and update the lockfile.
    Update,
    /// Require a matching lockfile entry for each dependency.
    Frozen,
}

/// Handling of undeclared inputs to a configure.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum HermeticMode {
    /// Disable hermetic input checks.
    #[default]
    Off,
    /// Warn about undeclared inputs.
    Warn,
    /// Reject undeclared inputs.
    Strict,
}

/// Requested configure acceleration and input checks.
#[derive(Clone, Debug, Default)]
pub struct SpeedOptions {
    /// Probe cache access mode.
    pub probe_cache: ProbeCacheMode,
    /// Requested limit for concurrent probe jobs.
    pub probe_jobs: Option<NonZeroUsize>,
    /// Enable incremental configure.
    pub incremental: bool,
    /// Fetch lockfile access mode.
    pub fetch_lock: FetchLockMode,
    /// Enable prefetch for dependencies.
    pub fetch_prefetch: bool,
    /// Requested limit for concurrent fetch jobs.
    pub fetch_jobs: Option<NonZeroUsize>,
    /// Handling of undeclared configure inputs.
    pub hermetic: HermeticMode,
}

/// A shared cancellation flag for one session.
#[derive(Clone, Debug, Default)]
pub struct CancelToken(Arc<AtomicBool>);

impl CancelToken {
    /// Request cancellation on this token and all its clones.
    pub fn cancel(&self) {
        self.0.store(true, Ordering::Release);
    }

    /// Return whether cancellation was requested.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }
}

/// Select which variable histories to collect.
#[derive(Clone, Debug, Default)]
pub enum LedgerLevel {
    /// Collect no variable history.
    #[default]
    Off,
    /// Collect only the listed variable names.
    Names(BTreeSet<BString>),
    /// Collect recent events for all names.
    Full {
        /// Maximum number of events retained for each variable name.
        per_name_cap: u32,
    },
}

/// Optional services shared by evaluator commands.
#[derive(Clone, Debug, Default)]
pub struct Services {
    /// Requested configure acceleration and input checks.
    pub speed: SpeedOptions,
    /// Collect source spans, labels, and suggested fixes for diagnostics.
    pub collect_annotations: bool,
    /// Variable history to collect.
    pub provenance: LedgerLevel,
    /// Shared cancellation flag, when supplied by the caller.
    pub cancel: Option<CancelToken>,
    /// Maximum command count for the session, when specified.
    pub step_budget: Option<u64>,
}

/// Position of a parsed command within a listfile or virtual source.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Site {
    /// Registered source that contains the command.
    pub source: SourceId,
    /// Zero-based command index within the source.
    pub cmd: u32,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cancellation_is_shared_by_clones_only() {
        let token = CancelToken::default();
        let worker = token.clone();
        let independent = CancelToken::default();
        assert!(!token.is_cancelled());
        worker.cancel();
        assert!(token.is_cancelled());
        assert!(!independent.is_cancelled());
        token.cancel();
        assert!(token.is_cancelled());
    }

    #[test]
    fn services_default_to_disabled() {
        let services = Services::default();
        assert_eq!(services.speed.probe_cache, ProbeCacheMode::Off);
        assert_eq!(services.speed.fetch_lock, FetchLockMode::Off);
        assert_eq!(services.speed.hermetic, HermeticMode::Off);
        assert_eq!(services.speed.probe_jobs, None);
        assert_eq!(services.speed.fetch_jobs, None);
        assert!(!services.speed.fetch_prefetch);
        assert!(!services.speed.incremental);
        assert!(!services.collect_annotations);
        assert!(matches!(services.provenance, LedgerLevel::Off));
        assert!(services.cancel.is_none());
        assert_eq!(services.step_budget, None);
    }
}
