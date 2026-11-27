//! Native find trace collector. It is separate from the configure-log record, so the
//! compatibility YAML keeps its enable rule, candidate order, and size.

use rcmake_base::BString;
use rcmake_eval::Cx;
use rcmake_eval::find_trace::{
    CallFrame, FindCandidate, FindMode, FindOutcome, FindPackageTrace, FindRequestSummary,
    FindSelection, FindTraceLimits, Rejection, SearchRoot, cut_text,
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
    trace: FindPackageTrace,
    /// The search reported the package as missing. A REQUIRED call then fails with an
    /// error, but its outcome is still "not found".
    missing: bool,
}

impl Collector {
    pub(super) fn start(cx: &mut Cx<'_>, name: &[u8], request: &Request) -> Option<Box<Self>> {
        let limits = cx.find_trace_limits()?;
        let call = cx
            .backtrace()
            .frames()
            .take(MAX_FRAMES)
            .map(|frame| CallFrame {
                file: BString::from(&*frame.file),
                line: frame.line,
                command: frame.name.as_deref().map(BString::from),
            })
            .collect();
        let dir_var = cat(&[name, b"_DIR"]);
        let dir_value_before = cx.cache().get(&dir_var).map(|entry| entry.value.clone());
        let mut config_files = request.configs.clone();
        if config_files.is_empty() {
            let names: &[BString] = &request.names;
            let names = if names.is_empty() {
                vec![BString::from(name)]
            } else {
                names.to_vec()
            };
            for name in names {
                config_files.push(cat(&[&name, b"Config.cmake"]));
                config_files.push(cat(&[&name.to_ascii_lowercase(), b"-config.cmake"]));
            }
        }
        let mode = if request.module {
            FindMode::Module
        } else if request.config_options.is_empty() {
            FindMode::Auto
        } else {
            FindMode::Config
        };
        let (components, optional_components) = request.components.iter().fold(
            (Vec::new(), Vec::new()),
            |(mut required, mut optional), (component, needed)| {
                if *needed {
                    required.push(component.clone());
                } else {
                    optional.push(component.clone());
                }
                (required, optional)
            },
        );
        let max_roots = usize::try_from(limits.max_roots).unwrap_or(usize::MAX);
        let roots = request
            .log_paths(cx)
            .into_iter()
            .take(max_roots)
            .map(|(category, enabled)| SearchRoot {
                category: category.into(),
                enabled,
            })
            .collect();
        let root_path_mode = match request
            .root_mode
            .resolve(cx, b"CMAKE_FIND_ROOT_PATH_MODE_PACKAGE")
        {
            RootMode::Default | RootMode::Both => "BOTH",
            RootMode::Never => "NEVER",
            RootMode::Only => "ONLY",
        };
        let mut collector = Self {
            missing: false,
            limits,
            trace: FindPackageTrace {
                name: name.into(),
                call,
                request: FindRequestSummary {
                    mode: Some(mode),
                    version: request.version.as_ref().map(|version| version.full.clone()),
                    exact: request.exact,
                    components,
                    optional_components,
                    required: request.required,
                    quiet: request.quiet,
                    names: request.names.clone(),
                    config_files,
                    dir_var,
                    dir_value_before,
                },
                roots,
                root_path_mode: root_path_mode.into(),
                candidates: Vec::new(),
                candidates_omitted: 0,
                fields_cut: 0,
                selected: None,
                outcome: FindOutcome::Error,
                version: None,
            },
        };
        collector.cut_request();
        Some(Box::new(collector))
    }

    fn cut(&mut self, value: &mut BString) {
        if cut_text(value, self.limits.max_detail_bytes) {
            self.trace.fields_cut = self.trace.fields_cut.saturating_add(1);
        }
    }

    fn cut_request(&mut self) {
        let mut request = std::mem::take(&mut self.trace.request);
        for value in request
            .components
            .iter_mut()
            .chain(&mut request.optional_components)
            .chain(&mut request.names)
            .chain(&mut request.config_files)
            .chain(request.version.as_mut())
            .chain(request.dir_value_before.as_mut())
        {
            self.cut(value);
        }
        self.trace.request = request;
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
        if self.trace.candidates.len() >= max {
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
        let mut path = path.clone();
        self.cut(&mut path);
        let mut message = (!message.is_empty()).then(|| message.clone());
        if let Some(message) = message.as_mut() {
            self.cut(message);
        }
        let mut found_version = detail.found_version.filter(|value| !value.is_empty());
        if let Some(version) = found_version.as_mut() {
            self.cut(version);
        }
        self.trace.candidates.push(FindCandidate {
            path,
            mode: mode.native(),
            reason,
            found_version,
            missing_components: detail.missing_components,
            detail: message,
        });
    }

    pub(super) fn selected(&mut self, file: &BString, mode: Mode, version: &BString) {
        let mut file = file.clone();
        self.cut(&mut file);
        self.trace.selected = Some(FindSelection {
            file,
            mode: mode.native(),
            version: (!version.is_empty()).then(|| version.clone()),
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
        let outcome = if outcome == FindOutcome::Error && self.missing {
            FindOutcome::NotFound
        } else {
            outcome
        };
        let mut trace = self.trace;
        if outcome == FindOutcome::Found {
            trace.version = cx
                .var(&cat(&[name, b"_VERSION"]))
                .filter(|version| !version.is_empty());
        }
        trace.outcome = outcome;
        cx.push_find_trace(trace);
    }
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
