use std::ops::Range;

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
}

impl PositionEncoding {
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Utf8 => "utf-8",
            Self::Utf16 => "utf-16",
        }
    }

    pub(crate) fn width(self, text: &str) -> usize {
        match self {
            Self::Utf8 => text.len(),
            Self::Utf16 if text.is_ascii() => text.len(),
            Self::Utf16 => text.encode_utf16().count(),
        }
    }
}

pub(crate) struct Lines(Vec<usize>);

impl Lines {
    pub(crate) fn new(text: &str) -> Self {
        Self(
            std::iter::once(0)
                .chain(text.match_indices('\n').map(|(offset, _)| offset + 1))
                .collect(),
        )
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
        if encoding == PositionEncoding::Utf8 || line.is_ascii() {
            let offset = position.character.min(line.len());
            return line.is_char_boundary(offset).then_some(start + offset);
        }
        let mut units = 0;
        for (offset, character) in line.char_indices() {
            if units == position.character {
                return Some(start + offset);
            }
            units += character.len_utf16();
            if units > position.character {
                return None;
            }
        }
        Some(start + line.len())
    }

    pub(crate) fn text<'a>(&self, text: &'a str, line: usize) -> Option<&'a str> {
        let start = self.start(line)?;
        let end = self.start(line + 1).unwrap_or(text.len());
        let line = text.get(start..end)?;
        Some(
            line.strip_suffix('\n')
                .map_or(line, |line| line.strip_suffix('\r').unwrap_or(line)),
        )
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
        let character = text
            .get(start..offset)
            .map_or(0, |prefix| encoding.width(prefix));
        Position { line, character }
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

pub(crate) fn range(start: Position, end: Position) -> Value {
    object([("start", start.json()), ("end", end.json())])
}
