use std::{collections::BTreeMap, ops::Range, sync::Arc};

use rcmake_base::Limits;
use rcmake_diag::{
    Diagnostic, Location, MessageKind, SourceId,
    source::{SourceKind, SourceMap},
};
use rcmake_json::Value;
use rcmake_syntax::{
    ArgKind, CommandInvocation, PartialCommandInvocation, parse_list_file_with_warnings,
    parse_tolerant,
};

use crate::{
    SemanticDiagnostic,
    annotate::{Annotations, Confidence, Edit, Fix, SourceSpan},
    catalog::{self, DEPRECATED, READONLY, Topic},
    expression::{self, Kind as ExpressionKind},
    features::{command_name, payload},
    json::{object, uint},
    lexical,
    position::{Lines, Position, PositionEncoding},
    roles::{Arguments, Role, Variable},
};

pub(crate) struct Document {
    pub(crate) text: String,
    pub(crate) version: i64,
    pub(crate) lines: Lines,
    pub(crate) commands: Arc<[CommandInvocation]>,
    pub(crate) unfinished: Vec<usize>,
    warnings: Vec<SemanticDiagnostic>,
    error: Option<rcmake_syntax::ParseError>,
}

impl Document {
    pub(crate) fn new(text: String, version: i64) -> Self {
        let lines = Lines::new(&text);
        let mut sources = SourceMap::default();
        let kind = SourceKind::File;
        let Some(source) = sources.add("".into(), kind, Arc::from(b"".as_slice())) else {
            return Self {
                text,
                version,
                lines,
                commands: Arc::default(),
                unfinished: Vec::new(),
                warnings: Vec::new(),
                error: None,
            };
        };
        let limits = Limits::default();
        let syntax = crate::position::syntax_text(&text);
        let (commands, unfinished, warnings, error) =
            match parse_list_file_with_warnings(syntax.as_bytes(), source, kind, &limits) {
                Ok(file) => (file.commands, Vec::new(), file.warnings, None),
                Err((error, warnings)) => {
                    let recovered = parse_tolerant(source, syntax.as_bytes());
                    if recovered.commands.is_empty() && recovered.partial_commands.is_empty() {
                        let (commands, unfinished) = lexical::recover(&syntax, &lines);
                        (commands.into(), unfinished, warnings, Some(error))
                    } else {
                        let mut commands = recovered.commands;
                        let mut unfinished = Vec::new();
                        for partial in recovered.partial_commands {
                            if partial.parenthesis_span.is_some() {
                                unfinished.push(partial.span.start);
                                commands.push(recover_partial(partial, &lines));
                            }
                        }
                        commands.sort_unstable_by_key(|command| command.span.start);
                        (commands.into(), unfinished, recovered.warnings, Some(error))
                    }
                }
            };
        let warnings = warnings
            .into_iter()
            .take(1024)
            .map(|diagnostic| syntax_warning(diagnostic, source, &text, &lines))
            .collect();
        Self {
            text,
            version,
            lines,
            commands,
            unfinished,
            warnings,
            error,
        }
    }

    pub(crate) fn diagnostics(
        &self,
        encoding: PositionEncoding,
        include_parse_error: bool,
        semantic: &[SemanticDiagnostic],
    ) -> Vec<Value> {
        let mut diagnostics: Vec<_> = self
            .warnings
            .iter()
            .chain(semantic)
            .take(1024)
            .map(|warning| self.render_diagnostic(warning, encoding).0)
            .collect();
        if include_parse_error && let Some(error) = &self.error {
            let span = point_span(&self.text, &self.lines, error.line, error.column);
            diagnostics.push(object([
                ("range", self.span(span, encoding)),
                ("severity", uint(1)),
                ("source", Value::string("rcmake")),
                (
                    "message",
                    Value::string(String::from_utf8_lossy(&error.message).into_owned()),
                ),
            ]));
        }
        diagnostics
    }

