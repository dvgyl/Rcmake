//! Bounded records of `find_package()` searches for native `rcmake` diagnostics.
//!
//! Compatibility personas never enable collection. The records do not feed the
//! configure log or `--debug-find` output.

#![forbid(unsafe_code)]

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
    /// Largest estimated serialized size of all kept calls together in bytes.
    pub max_total_bytes: u64,
}

impl Default for FindTraceLimits {
    fn default() -> Self {
        Self {
            max_calls: 512,
            max_roots: 256,
            max_candidates: 128,
            max_detail_bytes: 2048,
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
    /// File that the search selected, before the package script ran.
    pub selected: Option<FindSelection>,
    /// Final result of the call.
    pub outcome: FindOutcome,
    /// Final `<Pkg>_VERSION` value, when the package was found.
    pub version: Option<BString>,
}

impl FindPackageTrace {
    /// Estimated serialized size in bytes.
    #[must_use]
    pub fn estimated_bytes(&self) -> u64 {
        let text = |value: &[u8]| value.len() as u64 + 8;
        let list = |values: &[BString]| values.iter().map(|value| text(value)).sum::<u64>();
        let request = &self.request;
        let mut total = 512 + text(&self.name) + text(&self.root_path_mode);
        total += self
            .call
            .iter()
            .map(|frame| text(&frame.file) + 48)
            .sum::<u64>();
        total += list(&request.components)
            + list(&request.optional_components)
            + list(&request.names)
            + list(&request.config_files)
            + request.version.as_deref().map_or(0, |value| text(value))
            + request
                .dir_value_before
                .as_deref()
                .map_or(0, |value| text(value))
            + text(&request.dir_var);
        total += self
            .roots
            .iter()
            .map(|root| text(&root.category) + 24)
            .sum::<u64>();
        total += self
            .candidates
            .iter()
            .map(|candidate| {
                128 + text(&candidate.path)
                    + list(&candidate.missing_components)
                    + candidate
                        .found_version
                        .as_deref()
                        .map_or(0, |value| text(value))
                    + candidate.detail.as_deref().map_or(0, |value| text(value))
            })
            .sum::<u64>();
        total
            + self
                .selected
                .as_ref()
                .map_or(0, |selected| 64 + text(&selected.file))
    }
}

/// Records of a session, in call order.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FindTraceStore {
    /// Kept calls.
    pub calls: Vec<FindPackageTrace>,
    /// Calls past the count or byte limit. Their outcome is unknown.
    pub omitted_calls: u32,
    /// Estimated serialized size of the kept calls.
    pub bytes: u64,
}

impl FindTraceStore {
    /// Add `trace` when it fits in `limits`. Count it as omitted otherwise.
    pub fn push(&mut self, trace: FindPackageTrace, limits: &FindTraceLimits) {
        let bytes = trace.estimated_bytes();
        let fits = self.calls.len() < usize::try_from(limits.max_calls).unwrap_or(usize::MAX)
            && self.bytes.saturating_add(bytes) <= limits.max_total_bytes;
        if fits {
            self.bytes += bytes;
            self.calls.push(trace);
        } else {
            self.omitted_calls = self.omitted_calls.saturating_add(1);
        }
    }
}

/// Cut `value` at `limit` bytes on a UTF-8 boundary. Return whether it was cut.
pub fn cut_text(value: &mut BString, limit: u32) -> bool {
    let limit = usize::try_from(limit).unwrap_or(usize::MAX);
    if value.len() <= limit {
        return false;
    }
    let mut end = limit;
    while end > 0 && value.get(end).is_some_and(|byte| byte & 0xc0 == 0x80) {
        end -= 1;
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

    /// Add one finished call record to the session store.
    pub fn push_find_trace(&mut self, trace: FindPackageTrace) {
        let Some(limits) = self.find_trace_limits() else {
            return;
        };
        if let Some(store) = self.session_data_mut_or_init::<FindTraceStore>() {
            store.push(trace, &limits);
        }
    }
}

impl crate::Interp {
    /// Remove and return the find traces that this interpreter collected.
    pub fn take_find_traces(&mut self) -> FindTraceStore {
        self.session_data_mut_or_init::<FindTraceStore>()
            .map(std::mem::take)
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn trace(name: &str, candidates: usize) -> FindPackageTrace {
        FindPackageTrace {
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
            selected: None,
            outcome: FindOutcome::NotFound,
            version: None,
        }
    }

    #[test]
    fn store_enforces_call_and_byte_budgets() {
        let limits = FindTraceLimits {
            max_calls: 2,
            ..FindTraceLimits::default()
        };
        let mut store = FindTraceStore::default();
        for name in ["A", "B", "C"] {
            store.push(trace(name, 1), &limits);
        }
        assert_eq!(store.calls.len(), 2);
        assert_eq!(store.omitted_calls, 1);

        let tight = FindTraceLimits {
            max_total_bytes: trace("A", 10).estimated_bytes() + 10,
            ..FindTraceLimits::default()
        };
        let mut store = FindTraceStore::default();
        store.push(trace("A", 10), &tight);
        store.push(trace("B", 10), &tight);
        assert_eq!(store.calls.len(), 1);
        assert_eq!(store.omitted_calls, 1);
        assert!(store.bytes <= tight.max_total_bytes);
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
