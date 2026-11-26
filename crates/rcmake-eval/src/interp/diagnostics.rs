use std::ops::Range;
use std::path::Path;

use rcmake_base::BString;
use rcmake_diag::{
    Backtrace, DiagAction, DiagCategory, DiagState, Diagnostic, Effect, Frame, FrameKind, Location,
    MessageKind, PolicyId, SourceFile, SourceId, SourceMap,
    annotate::{Annotations, Confidence, Edit, Fix, SourceSpan},
    rich::{RichOptions, SourceLookup, render_rich},
    suggest::{Namespace, suggest_candidates_in},
};
use rcmake_host::{Stream, path::to_native};
use rcmake_model::DirId;
use rcmake_syntax::CommandInvocation;

use crate::{Interp, LogLevel};

struct EditSuggestions {
    range: Range<usize>,
    candidates: Vec<(BString, Confidence)>,
}

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

pub(crate) struct IssueClass {
    pub(crate) kind: MessageKind,
    pub(crate) effect: Effect,
    pub(crate) category: Option<DiagCategory>,
    pub(crate) policy: Option<PolicyId>,
}

impl IssueClass {
    pub(crate) fn policy(id: PolicyId) -> Self {
        Self {
            kind: MessageKind::AuthorWarning,
            effect: Effect::None,
            category: Some(DiagCategory::Policy),
            policy: Some(id),
        }
    }
}

impl Interp {
    /// Use rich formatting for diagnostics written by this interpreter.
    pub fn set_rich_diagnostics(&mut self, options: RichOptions) {
        self.engine.set_rich_options(options);
    }

    /// Borrow the source map that contains parsed listfiles for this session.
    #[must_use]
    pub fn diagnostic_sources(&self) -> &SourceMap {
        &self.sources
    }

    /// Take the source map that contains the retained diagnostic labels.
    pub fn take_diagnostic_sources(&mut self) -> SourceMap {
        std::mem::take(&mut self.sources)
    }

