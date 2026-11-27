//! Lossless concrete syntax view for formatting and static analysis tools.
//!
//! Produces a flat, contiguous token sequence covering `0..src.len()` (including
//! UTF-8 BOM, horizontal space, physical newlines, and line/bracket comments)
//! alongside flat index associations into the strict [`ListFile`].

#![forbid(unsafe_code)]

use core::ops::Range;

use rcmake_base::Limits;
use rcmake_diag::{Diagnostic, SourceId, source::SourceKind};

use crate::lexer::{Kind, RawKind, RawLexer, Token as ParserToken};
use crate::parser::{Separation, check_separation, error, unexpected, validate_flow_control};
use crate::{ArgKind, Argument, CommandInvocation, ListFile, ParseError};

/// Lossless token classification covering every byte from `0` to `src.len()`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TokenKind {
    /// Leading UTF-8 byte-order mark (`\xef\xbb\xbf`) on [`SourceKind::File`].
    Bom,
    /// Horizontal space (`' '`, `'\t'`, or bare `'\r'`).
    Space,
    /// One physical newline (`"\n"` or `"\r\n"` for [`SourceKind::File`]).
    Newline,
    /// Line comment (`#...`) excluding the terminating newline.
    LineComment,
    /// Bracket comment (`#[=[...]=]`).
    BracketComment,
    /// Unquoted identifier (`[A-Za-z_][A-Za-z0-9_]*`).
    Identifier,
    /// Non-identifier unquoted argument.
    Unquoted,
    /// Double-quoted argument (`"..."`).
    Quoted,
    /// Bracket argument (`[=[...]=]`).
    Bracket,
    /// Left parenthesis (`(`).
    LeftParen,
    /// Right parenthesis (`)`).
    RightParen,
    /// Unterminated quoted argument.
    UnterminatedQuote,
    /// Unterminated bracket argument or bracket comment.
    UnterminatedBracket,
    /// Invalid character or embedded NUL byte.
    BadCharacter,
}

impl TokenKind {
    const fn from_raw(kind: RawKind) -> Self {
        match kind {
            RawKind::Bom => Self::Bom,
            RawKind::Space => Self::Space,
            RawKind::Newline => Self::Newline,
            RawKind::LineComment => Self::LineComment,
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
        }
    }

    /// Return `true` if this token is horizontal space or a physical newline.
    #[must_use]
    pub const fn is_whitespace(self) -> bool {
        matches!(self, Self::Space | Self::Newline)
    }

    /// Return `true` if this token is a line or bracket comment.
    #[must_use]
    pub const fn is_comment(self) -> bool {
        matches!(self, Self::LineComment | Self::BracketComment)
    }
}

/// One lossless token referencing a raw byte range in the original buffer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Token {
    /// Token classification.
    pub kind: TokenKind,
    /// Contiguous raw byte range in the original source slice.
    pub span: Range<usize>,
    /// 1-based starting line number.
    pub start_line: u32,
    /// 1-based starting byte column number.
    pub start_column: u32,
    /// 1-based ending line number.
    pub end_line: u32,
    /// 1-based ending byte column number.
    pub end_column: u32,
}

impl Token {
    /// Return the exact raw bytes covered by this token in `src`.
    #[must_use]
    pub fn slice<'a>(&self, src: &'a [u8]) -> &'a [u8] {
        src.get(self.span.clone()).unwrap_or_default()
    }
}

/// Association between a parsed [`Argument`] and its lossless [`Token`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConcreteArgument {
    /// Index of this argument's token inside [`ConcreteListFile::tokens`].
    pub token_index: usize,
    /// Index of this argument inside [`CommandInvocation::args`].
    pub arg_index: usize,
    /// Delimiter kind.
    pub kind: ArgKind,
    /// Raw byte span in the original source buffer.
    pub span: Range<usize>,
    /// Nested parenthesis depth inside the command's argument list (`0` at top level).
    /// Nested `(` and `)` tokens record the depth outside that pair.
    pub paren_depth: u32,
}

