//! Bounded records of `find_package()` searches for native `rcmake` diagnostics.
//!
//! Compatibility personas never enable collection. The records do not feed the
//! configure log or `--debug-find` output.

#![forbid(unsafe_code)]

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use rcmake_base::BString;

use crate::Cx;

/// How much `find_package()` search data a session keeps.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum FindTraceLevel {
    /// Keep nothing. Compatibility personas always use this level.
    #[default]
    Off,
    /// Keep bounded records.
    Collect(FindTraceLimits),
}

/// Bounds for the records of one session.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FindTraceLimits {
    /// Most `find_package()` calls kept.
    pub max_calls: u32,
    /// Most search roots kept per call.
    pub max_roots: u32,
    /// Most candidates kept per call.
    pub max_candidates: u32,
    /// Longest kept text field in bytes.
    pub max_detail_bytes: u32,
    /// Most items kept in one list field, such as components or call frames.
    pub max_list_items: u32,
    /// Largest serialized size of all active and kept calls together in bytes.
    ///
    /// The collector charges each field against this limit before it copies the
    /// field. The charge is an upper bound of the compact JSON size of the field.
    pub max_total_bytes: u64,
}

impl Default for FindTraceLimits {
    fn default() -> Self {
        Self {
            max_calls: 512,
            max_roots: 256,
            max_candidates: 128,
            max_detail_bytes: 2048,
            max_list_items: 256,
            max_total_bytes: 8 << 20,
        }
    }
}

/// Search mode of a request, a candidate, or a selection.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FindMode {
    /// Module mode first, then config mode.
    Auto,
    /// `Find<Pkg>.cmake` module mode.
    Module,
    /// `<Pkg>Config.cmake` config mode.
    Config,
    /// Common Package Specification file.
    Cps,
    /// Dependency provider.
    Provider,
}

impl FindMode {
    /// Stable lowercase name used in reports.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Module => "module",
            Self::Config => "config",
            Self::Cps => "cps",
            Self::Provider => "provider",
        }
    }
}

/// Why a search step did not accept a candidate.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Rejection {
    /// The directory had no package file.
    NoFile,
    /// The version file reported an incompatible version.
    VersionMismatch,
    /// The version file set `PACKAGE_VERSION_UNSUITABLE`, for example for another architecture.
    VersionUnsuitable,
    /// A required component is missing.
    ComponentsMissing,
    /// `CMAKE_IGNORE_PATH` or a related variable excluded the path.
    Ignored,
    /// The package file ran and reported the package as not found.
    NotFound,
    /// The package file could not be read or raised an error.
    Error,
}

impl Rejection {
    /// Stable lowercase name used in reports.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::NoFile => "no_file",
            Self::VersionMismatch => "version_mismatch",
            Self::VersionUnsuitable => "version_unsuitable",
            Self::ComponentsMissing => "components_missing",
            Self::Ignored => "ignored",
            Self::NotFound => "not_found",
            Self::Error => "error",
        }
    }
}

/// One tested path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FindCandidate {
    /// File that was tested, or the directory that was checked.
    pub path: BString,
    /// Search mode of the candidate.
    pub mode: FindMode,
    /// Why the search did not accept the candidate.
    pub reason: Rejection,
    /// `PACKAGE_VERSION` that the version file reported.
    pub found_version: Option<BString>,
    /// Required components that the package did not provide.
    pub missing_components: Vec<BString>,
    /// Message from the package, cut at the detail limit.
    pub detail: Option<BString>,
}

/// File that the search selected before the package script ran.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FindSelection {
    /// Selected file, or the provider command.
    pub file: BString,
    /// Search mode of the selection.
    pub mode: FindMode,
    /// Version that the selection reported, when known.
    pub version: Option<BString>,
}

/// Final result of one `find_package()` call.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FindOutcome {
    /// The package was found.
    Found,
    /// The package was not found. A selection can exist when its script set `<Pkg>_FOUND` false.
    NotFound,
    /// `CMAKE_DISABLE_FIND_PACKAGE_<Pkg>` disabled the call.
    Disabled,
    /// The call, a provider, or a package script failed with an error.
    Error,
}

impl FindOutcome {
    /// Stable lowercase name used in reports.
    #[must_use]
    pub const fn label(&self) -> &'static str {
        match self {
            Self::Found => "found",
            Self::NotFound => "not_found",
            Self::Disabled => "disabled",
            Self::Error => "error",
        }
    }
}