    /// Write an issued diagnostic to standard error.
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
        if let Some(options) = self.engine.rich_options() {
            let annotated;
            let diag = if diag.annotations.is_none()
                && let Some(annotations) = self.annotate(&diag.location, &diag.body)
            {
                annotated = Diagnostic {
                    annotations: Some(Box::new(annotations)),
                    ..diag.clone()
                };
                &annotated
            } else {
                diag
            };
            if self.engine.record(diag, state) {
                self.write_rich(diag, options);
            }
        } else if let Some(bytes) = self.engine.issue(diag, state, &self.render) {
            self.config.host.write_out(Stream::Stderr, &bytes);
        }
    }

    fn write_rich(&self, diag: &Diagnostic, options: RichOptions) {
        let lookup = DiagnosticSources {
            sources: &self.sources,
            cmake_root: &self.config.cmake_root,
        };
        let bytes = render_rich(diag, None, &lookup, options);
        self.config.host.write_out(Stream::Stderr, &bytes);
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
        self.issue_at(
            IssueClass {
                kind,
                effect,
                category,
                policy: None,
            },
            location,
            body,
            mark_failed,
        );
    }

    pub(crate) fn issue_at(
        &mut self,
        class: IssueClass,
        location: Location,
        body: BString,
        mark_failed: bool,
    ) {
        let IssueClass {
            kind,
            effect,
            category,
            policy,
        } = class;
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
        let annotations = self
            .config
            .services
            .collect_annotations
            .then(|| self.annotate(&location, &body))
            .flatten()
            .map(Box::new);
        let diag = Diagnostic {
            annotations,
            kind,
            effect,
            category,
            policy,
            location,
            body,
        };
        self.emit(&diag, &state);
    }

    fn annotate(&self, location: &Location, body: &[u8]) -> Option<Annotations> {
        let Location::Backtrace(trace) = location else {
            return None;
        };
        let frame = trace.top()?;
        let (commands, index) = self.cur_cmd.as_ref()?;
        let command = commands.get(*index)?;
        if frame.line != command.line || frame.name.as_deref() != Some(command.name.as_slice()) {
            return None;
        }
        let source = self.sources.latest_file(&frame.file)?;
        let bytes = &self.sources.get(source)?.text;
        let name_span = command.span.start..command.span.start.checked_add(command.name.len())?;
        if bytes.get(name_span.clone())? != command.name.as_slice() {
            return None;
        }

        let suggestion = self.suggestion(body, command, bytes, name_span.clone());
        let selected = suggestion
            .as_ref()
            .map_or(name_span, |suggestions| suggestions.range.clone());
        let start = u32::try_from(selected.start).ok()?;
        let end = u32::try_from(selected.end).ok()?;
        let span = SourceSpan { source, start, end };
        let mut annotations = Annotations::primary(span, "");
        if let Some(suggestions) = suggestion {
            for (name, confidence) in suggestions.candidates {
                annotations.fixes.push(Fix {
                    message: format!("Use `{name}`.").into(),
                    edits: vec![Edit {
                        span,
                        replacement: name,
                    }],
                    confidence,
                });
            }
        }
        Some(annotations)
    }

    fn suggestion(
        &self,
        body: &[u8],
        command: &CommandInvocation,
        source: &[u8],
        name_span: Range<usize>,
    ) -> Option<EditSuggestions> {
        if quoted_name(body, b"Unknown CMake command \"", b'"') == Some(command.name.as_slice()) {
            let names = self.registry.names().chain(self.scripted.keys());
            let matches = suggest_candidates_in(Namespace::Command, &command.name, names);
            return Some(EditSuggestions {
                range: name_span,
                candidates: matches,
            });
        }
        if let Some(name) = quoted_name(body, b"uninitialized variable '", b'\'') {
            let range = variable_span(command, source, name)?;
            let names = self.vars.visible_names();
            let matches = suggest_candidates_in(
                Namespace::Name,
                name,
                names
                    .iter()
                    .chain(self.project.cache.iter().map(|(key, _)| key)),
            );
            return Some(EditSuggestions {
                range,
                candidates: matches,
            });
        }
        let dir = self.current_dir_id();
        if let Some(name) = target_name(body) {
            let range = argument_span(command, source, name)?;
            let global = self.project.global_target_bindings().map(|(name, _)| name);
            let local = self
                .project
                .ancestors(dir)
                .flat_map(|id| self.project.dir(id).local_targets.keys());
            let matches = suggest_candidates_in(Namespace::Name, name, global.chain(local));
            return Some(EditSuggestions {
                range,
                candidates: matches,
            });
        }
        let unknown = quoted_name(body, b"Unknown target property \"", b'"')?;
        let target = command.args.get(1)?.text.as_slice();
        let id = self.project.find_target(dir, target)?;
        let candidate = self.project.suggest_target_property(id, unknown)?;
        Some(EditSuggestions {
            range: argument_span(command, source, unknown)?,
            candidates: vec![candidate],
        })
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
        BString::from(id.warning_text())
    }

    pub(crate) fn issue_policy_warning(&mut self, id: PolicyId, preface: &[u8], postface: &[u8]) {
        let location = Location::Backtrace(self.bt.clone());
        self.issue_policy_warning_at(id, preface, postface, location);
    }

    /// Current backtrace of directory `dir` (`cmMakefile::GetBacktrace` of that
    /// directory). An active ancestor directory is at the `add_subdirectory` call that
    /// leads to the current directory. Other directories use the current backtrace.
    pub(crate) fn directory_backtrace(&self, dir: DirId) -> Backtrace {
        let mut frames = core::iter::once(&self.root_frame)
            .chain(&self.dir_stack)
            .skip_while(|frame| frame.id != dir);
        match (frames.next(), frames.next()) {
            (Some(_), Some(child)) if child.id != dir => {
                self.project.dir(child.id).backtrace.clone()
            }
            _ => self.bt.clone(),
        }
    }

    pub(crate) fn issue_policy_warning_at(
        &mut self,
        id: PolicyId,
        preface: &[u8],
        postface: &[u8],
        location: Location,
    ) {
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
        self.issue_at(IssueClass::policy(id), location, BString::from(body), true);
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

struct DiagnosticSources<'a> {
    sources: &'a SourceMap,
    cmake_root: &'a Path,
}

