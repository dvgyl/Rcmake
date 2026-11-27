//! Scan raw and normalized tokens while retaining byte offsets into the original buffer.

use rcmake_base::BString;
use rcmake_diag::source::SourceKind;

use crate::ParseError;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Position {
    pub(super) offset: usize,
    pub(super) line: u32,
    pub(super) column: u32,
}

/// Lossless token classification covering every byte of a listfile.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum RawKind {
    Bom,
    Space,
    Newline,
    LineComment,
    BracketComment,
    Identifier,
    Unquoted,
    Quoted,
    Bracket,
    LeftParen,
    RightParen,
    UnterminatedQuote,
    UnterminatedBracket,
    BadCharacter,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Kind {
    Space,
    Newline,
    BracketComment,
    Identifier,
    Unquoted,
    Quoted,
    Bracket,
    LeftParen,
    RightParen,
    UnterminatedQuote,
    UnterminatedBracket,
    BadCharacter,
}

impl Kind {
    pub(super) const fn description(self) -> &'static [u8] {
        match self {
            Self::Space => b"space",
            Self::Newline => b"newline",
            Self::BracketComment => b"bracket comment",
            Self::Identifier => b"identifier",
            Self::Unquoted => b"unquoted argument",
            Self::Quoted => b"quoted argument",
            Self::Bracket => b"bracket argument",
            Self::LeftParen => b"left paren",
            Self::RightParen => b"right paren",
            Self::UnterminatedQuote => b"unterminated string",
            Self::UnterminatedBracket => b"unterminated bracket",
            Self::BadCharacter => b"bad character",
        }
    }

    pub(super) const fn from_raw(kind: RawKind) -> Option<Self> {
        Some(match kind {
            RawKind::Bom | RawKind::LineComment => return None,
            RawKind::Space => Self::Space,
            RawKind::Newline => Self::Newline,
            RawKind::BracketComment => Self::BracketComment,
            RawKind::Identifier => Self::Identifier,
            RawKind::Unquoted => Self::Unquoted,
            RawKind::Quoted => Self::Quoted,
            RawKind::Bracket => Self::Bracket,
            RawKind::LeftParen => Self::LeftParen,
            RawKind::RightParen => Self::RightParen,
            RawKind::UnterminatedQuote => Self::UnterminatedQuote,
            RawKind::UnterminatedBracket => Self::UnterminatedBracket,
            RawKind::BadCharacter => Self::BadCharacter,
        })
    }
}

pub(super) struct RawToken {
    pub(super) kind: RawKind,
    pub(super) start: Position,
    pub(super) end: Position,
}

pub(super) struct Token {
    pub(super) kind: Kind,
    pub(super) text: BString,
    pub(super) start: Position,
    pub(super) end: Position,
}

pub(super) struct RawLexer<'a> {
    rest: &'a [u8],
    normalize_crlf: bool,
    pending_bom: bool,
    position: Position,
}

impl<'a> RawLexer<'a> {
    pub(super) fn new(src: &'a [u8], kind: SourceKind) -> Result<Self, ParseError> {
        let mut pending_bom = false;
        if kind == SourceKind::File {
            if has_unsupported_file_bom(src) {
                return Err(ParseError {
                    line: 0,
                    column: 0,
                    command: None,
                    message: "File starts with a Byte-Order-Mark that is not UTF-8.".into(),
                });
            }
            if src.starts_with(b"\xef\xbb\xbf") {
                pending_bom = true;
            }
        }
        Ok(Self {
            rest: src,
            normalize_crlf: kind == SourceKind::File,
            pending_bom,
            position: Position {
                offset: 0,
                line: 1,
                column: 1,
            },
        })
    }

    pub(super) fn resume(rest: &'a [u8], position: Position) -> Self {
        Self {
            rest,
            normalize_crlf: true,
            pending_bom: false,
            position,
        }
    }

    pub(super) fn position(&self) -> Position {
        self.position
    }

    pub(super) fn next_raw(&mut self) -> Option<RawToken> {
        self.next_inner(false).map(|(token, _)| token)
    }

    pub(super) fn next_with_text(&mut self) -> Option<(RawToken, BString)> {
        self.next_inner(true)
    }

