use std::{borrow::Cow, ops::Range};

use rcmake_json::Value;

use crate::json::{get, int, object, uint};

/// A zero-based line and character offset in the negotiated encoding.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Position {
    /// Zero-based line number.
    pub line: usize,
    /// Character offset in the current position encoding.
    pub character: usize,
}

impl Position {
    pub(crate) fn parse(value: &Value) -> Option<Self> {
        Some(Self {
            line: usize::try_from(int(get(value, "line")?)?).ok()?,
            character: usize::try_from(int(get(value, "character")?)?).ok()?,
        })
    }

    pub(crate) fn json(self) -> Value {
        object([
            ("line", uint(self.line)),
            ("character", uint(self.character)),
        ])
    }
}

/// Position encoding used for client requests and server responses.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum PositionEncoding {
    /// Count bytes in UTF-8 text.
    Utf8,
    /// Count UTF-16 code units. This is the LSP default.
    #[default]
    Utf16,
    /// Count Unicode scalar values (UTF-32 code units).
    Utf32,
}

impl PositionEncoding {
    pub(crate) fn from_name(name: &str) -> Option<Self> {
        match name {
            "utf-8" => Some(Self::Utf8),
            "utf-16" => Some(Self::Utf16),
            "utf-32" => Some(Self::Utf32),
            _ => None,
        }
    }

    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Utf8 => "utf-8",
            Self::Utf16 => "utf-16",
            Self::Utf32 => "utf-32",
        }
    }

    pub(crate) fn width(self, text: &str) -> usize {
        match self {
            Self::Utf8 => text.len(),
            Self::Utf16 if text.is_ascii() => text.len(),
            Self::Utf16 => text.encode_utf16().count(),
            Self::Utf32 if text.is_ascii() => text.len(),
            Self::Utf32 => text.chars().count(),
        }
    }
}

#[derive(Clone)]
pub(crate) struct Lines(Vec<usize>);

impl Lines {
    pub(crate) fn new(text: &str) -> Self {
        Self(std::iter::once(0).chain(line_starts(text)).collect())
    }

    pub(crate) fn len(&self) -> usize {
        self.0.len()
    }

    pub(crate) fn start(&self, line: usize) -> Option<usize> {
        self.0.get(line).copied()
    }

    pub(crate) fn line_at(&self, offset: usize) -> usize {
        self.0
            .partition_point(|&start| start <= offset)
            .saturating_sub(1)
    }

    pub(crate) fn offset(
        &self,
        text: &str,
        position: Position,
        encoding: PositionEncoding,
    ) -> Option<usize> {
        let start = self.start(position.line)?;
        let line = self.text(text, position.line)?;
        Some(start + line_offset(line, position.character, encoding).0)
    }

    pub(crate) fn edit_offset(
        &self,
        text: &str,
        position: Position,
        encoding: PositionEncoding,
    ) -> Option<usize> {
        let start = self.start(position.line)?;
        let line = self.text(text, position.line)?;
        let (offset, exact) = line_offset(line, position.character, encoding);
        exact.then_some(start + offset)
    }

