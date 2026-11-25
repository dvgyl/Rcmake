use rcmake_base::BString;
use rcmake_diag::{
    Backtrace, DiagAction, DiagCategory, DiagState, Diagnostic, Effect, Frame, FrameKind, Location,
    MessageKind, PolicyId,
};
use rcmake_host::Stream;

use crate::util::cat;
use crate::{Interp, LogLevel};

/// Map a warning kind to the matching error kind.
fn error_kind(kind: MessageKind) -> MessageKind {
    match kind {
        MessageKind::Warning | MessageKind::Log => MessageKind::Error,
        MessageKind::AuthorWarning => MessageKind::AuthorError,
        MessageKind::DeprecationWarning => MessageKind::DeprecationError,
        other => other,
    }
}

/// Map an error kind to the matching warning kind.
fn warning_kind(kind: MessageKind) -> MessageKind {
    match kind {
        MessageKind::Error | MessageKind::InternalError => MessageKind::Warning,
        MessageKind::AuthorError => MessageKind::AuthorWarning,
        MessageKind::DeprecationError => MessageKind::DeprecationWarning,
        other => other,
    }
}

impl Interp {
    /// Write a rendered diagnostic to stderr through the [`rcmake_diag::DiagEngine`].
    pub(crate) fn emit(&mut self, diag: &Diagnostic, state: &DiagState) {
        let shown;
        let diag = if let Location::Backtrace(bt) = &diag.location {
            shown = Diagnostic {
                location: Location::Backtrace(display_backtrace(bt)),
                ..diag.clone()
            };
            &shown
        } else {
            diag
        };
        if let Some(bytes) = self.engine.issue(diag, state, &self.render) {
            self.config.host.write_out(Stream::Stderr, &bytes);
        }
    }

    /// Issue a diagnostic at the current backtrace.
    ///
    /// `mark_failed` marks the current command as failed when the effective severity is
    /// an error (`cmMakefile::IssueMessage` / `IssueDiagnostic`). `message()` passes
    /// `false` because it displays through the messenger directly.
    pub(crate) fn issue_here(
        &mut self,
        kind: MessageKind,
        effect: Effect,
        category: Option<DiagCategory>,
        body: BString,
        mark_failed: bool,
    ) {
        let location = if self
            .project
            .global
            .props
            .get(b"\0RCMAKE_IN_RULE_CONFIGURATOR")
            .is_some()
        {
            let directory = &self.project.dir(self.current_dir_id()).source_dir;
            let file = rcmake_base::path_lex::Lexical::new(directory).append(b"CMakeLists.txt");
            Location::FileLine {
                file: file.as_slice().into(),
                line: 0,
            }
        } else {
            Location::Backtrace(self.bt.clone())
        };
        self.issue_at(kind, effect, category, None, location, body, mark_failed);
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn issue_at(
        &mut self,
        kind: MessageKind,
        effect: Effect,
        category: Option<DiagCategory>,
        policy: Option<PolicyId>,
        location: Location,
        body: BString,
        mark_failed: bool,
    ) {
        let state = *self.diag_top();
        let (kind, effect) = match category {
            Some(cat) => match state.get(cat) {
                DiagAction::Ignore => return,
                DiagAction::Warn => (warning_kind(kind), Effect::None),
                DiagAction::SendError => (error_kind(kind), Effect::SetError),
                DiagAction::FatalError => (error_kind(kind), Effect::Fatal),
            },
            None => (kind, effect),
        };
        match effect {
            Effect::None => {}
            Effect::SetError => {
                if mark_failed {
                    self.cmd_failed = true;
                }
            }
            Effect::Fatal => {
                self.fatal = true;
                if mark_failed {
                    self.cmd_failed = true;
                }
            }
        }
        let diag = Diagnostic {
            annotations: None,
            kind,
            effect,
            category,
            policy,
            location,
            body,
        };
        self.emit(&diag, &state);
    }

    /// `IssueMessage(FATAL_ERROR, text)`: an error that fails the current command.
    pub(crate) fn error_here(&mut self, body: BString) {
        self.issue_here(MessageKind::Error, Effect::SetError, None, body, true);
    }

    /// `IssueMessage(FATAL_ERROR, text)` plus `SetFatalErrorOccurred`.
    pub(crate) fn fatal_here(&mut self, body: BString) {
        self.issue_here(MessageKind::Error, Effect::Fatal, None, body, true);
    }

    /// `cmSystemTools::Error(text)`: `CMake Error: <text>`.
    pub(crate) fn error_no_location(&mut self, body: BString) {
        let state = *self.diag_top();
        let diag = Diagnostic {
            annotations: None,
            kind: MessageKind::Error,
            effect: Effect::SetError,
            category: None,
            policy: None,
            location: Location::None,
            body,
        };
        self.emit(&diag, &state);
    }

    /// `cmPolicies::GetPolicyWarning`.
    pub(crate) fn policy_warning_text(id: PolicyId) -> BString {
        let summary = if id == PolicyId::new(82) {
            b"Install rules from add_subdirectory() are interleaved with those in caller.".to_vec()
        } else if id == PolicyId::new(218) {
            b"The CMAKE_WARN_DEPRECATED and CMAKE_ERROR_DEPRECATED variables are ignored.".to_vec()
        } else {
            id.info()
                .map(|i| crate::util::replace_all(i.summary.as_bytes(), b"``", b""))
                .unwrap_or_default()
        };
        let name = id.to_string();
        cat(&[
            b"Policy ",
            name.as_bytes(),
            b" is not set: ",
            &summary,
            b"  Run \"cmake --help-policy ",
            name.as_bytes(),
            b"\" for policy details.  Use the cmake_policy command to set the policy and suppress this warning.",
        ])
    }

    pub(crate) fn issue_policy_warning(&mut self, id: PolicyId, preface: &[u8], postface: &[u8]) {
        let warning = Self::policy_warning_text(id);
        let mut body = Vec::new();
        if !preface.is_empty() {
            body.extend_from_slice(preface);
            body.push(b'\n');
        }
        body.extend_from_slice(&warning);
        if !postface.is_empty() {
            body.push(b'\n');
            body.extend_from_slice(postface);
        }
        let location = Location::Backtrace(self.bt.clone());
        self.issue_at(
            MessageKind::AuthorWarning,
            Effect::None,
            Some(DiagCategory::Policy),
            Some(id),
            location,
            BString::from(body),
            true,
        );
    }

    pub(crate) fn log_level(&self) -> LogLevel {
        if let Some(level) = self.log_level {
            return level;
        }
        self.lookup_var(b"CMAKE_MESSAGE_LOG_LEVEL")
            .and_then(|v| LogLevel::parse(v))
            .unwrap_or(LogLevel::Status)
    }
}

/// Backtrace as CMake prints it: the top frame, then only command frames. File frames
/// (`FrameKind::ModuleInclude`) locate file-level diagnostics (`in <file>:`) but do not
/// appear in the `Call Stack` list.
pub(crate) fn display_backtrace(bt: &Backtrace) -> Backtrace {
    let mut frames = bt.frames();
    let Some(top) = frames.next() else {
        return Backtrace::default();
    };
    let callers: Vec<&Frame> = frames
        .filter(|f| f.kind != FrameKind::ModuleInclude)
        .collect();
    if callers.len() + 1 == bt.frames().count() {
        return bt.clone();
    }
    let mut out = Backtrace::default();
    for frame in callers.into_iter().rev() {
        out = out.push(frame.clone());
    }
    out.push(top.clone())
}