    fn render_diagnostic(
        &self,
        item: &SemanticDiagnostic,
        encoding: PositionEncoding,
    ) -> (Value, Range<usize>) {
        let diagnostic = &item.diagnostic;
        let span = item
            .annotations
            .labels
            .iter()
            .filter(|label| label.span.source == item.source)
            .filter_map(|label| {
                self.checked_span(label.span)
                    .map(|span| (label.primary, span))
            })
            .max_by_key(|(primary, _)| *primary)
            .map_or_else(
                || point_span(&self.text, &self.lines, diagnostic_line(diagnostic), 1),
                |(_, span)| span,
            );
        let severity = match diagnostic.kind {
            MessageKind::Error
            | MessageKind::InternalError
            | MessageKind::AuthorError
            | MessageKind::DeprecationError => 1,
            _ => 2,
        };
        let mut fields = vec![
            ("range", self.span(span.clone(), encoding)),
            ("severity", uint(severity)),
            ("source", Value::string("rcmake")),
            (
                "message",
                Value::string(String::from_utf8_lossy(&diagnostic.body).into_owned()),
            ),
        ];
        if let Some(policy) = diagnostic.policy {
            fields.push(("code", Value::string(policy.to_string())));
            let href = format!("{}/policy/{policy}.html", catalog::DOCS);
            fields.push(("codeDescription", object([("href", Value::string(href))])));
        }
        (object(fields), span)
    }

    fn checked_span(&self, span: SourceSpan) -> Option<Range<usize>> {
        let range = span.start as usize..span.end as usize;
        self.text.get(range.clone())?;
        Some(range)
    }

