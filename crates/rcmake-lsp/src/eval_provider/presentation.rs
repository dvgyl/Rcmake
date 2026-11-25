use rcmake_api::{
    CallableDefinition, CompletionState, ConfigureResult, SourceLocation, TargetDefinition,
    VariableDefinition,
};
use rcmake_diag::{Location, MessageKind};
use rcmake_host::path::to_native;
use rcmake_json::Value;

use crate::{
    Position,
    catalog::escape_markdown,
    json::{object, uint},
    position::range,
    uri,
};

use super::{Buffers, EvalSemanticProvider, Snapshot, SnapshotReply, buffer_path, read_file};

const MAX_DIAGNOSTICS: usize = 256;
const MAX_HOVER_SECTION: usize = 4096;
const DENIED_HINT: &str =
    "The editor cannot run this command. Run a local configure to get the result.";

impl EvalSemanticProvider {
    /// Get the current configure diagnostics for a file URI.
    ///
    /// If the session denied a process request, add one information item to the
    /// top-level CMakeLists.txt. Diagnostics from a previous edit are omitted.
    #[must_use]
    pub fn diagnostics(&self, document_uri: &str) -> Vec<Value> {
        self.diagnostics_reply(document_uri)
            .and_then(|reply| reply.current(&self.snapshots))
            .unwrap_or_default()
    }

    /// Compute configure diagnostics that can be delivered on another thread.
    #[must_use]
    pub fn diagnostics_reply(&self, document_uri: &str) -> Option<SnapshotReply<Vec<Value>>> {
        let path = uri::path(document_uri)?;
        self.snapshots.query(|snapshot, buffers| {
            if !snapshot.fresh(buffers) {
                return Vec::new();
            }
            self.query_diagnostics(document_uri, &path, snapshot, buffers)
        })
    }

    fn query_diagnostics(
        &self,
        document_uri: &str,
        path: &std::path::Path,
        snapshot: &Snapshot,
        buffers: &Buffers,
    ) -> Vec<Value> {
        let path = buffer_path(self.host(), path);
        let root = buffer_path(
            self.host(),
            &self.snapshots.project_root().join("CMakeLists.txt"),
        );
        let result = &snapshot.result;
        let mut items = Vec::new();
        if path == root
            && matches!(&result.completion, CompletionState::Partial {
                denied_processes, omitted_denials, ..
            } if !denied_processes.is_empty() || *omitted_denials > 0)
        {
            items.push(diagnostic(0, 3, DENIED_HINT));
        }
        for entry in &result.diagnostics {
            let site = match &entry.location {
                Location::FileLine { file, line } => Some((file.as_ref(), *line)),
                Location::Backtrace(trace) => {
                    trace.top().map(|frame| (frame.file.as_ref(), frame.line))
                }
                Location::None => None,
            };
            let line = if let Some((file, line)) = site.filter(|(file, _)| !file.is_empty()) {
                let file = self.snapshots.project_root().join(to_native(file));
                if buffer_path(self.host(), &file) != path {
                    continue;
                }
                let matches = buffers.by_uri.get(document_uri).map_or_else(
                    || {
                        read_file(self.host(), &file)
                            .is_ok_and(|text| snapshot.matches(&file, &text))
                    },
                    |buffer| snapshot.matches(&file, buffer.document.text.as_bytes()),
                );
                if !matches {
                    continue;
                }
                line.saturating_sub(1) as usize
            } else if path == root {
                0
            } else {
                continue;
            };
            if buffers
                .by_uri
                .get(document_uri)
                .is_some_and(|buffer| buffer.document.lines.start(line).is_none())
            {
                continue;
            }
            let severity = match entry.kind {
                MessageKind::Error
                | MessageKind::InternalError
                | MessageKind::AuthorError
                | MessageKind::DeprecationError => 1,
                MessageKind::Log => 3,
                _ => 2,
            };
            items.push(diagnostic(line, severity, &preview(&entry.body, 4096)));
            if items.len() == MAX_DIAGNOSTICS {
                break;
            }
        }
        items
    }
}

fn diagnostic(line: usize, severity: usize, text: &str) -> Value {
    let position = Position { line, character: 0 };
    object([
        ("range", range(position, position)),
        ("severity", uint(severity)),
        ("source", Value::string("rcmake configure")),
        ("message", Value::string(text)),
    ])
}

pub(super) enum ValueAt<'a> {
    WriteSite,
    Directory(&'a [u8]),
    FinalCache,
}