/// Request options of one call.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FindRequestSummary {
    /// Requested mode.
    pub mode: Option<FindMode>,
    /// Version text as written, or a range such as `1.2...<2`.
    pub version: Option<BString>,
    /// `EXACT` was given.
    pub exact: bool,
    /// Required components.
    pub components: Vec<BString>,
    /// Optional components.
    pub optional_components: Vec<BString>,
    /// The call is required.
    pub required: bool,
    /// `QUIET` was given.
    pub quiet: bool,
    /// Package names searched.
    pub names: Vec<BString>,
    /// Package file names searched.
    pub config_files: Vec<BString>,
    /// `<Pkg>_DIR` variable name.
    pub dir_var: BString,
    /// Cached `<Pkg>_DIR` value when the call started.
    pub dir_value_before: Option<BString>,
}

/// Root category and its use in one call.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SearchRoot {
    /// Category name, for example `CMAKE_PREFIX_PATH`.
    pub category: BString,
    /// The call searched this category.
    pub enabled: bool,
}

/// Call site frame.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CallFrame {
    /// Listfile path.
    pub file: BString,
    /// 1-based line, or 0.
    pub line: u32,
    /// Command name as written, when known.
    pub command: Option<BString>,
}

/// Record of one `find_package()` call.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FindPackageTrace {
    /// 1-based position of the call among all calls of the session, in entry order.
    pub ordinal: u32,
    /// 1-based position of the call among the calls for this package, in entry
    /// order. `None` when the session cannot count the calls for the package.
    pub package_call: Option<u32>,
    /// Package name.
    pub name: BString,
    /// Call stack, innermost first, at most 16 frames.
    pub call: Vec<CallFrame>,
    /// Request options.
    pub request: FindRequestSummary,
    /// Root categories in search order with their enable state.
    pub roots: Vec<SearchRoot>,
    /// `CMAKE_FIND_ROOT_PATH_MODE_PACKAGE` mode: `BOTH`, `NEVER`, or `ONLY`.
    pub root_path_mode: BString,
    /// Tested paths in search order.
    pub candidates: Vec<FindCandidate>,
    /// Number of candidates past the limit.
    pub candidates_omitted: u32,
    /// Text fields that the limit cut.
    pub fields_cut: u32,
    /// List items and text fields left out at a count or byte limit.
    pub items_omitted: u32,
    /// File that the search selected, before the package script ran.
    pub selected: Option<FindSelection>,
    /// Final result of the call.
    pub outcome: FindOutcome,
    /// Final `<Pkg>_VERSION` value, when the package was found.
    pub version: Option<BString>,
}

/// Upper bound of the compact JSON size of one text field or list item.
///
/// The JSON writer can write one input byte as six output bytes, for example
/// `\u0001`. The constant part covers the quotes, the separator, and the key.
#[must_use]
pub const fn text_cost(len: usize) -> u64 {
    (len as u64).saturating_mul(6).saturating_add(32)
}

/// Upper bound of the JSON size of one call record without its text and lists.
pub const CALL_COST: u64 = 1024;
/// Upper bound of the JSON size of one candidate without its text and lists.
pub const CANDIDATE_COST: u64 = 256;
/// Upper bound of the JSON size of one call frame without its text.
pub const FRAME_COST: u64 = 96;
/// Upper bound of the JSON size of one search root without its text.
pub const ROOT_COST: u64 = 64;
/// Upper bound of the JSON size of one selection without its text.
pub const SELECTION_COST: u64 = 96;

fn opt_cost(value: Option<&BString>) -> u64 {
    value.map_or(0, |value| text_cost(value.len()))
}

fn list_cost(values: &[BString]) -> u64 {
    values.iter().fold(0, |total: u64, value| {
        total.saturating_add(text_cost(value.len()))
    })
}