    pub(crate) fn quick_fixes<'docs>(
        &self,
        uri: &str,
        selection: Range<Position>,
        encoding: PositionEncoding,
        versioned_edits: bool,
        semantic: &[SemanticDiagnostic],
        open: impl Fn(&str, i64) -> Option<&'docs Document>,
    ) -> Vec<Value> {
        let Some(start) = self
            .lines
            .edit_offset(&self.text, selection.start, encoding)
        else {
            return Vec::new();
        };
        let Some(end) = self.lines.edit_offset(&self.text, selection.end, encoding) else {
            return Vec::new();
        };
        let mut actions = Vec::new();
        for item in self.warnings.iter().chain(semantic).take(1024) {
            if item.annotations.fixes.is_empty() {
                continue;
            }
            let (diagnostic, span) = self.render_diagnostic(item, encoding);
            if span.end < start || end < span.start {
                continue;
            }
            for fix in &item.annotations.fixes {
                if let Some(edit) = self.fix_edit(fix, item, uri, encoding, versioned_edits, &open)
                {
                    actions.push(object([
                        (
                            "title",
                            Value::string(String::from_utf8_lossy(&fix.message).into_owned()),
                        ),
                        ("kind", Value::string("quickfix")),
                        (
                            "isPreferred",
                            Value::Bool(fix.confidence == Confidence::MachineApplicable),
                        ),
                        ("diagnostics", Value::Array(vec![diagnostic.clone()])),
                        ("edit", edit),
                    ]));
                }
                if actions.len() >= 256 {
                    return actions;
                }
            }
        }
        actions
    }

    fn fix_edit<'docs>(
        &self,
        fix: &Fix,
        item: &SemanticDiagnostic,
        uri: &str,
        encoding: PositionEncoding,
        versioned_edits: bool,
        open: &impl Fn(&str, i64) -> Option<&'docs Document>,
    ) -> Option<Value> {
        if fix.edits.is_empty() || fix.edits.len() > 1024 {
            return None;
        }
        let mut by_uri = BTreeMap::new();
        for edit in &fix.edits {
            let (uri, doc) = if edit.span.source == item.source {
                (uri, self)
            } else {
                let source = item
                    .related_sources
                    .iter()
                    .find(|source| source.source == edit.span.source)?;
                (source.uri.as_str(), open(&source.uri, source.version)?)
            };
            let (span, text) = edit.validate(&doc.text)?;
            for offset in [span.start, span.end] {
                let position = doc.lines.position(&doc.text, offset, encoding);
                if doc.lines.edit_offset(&doc.text, position, encoding) != Some(offset) {
                    return None;
                }
            }
            by_uri
                .entry(uri)
                .or_insert_with(|| (doc, Vec::new()))
                .1
                .push((span, text));
        }
        let mut changes = Vec::new();
        for (uri, (doc, mut edits)) in by_uri {
            edits.sort_unstable_by_key(|(span, _)| span.start);
            let mut prev: Option<Range<usize>> = None;
            let mut values = Vec::new();
            for (span, text) in edits {
                if prev
                    .as_ref()
                    .is_some_and(|prev| span.start < prev.end || span.start == prev.start)
                {
                    return None;
                }
                values.push(object([
                    ("range", doc.span(span.clone(), encoding)),
                    ("newText", Value::string(text)),
                ]));
                prev = Some(span);
            }
            changes.push((uri, doc.version, Value::Array(values)));
        }
        if versioned_edits {
            let documents = changes
                .into_iter()
                .map(|(uri, version, edits)| {
                    object([
                        (
                            "textDocument",
                            object([
                                ("uri", Value::string(uri)),
                                ("version", Value::Int(version)),
                            ]),
                        ),
                        ("edits", edits),
                    ])
                })
                .collect();
            Some(object([("documentChanges", Value::Array(documents))]))
        } else {
            Some(object([(
                "changes",
                object(changes.into_iter().map(|(uri, _, edits)| (uri, edits))),
            )]))
        }
    }

    pub(crate) fn span(&self, span: Range<usize>, encoding: PositionEncoding) -> Value {
        self.lines.range(&self.text, span, encoding)
    }

    pub(crate) fn command_at(&self, offset: usize) -> Option<&CommandInvocation> {
        let index = self
            .commands
            .partition_point(|command| command.span.start <= offset)
            .checked_sub(1)?;
        self.commands.get(index).filter(|command| {
            command.span.contains(&offset)
                || (offset == command.span.end && self.is_unfinished(command))
        })
    }

    pub(crate) fn is_unfinished(&self, command: &CommandInvocation) -> bool {
        self.unfinished.binary_search(&command.span.start).is_ok()
    }

    pub(crate) fn comments(&self) -> Vec<Range<usize>> {
        let mut found = Vec::new();
        let mut cursor = 0;
        for arg in self.commands.iter().flat_map(|command| &command.args) {
            collect_comments(&self.text, cursor..arg.span.start, &mut found);
            cursor = arg.span.end;
        }
        collect_comments(&self.text, cursor..self.text.len(), &mut found);
        found
    }

    pub(crate) fn formatting(&self, encoding: PositionEncoding, indent: &str) -> Option<Value> {
        if self.error.is_some() {
            return Some(Value::Array(Vec::new()));
        }
        let mut verbatim = self.comments();
        verbatim.extend(
            self.commands
                .iter()
                .flat_map(|command| &command.args)
                .filter(|arg| arg.kind != ArgKind::Unquoted)
                .map(|arg| arg.span.clone()),
        );
        let mut protected = vec![false; self.lines.len()];
        for span in verbatim {
            let first = self.lines.line_at(span.start);
            let last = self.lines.line_at(span.end.saturating_sub(1));
            if let Some(lines) = protected.get_mut(first + 1..=last) {
                lines.fill(true);
            }
        }
        let mut edits = Vec::new();
        let mut response_bytes = 1024;
        let mut depth = 0usize;
        let mut commands = self.commands.iter().peekable();
        let mut continuation = None;
        let indentation_byte = indent.as_bytes().first().copied().unwrap_or(b' ');
        for line in 0..self.lines.len() {
            let raw = self.lines.text(&self.text, line).unwrap_or_default();
            let (bom, rest) = if line == 0 && raw.starts_with('\u{feff}') {
                ("\u{feff}", raw.strip_prefix('\u{feff}').unwrap_or(raw))
            } else {
                ("", raw)
            };
            let trimmed = rest.trim_start_matches([' ', '\t']);
            let is_blank = trimmed.trim_ascii().is_empty();
            let mut line_depth = match continuation {
                Some((end, opening_depth)) if line <= end => {
                    opening_depth + usize::from(line < end || !closing_only(trimmed))
                }
                _ => depth,
            };
            let mut first = true;
            while let Some(command) = commands.next_if(|command| command.line as usize == line + 1)
            {
                let name = command.lower.as_slice();
                if block_end(name) || branch(name) {
                    depth = depth.saturating_sub(1);
                    if first {
                        line_depth = depth;
                    }
                }
                continuation = Some((command.close_line.saturating_sub(1) as usize, depth));
                if block_start(name) || branch(name) {
                    depth += 1;
                }
                first = false;
            }
            if is_blank || protected.get(line).copied().unwrap_or(false) {
                continue;
            }
            let original_len = rest.len() - trimmed.len();
            let line_depth = line_depth.min(1000);
            let new_len = indent.len() * line_depth;
            if original_len == new_len
                && rest
                    .as_bytes()
                    .get(..original_len)
                    .is_some_and(|prefix| prefix.iter().all(|&byte| byte == indentation_byte))
            {
                continue;
            }
            let escaped_len = if indent == "\t" { new_len * 2 } else { new_len };
            response_bytes += 128 + escaped_len;
            if response_bytes > crate::rpc::MAX_MESSAGE_BYTES {
                return None;
            }
            let start = self.lines.start(line).unwrap_or_default() + bom.len();
            edits.push(object([
                ("range", self.span(start..start + original_len, encoding)),
                ("newText", Value::string(indent.repeat(line_depth))),
            ]));
        }
        Some(Value::Array(edits))
    }
}