pub(super) enum Binding<'a> {
    Target(&'a TargetDefinition),
    Callable(&'a CallableDefinition),
    Variable {
        name: &'a [u8],
        value: &'a [u8],
        origin: Option<&'a SourceLocation>,
        cached: bool,
        value_at: ValueAt<'a>,
    },
}

impl<'a> Binding<'a> {
    pub(super) fn variable(
        name: &'a [u8],
        value: &'a VariableDefinition,
        value_at: ValueAt<'a>,
    ) -> Self {
        Self::Variable {
            name,
            value: &value.value,
            origin: value.location.as_ref(),
            cached: value.cached,
            value_at,
        }
    }

    pub(super) fn snapshot_variable(
        name: &'a [u8],
        value: &'a VariableDefinition,
        directory: &'a [u8],
    ) -> Self {
        let value_at = if value.cached {
            ValueAt::FinalCache
        } else {
            ValueAt::Directory(directory)
        };
        Self::variable(name, value, value_at)
    }

    pub(super) fn origin(&self) -> Option<&'a SourceLocation> {
        match self {
            Self::Target(target) => target.location.as_ref(),
            Self::Callable(command) => Some(&command.location),
            Self::Variable { origin, .. } => *origin,
        }
    }

    pub(super) fn completion_kind(&self) -> usize {
        match self {
            Self::Target(_) => 7,       // Class
            Self::Callable(_) => 3,     // Function
            Self::Variable { .. } => 6, // Variable
        }
    }

    pub(super) fn detail(&self) -> String {
        match self {
            Self::Target(target) => target.alias_of.as_ref().map_or_else(
                || target.kind.as_str().to_owned(),
                |name| format!("{} alias for {}", target.kind.as_str(), single_line(name)),
            ),
            Self::Callable(command) => signature(command),
            Self::Variable { value, cached, .. } => {
                let kind = if *cached {
                    "Cache variable"
                } else {
                    "Variable"
                };
                format!("{kind} = {}", single_line(value))
            }
        }
    }

    pub(super) fn hover(&self, result: &ConfigureResult, markdown: bool) -> String {
        let mut text = match self {
            Self::Target(target) => {
                let mut text = format!(
                    "{}\n\n{}",
                    code(&single_line(&target.name), markdown),
                    prose(&self.detail(), markdown),
                );
                if let Some(real) = result.model.target(target.id) {
                    let groups: [(&str, &[&str]); 2] = [
                        (
                            "Compile definitions",
                            &["COMPILE_DEFINITIONS", "INTERFACE_COMPILE_DEFINITIONS"],
                        ),
                        (
                            "Include directories",
                            &[
                                "INCLUDE_DIRECTORIES",
                                "INTERFACE_INCLUDE_DIRECTORIES",
                                "INTERFACE_SYSTEM_INCLUDE_DIRECTORIES",
                            ],
                        ),
                    ];
                    for (label, properties) in groups {
                        let values = real
                            .usage
                            .fields
                            .iter()
                            .filter(|(field, _)| properties.contains(&field.name()))
                            .flat_map(|(_, entries)| {
                                entries.iter().map(|entry| entry.value.as_slice())
                            });
                        append_list(&mut text, label, values, markdown);
                    }
                    let links = real
                        .usage
                        .link_libraries
                        .iter()
                        .chain(&real.usage.interface_link_libraries)
                        .flatten()
                        .map(|entry| entry.text.value.as_slice());
                    append_list(&mut text, "Link libraries", links, markdown);
                } else {
                    append_list(
                        &mut text,
                        "Link libraries",
                        target.declared_links.iter().map(|link| link.as_slice()),
                        markdown,
                    );
                }
                text
            }
            Self::Callable(command) => {
                let mut text = code(&signature(command), markdown);
                if let Some(documentation) = &command.documentation {
                    text.push_str("\n\n");
                    text.push_str(&prose(&preview(documentation, 1024), markdown));
                }
                text
            }
            Self::Variable {
                name,
                value,
                cached,
                value_at,
                ..
            } => {
                let kind = if *cached {
                    "Cache variable"
                } else {
                    "Variable"
                };
                let label = match value_at {
                    ValueAt::WriteSite => kind.to_owned(),
                    ValueAt::Directory(dir) => {
                        format!(
                            "{kind}\n\nValue at end of directory {}",
                            prose(&single_line(dir), markdown)
                        )
                    }
                    ValueAt::FinalCache => "Cache value at end of evaluation".to_owned(),
                };
                format!(
                    "{}\n\n{label}",
                    code(
                        &format!("{} = {}", single_line(name), preview(value, 1024)),
                        markdown
                    )
                )
            }
        };
        if let Some(origin) = self.origin() {
            let path = single_line(&origin.path);
            let location = if origin.line == 0 {
                path
            } else {
                format!("{path}:{}", origin.line)
            };
            text.push_str("\n\nDefined at ");
            text.push_str(&prose(&location, markdown));
        }
        // Each section opens with its only code fence. The snippet and both
        // fence markers fit before this limit, even for an all-backtick value.
        clip(&mut text, MAX_HOVER_SECTION);
        text
    }
}