impl FindPackageTrace {
    /// Upper bound of the compact JSON size of this record in bytes.
    #[must_use]
    pub fn estimated_bytes(&self) -> u64 {
        let request = &self.request;
        let mut total = CALL_COST
            .saturating_add(text_cost(self.name.len()))
            .saturating_add(text_cost(self.root_path_mode.len()))
            .saturating_add(text_cost(request.dir_var.len()))
            .saturating_add(opt_cost(request.version.as_ref()))
            .saturating_add(opt_cost(request.dir_value_before.as_ref()))
            .saturating_add(list_cost(&request.components))
            .saturating_add(list_cost(&request.optional_components))
            .saturating_add(list_cost(&request.names))
            .saturating_add(list_cost(&request.config_files))
            .saturating_add(opt_cost(self.version.as_ref()));
        for frame in &self.call {
            total = total
                .saturating_add(FRAME_COST)
                .saturating_add(text_cost(frame.file.len()))
                .saturating_add(opt_cost(frame.command.as_ref()));
        }
        for root in &self.roots {
            total = total
                .saturating_add(ROOT_COST)
                .saturating_add(text_cost(root.category.len()));
        }
        for candidate in &self.candidates {
            total = total.saturating_add(candidate_cost(candidate));
        }
        if let Some(selected) = &self.selected {
            total = total.saturating_add(selection_cost(selected));
        }
        total
    }
}

/// Upper bound of the compact JSON size of one candidate.
#[must_use]
pub fn candidate_cost(candidate: &FindCandidate) -> u64 {
    CANDIDATE_COST
        .saturating_add(text_cost(candidate.path.len()))
        .saturating_add(list_cost(&candidate.missing_components))
        .saturating_add(opt_cost(candidate.found_version.as_ref()))
        .saturating_add(opt_cost(candidate.detail.as_ref()))
}

/// Upper bound of the compact JSON size of one selection.
#[must_use]
pub fn selection_cost(selected: &FindSelection) -> u64 {
    SELECTION_COST
        .saturating_add(text_cost(selected.file.len()))
        .saturating_add(opt_cost(selected.version.as_ref()))
}

/// Records of a session, in entry order of the calls.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FindTraceStore {
    /// Kept calls, sorted by [`FindPackageTrace::ordinal`].
    pub calls: Vec<FindPackageTrace>,
    /// Number of calls that started. The ordinals `1..=calls_started` exist.
    pub calls_started: u32,
    /// Started calls that a count or byte limit left out. Their outcome is unknown.
    pub omitted_calls: u32,
    /// Upper bound of the serialized size of the kept calls.
    pub bytes: u64,
}

impl FindTraceStore {
    /// Add a finished record and keep the store sorted by ordinal.
    ///
    /// The store keeps the charge of `budget` when it keeps the record. When the
    /// call limit is full, the store counts the call as omitted and `budget`
    /// returns its charge to the session ledger.
    pub fn push(
        &mut self,
        trace: FindPackageTrace,
        mut budget: TraceBudget,
        limits: &FindTraceLimits,
    ) {
        let max = usize::try_from(limits.max_calls).unwrap_or(usize::MAX);
        if self.calls.len() >= max {
            self.omitted_calls = self.omitted_calls.saturating_add(1);
            return;
        }
        self.bytes = self.bytes.saturating_add(budget.keep());
        let index = self
            .calls
            .partition_point(|call| call.ordinal < trace.ordinal);
        self.calls.insert(index, trace);
    }

    /// Count a call that started but left no record.
    pub fn omit(&mut self) {
        self.omitted_calls = self.omitted_calls.saturating_add(1);
    }

    /// Started calls with neither a kept record nor an omitted count, for example a
    /// call that a fatal error stopped before it finished.
    #[must_use]
    pub fn unfinished_calls(&self) -> u32 {
        let kept = u32::try_from(self.calls.len()).unwrap_or(u32::MAX);
        self.calls_started
            .saturating_sub(kept)
            .saturating_sub(self.omitted_calls)
    }

    /// Return whether every started call has a kept record.
    #[must_use]
    pub fn all_calls_kept(&self) -> bool {
        self.omitted_calls == 0 && self.unfinished_calls() == 0
    }
}

/// Most package names for which a session counts calls.
const MAX_COUNTED_PACKAGES: usize = 4096;

/// Session state that is shared by the active records of one session.
#[derive(Debug, Default)]
struct FindTraceSession {
    /// Bytes charged by active and kept records.
    used: Arc<AtomicU64>,
    /// Calls for each package name, for [`FindPackageTrace::package_call`].
    per_package: BTreeMap<BString, u32>,
}

/// Byte allowance of one active record.
///
/// Each charge goes to the session ledger at once, so active nested records count
/// against the same limit. A budget that is dropped before the store keeps it
/// returns its charge.
#[derive(Debug)]
pub struct TraceBudget {
    used: Arc<AtomicU64>,
    max: u64,
    charged: u64,
    exhausted: bool,
}