    fn next_inner(&mut self, capture_text: bool) -> Option<(RawToken, BString)> {
        if self.pending_bom {
            self.pending_bom = false;
            let start = self.position;
            self.rest = self.rest.get(3..).unwrap_or_default();
            self.position.offset += 3;
            return Some((
                RawToken {
                    kind: RawKind::Bom,
                    start,
                    end: self.position,
                },
                BString::default(),
            ));
        }

        let start = self.position;
        let (kind, text) = match self.peek()?.0 {
            b' ' | b'\t' | b'\r' => {
                while matches!(self.peek(), Some((b' ' | b'\t' | b'\r', _))) {
                    self.bump();
                }
                (RawKind::Space, BString::default())
            }
            b'\n' => {
                self.bump();
                let text = if capture_text {
                    "\n".into()
                } else {
                    BString::default()
                };
                (RawKind::Newline, text)
            }
            b'(' => {
                self.bump();
                let text = if capture_text {
                    "(".into()
                } else {
                    BString::default()
                };
                (RawKind::LeftParen, text)
            }
            b')' => {
                self.bump();
                let text = if capture_text {
                    ")".into()
                } else {
                    BString::default()
                };
                (RawKind::RightParen, text)
            }
            b'"' => self.quoted(capture_text),
            b'[' => match bracket_opener(self.rest) {
                Some(equals) => self.bracket(equals, RawKind::Bracket, capture_text),
                None => self.unquoted(capture_text),
            },
            b'#' => {
                self.bump();
                if let Some(equals) = bracket_opener(self.rest) {
                    self.bracket(equals, RawKind::BracketComment, capture_text)
                } else {
                    while let Some((byte, _)) = self.peek() {
                        if byte == b'\n' {
                            break;
                        }
                        self.bump();
                    }
                    (RawKind::LineComment, BString::default())
                }
            }
            _ => self.unquoted(capture_text),
        };
        Some((
            RawToken {
                kind,
                start,
                end: self.position,
            },
            text,
        ))
    }

    fn peek(&self) -> Option<(u8, usize)> {
        match self.rest {
            [b'\r', b'\n', ..] if self.normalize_crlf => Some((b'\n', 2)),
            [byte, ..] => Some((*byte, 1)),
            [] => None,
        }
    }

    fn bump(&mut self) -> Option<u8> {
        let (byte, width) = self.peek()?;
        self.rest = self.rest.get(width..)?;
        self.position.offset += width;
        if byte == b'\n' {
            self.position.line = self.position.line.saturating_add(1);
            self.position.column = 1;
        } else {
            self.position.column = self.position.column.saturating_add(1);
        }
        Some(byte)
    }

    fn advance_inline(&mut self, remaining: &'a [u8]) {
        let consumed = self.rest.len() - remaining.len();
        self.rest = remaining;
        self.position.offset += consumed;
        let columns = u32::try_from(consumed).unwrap_or(u32::MAX);
        self.position.column = self.position.column.saturating_add(columns);
    }

    fn quoted(&mut self, capture_text: bool) -> (RawKind, BString) {
        self.bump();
        let mut text = Vec::new();
        let mut has_nul = false;
        while let Some(byte) = self.bump() {
            match byte {
                b'"' => {
                    let kind = if has_nul {
                        RawKind::BadCharacter
                    } else {
                        RawKind::Quoted
                    };
                    return (kind, text.into());
                }
                b'\\' => match self.bump() {
                    Some(b'\n') => {}
                    Some(next) => {
                        has_nul |= next == 0;
                        if capture_text {
                            text.extend_from_slice(&[byte, next]);
                        }
                    }
                    None => {
                        if capture_text {
                            text.push(byte);
                        }
                    }
                },
                _ => {
                    has_nul |= byte == 0;
                    if capture_text {
                        text.push(byte);
                    }
                }
            }
        }
        (RawKind::UnterminatedQuote, text.into())
    }

    fn bracket(&mut self, equals: usize, kind: RawKind, capture_text: bool) -> (RawKind, BString) {
        if let Some(after_opener) = self.rest.get(equals + 2..) {
            self.advance_inline(after_opener);
        }
        if matches!(self.peek(), Some((b'\n', _))) {
            self.bump();
        }

        let mut text = Vec::new();
        let mut has_nul = false;
        while let Some(byte) = self.bump() {
            if byte == b']' {
                let count = leading_equals(self.rest);
                if count == equals
                    && let Some([b']', remaining @ ..]) = self.rest.get(count..)
                {
                    self.advance_inline(remaining);
                    let final_kind = if has_nul { RawKind::BadCharacter } else { kind };
                    return (final_kind, text.into());
                }
            }
            has_nul |= byte == 0;
            if capture_text {
                text.push(byte);
            }
        }
        (RawKind::UnterminatedBracket, text.into())
    }

