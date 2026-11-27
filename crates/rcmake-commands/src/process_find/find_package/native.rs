//! Native find trace collector. It is separate from the configure-log record, so the
//! compatibility YAML keeps its enable rule, candidate order, and size.
//!
//! The collector reserves the call identity when the call starts. It charges the
//! byte budget of the session before it copies a field, and it caps every list.

use rcmake_base::BString;
use rcmake_eval::Cx;
use rcmake_eval::find_trace::{
    CANDIDATE_COST, CallFrame, FRAME_COST, FindCandidate, FindMode, FindOutcome, FindPackageTrace,
    FindRequestSummary, FindSelection, FindTraceLimits, FindTraceTicket, ROOT_COST, Rejection,
    SELECTION_COST, SearchRoot, TraceBudget, cut_len, text_cost,
};

use super::super::RootMode;
use super::log::{Mode, Reason};
use super::{Request, cat};

const MAX_FRAMES: usize = 16;

/// Extra facts about a rejected candidate.
#[derive(Default)]
pub(super) struct Detail {
    pub(super) found_version: Option<BString>,
    pub(super) unsuitable: bool,
    pub(super) missing_components: Vec<BString>,
}

pub(super) struct Collector {
    limits: FindTraceLimits,
    budget: TraceBudget,
    /// The fixed part of the record fit in the budget. A record without it is omitted.
    recorded: bool,
    trace: FindPackageTrace,
    /// Charge of the current selection, refunded when a later selection replaces it.
    selection_cost: u64,
    /// The search reported the package as missing. A REQUIRED call then fails with an
    /// error, but its outcome is still "not found".
    missing: bool,
}

impl Collector {
    /// Reserve the identity of a call that starts now. Call this before the call can fail.
    pub(super) fn begin(cx: &mut Cx<'_>, name: &[u8]) -> Option<Box<Self>> {
        let FindTraceTicket {
            ordinal,
            package_call,
            limits,
            budget,
        } = cx.begin_find_trace(name)?;
        let mut collector = Self {
            limits,
            budget,
            recorded: false,
            selection_cost: 0,
            missing: false,
            trace: FindPackageTrace {
                ordinal,
                package_call,
                name: BString::default(),
                call: Vec::new(),
                request: FindRequestSummary::default(),
                roots: Vec::new(),
                root_path_mode: BString::default(),
                candidates: Vec::new(),
                candidates_omitted: 0,
                fields_cut: 0,
                items_omitted: 0,
                selected: None,
                outcome: FindOutcome::Error,
                version: None,
            },
        };
        let name_len = cut_len(name, limits.max_detail_bytes);
        let dir_var_len = name_len.saturating_add(4);
        collector.recorded = collector.budget.charge(
            rcmake_eval::find_trace::CALL_COST
                .saturating_add(text_cost(name_len))
                .saturating_add(text_cost(dir_var_len))
                .saturating_add(text_cost(8)),
        );
        if !collector.recorded {
            return Some(Box::new(collector));
        }
        collector.trace.name = collector.prefix(name, name_len);
        collector.trace.request.dir_var = cat(&[&collector.trace.name, b"_DIR"]);
        collector.trace.root_path_mode = "BOTH".into();
        let frames: Vec<CallFrame> = cx
            .backtrace()
            .frames()
            .take(MAX_FRAMES)
            .map_while(|frame| {
                let command = frame.name.as_deref().unwrap_or_default();
                let file_len = cut_len(&frame.file, limits.max_detail_bytes);
                let command_len = cut_len(command, limits.max_detail_bytes);
                let cost = FRAME_COST
                    .saturating_add(text_cost(file_len))
                    .saturating_add(text_cost(command_len));
                if !collector.budget.charge(cost) {
                    return None;
                }
                Some(CallFrame {
                    file: collector.prefix(&frame.file, file_len),
                    line: frame.line,
                    command: frame
                        .name
                        .as_deref()
                        .map(|name| collector.prefix(name, command_len)),
                })
            })
            .collect();
        let total = cx.backtrace().frames().take(MAX_FRAMES).count();
        collector.omit_items(total.saturating_sub(frames.len()));
        collector.trace.call = frames;
        Some(Box::new(collector))
    }