/// Association between a parsed [`CommandInvocation`] and its lossless token range.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConcreteCommand {
    /// Index of this command inside [`ListFile::commands`].
    pub command_index: usize,
    /// Index of the command identifier token in [`ConcreteListFile::tokens`].
    pub name_token: usize,
    /// Index of the opening `(` token in [`ConcreteListFile::tokens`].
    pub open_paren_token: usize,
    /// Index of the closing `)` token in [`ConcreteListFile::tokens`].
    pub close_paren_token: usize,
    /// Half-open token range `name_token..close_paren_token + 1`.
    pub token_range: Range<usize>,
    /// Ordered argument associations matching [`CommandInvocation::args`].
    pub args: Vec<ConcreteArgument>,
}

/// Token attachment metadata for a non-fatal syntax warning in [`ListFile::warnings`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AttachedWarning {
    /// Index into [`ListFile::warnings`].
    pub diagnostic_index: usize,
    /// Index of the containing command in [`ListFile::commands`].
    pub command_index: usize,
    /// Index of the warning-triggering argument in [`CommandInvocation::args`].
    pub arg_index: usize,
    /// Index of the warning-triggering token in [`ConcreteListFile::tokens`].
    pub token_index: usize,
}

/// Strict parsed listfile paired with its lossless flat token sequence.
#[derive(Clone, Debug)]
pub struct ConcreteListFile {
    /// Strict parsed commands and syntax warnings.
    pub list_file: ListFile,
    /// Flat contiguous token sequence covering `0..src.len()`.
    pub tokens: Vec<Token>,
    /// Flat per-command token index associations matching `list_file.commands`.
    pub commands: Vec<ConcreteCommand>,
    /// Token attachment records for every warning in `list_file.warnings`.
    pub attached_warnings: Vec<AttachedWarning>,
}

/// Lex `src` into a lossless contiguous token sequence covering `0..src.len()`.
///
/// Does not allocate decoded argument text buffers. Rejects unsupported UTF-16/UTF-32
/// BOMs when `kind` is [`SourceKind::File`].
pub fn lex_tokens(src: &[u8], kind: SourceKind) -> Result<Vec<Token>, ParseError> {
    let mut lexer = RawLexer::new(src, kind)?;
    let mut tokens = Vec::new();
    while let Some(raw) = lexer.next_raw() {
        tokens.push(Token {
            kind: TokenKind::from_raw(raw.kind),
            span: raw.start.offset..raw.end.offset,
            start_line: raw.start.line,
            start_column: raw.start.column,
            end_line: raw.end.line,
            end_column: raw.end.column,
        });
    }
    Ok(tokens)
}

/// Strictly parse `src` and retain the lossless flat concrete token sequence.
pub fn parse_concrete(
    src: &[u8],
    source: SourceId,
    kind: SourceKind,
    limits: &Limits,
) -> Result<ConcreteListFile, (ParseError, Vec<Diagnostic>)> {
    parse_concrete_with_budget(src, source, kind, limits, usize::MAX)
}

/// Strictly parse `src` with an explicit upper bound on the number of lossless tokens.
pub fn parse_concrete_with_budget(
    src: &[u8],
    source: SourceId,
    kind: SourceKind,
    limits: &Limits,
    max_tokens: usize,
) -> Result<ConcreteListFile, (ParseError, Vec<Diagnostic>)> {
    let over_limit = match kind {
        SourceKind::File => src.len() as u64 > limits.max_file_read,
        SourceKind::VirtualEval => src.len() > limits.max_value_bytes,
    };
    if over_limit {
        return Err((
            ParseError {
                line: 1,
                column: 0,
                command: None,
                message: "Listfile exceeds the input size limit.".into(),
            },
            Vec::new(),
        ));
    }

    let mut warnings = Vec::new();
    let mut attached_warnings = Vec::new();
    let mut tokens = Vec::new();
    let mut commands = Vec::new();
    let mut concrete_commands = Vec::new();

    let mut lexer = match RawLexer::new(src, kind) {
        Ok(lexer) => lexer,
        Err(err) => return Err((err, warnings)),
    };

    let mut need_newline = false;
    loop {
        let next = match next_parser_token(&mut lexer, &mut tokens, max_tokens) {
            Ok(Some(item)) => item,
            Ok(None) => break,
            Err(err) => return Err((err, warnings)),
        };
        let (token_index, token) = next;
        match token.kind {
            Kind::Space => {}
            Kind::Newline => need_newline = false,
            Kind::BracketComment => need_newline = true,
            Kind::Identifier if need_newline => {
                return Err((
                    unexpected(b"Parse error.  Expected a newline, got ", &token),
                    warnings,
                ));
            }
            Kind::Identifier => {
                let command_index = commands.len();
                match parse_concrete_command(
                    &mut lexer,
                    &mut tokens,
                    max_tokens,
                    command_index,
                    token_index,
                    token,
                    &mut warnings,
                    &mut attached_warnings,
                ) {
                    Ok((cmd, concrete_cmd)) => {
                        commands.push(cmd);
                        concrete_commands.push(concrete_cmd);
                        need_newline = true;
                    }
                    Err(err) => return Err((err, warnings)),
                }
            }
            _ => {
                return Err((
                    unexpected(b"Parse error.  Expected a command name, got ", &token),
                    warnings,
                ));
            }
        }
    }

    if let Err(err) = validate_flow_control(&commands) {
        return Err((err, warnings));
    }

    Ok(ConcreteListFile {
        list_file: ListFile {
            source,
            commands: commands.into(),
            warnings,
        },
        tokens,
        commands: concrete_commands,
        attached_warnings,
    })
}