    fn unquoted(&mut self, capture_text: bool) -> (RawKind, BString) {
        let mut rest = self.rest;
        while let [byte, after @ ..] = rest {
            rest = match byte {
                b' ' | b'\t' | b'\r' | b'\n' | b'(' | b')' | b'#' => break,
                b'\\' => match escaped(after, self.normalize_crlf) {
                    Some(remaining) => remaining,
                    None => break,
                },
                b'"' => match legacy_quote(after, self.normalize_crlf) {
                    Some(remaining) => remaining,
                    None => break,
                },
                b'$' => match make_reference(rest) {
                    Some(remaining) => remaining,
                    None => after,
                },
                _ => after,
            };
        }

        let length = self.rest.len() - rest.len();
        if length == 0 {
            let byte = self.bump();
            let text = if capture_text {
                byte.into_iter().collect()
            } else {
                BString::default()
            };
            return (RawKind::BadCharacter, text);
        }
        let slice = self.rest.get(..length).unwrap_or_default();
        self.advance_inline(rest);
        let kind = match slice {
            [first, tail @ ..]
                if identifier_start(*first) && tail.iter().copied().all(identifier_byte) =>
            {
                RawKind::Identifier
            }
            _ => check_nul(RawKind::Unquoted, slice),
        };
        let text = if capture_text {
            slice.into()
        } else {
            BString::default()
        };
        (kind, text)
    }
}

pub(super) struct Lexer<'a> {
    raw: RawLexer<'a>,
}

impl<'a> Lexer<'a> {
    pub(super) fn new(src: &'a [u8], kind: SourceKind) -> Result<Self, ParseError> {
        let mut raw = RawLexer::new(src, kind)?;
        if raw.pending_bom {
            let _ = raw.next_raw();
        }
        Ok(Self { raw })
    }

    pub(super) fn resume(rest: &'a [u8], position: Position) -> Self {
        Self {
            raw: RawLexer::resume(rest, position),
        }
    }

    pub(super) fn position(&self) -> Position {
        self.raw.position()
    }

    pub(super) fn next(&mut self) -> Option<Token> {
        loop {
            let (raw, text) = self.raw.next_with_text()?;
            let Some(kind) = Kind::from_raw(raw.kind) else {
                continue;
            };
            return Some(Token {
                kind,
                text,
                start: raw.start,
                end: raw.end,
            });
        }
    }
}

fn has_unsupported_file_bom(src: &[u8]) -> bool {
    // A three-byte input can match the UTF-8 or UTF-16BE mark, but cannot
    // match the UTF-16LE mark because UTF-32LE has the same first two bytes.
    matches!(
        src,
        [0xff, 0xfe] | [0xff, 0xfe, _, _, ..] | [0xfe, 0xff, ..] | [0, 0, 0xfe, 0xff, ..]
    )
}

fn check_nul(kind: RawKind, text: &[u8]) -> RawKind {
    if text.contains(&0) {
        RawKind::BadCharacter
    } else {
        kind
    }
}

pub(super) fn identifier_start(byte: u8) -> bool {
    byte == b'_' || byte.is_ascii_alphabetic()
}

pub(super) fn identifier_byte(byte: u8) -> bool {
    byte == b'_' || byte.is_ascii_alphanumeric()
}

fn leading_equals(input: &[u8]) -> usize {
    input.iter().take_while(|&&byte| byte == b'=').count()
}

fn bracket_opener(input: &[u8]) -> Option<usize> {
    let after_bracket = input.strip_prefix(b"[")?;
    let equals = leading_equals(after_bracket);
    matches!(after_bracket.get(equals), Some(b'[')).then_some(equals)
}

fn escaped(after_slash: &[u8], normalize_crlf: bool) -> Option<&[u8]> {
    match after_slash {
        [b'\r', b'\n', ..] if normalize_crlf => None,
        [b'\n', ..] | [] => None,
        [_, remaining @ ..] => Some(remaining),
    }
}

fn make_reference(input: &[u8]) -> Option<&[u8]> {
    let name = input.strip_prefix(b"$(")?;
    let length = name
        .iter()
        .take_while(|&&byte| identifier_byte(byte))
        .count();
    name.get(length..)?.strip_prefix(b")")
}

fn legacy_quote(mut input: &[u8], normalize_crlf: bool) -> Option<&[u8]> {
    while let [byte, after @ ..] = input {
        input = match byte {
            b'"' => return Some(after),
            b'\r' | b'\n' | b'(' | b')' | b'#' => return None,
            b'\\' => escaped(after, normalize_crlf)?,
            b'$' => make_reference(input).unwrap_or(after),
            _ => after,
        };
    }
    None
}
