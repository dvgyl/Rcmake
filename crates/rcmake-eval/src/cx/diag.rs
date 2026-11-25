use rcmake_base::BString;
use rcmake_diag::{
    Backtrace, DiagCategory, DiagEngine, DiagState, Effect, FrameKind, Location, MessageKind,
    PolicyId,
};
use rcmake_model::DirId;

use super::Cx;
use crate::interp::IssueClass;
use crate::{CmdError, LogLevel};

impl Cx<'_> {
    /// Check whether to display find diagnostics for the given output variable.
    #[must_use]
    pub fn find_variable_debug(&self, name: &[u8]) -> bool {
        self.var_is_on(b"CMAKE_FIND_DEBUG_MODE")
            || self
                .interp
                .find_debug_variables
                .iter()
                .any(|item| item == name)
    }

    /// Check whether to display find diagnostics for the given package.
    #[must_use]
    pub fn find_package_debug(&self, name: &[u8]) -> bool {
        self.var_is_on(b"CMAKE_FIND_DEBUG_MODE")
            || self
                .interp
                .find_debug_packages
                .iter()
                .any(|item| item == name)
    }

    /// Report an unresolved template reference at the current command.
    pub fn warn_uninitialized(&mut self, name: &[u8]) {
        self.interp.warn_uninitialized(name, true);
    }

    /// Query a diagnostic category's active state.
    #[must_use]
    pub fn diag_state(&self) -> &DiagState {
        self.interp.diag_top()
    }

    /// Mutable access to the top of the diagnostic stack (no weak write-through; prefer
    /// [`Self::update_diag_state`]).
    pub fn diag_state_mut(&mut self) -> &mut DiagState {
        self.interp.diag_top_mut()
    }

    /// Update the diagnostic state with weak write-through (`cmake_diagnostic(SET)`).
    pub fn update_diag_state(&mut self, update: impl FnMut(&mut DiagState)) {
        self.interp.diags.update(update);
    }

    /// `cmake_diagnostic(PUSH)`.
    pub fn push_diag_scope(&mut self) {
        self.interp.diags.push_copy();
    }

    /// `cmake_diagnostic(POP)`. Issues `cmake_diagnostic POP without matching PUSH` as a
    /// fatal error when the current snapshot has no matching push.
    pub fn pop_diag_scope(&mut self) -> Result<(), CmdError> {
        self.interp.pop_diag()
    }

    /// Diagnostic engine (error flags and SARIF results).
    #[must_use]
    pub fn diag_engine(&self) -> &DiagEngine {
        &self.interp.engine
    }

    /// Issue a diagnostic at the current call site.
    ///
    /// - With a `category`, the active diagnostic state selects ignore, warning, error, or
    ///   fatal error (`IssueDiagnostic`).
    /// - Without a category, `effect` decides: [`Effect::SetError`] marks the current
    ///   command as failed and [`Effect::Fatal`] also stops processing.
    pub fn issue(
        &mut self,
        kind: MessageKind,
        effect: Effect,
        category: Option<DiagCategory>,
        body: BString,
    ) {
        self.interp.issue_here(kind, effect, category, body, true);
    }

    /// Issue a diagnostic with the caller stack of the current directory.
    pub fn issue_in_directory(
        &mut self,
        kind: MessageKind,
        effect: Effect,
        category: Option<DiagCategory>,
        body: BString,
    ) {
        let source = self.interp.current_source_dir();
        let root_file = rcmake_host::path::from_native_cmake(&source.join("CMakeLists.txt"));
        let frames: Vec<_> = self
            .interp
            .bt
            .frames()
            .take_while(|frame| {
                frame.kind != FrameKind::ModuleInclude
                    || frame.file.as_ref() != root_file.as_slice()
            })
            .collect();
        let trace = frames
            .into_iter()
            .rev()
            .fold(Backtrace::default(), |trace, frame| {
                trace.push(frame.clone())
            });
        self.interp.issue_at(
            IssueClass {
                kind,
                effect,
                category,
                policy: None,
            },
            Location::Backtrace(trace),
            body,
            true,
        );
    }

    /// Issue a diagnostic at the current backtrace of directory `dir`
    /// (`cmMakefile::IssueMessage` on the makefile of that directory).
    pub fn issue_for_directory(
        &mut self,
        dir: DirId,
        kind: MessageKind,
        effect: Effect,
        category: Option<DiagCategory>,
        body: BString,
    ) {
        let location = Location::Backtrace(self.interp.directory_backtrace(dir));
        self.interp.issue_at(
            IssueClass {
                kind,
                effect,
                category,
                policy: None,
            },
            location,
            body,
            true,
        );
    }

    /// Report an error associated with a directory's main listfile.
    pub fn issue_directory_error(&mut self, dir: DirId, body: BString) {
        let source = &self.interp.project.dir(dir).source_dir;
        let file = rcmake_host::path::from_native_cmake(
            &rcmake_host::path::to_native(source).join("CMakeLists.txt"),
        );
        self.interp.issue_at(
            IssueClass {
                kind: MessageKind::Error,
                effect: Effect::SetError,
                category: None,
                policy: None,
            },
            Location::FileLine {
                file: file.as_slice().into(),
                line: 0,
            },
            body,
            true,
        );
    }

    /// Issue the standard `Policy CMPnnnn is not set: ...` warning (`CMD_POLICY`),
    /// with optional text before and after it.
    pub fn issue_policy_warning(&mut self, id: PolicyId, preface: &[u8], postface: &[u8]) {
        self.interp.issue_policy_warning(id, preface, postface);
    }

    /// Issue a policy warning at the current backtrace of directory `dir`.
    ///
    /// CMake issues some property diagnostics through the makefile of the directory that
    /// owns the property. An active ancestor directory reports at its
    /// `add_subdirectory` call.
    pub fn issue_policy_warning_in(
        &mut self,
        dir: DirId,
        id: PolicyId,
        preface: &[u8],
        postface: &[u8],
    ) {
        let location = Location::Backtrace(self.interp.directory_backtrace(dir));
        self.interp
            .issue_policy_warning_at(id, preface, postface, location);
    }

    /// Mark processing as stopped (`cmSystemTools::SetFatalErrorOccurred`).
    pub fn set_fatal(&mut self) {
        self.interp.fatal = true;
    }

    /// `true` once a fatal error stopped processing.
    #[must_use]
    pub fn fatal_occurred(&self) -> bool {
        self.interp.fatal
    }

    /// Active message log level.
    #[must_use]
    pub fn log_level(&self) -> LogLevel {
        self.interp.log_level()
    }

    /// Issue a diagnostic but let the current command succeed (`cmMakefile::IssueMessage`
    /// followed by `return true`). A script continues after such an error.
    pub fn issue_and_continue(
        &mut self,
        kind: MessageKind,
        effect: Effect,
        category: Option<DiagCategory>,
        body: BString,
    ) {
        self.interp.issue_here(kind, effect, category, body, false);
    }

    /// Report `CMake Error: <body>` without a location and continue
    /// (`cmSystemTools::Error`). The current command does not fail.
    pub fn error_no_location(&mut self, body: BString) {
        self.interp.error_no_location(body);
    }
}