fn signature(command: &CallableDefinition) -> String {
    // UserCommandKind belongs to the evaluator. Its two variants are Function and Macro.
    let kind = format!("{:?}", command.kind).to_ascii_lowercase();
    let mut text = format!("{kind}({}", single_line(&command.name));
    for parameter in command.parameters.iter().take(16) {
        text.push(' ');
        text.push_str(&single_line(parameter));
    }
    if command.parameters.len() > 16 {
        text.push_str(" …");
    }
    clip(&mut text, 1023);
    text.push(')');
    text
}

fn append_list<'a>(
    text: &mut String,
    label: &str,
    values: impl Iterator<Item = &'a [u8]>,
    markdown: bool,
) {
    let mut values = values.peekable();
    if values.peek().is_none() {
        return;
    }
    if markdown {
        text.push_str(&format!("\n\n**{label}**"));
    } else {
        text.push_str(&format!("\n\n{label}"));
    }
    for value in values.by_ref().take(8) {
        text.push_str("\n- ");
        text.push_str(&prose(&single_line(value), markdown));
    }
    if values.next().is_some() {
        text.push_str("\n- …");
    }
}

fn prose(text: &str, markdown: bool) -> String {
    if markdown {
        escape_markdown(text)
            .replace('_', "\\_")
            .replace('.', "\\.")
    } else {
        text.to_owned()
    }
}

fn code(text: &str, markdown: bool) -> String {
    if !markdown {
        return text.to_owned();
    }
    let longest = text
        .as_bytes()
        .split(|&byte| byte != b'`')
        .map(<[u8]>::len)
        .max()
        .unwrap_or_default();
    let fence = "`".repeat((longest + 1).max(3));
    format!("{fence}cmake\n{text}\n{fence}")
}

fn single_line(bytes: &[u8]) -> String {
    preview(bytes, 160).replace(['\r', '\n'], " ")
}

fn preview(bytes: &[u8], limit: usize) -> String {
    let prefix = bytes.get(..limit.saturating_add(3)).unwrap_or(bytes);
    let mut text = String::from_utf8_lossy(prefix).into_owned();
    clip(&mut text, limit);
    text
}

fn clip(text: &mut String, limit: usize) {
    if text.len() > limit {
        text.truncate(text.floor_char_boundary(limit - '…'.len_utf8()));
        text.push('…');
    }
}

impl EvalSemanticProvider {
    pub(super) fn typed_diagnostics(
        &self,
        document: &crate::SemanticDocument<'_>,
        selection: Option<std::ops::Range<Position>>,
    ) -> Option<(Vec<crate::SemanticDiagnostic>, Vec<Value>)> {
        let path = uri::path(document.uri)?;
        self.snapshots
            .query(|snapshot, buffers| {
                let open = buffers.by_uri.get(document.uri)?;
                if !snapshot.fresh(buffers)
                    || open.document.version != document.version
                    || open.document.text != document.text
                    || open.path != buffer_path(self.host(), &path)
                {
                    return None;
                }
                let mut fallback = rcmake_diag::SourceMap::default();
                let fallback = fallback.add(
                    "".into(),
                    rcmake_diag::source::SourceKind::File,
                    std::sync::Arc::from(b"".as_slice()),
                )?;
                let root = buffer_path(
                    self.host(),
                    &self.snapshots.project_root().join("CMakeLists.txt"),
                );
                let result = &snapshot.result;
                let mut diagnostics = Vec::new();
                let mut actions = Vec::new();
                if open.path == root
                    && matches!(&result.completion, CompletionState::Partial {
                    denied_processes, omitted_denials, ..
                } if !denied_processes.is_empty() || *omitted_denials > 0)
                {
                    diagnostics.push(crate::SemanticDiagnostic::new(
                        rcmake_diag::Diagnostic {
                            kind: MessageKind::Log,
                            effect: rcmake_diag::Effect::None,
                            category: None,
                            policy: None,
                            location: Location::None,
                            body: DENIED_HINT.into(),
                            annotations: None,
                        },
                        fallback,
                        Default::default(),
                    ));
                }
                for entry in &result.diagnostics {
                    if let Some(item) =
                        typed_diagnostic(self, entry, snapshot, buffers, open, &root, fallback)
                    {
                        if let Some(range) = selection.as_ref() {
                            actions.extend(advice_actions(&item, open, document.encoding, range));
                            if actions.len() >= MAX_DIAGNOSTICS {
                                actions.truncate(MAX_DIAGNOSTICS);
                                break;
                            }
                        }
                        diagnostics.push(item);
                        if diagnostics.len() == MAX_DIAGNOSTICS {
                            break;
                        }
                    }
                }
                Some((diagnostics, actions))
            })?
            .current(&self.snapshots)?
    }
}

