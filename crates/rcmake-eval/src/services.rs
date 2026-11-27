//! Optional settings for an evaluator session.

#![forbid(unsafe_code)]

use core::num::NonZeroUsize;
use std::any::{Any, TypeId};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use rcmake_base::BString;
use rcmake_diag::SourceId;
use rcmake_expr::ExpandedArg;

use crate::{Cx, Interp, SharedParseCache};

pub(crate) struct ExpandedObserver {
    pub(crate) names: &'static [&'static [u8]],
    pub(crate) notify: fn(&mut Cx<'_>, &[ExpandedArg]),
}
pub(crate) type SessionData = BTreeMap<TypeId, Box<dyn Any + Send + Sync>>;

impl Interp {
    /// Install data for commands in this interpreter.
    pub fn add_session_data<T: Any + Send + Sync>(&mut self, value: T) {
        let boxed: Box<dyn Any + Send + Sync> = Box::new(value);
        if let Some(ctx) = boxed.downcast_ref::<crate::hermetic::HermeticContext>() {
            self.hermetic = Some(ctx.clone());
        } else if let Some(sources) = boxed.downcast_ref::<crate::hermetic::HermeticSources>() {
            self.hermetic_sources = Some(sources.clone());
        }
        self.session_data.insert(TypeId::of::<T>(), boxed);
    }

    /// Get the session data of type `T`.
    #[must_use]
    pub fn session_data<T: Any + Send + Sync>(&self) -> Option<&T> {
        self.session_data.get(&TypeId::of::<T>())?.downcast_ref()
    }

    /// Get or lazily initialize mutable session data of type `T`.
    pub fn session_data_mut_or_init<T: Any + Send + Sync + Default>(&mut self) -> Option<&mut T> {
        self.session_data
            .entry(TypeId::of::<T>())
            .or_insert_with(|| Box::new(T::default()))
            .downcast_mut::<T>()
    }

    /// Install a hook for the listed lowercase command names.
    /// The hook runs after the command's arguments expand.
    pub fn set_expanded_observer(
        &mut self,
        names: &'static [&'static [u8]],
        notify: fn(&mut Cx<'_>, &[ExpandedArg]),
    ) {
        self.expanded_observer = Some(ExpandedObserver { names, notify });
    }

    /// Retain parsed listfiles for reuse in this interpreter.
    pub fn enable_parse_cache(&mut self) {
        self.parse_cache.get_or_insert_with(Default::default);
    }

    /// Share parsed listfiles with other interpreters in this configure session.
    pub fn set_parse_cache(&mut self, cache: Arc<SharedParseCache>) {
        self.parse_cache = Some(cache);
    }

    /// Return whether tracing, command profiling, or a debugger is active.
    #[must_use]
    pub fn has_command_observers(&self) -> bool {
        self.trace.is_some() || self.command_profiler.is_some() || self.has_debugger()
    }
}

impl Cx<'_> {
    /// Get the cache for parsed listfiles in this configure session.
    #[must_use]
    pub fn parse_cache(&self) -> Option<&Arc<SharedParseCache>> {
        self.interp.parse_cache.as_ref()
    }
}

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
    /// `find_package()` records for native diagnostics. Compatibility personas keep `Off`.
    pub find_trace: crate::FindTraceLevel,
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
        assert_eq!(services.find_trace, crate::FindTraceLevel::Off);
    }
}