fn recover_partial(partial: PartialCommandInvocation, lines: &Lines) -> CommandInvocation {
    CommandInvocation {
        name: partial.name,
        lower: partial.lower,
        line: partial.line,
        close_line: u32::try_from(lines.line_at(partial.span.end) + 1).unwrap_or(u32::MAX),
        span: partial.span,
        args: partial.args,
    }
}

fn diagnostic_line(diagnostic: &Diagnostic) -> u32 {
    match &diagnostic.location {
        Location::FileLine { line, .. } => *line,
        Location::Backtrace(trace) => trace.top().map_or(0, |frame| frame.line),
        Location::None => 0,
    }
}

fn point_span(text: &str, lines: &Lines, line: u32, column: u32) -> Range<usize> {
    let line = line.saturating_sub(1) as usize;
    let raw = lines.text(text, line).unwrap_or_default();
    let bom = usize::from(line == 0 && raw.starts_with('\u{feff}')) * 3;
    let offset = raw.floor_char_boundary(
        (column.saturating_sub(1) as usize)
            .saturating_add(bom)
            .min(raw.len()),
    );
    let width = raw
        .get(offset..)
        .and_then(|tail| tail.chars().next())
        .map_or(0, char::len_utf8);
    let start = lines.start(line).unwrap_or(text.len()) + offset;
    start..start + width
}

fn syntax_warning(
    diagnostic: Diagnostic,
    source: SourceId,
    text: &str,
    lines: &Lines,
) -> SemanticDiagnostic {
    let mut annotations = Annotations::default();
    if let Ok(message) = std::str::from_utf8(&diagnostic.body)
        && let Some(column) = message
            .strip_prefix("Syntax Warning in cmake code at column ")
            .and_then(|message| {
                message
                    .strip_suffix("\n\nArgument not separated from preceding token by whitespace.")
            })
            .and_then(|column| column.parse().ok())
    {
        let range = point_span(text, lines, diagnostic_line(&diagnostic), column);
        if let (Ok(start), Ok(end)) = (u32::try_from(range.start), u32::try_from(range.end))
            && start < end
        {
            annotations =
                Annotations::primary(SourceSpan { source, start, end }, "").with_fix(Fix {
                    message: "Insert a space between arguments.".into(),
                    edits: vec![Edit {
                        span: SourceSpan {
                            source,
                            start,
                            end: start,
                        },
                        replacement: " ".into(),
                    }],
                    confidence: Confidence::MachineApplicable,
                });
        }
    }
    SemanticDiagnostic::new(diagnostic, source, annotations)
}

fn closing_only(line: &str) -> bool {
    if !line.starts_with(')') {
        return false;
    }
    let mut bytes = line.as_bytes();
    loop {
        match bytes {
            [] => return true,
            [b')' | b' ' | b'\t', tail @ ..] => bytes = tail,
            [b'#', ..] => {
                let end = lexical::comment_end(bytes, 0);
                bytes = bytes.get(end..).unwrap_or_default();
            }
            _ => return false,
        }
    }
}

fn block_start(name: &[u8]) -> bool {
    matches!(
        name,
        b"if" | b"foreach" | b"while" | b"function" | b"macro" | b"block"
    )
}

fn block_end(name: &[u8]) -> bool {
    matches!(
        name,
        b"endif" | b"endforeach" | b"endwhile" | b"endfunction" | b"endmacro" | b"endblock"
    )
}