impl TraceBudget {
    /// Make a budget with its own ledger, for tests and standalone records.
    #[must_use]
    pub fn standalone(max: u64) -> Self {
        Self {
            used: Arc::default(),
            max,
            charged: 0,
            exhausted: false,
        }
    }

    /// Charge `bytes` when the session ledger has room. Return whether it did.
    ///
    /// The first failed charge marks the budget as exhausted, and every later
    /// charge fails too. The record then keeps its fields as they are.
    pub fn charge(&mut self, bytes: u64) -> bool {
        if self.exhausted {
            return false;
        }
        let max = self.max;
        let charged = self
            .used
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |used| {
                used.checked_add(bytes).filter(|total| *total <= max)
            })
            .is_ok();
        if charged {
            self.charged = self.charged.saturating_add(bytes);
        } else {
            self.exhausted = true;
        }
        charged
    }

    /// Return `bytes` of an earlier charge, for example for a replaced field.
    pub fn refund(&mut self, bytes: u64) {
        let bytes = bytes.min(self.charged);
        self.charged -= bytes;
        let _ = self
            .used
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |used| {
                Some(used.saturating_sub(bytes))
            });
    }

    /// Return whether a charge failed.
    #[must_use]
    pub const fn exhausted(&self) -> bool {
        self.exhausted
    }

    /// Keep the charge in the ledger for good and return it.
    fn keep(&mut self) -> u64 {
        std::mem::take(&mut self.charged)
    }
}

impl Drop for TraceBudget {
    fn drop(&mut self) {
        let charged = self.charged;
        self.refund(charged);
    }
}

/// Identity and allowance of one call, reserved when the call starts.
#[derive(Debug)]
pub struct FindTraceTicket {
    /// See [`FindPackageTrace::ordinal`].
    pub ordinal: u32,
    /// See [`FindPackageTrace::package_call`].
    pub package_call: Option<u32>,
    /// Limits of the session.
    pub limits: FindTraceLimits,
    /// Byte allowance of the record.
    pub budget: TraceBudget,
}

/// Return the length of the prefix of `value` that fits in `limit` bytes and
/// ends on a UTF-8 boundary.
#[must_use]
pub fn cut_len(value: &[u8], limit: u32) -> usize {
    let limit = usize::try_from(limit).unwrap_or(usize::MAX);
    if value.len() <= limit {
        return value.len();
    }
    let mut end = limit;
    while end > 0 && value.get(end).is_some_and(|byte| byte & 0xc0 == 0x80) {
        end -= 1;
    }
    end
}

/// Cut `value` at `limit` bytes on a UTF-8 boundary. Return whether it was cut.
pub fn cut_text(value: &mut BString, limit: u32) -> bool {
    let end = cut_len(value, limit);
    if end == value.len() {
        return false;
    }
    value.truncate(end);
    true
}

impl Cx<'_> {
    /// Return the collection limits when this session keeps find traces.
    #[must_use]
    pub fn find_trace_limits(&self) -> Option<FindTraceLimits> {
        match self.services().find_trace {
            FindTraceLevel::Off => None,
            FindTraceLevel::Collect(limits) => Some(limits),
        }
    }

    /// Reserve the identity of a call that starts now.
    ///
    /// Call this once when a `find_package()` call starts, before the call can fail.
    /// Return `None` when the session keeps no find traces.
    pub fn begin_find_trace(&mut self, name: &[u8]) -> Option<FindTraceTicket> {
        let limits = self.find_trace_limits()?;
        let store = self.session_data_mut_or_init::<FindTraceStore>()?;
        store.calls_started = store.calls_started.saturating_add(1);
        let ordinal = store.calls_started;
        let session = self.session_data_mut_or_init::<FindTraceSession>()?;
        let countable = name.len() <= usize::try_from(limits.max_detail_bytes).unwrap_or(0);
        let package_call = if !countable {
            None
        } else if let Some(count) = session.per_package.get_mut(name) {
            *count = count.saturating_add(1);
            Some(*count)
        } else if session.per_package.len() < MAX_COUNTED_PACKAGES {
            session.per_package.insert(name.into(), 1);
            Some(1)
        } else {
            None
        };
        let budget = TraceBudget {
            used: Arc::clone(&session.used),
            max: limits.max_total_bytes,
            charged: 0,
            exhausted: false,
        };
        Some(FindTraceTicket {
            ordinal,
            package_call,
            limits,
            budget,
        })
    }

    /// Add one finished call record and the charge of its budget to the session store.
    pub fn push_find_trace(&mut self, trace: FindPackageTrace, budget: TraceBudget) {
        let Some(limits) = self.find_trace_limits() else {
            return;
        };
        if let Some(store) = self.session_data_mut_or_init::<FindTraceStore>() {
            store.push(trace, budget, &limits);
        }
    }

    /// Count a started call that leaves no record.
    pub fn omit_find_trace(&mut self) {
        if self.find_trace_limits().is_some()
            && let Some(store) = self.session_data_mut_or_init::<FindTraceStore>()
        {
            store.omit();
        }
    }
}