    /// Record the request options. Call this once the request is known.
    pub(super) fn describe(&mut self, cx: &mut Cx<'_>, name: &[u8], request: &Request) {
        if !self.recorded {
            return;
        }
        let dir_var = self.trace.request.dir_var.clone();
        let limit = self.limits.max_detail_bytes;
        let dir_value_before = cx.cache().get(&cat(&[name, b"_DIR"])).map(|entry| {
            let len = cut_len(&entry.value, limit);
            let prefix = BString::from(entry.value.get(..len).unwrap_or_default());
            (prefix, len < entry.value.len())
        });
        let dir_value_before = dir_value_before.and_then(|(value, cut)| {
            let value = self.text(&value)?;
            if cut {
                self.trace.fields_cut = self.trace.fields_cut.saturating_add(1);
            }
            Some(value)
        });
        let mut config_files = Vec::new();
        if request.configs.is_empty() {
            let names: Vec<&[u8]> = if request.names.is_empty() {
                vec![name]
            } else {
                request.names.iter().map(|name| name.as_slice()).collect()
            };
            for name in names {
                let len = cut_len(name, self.limits.max_detail_bytes);
                let prefix = name.get(..len).unwrap_or_default();
                for suffix in [b"Config.cmake".as_slice(), b"-config.cmake"] {
                    if config_files.len() >= self.max_items()
                        || !self
                            .budget
                            .charge(text_cost(len.saturating_add(suffix.len())))
                    {
                        self.omit_items(1);
                        continue;
                    }
                    let file = if suffix == b"Config.cmake" {
                        cat(&[prefix, suffix])
                    } else {
                        cat(&[&prefix.to_ascii_lowercase(), suffix])
                    };
                    config_files.push(file);
                }
                if len < name.len() {
                    self.trace.fields_cut = self.trace.fields_cut.saturating_add(1);
                }
            }
        } else {
            config_files = self.list(request.configs.iter().map(|value| value.as_slice()));
        }
        let mode = if request.module {
            FindMode::Module
        } else if request.config_options.is_empty() {
            FindMode::Auto
        } else {
            FindMode::Config
        };
        let components = self.list(
            request
                .components
                .iter()
                .filter(|(_, needed)| *needed)
                .map(|(component, _)| component.as_slice()),
        );
        let optional_components = self.list(
            request
                .components
                .iter()
                .filter(|(_, needed)| !*needed)
                .map(|(component, _)| component.as_slice()),
        );
        let names = self.list(request.names.iter().map(|value| value.as_slice()));
        let version = request
            .version
            .as_ref()
            .and_then(|version| self.text(&version.full));
        let paths = request.log_paths(cx);
        let max_roots = usize::try_from(self.limits.max_roots).unwrap_or(usize::MAX);
        let mut roots = Vec::new();
        for (category, enabled) in &paths {
            if roots.len() >= max_roots
                || !self
                    .budget
                    .charge(ROOT_COST.saturating_add(text_cost(category.len())))
            {
                self.omit_items(1);
                continue;
            }
            roots.push(SearchRoot {
                category: (*category).into(),
                enabled: *enabled,
            });
        }
        self.trace.root_path_mode = match request
            .root_mode
            .resolve(cx, b"CMAKE_FIND_ROOT_PATH_MODE_PACKAGE")
        {
            RootMode::Default | RootMode::Both => "BOTH",
            RootMode::Never => "NEVER",
            RootMode::Only => "ONLY",
        }
        .into();
        self.trace.roots = roots;
        self.trace.request = FindRequestSummary {
            mode: Some(mode),
            version,
            exact: request.exact,
            components,
            optional_components,
            required: request.required,
            quiet: request.quiet,
            names,
            config_files,
            dir_var,
            dir_value_before,
        };
    }

    fn max_items(&self) -> usize {
        usize::try_from(self.limits.max_list_items).unwrap_or(usize::MAX)
    }

    fn omit_items(&mut self, count: usize) {
        let count = u32::try_from(count).unwrap_or(u32::MAX);
        self.trace.items_omitted = self.trace.items_omitted.saturating_add(count);
    }

    /// Copy the first `len` bytes of `value`, and count a cut field.
    fn prefix(&mut self, value: &[u8], len: usize) -> BString {
        if len < value.len() {
            self.trace.fields_cut = self.trace.fields_cut.saturating_add(1);
        }
        BString::from(value.get(..len).unwrap_or_default())
    }

    /// Charge and copy one text field. Return `None` and count it when it does not fit.
    fn text(&mut self, value: &[u8]) -> Option<BString> {
        let len = cut_len(value, self.limits.max_detail_bytes);
        if !self.budget.charge(text_cost(len)) {
            self.omit_items(1);
            return None;
        }
        Some(self.prefix(value, len))
    }

