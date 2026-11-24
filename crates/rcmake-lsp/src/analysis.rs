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
    position::{Lines, Position, PositionEncoding, range},
};

pub(crate) struct Document {
    pub(crate) text: String,
    pub(crate) version: i64,
    pub(crate) lines: Lines,
    pub(crate) commands: Arc<[CommandInvocation]>,
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
                warnings: Vec::new(),
                error: None,
            };
        };
        let limits = Limits::default();
        let (commands, warnings, error) =
            match parse_list_file_with_warnings(text.as_bytes(), source, kind, &limits) {
                Ok(file) => (file.commands, file.warnings, None),
                Err((error, warnings)) => {
                    let prefix = lines
                        .start(error.line.saturating_sub(1) as usize)
                        .and_then(|end| text.get(..end))
                        .and_then(|prefix| {
                            rcmake_syntax::parse_list_file(prefix.as_bytes(), source, kind, &limits)
                                .ok()
                        });
                    (
                        prefix.map_or_else(Arc::default, |file| file.commands),
                        warnings,
                        Some(error),
                    )
                }
            };
        Self {
            text,
            version,
            lines,
            commands,
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
            .partition_point(|command| command.span.end < offset);
        self.commands
            .get(index)
            .filter(|command| command.span.start <= offset)
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

    pub(crate) fn formatting(&self, encoding: PositionEncoding, indent: &str) -> Value {
        if self.error.is_some() {
            return Value::Array(Vec::new());
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
        let mut response_bytes = 0usize;
        let mut depth = 0usize;
        let mut commands = self.commands.iter().peekable();
        let mut continuation = None;
        for (line, raw) in self.text.split_inclusive('\n').enumerate() {
            let (bom, rest) = if line == 0 && raw.starts_with('\u{feff}') {
                ("\u{feff}", raw.strip_prefix('\u{feff}').unwrap_or(raw))
            } else {
                ("", raw)
            };
            let trimmed = rest.trim_start_matches([' ', '\t']);
            let is_blank = trimmed.trim_ascii().is_empty();
            let mut line_depth = match continuation {
                Some((end, opening_depth)) if line <= end => {
                    opening_depth + usize::from(line < end)
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
            let new_indent = indent.repeat(line_depth.min(1000));
            if rest.get(..original_len) == Some(new_indent.as_str()) {
                continue;
            }
            response_bytes += 128 + new_indent.len() * 2;
            if response_bytes > crate::rpc::MAX_MESSAGE_BYTES {
                return Value::Array(Vec::new());
            }
            let start = self.lines.start(line).unwrap_or_default() + bom.len();
            edits.push(object([
                ("range", self.span(start..start + original_len, encoding)),
                ("newText", Value::string(new_indent)),
            ]));
        }
        Value::Array(edits)
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
            [b'"', rest @ ..] => {
                cursor += 1;
                let mut escaped = false;
                for &byte in rest {
                    cursor += 1;
                    if !escaped && byte == b'"' {
                        break;
                    }
                    escaped = !escaped && byte == b'\\';
                }
            }
            [b'#', rest @ ..] => {
                let end = bracket_end(rest).unwrap_or_else(|| {
                    rest.iter()
                        .position(|&byte| byte == b'\n')
                        .unwrap_or(rest.len())
                });
                output.push(span.start + cursor..span.start + cursor + 1 + end);
                cursor += 1 + end;
            }
            [b'[', ..] => cursor += bracket_end(tail).unwrap_or(1),
            _ => cursor += 1,
        }
    }
}

fn bracket_end(input: &[u8]) -> Option<usize> {
    let payload = input.strip_prefix(b"[")?;
    let equals = payload.iter().take_while(|&&byte| byte == b'=').count();
    let tail = payload.get(equals..)?.strip_prefix(b"[")?;
    for (index, &byte) in tail.iter().enumerate() {
        if byte == b']' {
            let mark = tail.get(index + 1..)?;
            let count = mark.iter().take_while(|&&byte| byte == b'=').count();
            if count == equals && mark.get(count) == Some(&b']') {
                return Some(equals + index + count + 4);
            }
        }
    }
    Some(input.len())
}