fn next_parser_token(
    lexer: &mut RawLexer<'_>,
    tokens: &mut Vec<Token>,
    max_tokens: usize,
) -> Result<Option<(usize, ParserToken)>, ParseError> {
    loop {
        let Some((raw, text)) = lexer.next_with_text() else {
            return Ok(None);
        };
        if tokens.len() >= max_tokens {
            return Err(ParseError {
                line: raw.start.line,
                column: raw.start.column,
                command: None,
                message: "Listfile exceeds the concrete token count limit.".into(),
            });
        }
        let token_index = tokens.len();
        tokens.push(Token {
            kind: TokenKind::from_raw(raw.kind),
            span: raw.start.offset..raw.end.offset,
            start_line: raw.start.line,
            start_column: raw.start.column,
            end_line: raw.end.line,
            end_column: raw.end.column,
        });
        let Some(kind) = Kind::from_raw(raw.kind) else {
            continue;
        };
        return Ok(Some((
            token_index,
            ParserToken {
                kind,
                text,
                start: raw.start,
                end: raw.end,
            },
        )));
    }
}

#[allow(clippy::too_many_arguments)]
fn parse_concrete_command(
    lexer: &mut RawLexer<'_>,
    tokens: &mut Vec<Token>,
    max_tokens: usize,
    command_index: usize,
    name_token: usize,
    name: ParserToken,
    warnings: &mut Vec<Diagnostic>,
    attached_warnings: &mut Vec<AttachedWarning>,
) -> Result<(CommandInvocation, ConcreteCommand), ParseError> {
    let (open_paren_token, opening) = loop {
        match next_parser_token(lexer, tokens, max_tokens)? {
            Some((_, token)) if token.kind == Kind::Space => continue,
            Some(item) => break item,
            None => {
                return Err(error(
                    name.start,
                    "Unexpected end of file.\n\nParse error.  Function missing opening \"(\".",
                ));
            }
        }
    };
    if opening.kind != Kind::LeftParen {
        return Err(unexpected(b"Parse error.  Expected \"(\", got ", &opening));
    }

    let mut args = Vec::new();
    let mut concrete_args = Vec::new();
    let mut depth: u32 = 0;
    let mut separation = Separation::Space;

    while let Some((token_index, token)) = next_parser_token(lexer, tokens, max_tokens)? {
        let (kind, arg_depth) = match token.kind {
            Kind::Space | Kind::Newline => {
                separation = Separation::Space;
                continue;
            }
            Kind::BracketComment => {
                separation = Separation::Bracket;
                continue;
            }
            Kind::LeftParen => {
                let outer_depth = depth;
                depth = depth.saturating_add(1);
                separation = Separation::Space;
                (ArgKind::Unquoted, outer_depth)
            }
            Kind::RightParen if depth == 0 => {
                let lower = name.text.as_slice().to_ascii_lowercase().into();
                let cmd = CommandInvocation {
                    name: name.text,
                    lower,
                    line: name.start.line,
                    close_line: token.start.line,
                    span: name.start.offset..token.end.offset,
                    args,
                };
                let concrete = ConcreteCommand {
                    command_index,
                    name_token,
                    open_paren_token,
                    close_paren_token: token_index,
                    token_range: name_token..token_index + 1,
                    args: concrete_args,
                };
                return Ok((cmd, concrete));
            }
            Kind::RightParen => {
                depth = depth.saturating_sub(1);
                separation = Separation::None;
                (ArgKind::Unquoted, depth)
            }
            Kind::Bracket | Kind::Quoted | Kind::Identifier | Kind::Unquoted => {
                let kind = match token.kind {
                    Kind::Bracket => ArgKind::Bracket,
                    Kind::Quoted => ArgKind::Quoted,
                    _ => ArgKind::Unquoted,
                };
                let warnings_before = warnings.len();
                separation = check_separation(separation, kind, token.start, warnings)?;
                if warnings.len() > warnings_before {
                    attached_warnings.push(AttachedWarning {
                        diagnostic_index: warnings_before,
                        command_index,
                        arg_index: args.len(),
                        token_index,
                    });
                }
                (kind, depth)
            }
            _ => {
                return Err(unexpected(
                    b"Parse error.  Function missing ending \")\".  Instead found ",
                    &token,
                ));
            }
        };
        let span = token.start.offset..token.end.offset;
        let arg_index = args.len();
        args.push(Argument {
            kind,
            text: token.text,
            line: token.start.line,
            span: span.clone(),
        });
        concrete_args.push(ConcreteArgument {
            token_index,
            arg_index,
            kind,
            span,
            paren_depth: arg_depth,
        });
    }

    Err(error(
        name.start,
        "Parse error.  Function missing ending \")\".  End of file reached.",
    ))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use rcmake_diag::SourceMap;

    use super::*;
    use crate::{parse_list_file_with_warnings, parse_tolerant};

    fn assert_contiguous(src: &[u8], tokens: &[Token]) {
        if src.is_empty() {
            assert!(tokens.is_empty());
            return;
        }
        assert_eq!(tokens.first().map(|t| t.span.start), Some(0));
        assert_eq!(tokens.last().map(|t| t.span.end), Some(src.len()));
        let mut reconstructed = Vec::with_capacity(src.len());
        for pair in tokens.windows(2) {
            assert_eq!(pair[0].span.end, pair[1].span.start);
        }
        for tok in tokens {
            assert!(tok.span.start < tok.span.end);
            reconstructed.extend_from_slice(tok.slice(src));
        }
        assert_eq!(reconstructed.as_slice(), src);
    }

    fn source_id(src: &[u8], kind: SourceKind) -> SourceId {
        let mut map = SourceMap::default();
        map.add("test.cmake".into(), kind, Arc::from(src))
            .expect("source id")
    }

    fn diag_line(diag: &Diagnostic) -> Option<u32> {
        match &diag.location {
            rcmake_diag::Location::FileLine { line, .. } => Some(*line),
            _ => None,
        }
    }

    #[test]
    fn concrete_tokens_cover_every_byte_and_match_strict_parser() {
        let samples: &[&[u8]] = &[
            b"",
            b"\xef\xbb\xbf",
            b"\xef\xbb\xbf# lead\r\nset(A \"a\rb\r\nc\" [=[\r\nd\re\r\n]=]) # tail",
            b"if((A AND B) OR \"x\"y)\n  #[=[\n  comment\n  ]=]\n  message(STATUS \xfe \"\xff\")\nendif()",
            b"set(X a\\\r\nb) # bare\rcr comment\r\nmessage(done) # eof comment",
            b"#[=[bracket comment at eof]=]",
            b"x(\t\"\xc3\xa9\"y)\n",
        ];

        let limits = Limits::default();
        for &sample in samples {
            for kind in [SourceKind::File, SourceKind::VirtualEval] {
                let sid = source_id(sample, kind);
                let strict = parse_list_file_with_warnings(sample, sid, kind, &limits);
                let concrete = parse_concrete(sample, sid, kind, &limits);

                if let Ok(raw) = lex_tokens(sample, kind) {
                    assert_contiguous(sample, &raw);
                }

                match (strict, concrete) {
                    (Ok(s), Ok(c)) => {
                        assert_contiguous(sample, &c.tokens);
                        assert_eq!(c.list_file.commands.as_ref(), s.commands.as_ref());
                        assert_eq!(c.list_file.warnings.len(), s.warnings.len());
                        for (cw, sw) in c.list_file.warnings.iter().zip(&s.warnings) {
                            assert_eq!(cw.body, sw.body);
                            assert_eq!(diag_line(cw), diag_line(sw));
                            assert_eq!(cw.kind, sw.kind);
                        }
                        assert_eq!(c.attached_warnings.len(), s.warnings.len());
                        assert_eq!(c.commands.len(), s.commands.len());
                        for (cc, sc) in c.commands.iter().zip(s.commands.iter()) {
                            assert_eq!(c.tokens[cc.name_token].slice(sample), sc.name.as_slice());
                            assert_eq!(c.tokens[cc.open_paren_token].kind, TokenKind::LeftParen);
                            assert_eq!(c.tokens[cc.close_paren_token].kind, TokenKind::RightParen);
                            assert_eq!(cc.args.len(), sc.args.len());
                            for (ca, sa) in cc.args.iter().zip(&sc.args) {
                                assert_eq!(ca.span, sa.span);
                                assert_eq!(c.tokens[ca.token_index].span, sa.span);
                            }
                        }
                    }
                    (Err((se, sw)), Err((ce, cw))) => {
                        assert_eq!(ce, se);
                        assert_eq!(cw.len(), sw.len());
                        for (a, b) in cw.iter().zip(&sw) {
                            assert_eq!(a.body, b.body);
                            assert_eq!(diag_line(a), diag_line(b));
                        }
                    }
                    (s, c) => {
                        panic!("mismatch for {sample:?} ({kind:?}): strict={s:?} concrete={c:?}")
                    }
                }
            }
        }
    }

    #[test]
    fn concrete_handles_deep_nesting_iteratively_and_enforces_token_budget() {
        let mut src = b"x(".to_vec();
        src.extend(std::iter::repeat_n(b'(', 12_000));
        src.extend(std::iter::repeat_n(b')', 12_001));
        let sid = source_id(&src, SourceKind::File);
        let parsed = parse_concrete(&src, sid, SourceKind::File, &Limits::default())
            .expect("deep nesting parses iteratively");
        assert_contiguous(&src, &parsed.tokens);
        assert_eq!(parsed.commands[0].args.len(), 24_000);
        assert_eq!(parsed.commands[0].args[0].paren_depth, 0);
        assert_eq!(parsed.commands[0].args[11_999].paren_depth, 11_999);
        assert_eq!(parsed.commands[0].args[12_000].paren_depth, 11_999);
        assert_eq!(parsed.commands[0].args[23_999].paren_depth, 0);

        let err = parse_concrete_with_budget(&src, sid, SourceKind::File, &Limits::default(), 100)
            .expect_err("token budget enforced");
        assert!(
            err.0
                .message
                .as_slice()
                .windows(b"token count limit".len())
                .any(|w| w == b"token count limit")
        );
    }

    #[test]
    fn bare_cr_is_horizontal_space_and_preserves_warning_columns_after_bom() {
        let src = b"\xef\xbb\xbfx(\r\"\xc3\xa9\"y)\r\n";
        let sid = source_id(src, SourceKind::File);
        let parsed = parse_concrete(src, sid, SourceKind::File, &Limits::default()).expect("ok");
        assert_contiguous(src, &parsed.tokens);
        assert_eq!(parsed.tokens[0].kind, TokenKind::Bom);
        assert_eq!(parsed.tokens[3].kind, TokenKind::Space);
        assert_eq!(parsed.tokens[3].slice(src), b"\r");
        assert_eq!(parsed.attached_warnings.len(), 1);
        assert_eq!(parsed.attached_warnings[0].command_index, 0);
        assert_eq!(parsed.attached_warnings[0].arg_index, 1);
        assert_eq!(
            parsed.list_file.warnings[0].body.as_slice(),
            b"Syntax Warning in cmake code at column 8\n\nArgument not separated from preceding token by whitespace."
        );

        let tolerant = parse_tolerant(sid, src);
        assert_eq!(tolerant.warnings.len(), 1);
        assert_eq!(tolerant.warnings[0].body, parsed.list_file.warnings[0].body);
        assert_eq!(
            tolerant.commands.as_slice(),
            parsed.list_file.commands.as_ref()
        );
    }
}