fn typed_diagnostic(
    provider: &EvalSemanticProvider,
    diagnostic: &rcmake_diag::Diagnostic,
    snapshot: &Snapshot,
    buffers: &Buffers,
    open: &super::Buffer,
    root: &std::path::Path,
    fallback: rcmake_diag::SourceId,
) -> Option<crate::SemanticDiagnostic> {
    let file = match &diagnostic.location {
        Location::FileLine { file, .. } => Some(file.as_ref()),
        Location::Backtrace(trace) => trace.top().map(|frame| frame.file.as_ref()),
        Location::None => None,
    };
    if let Some(file) = file.filter(|file| !file.is_empty()) {
        let path = provider.snapshots.project_root().join(to_native(file));
        if buffer_path(provider.host(), &path) != open.path
            || !snapshot.matches(&path, open.document.text.as_bytes())
        {
            return None;
        }
    } else if open.path != root {
        return None;
    }

    let mut retained = diagnostic.clone();
    let annotations = retained.annotations.take();
    let mut item = crate::SemanticDiagnostic::new(retained, fallback, Default::default());
    let Some((annotations, sources)) = annotations.zip(snapshot.result.facts.source_map.as_ref())
    else {
        return Some(item);
    };
    let known = |id| {
        let file = sources.get(id)?;
        let path = to_native(&file.path);
        let buffer = buffers.at(provider.host(), &path)?;
        (file.text.as_ref() == buffer.document.text.as_bytes()).then_some(buffer)
    };
    let Some(primary) = annotations.labels.iter().find(|label| {
        label.primary && known(label.span.source).is_some_and(|buffer| buffer.uri == open.uri)
    }) else {
        return Some(item);
    };
    item.source = primary.span.source;
    item.annotations = *annotations;
    for source in item
        .annotations
        .fixes
        .iter()
        .flat_map(|fix| &fix.edits)
        .map(|edit| edit.span.source)
        .collect::<std::collections::BTreeSet<_>>()
    {
        if source != item.source
            && let Some(buffer) = known(source)
        {
            item.related_sources.push(crate::SemanticSource {
                source,
                uri: buffer.uri.clone(),
                version: buffer.document.version,
            });
        }
    }
    Some(item)
}

fn advice_actions(
    item: &crate::SemanticDiagnostic,
    open: &super::Buffer,
    encoding: crate::PositionEncoding,
    selection: &std::ops::Range<Position>,
) -> Vec<Value> {
    let Some(label) = item
        .annotations
        .labels
        .iter()
        .find(|label| label.primary && label.span.source == item.source)
    else {
        return Vec::new();
    };
    let start =
        open.document
            .lines
            .position(&open.document.text, label.span.start as usize, encoding);
    let end = open
        .document
        .lines
        .position(&open.document.text, label.span.end as usize, encoding);
    let position = |position: Position| (position.line, position.character);
    if position(end) < position(selection.start) || position(selection.end) < position(start) {
        return Vec::new();
    }
    item.annotations
        .fixes
        .iter()
        .filter(|fix| fix.edits.is_empty())
        .map(|fix| {
            object([
                ("title", Value::string(fix.message.as_slice())),
                ("kind", Value::string("quickfix")),
                (
                    "disabled",
                    object([("reason", Value::string("Apply this suggestion manually."))]),
                ),
            ])
        })
        .collect()
}
