//! Track JSON value locations for preset diagnostics.

use std::collections::BTreeMap;
use std::fmt::Display;
use std::path::Path;
use std::sync::Arc;

use rcmake_base::BString;
use rcmake_json::{JsonError, ReadOptions, Value};

use crate::PresetsError;

pub(crate) const EXPECT_CONTAINER: &str =
    "A valid JSON document must be either an array or an object value.";

#[derive(Debug)]
pub(crate) struct Span {
    pub(crate) start: usize,
    children: Children,
}

#[derive(Debug)]
enum Children {
    None,
    Array(Vec<Arc<Span>>),
    Object(BTreeMap<BString, Arc<Span>>),
}

impl Span {
    pub(crate) fn scan(bytes: &[u8]) -> Option<Arc<Self>> {
        // The JSON parser has already checked the document and its nesting limit.
        Scanner { bytes, pos: 0 }.value()
    }

    pub(crate) fn member(&self, key: &[u8]) -> Option<Arc<Self>> {
        match &self.children {
            Children::Object(fields) => fields.get(key).cloned(),
            _ => None,
        }
    }

    pub(crate) fn item(&self, index: usize) -> Option<Arc<Self>> {
        match &self.children {
            Children::Array(items) => items.get(index).cloned(),
            _ => None,
        }
    }
}

struct Scanner<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl Scanner<'_> {
    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.pos).copied()
    }

    fn spaces(&mut self) {
        while self.peek().is_some_and(|byte| byte.is_ascii_whitespace()) {
            self.pos += 1;
        }
    }

    fn string(&mut self) -> Option<&[u8]> {
        let start = self.pos;
        self.pos += 1;
        while let Some(byte) = self.peek() {
            self.pos += 1;
            match byte {
                b'"' => return self.bytes.get(start..self.pos),
                b'\\' => self.pos += 1,
                _ => {}
            }
        }
        None
    }

    fn value(&mut self) -> Option<Arc<Span>> {
        self.spaces();
        let start = self.pos;
        let children = match self.peek()? {
            b'{' => {
                self.pos += 1;
                self.spaces();
                let mut fields = BTreeMap::new();
                while self.peek() == Some(b'"') {
                    let key = self.string()?;
                    let key = if key.contains(&b'\\') {
                        let Value::String(key) =
                            rcmake_json::parse(key, &ReadOptions::default()).ok()?
                        else {
                            return None;
                        };
                        key
                    } else {
                        BString::from(key.get(1..key.len().checked_sub(1)?)?)
                    };
                    self.spaces();
                    self.pos += 1;
                    fields.insert(key, self.value()?);
                    self.spaces();
                    if self.peek() != Some(b',') {
                        break;
                    }
                    self.pos += 1;
                    self.spaces();
                }
                self.pos += 1;
                Children::Object(fields)
            }
            b'[' => {
                self.pos += 1;
                self.spaces();
                let mut items = Vec::new();
                if self.peek() != Some(b']') {
                    loop {
                        items.push(self.value()?);
                        self.spaces();
                        if self.peek() != Some(b',') {
                            break;
                        }
                        self.pos += 1;
                    }
                }
                self.pos += 1;
                Children::Array(items)
            }
            b'"' => {
                self.string()?;
                Children::None
            }
            _ => {
                while self.peek().is_some_and(|byte| {
                    !byte.is_ascii_whitespace() && !matches!(byte, b',' | b']' | b'}')
                }) {
                    self.pos += 1;
                }
                Children::None
            }
        };
        Some(Arc::new(Span { start, children }))
    }
}

pub(crate) fn diagnostic(
    path: &Path,
    bytes: &[u8],
    offset: usize,
    message: impl Display,
) -> PresetsError {
    let before = bytes.get(..offset).unwrap_or(bytes);
    let (line, column) = coordinates(before);
    render(path, bytes, line, column, message)
}

fn coordinates(before: &[u8]) -> (usize, usize) {
    let line = before.iter().filter(|byte| **byte == b'\n').count() + 1;
    let column = before
        .rsplit(|byte| *byte == b'\n')
        .next()
        .map_or(0, <[u8]>::len);
    (line, column)
}

pub(crate) fn syntax_error(path: &Path, bytes: &[u8], error: &JsonError) -> PresetsError {
    let line = error.line as usize;
    let column = error.column.saturating_sub(1) as usize;
    let mut result = render(path, bytes, line, column, &error.message);
    let offset = bytes
        .iter()
        .position(|byte| !byte.is_ascii_whitespace())
        .unwrap_or(bytes.len());
    if error.message == "Syntax error: value, object or array expected."
        && let Some((before, tail)) = bytes.split_at_checked(offset)
        && coordinates(before) == (line, column)
    {
        let end = match tail {
            [b'/', b'/', ..] => tail
                .iter()
                .position(|byte| *byte == b'\n')
                .unwrap_or(tail.len()),
            [b'/', b'*', rest @ ..] => rest
                .windows(2)
                .position(|pair| pair == b"*/")
                .map_or(tail.len(), |end| end + 4),
            _ => usize::from(!tail.is_empty()),
        };
        let extra = match tail
            .get(end..)
            .unwrap_or_default()
            .iter()
            .position(|byte| !byte.is_ascii_whitespace())
        {
            Some(next) => diagnostic(
                path,
                bytes,
                offset + end + next,
                "Extra non-whitespace after JSON value.",
            ),
            None => diagnostic(path, bytes, 0, EXPECT_CONTAINER),
        };
        result.message.push('\n');
        result.message.push_str(&extra.message);
    }
    result
}

fn render(
    path: &Path,
    bytes: &[u8],
    line: usize,
    column: usize,
    message: impl Display,
) -> PresetsError {
    let filename = path
        .file_name()
        .unwrap_or(path.as_os_str())
        .to_string_lossy();
    let text = bytes
        .split(|byte| *byte == b'\n')
        .nth(line.saturating_sub(1))
        .unwrap_or_default();
    let text = String::from_utf8_lossy(text.strip_suffix(b"\r").unwrap_or(text));
    PresetsError::new(format!(
        "{filename}:{line}: {message}\n{text}\n{:column$}^",
        ""
    ))
}