    pub(crate) fn text<'a>(&self, text: &'a str, line: usize) -> Option<&'a str> {
        let start = self.start(line)?;
        let end = self.start(line + 1).unwrap_or(text.len());
        let line = text.get(start..end)?;
        Some(
            line.strip_suffix("\r\n")
                .or_else(|| line.strip_suffix(['\r', '\n']))
                .unwrap_or(line),
        )
    }

    pub(crate) fn replace(
        &mut self,
        text: &mut String,
        span: Range<usize>,
        replacement: &str,
        work_left: &mut usize,
    ) -> bool {
        // Include the adjacent bytes in case an edit joins or splits CRLF.
        let from = text.floor_char_boundary(span.start.saturating_sub(1));
        let mut to = text.ceil_char_boundary(span.end.saturating_add(1).min(text.len()));
        if text.as_bytes().get(to.saturating_sub(1)..=to) == Some(b"\r\n") {
            to += 1;
        }
        let first = self.0.partition_point(|&start| start <= from);
        let last = self.0.partition_point(|&start| start <= to);
        let removed = span.len();
        let new_end = to - removed + replacement.len();
        let mut work = new_end - from;
        if removed != replacement.len() {
            work += text.len() - span.end;
        }
        if removed != replacement.len() || first != last || replacement.contains(['\r', '\n']) {
            work += (self.len() - last) * size_of::<usize>();
        }
        let Some(remaining) = work_left.checked_sub(work) else {
            return false;
        };
        *work_left = remaining;

        text.replace_range(span, replacement);
        if removed != replacement.len()
            && let Some(tail) = self.0.get_mut(last..)
        {
            for start in tail {
                *start = *start - removed + replacement.len();
            }
        }
        let starts =
            line_starts(text.get(from..new_end).unwrap_or_default()).map(|start| from + start);
        self.0.splice(first..last, starts);
        true
    }

    pub(crate) fn position(
        &self,
        text: &str,
        offset: usize,
        encoding: PositionEncoding,
    ) -> Position {
        let offset = offset.min(text.len());
        let line = self.line_at(offset);
        let start = self.start(line).unwrap_or(0);
        let character = self.text(text, line).map_or(0, |text| {
            let offset = text.floor_char_boundary((offset - start).min(text.len()));
            encoding.width(text.get(..offset).unwrap_or_default())
        });
        Position { line, character }
    }

    pub(crate) fn exact_position(
        &self,
        text: &str,
        offset: usize,
        encoding: PositionEncoding,
    ) -> Option<Position> {
        if offset > text.len() || !text.is_char_boundary(offset) {
            return None;
        }
        // Reject endpoints that split a CRLF pair.
        if offset > 0 && text.as_bytes().get(offset - 1..=offset) == Some(b"\r\n") {
            return None;
        }
        let pos = self.position(text, offset, encoding);
        (self.edit_offset(text, pos, encoding) == Some(offset)).then_some(pos)
    }

    pub(crate) fn range(
        &self,
        text: &str,
        span: Range<usize>,
        encoding: PositionEncoding,
    ) -> Value {
        range(
            self.position(text, span.start, encoding),
            self.position(text, span.end, encoding),
        )
    }
}

fn line_starts(text: &str) -> impl Iterator<Item = usize> {
    text.match_indices(['\r', '\n'])
        .filter_map(|(offset, ending)| {
            (ending == "\n" || text.as_bytes().get(offset + 1) != Some(&b'\n'))
                .then_some(offset + 1)
        })
}

fn line_offset(line: &str, character: usize, encoding: PositionEncoding) -> (usize, bool) {
    let byte = character.min(line.len());
    if encoding == PositionEncoding::Utf8 {
        let boundary = line.floor_char_boundary(byte);
        return (boundary, boundary == byte);
    }
    if line.as_bytes().get(..byte).is_some_and(<[u8]>::is_ascii) {
        return (byte, true);
    }
    let mut units = 0;
    for (offset, scalar) in line.char_indices() {
        if units == character {
            return (offset, true);
        }
        units += match encoding {
            PositionEncoding::Utf16 => scalar.len_utf16(),
            PositionEncoding::Utf32 => 1,
            PositionEncoding::Utf8 => scalar.len_utf8(),
        };
        if units > character {
            return (offset, false);
        }
    }
    (line.len(), true)
}

pub(crate) fn syntax_text(text: &str) -> Cow<'_, str> {
    let mut bare_cr = text.match_indices('\r').filter_map(|(offset, _)| {
        (text.as_bytes().get(offset + 1) != Some(&b'\n')).then_some(offset)
    });
    let Some(first) = bare_cr.next() else {
        return Cow::Borrowed(text);
    };
    // Each replacement has the same length. Lexer offsets identify the original bytes.
    let mut normalized = String::with_capacity(text.len());
    let mut start = 0;
    for offset in std::iter::once(first).chain(bare_cr) {
        normalized.push_str(text.get(start..offset).unwrap_or_default());
        normalized.push('\n');
        start = offset + 1;
    }
    normalized.push_str(text.get(start..).unwrap_or_default());
    Cow::Owned(normalized)
}

pub(crate) fn range(start: Position, end: Position) -> Value {
    object([("start", start.json()), ("end", end.json())])
}
