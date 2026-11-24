use std::{ops::Range, sync::Arc};

use rcmake_base::Limits;
use rcmake_diag::{
    Diagnostic, Location, MessageKind,
    source::{SourceKind, SourceMap},
};
use rcmake_json::Value;
use rcmake_syntax::{ArgKind, CommandInvocation, parse_list_file_with_warnings};

use crate::{
    json::{object, uint},
    lexical,
    position::{Lines, Position, PositionEncoding, range},
};

pub(crate) struct Document {
    pub(crate) text: String,
    pub(crate) version: i64,
    pub(crate) lines: Lines,
    pub(crate) commands: Arc<[CommandInvocation]>,
    pub(crate) unfinished: Vec<usize>,
    warnings: Vec<Diagnostic>,
    error: Option<rcmake_syntax::ParseError>,
}

impl Document {
    pub(crate) fn new(text: String, version: i64) -> Self {
        let lines = Lines::new(&text);
        let mut sources = SourceMap::default();
        let kind = SourceKind::File;
        let Some(source) = sources.add("".into(), kind, Arc::from(&b""[..])) else {
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
                    let (commands, unfinished) = lexical::recover(&syntax, &lines);
                    (commands.into(), unfinished, warnings, Some(error))
                }
            };
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

    pub(crate) fn diagnostics(&self, encoding: PositionEncoding) -> Vec<Value> {
        let mut diagnostics = Vec::new();
        for warning in self.warnings.iter().take(1024) {
            let line = match &warning.location {
                Location::FileLine { line, .. } => *line,
                Location::Backtrace(trace) => trace.top().map_or(0, |frame| frame.line),
                Location::None => 0,
            };
            let severity = match warning.kind {
                MessageKind::Error
                | MessageKind::InternalError
                | MessageKind::AuthorError
                | MessageKind::DeprecationError => 1,
                _ => 2,
            };
            diagnostics.push(self.diagnostic(line, 0, severity, &warning.body, encoding));
        }
        if let Some(error) = &self.error {
            diagnostics.push(self.diagnostic(
                error.line,
                error.column,
                1,
                &error.message,
                encoding,
            ));
        }
        diagnostics
    }

    fn diagnostic(
        &self,
        line: u32,
        column: u32,
        severity: usize,
        message: &[u8],
        encoding: PositionEncoding,
    ) -> Value {
        let line = line.saturating_sub(1) as usize;
        let text = self.lines.text(&self.text, line).unwrap_or_default();
        let bom = usize::from(line == 0 && text.starts_with('\u{feff}')) * 3;
        let mut offset = (column.saturating_sub(1) as usize + bom).min(text.len());
        while !text.is_char_boundary(offset) {
            offset = offset.saturating_sub(1);
        }
        let character = encoding.width(text.get(..offset).unwrap_or_default());
        let end = text
            .get(offset..)
            .and_then(|tail| tail.chars().next())
            .map_or(character, |next| {
                character
                    + match encoding {
                        PositionEncoding::Utf8 => next.len_utf8(),
                        PositionEncoding::Utf16 => next.len_utf16(),
                    }
            });
        object([
            (
                "range",
                range(
                    Position { line, character },
                    Position {
                        line,
                        character: end,
                    },
                ),
            ),
            ("severity", uint(severity)),
            ("source", Value::string("rcmake")),
            (
                "message",
                Value::string(String::from_utf8_lossy(message).into_owned()),
            ),
        ])
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
