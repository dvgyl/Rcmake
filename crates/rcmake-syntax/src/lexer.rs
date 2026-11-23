//! Scan normalized tokens and retain offsets into the original byte buffer.

use rcmake_base::BString;
use rcmake_diag::source::SourceKind;

use crate::ParseError;

#[derive(Clone, Copy)]
pub(super) struct Position {
    pub(super) offset: usize,
    pub(super) line: u32,
    pub(super) column: u32,
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
}

pub(super) struct Token {
    pub(super) kind: Kind,
    pub(super) text: BString,
    pub(super) start: Position,
    pub(super) end: Position,
}

pub(super) struct Lexer<'a> {
    rest: &'a [u8],
    normalize_crlf: bool,
    position: Position,
}

impl<'a> Lexer<'a> {
    pub(super) fn new(src: &'a [u8], kind: SourceKind) -> Result<Self, ParseError> {
        let mut rest = src;
        if kind == SourceKind::File {
            if src.starts_with(b"\xff\xfe")
                || src.starts_with(b"\xfe\xff")
                || src.starts_with(b"\x00\x00\xfe\xff")
            {
                return Err(ParseError {
                    line: 1,
                    column: 0,
                    command: None,
                    message: "File starts with a Byte-Order-Mark that is not UTF-8.".into(),
                });
            }
            if let Some(without_bom) = src.strip_prefix(b"\xef\xbb\xbf") {
                rest = without_bom;
            }
        }
        Ok(Self {
            rest,
            normalize_crlf: kind == SourceKind::File,
            position: Position {
                offset: src.len() - rest.len(),
                line: 1,
                column: 1,
            },
        })
    }

    pub(super) fn next(&mut self) -> Option<Token> {
        loop {
            let start = self.position;
            let (kind, text) = match self.peek()?.0 {
                b' ' | b'\t' | b'\r' => {
                    while matches!(self.peek(), Some((b' ' | b'\t' | b'\r', _))) {
                        self.bump();
                    }
                    (Kind::Space, BString::default())
                }
                b'\n' => {
                    self.bump();
                    (Kind::Newline, "\n".into())
                }
                b'(' => {
                    self.bump();
                    (Kind::LeftParen, "(".into())
                }
                b')' => {
                    self.bump();
                    (Kind::RightParen, ")".into())
                }
                b'"' => self.quoted(),
                b'[' => match bracket_opener(self.rest) {
                    Some(equals) => self.bracket(equals, Kind::Bracket),
                    None => self.unquoted(),
                },
                b'#' => {
                    self.bump();
                    if let Some(equals) = bracket_opener(self.rest) {
                        self.bracket(equals, Kind::BracketComment)
                    } else {
                        while let Some((byte, _)) = self.peek() {
                            if byte == b'\n' {
                                break;
                            }
                            self.bump();
                        }
                        continue;
                    }
                }
                _ => self.unquoted(),
            };
            return Some(Token {
                kind,
                text,
                start,
                end: self.position,
            });
        }
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

    fn quoted(&mut self) -> (Kind, BString) {
        self.bump();
        let mut text = Vec::new();
        while let Some(byte) = self.bump() {
            match byte {
                b'"' => return (check_nul(Kind::Quoted, &text), text.into()),
                b'\\' => match self.bump() {
                    Some(b'\n') => {}
                    Some(next) => text.extend_from_slice(&[byte, next]),
                    None => text.push(byte),
                },
                _ => text.push(byte),
            }
        }
        (Kind::UnterminatedQuote, text.into())
    }

    fn bracket(&mut self, equals: usize, kind: Kind) -> (Kind, BString) {
        if let Some(after_opener) = self.rest.get(equals + 2..) {
            self.advance_inline(after_opener);
        }
        if matches!(self.peek(), Some((b'\n', _))) {
            self.bump();
        }

        let mut text = Vec::new();
        while let Some(byte) = self.bump() {
            if byte == b']' {
                let count = leading_equals(self.rest);
                if count == equals
                    && let Some([b']', remaining @ ..]) = self.rest.get(count..)
                {
                    self.advance_inline(remaining);
                    return (check_nul(kind, &text), text.into());
                }
            }
            text.push(byte);
        }
        (Kind::UnterminatedBracket, text.into())
    }

    fn unquoted(&mut self) -> (Kind, BString) {
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
            return (Kind::BadCharacter, self.bump().into_iter().collect());
        }
        let text = self.rest.get(..length).unwrap_or_default();
        self.advance_inline(rest);
        let kind = match text {
            [first, tail @ ..]
                if identifier_start(*first) && tail.iter().copied().all(identifier_byte) =>
            {
                Kind::Identifier
            }
            _ => check_nul(Kind::Unquoted, text),
        };
        (kind, text.into())
    }
}

fn check_nul(kind: Kind, text: &[u8]) -> Kind {
    if text.contains(&0) {
        Kind::BadCharacter
    } else {
        kind
    }
}

fn identifier_start(byte: u8) -> bool {
    byte == b'_' || byte.is_ascii_alphabetic()
}

fn identifier_byte(byte: u8) -> bool {
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