fn branch(name: &[u8]) -> bool {
    matches!(name, b"else" | b"elseif")
}

fn collect_comments(text: &str, span: Range<usize>, output: &mut Vec<Range<usize>>) {
    let Some(chunk) = text.as_bytes().get(span.clone()) else {
        return;
    };
    let mut cursor = 0;
    while let Some(tail) = chunk.get(cursor..) {
        match tail {
            [] => break,
            [b'\\', _, ..] => cursor += 2,
            [b'"', ..] => cursor = lexical::quoted_end(chunk, cursor).0,
            [b'#', ..] => {
                let end = lexical::comment_end(chunk, cursor);
                output.push(span.start + cursor..span.start + end);
                cursor = end;
            }
            [b'[', ..] => {
                cursor = lexical::bracket(chunk, cursor).map_or(cursor + 1, |(end, _)| end);
            }
            _ => cursor += 1,
        }
    }
}

pub(crate) const TOKEN_TYPES: [&str; 7] = [
    "function", "keyword", "variable", "string", "comment", "class", "operator",
];
pub(crate) const TOKEN_MODIFIERS: [&str; 3] = ["defaultLibrary", "deprecated", "readonly"];
const DEFAULT_LIBRARY: usize = 1;
const COMMAND: usize = 0;
const KEYWORD: usize = 1;
const VARIABLE: usize = 2;
const STRING: usize = 3;
const COMMENT: usize = 4;
const TARGET: usize = 5;
const GENERATOR: usize = 6;

fn variable_modifiers(name: &str, namespace: Variable) -> usize {
    if namespace == Variable::Environment {
        return 0;
    }
    Topic::Variable
        .lookup(name)
        .map_or(0, |entry| entry.4 & (DEPRECATED | READONLY))
}

pub(crate) fn semantic_tokens(doc: &Document, encoding: PositionEncoding) -> Value {
    let mut comments = doc.comments().into_iter().peekable();
    let mut tokens = Tokens {
        doc,
        encoding,
        data: Vec::new(),
        last: Position::default(),
        last_offset: 0,
        last_end: 0,
    };
    for command in doc.commands.iter() {
        let name = command_name(command);
        tokens.comments_before(&mut comments, name.start);
        let modifiers = std::str::from_utf8(&command.lower)
            .ok()
            .and_then(|name| Topic::Command.lookup(name))
            .map_or(0, |entry| DEFAULT_LIBRARY | (entry.4 & DEPRECATED));
        tokens.push_modified(name, COMMAND, modifiers);
        let arguments = Arguments::new(command);
        for (index, arg) in command.args.iter().enumerate() {
            tokens.comments_before(&mut comments, arg.span.start);
            if arg.kind == ArgKind::Bracket {
                tokens.push(arg.span.clone(), STRING);
                continue;
            }
            let Some(payload) = payload(doc, arg) else {
                continue;
            };
            let Some(text) = doc.text.get(payload.clone()) else {
                continue;
            };
            let style = if arguments.keyword(index, arg).is_some() {
                Some(KEYWORD)
            } else {
                match arguments.role(index) {
                    Role::Variable(_) => Some(VARIABLE),
                    Role::CommandDeclaration => Some(COMMAND),
                    Role::Target | Role::TargetDeclaration if expression::target_name(text) => {
                        Some(TARGET)
                    }
                    _ => None,
                }
            };
            let modifiers = match arguments.role(index) {
                Role::Variable(namespace) if style == Some(VARIABLE) => {
                    variable_modifiers(text, namespace)
                }
                _ => 0,
            };
            let background = (arg.kind == ArgKind::Quoted).then_some(STRING);
            let styled_payload = style.is_some() && background.is_some();
            if styled_payload {
                tokens.push(arg.span.start..payload.start, STRING);
            }
            let body = if styled_payload {
                payload.clone()
            } else {
                arg.span.clone()
            };
            let mut start = body.start;
            for expr in expression::expressions(text) {
                let span = payload.start + expr.span.start..payload.start + expr.span.end;
                if let Some(style) = style.or(background) {
                    tokens.push_modified(start..span.start, style, modifiers);
                }
                start = span.end;
                tokens.push_modified(
                    span,
                    match expr.kind {
                        ExpressionKind::Reference(_) => VARIABLE,
                        ExpressionKind::Generator => GENERATOR,
                        ExpressionKind::Target => TARGET,
                    },
                    match expr.kind {
                        ExpressionKind::Reference(namespace) => text
                            .get(expr.name)
                            .map_or(0, |name| variable_modifiers(name, namespace)),
                        _ => 0,
                    },
                );
            }
            if let Some(style) = style.or(background) {
                tokens.push_modified(start..body.end, style, modifiers);
            }
            if styled_payload {
                tokens.push(payload.end..arg.span.end, STRING);
            }
        }
    }
    tokens.comments_before(&mut comments, usize::MAX);
    object([("data", Value::Array(tokens.data))])
}