    /// Charge and copy list items up to the list and byte limits.
    fn list<'a>(&mut self, values: impl Iterator<Item = &'a [u8]>) -> Vec<BString> {
        let mut out = Vec::new();
        for value in values {
            if out.len() >= self.max_items() {
                self.omit_items(1);
                continue;
            }
            if let Some(value) = self.text(value) {
                out.push(value);
            }
        }
        out
    }

    pub(super) fn candidate(
        &mut self,
        path: &BString,
        mode: Mode,
        reason: Reason,
        message: &BString,
        detail: Detail,
    ) {
        let max = usize::try_from(self.limits.max_candidates).unwrap_or(usize::MAX);
        let limit = self.limits.max_detail_bytes;
        let path_len = cut_len(path, limit);
        let message_len = cut_len(message, limit);
        let found_version = detail.found_version.filter(|value| !value.is_empty());
        let version_len = found_version
            .as_ref()
            .map(|version| cut_len(version, limit));
        let base = CANDIDATE_COST
            .saturating_add(text_cost(path_len))
            .saturating_add(if message.is_empty() {
                0
            } else {
                text_cost(message_len)
            })
            .saturating_add(version_len.map_or(0, text_cost));
        if !self.recorded || self.trace.candidates.len() >= max || !self.budget.charge(base) {
            self.trace.candidates_omitted = self.trace.candidates_omitted.saturating_add(1);
            return;
        }
        let reason = match reason {
            Reason::NoExist => Rejection::NoFile,
            Reason::Ignored => Rejection::Ignored,
            Reason::InsufficientVersion if detail.unsuitable => Rejection::VersionUnsuitable,
            Reason::InsufficientVersion => Rejection::VersionMismatch,
            Reason::InsufficientComponents => Rejection::ComponentsMissing,
            Reason::Error => Rejection::Error,
            Reason::NotFound => Rejection::NotFound,
        };
        let path = self.prefix(path, path_len);
        let message = (!message.is_empty()).then(|| self.prefix(message, message_len));
        let found_version = found_version
            .zip(version_len)
            .map(|(version, len)| self.prefix(&version, len));
        let missing_components = self.list(
            detail
                .missing_components
                .iter()
                .map(|component| component.as_slice()),
        );
        self.trace.candidates.push(FindCandidate {
            path,
            mode: mode.native(),
            reason,
            found_version,
            missing_components,
            detail: message,
        });
    }

    pub(super) fn selected(&mut self, file: &BString, mode: Mode, version: &BString) {
        if !self.recorded {
            return;
        }
        let limit = self.limits.max_detail_bytes;
        let file_len = cut_len(file, limit);
        let version_len = cut_len(version, limit);
        let cost = SELECTION_COST
            .saturating_add(text_cost(file_len))
            .saturating_add(if version.is_empty() {
                0
            } else {
                text_cost(version_len)
            });
        let previous = std::mem::take(&mut self.selection_cost);
        self.budget.refund(previous);
        if !self.budget.charge(cost) {
            self.trace.selected = None;
            self.omit_items(1);
            return;
        }
        self.selection_cost = cost;
        let file = self.prefix(file, file_len);
        let version = (!version.is_empty()).then(|| self.prefix(version, version_len));
        self.trace.selected = Some(FindSelection {
            file,
            mode: mode.native(),
            version,
        });
    }

    pub(super) fn module_search(&mut self, considered: &[std::path::PathBuf]) {
        for path in considered {
            let path = rcmake_host::path::from_native_cmake(path);
            self.candidate(
                &path,
                Mode::Module,
                Reason::NoExist,
                &BString::default(),
                Detail::default(),
            );
        }
    }

    pub(super) fn mark_missing(&mut self) {
        self.missing = true;
    }

    /// Finish the record with the call result and add it to the session store.
    pub(super) fn finish(self: Box<Self>, cx: &mut Cx<'_>, name: &[u8], outcome: FindOutcome) {
        let Self {
            mut budget,
            recorded,
            mut trace,
            missing,
            ..
        } = *self;
        if !recorded {
            cx.omit_find_trace();
            return;
        }
        let outcome = if outcome == FindOutcome::Error && missing {
            FindOutcome::NotFound
        } else {
            outcome
        };
        if outcome == FindOutcome::Found
            && let Some(version) = cx
                .var(&cat(&[name, b"_VERSION"]))
                .filter(|version| !version.is_empty())
        {
            let len = cut_len(&version, budget_limit(cx));
            if budget.charge(text_cost(len)) {
                if len < version.len() {
                    trace.fields_cut = trace.fields_cut.saturating_add(1);
                }
                trace.version = Some(BString::from(version.get(..len).unwrap_or_default()));
            } else {
                trace.items_omitted = trace.items_omitted.saturating_add(1);
            }
        }
        trace.outcome = outcome;
        cx.push_find_trace(trace, budget);
    }
}

fn budget_limit(cx: &Cx<'_>) -> u32 {
    cx.find_trace_limits()
        .map_or(0, |limits| limits.max_detail_bytes)
}

impl Mode {
    pub(super) const fn native(self) -> FindMode {
        match self {
            Self::Any => FindMode::Auto,
            Self::Module => FindMode::Module,
            Self::Config => FindMode::Config,
            Self::Cps => FindMode::Cps,
            Self::Provider => FindMode::Provider,
        }
    }
}