impl SourceLookup for DiagnosticSources<'_> {
    fn source(&self, id: SourceId) -> Option<&SourceFile> {
        self.sources.get(id)
    }

    fn source_by_path(&self, path: &[u8]) -> Option<&SourceFile> {
        self.sources
            .latest_file(path)
            .and_then(|id| self.sources.get(id))
    }

    fn is_bundled_module(&self, path: &[u8]) -> bool {
        is_bundled_module(self.cmake_root, path)
    }
}

fn is_bundled_module(cmake_root: &Path, path: &[u8]) -> bool {
    !cmake_root.as_os_str().is_empty() && to_native(path).starts_with(cmake_root.join("Modules"))
}

impl crate::Cx<'_> {
    /// Return whether the active command comes from a bundled CMake module.
    #[must_use]
    pub fn is_bundled_module(&self) -> bool {
        self.interp
            .bt
            .top()
            .is_some_and(|frame| is_bundled_module(&self.interp.config.cmake_root, &frame.file))
    }

    /// Return whether this session collects diagnostic annotations.
    #[must_use]
    pub fn collects_diagnostic_annotations(&self) -> bool {
        self.interp.config.services.collect_annotations
            || self.interp.engine.rich_options().is_some()
    }

    /// Supply advice for a successful command that can still contain a typo.
    pub fn advise(&mut self, body: BString) {
        if !self.collects_diagnostic_annotations() {
            return;
        }
        let interp = &mut *self.interp;
        let location = Location::Backtrace(display_backtrace(&interp.bt));
        let annotations = interp.annotate(&location, &body).map(Box::new);
        let diag = Diagnostic {
            kind: MessageKind::Log,
            effect: Effect::None,
            category: None,
            policy: None,
            location,
            body,
            annotations,
        };
        interp.engine.record_advice(&diag);
        if let Some(options) = interp.engine.rich_options() {
            interp.write_rich(&diag, options);
        }
    }
}

fn quoted_name<'a>(body: &'a [u8], prefix: &[u8], end: u8) -> Option<&'a [u8]> {
    let tail = body.strip_prefix(prefix)?;
    let name = tail.split(|&byte| byte == end).next()?;
    (name.len() < tail.len()).then_some(name)
}

fn target_name(body: &[u8]) -> Option<&[u8]> {
    if let Some(name) = quoted_name(
        body,
        b"get_target_property() called with non-existent target \"",
        b'"',
    ) {
        return Some(name);
    }
    let body = body.strip_prefix(b"Cannot specify ")?;
    let prefix = b" for target \"";
    let start = body
        .windows(prefix.len())
        .position(|bytes| bytes == prefix)?;
    quoted_name(body.get(start..)?, prefix, b'"')
}

fn variable_span(command: &CommandInvocation, source: &[u8], name: &[u8]) -> Option<Range<usize>> {
    let token = [b"${".as_slice(), name, b"}"].concat();
    let token = token.as_slice();
    let mut spans = command.args.iter().flat_map(|arg| {
        source
            .get(arg.span.clone())
            .into_iter()
            .flat_map(move |bytes| {
                bytes
                    .windows(token.len())
                    .enumerate()
                    .filter(move |(_, raw)| *raw == token)
                    .map(move |(offset, _)| arg.span.start + offset + 2)
            })
    });
    let start = spans.next().filter(|_| spans.next().is_none())?;
    Some(start..start + name.len())
}

fn argument_span(command: &CommandInvocation, source: &[u8], name: &[u8]) -> Option<Range<usize>> {
    let mut spans = command.args.iter().filter_map(|arg| {
        let raw = source.get(arg.span.clone())?;
        if raw == name {
            Some(arg.span.clone())
        } else if raw
            .strip_prefix(b"\"")
            .and_then(|text| text.strip_suffix(b"\""))
            == Some(name)
        {
            Some(arg.span.start + 1..arg.span.end - 1)
        } else {
            None
        }
    });
    spans.next().filter(|_| spans.next().is_none())
}

/// Format the backtrace in CMake order. Start with the top frame, then include
/// only command frames. File frames
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