struct Tokens<'a> {
    doc: &'a Document,
    encoding: PositionEncoding,
    data: Vec<Value>,
    last: Position,
    last_offset: usize,
    last_end: usize,
}

impl Tokens<'_> {
    fn comments_before(
        &mut self,
        comments: &mut std::iter::Peekable<impl Iterator<Item = Range<usize>>>,
        offset: usize,
    ) {
        while let Some(comment) = comments.next_if(|comment| comment.start < offset) {
            self.push(comment, COMMENT);
        }
    }

    fn push(&mut self, span: Range<usize>, kind: usize) {
        self.push_modified(span, kind, 0);
    }

    fn push_modified(&mut self, span: Range<usize>, kind: usize, modifiers: usize) {
        let mut cursor = span.start;
        while cursor < span.end && self.data.len() < 500_000 {
            let line = self.doc.lines.line_at(cursor);
            let start = self.doc.lines.start(line).unwrap_or(0);
            let line_end = start
                + self
                    .doc
                    .lines
                    .text(&self.doc.text, line)
                    .map_or(0, str::len);
            let end = span.end.min(line_end);
            if let Some(text) = self
                .doc
                .text
                .get(cursor..end)
                .filter(|text| !text.is_empty())
            {
                let Some(delta_line) = line.checked_sub(self.last.line) else {
                    return;
                };
                if cursor < self.last_end {
                    return;
                }
                let (prior_offset, prior_character) = if delta_line == 0 {
                    (self.last_offset, self.last.character)
                } else {
                    (start, 0)
                };
                let character = self
                    .doc
                    .text
                    .get(prior_offset..cursor)
                    .map_or(prior_character, |prefix| {
                        prior_character + self.encoding.width(prefix)
                    });
                let delta_character = if delta_line == 0 {
                    character.saturating_sub(self.last.character)
                } else {
                    character
                };
                self.data.extend(
                    [
                        delta_line,
                        delta_character,
                        self.encoding.width(text),
                        kind,
                        modifiers,
                    ]
                    .map(uint),
                );
                self.last = Position { line, character };
                self.last_offset = cursor;
                self.last_end = end;
            }
            let Some(next) = self.doc.lines.start(line + 1) else {
                break;
            };
            cursor = next;
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::json::{array, get};
    use crate::position::range;

    use super::*;

    #[test]
    fn partial_edit_keeps_warning_and_its_quick_fix() -> Result<(), &'static str> {
        let doc = Document::new("x(\"é\"y)\nmessage(".into(), 2);
        let encoding = PositionEncoding::Utf16;
        let immediate = doc.diagnostics(encoding, false, &[]);
        assert_eq!(immediate.len(), 1);
        assert_eq!(doc.diagnostics(encoding, true, &[]).len(), 2);
        let start = Position {
            line: 0,
            character: 5,
        };
        let uri = "file:///work/CMakeLists.txt";
        let actions = doc.quick_fixes(uri, start..start, encoding, true, &[], |_, _| None);
        let [action] = actions.as_slice() else {
            return Err("Expected one quick fix.");
        };
        assert_eq!(get(action, "isPreferred"), Some(&Value::Bool(true)));
        let edits = get(action, "edit")
            .and_then(|edit| get(edit, "documentChanges"))
            .and_then(array)
            .and_then(|documents| documents.first())
            .and_then(|document| get(document, "edits"))
            .and_then(array)
            .ok_or("Expected versioned document edits.")?;
        assert_eq!(
            edits,
            &[object([
                ("range", range(start, start)),
                ("newText", Value::string(" ")),
            ])]
        );
        Ok(())
    }
}
