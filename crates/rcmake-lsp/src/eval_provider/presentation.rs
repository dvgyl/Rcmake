use rcmake_api::{
    CallableDefinition, CompletionState, ConfigureResult, SourceLocation, TargetDefinition,
    VariableDefinition,
};
use rcmake_diag::{Location, MessageKind};
use rcmake_host::path::to_native;
use rcmake_json::Value;

use crate::{
    Position,
    features::escape_markdown,
    json::{object, uint},
    position::range,
    uri,
};

use super::{EvalSemanticProvider, buffer_path};

const MAX_DIAGNOSTICS: usize = 256;
const DENIED_HINT: &str =
    "The editor cannot run this command. Run a local configure to get the result.";

impl EvalSemanticProvider {
    /// Get the current configure diagnostics for a file URI.
    ///
    /// If the session denied a process request, add one information item to the
    /// top-level CMakeLists.txt. Diagnostics from a previous edit are omitted.
    #[must_use]
    pub fn diagnostics(&self, document_uri: &str) -> Vec<Value> {
        let Some(path) = uri::path(document_uri) else {
            return Vec::new();
        };
        let Some((snapshot, buffers)) = self.snapshots.view() else {
            return Vec::new();
        };
        if !std::sync::Arc::ptr_eq(&snapshot.inputs, &buffers) {
            return Vec::new();
        }
        let path = buffer_path(self.host(), &path);
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
                line.saturating_sub(1) as usize
            } else if path == root {
                0
            } else {
                continue;
            };
            if buffers
                .get(&path)
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

pub(super) enum Binding<'a> {
    Target(&'a TargetDefinition),
    Callable(&'a CallableDefinition),
    Variable {
        name: &'a [u8],
        value: &'a [u8],
        origin: Option<&'a SourceLocation>,
        cached: bool,
    },
}

impl<'a> Binding<'a> {
    pub(super) fn variable(name: &'a [u8], value: &'a VariableDefinition) -> Self {
        Self::Variable {
            name,
            value: &value.value,
            origin: value.location.as_ref(),
            cached: value.cached,
        }
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
                    for (label, properties) in [
                        (
                            "Compile definitions",
                            &["COMPILE_DEFINITIONS", "INTERFACE_COMPILE_DEFINITIONS"][..],
                        ),
                        (
                            "Include directories",
                            &[
                                "INCLUDE_DIRECTORIES",
                                "INTERFACE_INCLUDE_DIRECTORIES",
                                "INTERFACE_SYSTEM_INCLUDE_DIRECTORIES",
                            ][..],
                        ),
                    ] {
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
                ..
            } => {
                let kind = if *cached {
                    "Cache variable"
                } else {
                    "Variable"
                };
                format!(
                    "{}\n\n{kind}",
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
    let clipped = bytes.get(..limit).unwrap_or(bytes);
    let mut text = String::from_utf8_lossy(clipped).into_owned();
    if clipped.len() < bytes.len() {
        text.push('…');
    }
    text
}