impl crate::Interp {
    /// Remove and return the find traces that this interpreter collected.
    ///
    /// Later calls start a new store with new ordinals and a new byte ledger.
    pub fn take_find_traces(&mut self) -> FindTraceStore {
        if let Some(session) = self.session_data_mut_or_init::<FindTraceSession>() {
            *session = FindTraceSession::default();
        }
        self.session_data_mut_or_init::<FindTraceStore>()
            .map(std::mem::take)
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn trace(ordinal: u32, name: &str, candidates: usize) -> FindPackageTrace {
        FindPackageTrace {
            ordinal,
            package_call: Some(1),
            name: name.into(),
            call: Vec::new(),
            request: FindRequestSummary::default(),
            roots: Vec::new(),
            root_path_mode: "BOTH".into(),
            candidates: (0..candidates)
                .map(|index| FindCandidate {
                    path: format!("/p/{index}/{name}Config.cmake").into(),
                    mode: FindMode::Config,
                    reason: Rejection::NoFile,
                    found_version: None,
                    missing_components: Vec::new(),
                    detail: None,
                })
                .collect(),
            candidates_omitted: 0,
            fields_cut: 0,
            items_omitted: 0,
            selected: None,
            outcome: FindOutcome::NotFound,
            version: None,
        }
    }

    fn charged(ledger: &Arc<AtomicU64>, max: u64, trace: &FindPackageTrace) -> TraceBudget {
        let mut budget = TraceBudget {
            used: Arc::clone(ledger),
            max,
            charged: 0,
            exhausted: false,
        };
        budget.charge(trace.estimated_bytes());
        budget
    }

    #[test]
    fn store_enforces_call_limit_and_sorts_by_entry_order() {
        let limits = FindTraceLimits {
            max_calls: 2,
            ..FindTraceLimits::default()
        };
        let ledger = Arc::default();
        let mut store = FindTraceStore {
            calls_started: 3,
            ..FindTraceStore::default()
        };
        for (ordinal, name) in [(2, "Inner"), (1, "Outer"), (3, "Late")] {
            let trace = trace(ordinal, name, 1);
            let budget = charged(&ledger, u64::MAX, &trace);
            store.push(trace, budget, &limits);
        }
        let names: Vec<_> = store.calls.iter().map(|call| call.ordinal).collect();
        assert_eq!(names, [1, 2]);
        assert_eq!(store.omitted_calls, 1);
        assert!(!store.all_calls_kept());
        assert_eq!(ledger.load(Ordering::Relaxed), store.bytes);
    }

    #[test]
    fn budget_counts_active_records_and_refunds_dropped_ones() {
        let ledger = Arc::new(AtomicU64::new(0));
        let mut outer = TraceBudget {
            used: Arc::clone(&ledger),
            max: 100,
            charged: 0,
            exhausted: false,
        };
        let mut inner = TraceBudget {
            used: Arc::clone(&ledger),
            max: 100,
            charged: 0,
            exhausted: false,
        };
        assert!(outer.charge(60));
        assert!(!inner.charge(50), "the active outer record holds 60 bytes");
        assert!(inner.exhausted());
        assert!(!inner.charge(1), "an exhausted budget stays exhausted");
        drop(outer);
        assert_eq!(ledger.load(Ordering::Relaxed), 0);
        assert!(!inner.charge(0));
    }

    #[test]
    fn estimate_bounds_escaped_text() {
        let mut call = trace(1, "A", 1);
        call.name = BString::from(vec![1_u8; 100]);
        assert!(call.estimated_bytes() >= 600);
    }

    #[test]
    fn cut_text_keeps_utf8_boundaries() {
        let mut value = BString::from("aé");
        assert!(cut_text(&mut value, 2));
        assert_eq!(value, "a");
        let mut short = BString::from("ok");
        assert!(!cut_text(&mut short, 8));
    }
}
