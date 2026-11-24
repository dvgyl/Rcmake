//! Recover listfile syntax for editors.

use core::ops::Range;

use rcmake_base::{BString, Limits};
use rcmake_diag::{Diagnostic, SourceId, source::SourceKind};

use crate::lexer::{Kind, Lexer, Position, Token, identifier_byte, identifier_start};
use crate::parser::{Separation, check_separation, error, unexpected};
use crate::{ArgKind, Argument, CommandInvocation, ParseError};

/// Syntax recovered from an editor buffer.
#[derive(Clone, Debug)]
pub struct TolerantParse {
    /// Registered source identifier.
    pub source: SourceId,
    /// Complete invocations in source order, including commands in open blocks.
    pub commands: Vec<CommandInvocation>,
    /// Partial invocations in source order.
    pub partial_commands: Vec<PartialCommandInvocation>,
    /// Indices into [`Self::commands`] of block openers without a matching closer.
    pub unclosed_blocks: Vec<usize>,
    /// Syntax errors found during recovery.
    pub errors: Vec<ParseError>,
    /// Non-fatal syntax warnings in source order.
    pub warnings: Vec<Diagnostic>,
}

/// An invocation that ends at a syntax error, a recovery boundary, or the end of input.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PartialCommandInvocation {
    /// Original command spelling.
    pub name: BString,
    /// ASCII-lowercased command name.
    pub lower: BString,
    /// 1-based opening line number.
    pub line: u32,
    /// Raw byte span from the command name to the end of the recovered invocation.
    pub span: Range<usize>,
    /// Raw byte span from the opening parenthesis to the end of the invocation.
    /// A command name without an opening parenthesis has no such span.
    pub parenthesis_span: Option<Range<usize>>,
    /// Unexpanded arguments in source order. The final argument can have an unclosed delimiter.
    pub args: Vec<Argument>,
}

/// Parse an editor buffer and retain as much syntax as possible.
///
/// Spans refer to the original bytes, including in buffers with CRLF or a UTF-8 BOM.
/// Flow-control errors do not prevent syntax recovery. For an invocation with no
/// matching closing parenthesis, a command-shaped line can start a new invocation.
/// If malformed multiline tokens repeatedly hide those lines, recovery limits
/// further tokens at command-shaped lines to bound the cost of rescanning.
///
/// Use [`crate::parse_list_file`] to validate a listfile before execution.
#[must_use]
pub fn parse_tolerant(source: SourceId, input: &[u8]) -> TolerantParse {
    let mut result = TolerantParse {
        source,
        commands: Vec::new(),
        partial_commands: Vec::new(),
        unclosed_blocks: Vec::new(),
        errors: Vec::new(),
        warnings: Vec::new(),
    };
    if input.len() as u64 > Limits::default().max_file_read {
        result.errors.push(ParseError {
            line: 1,
            column: 0,
            command: None,
            message: "Listfile exceeds the input size limit.".into(),
        });
        return result;
    }
    let mut start = match Lexer::new(input, SourceKind::File) {
        Ok(lexer) => lexer.position(),
        Err(error) => {
            result.errors.push(error);
            return result;
        }
    };
    let mut budget = input.len().saturating_mul(3);
    loop {
        let remaining = input.len() - start.offset;
        let bounded = budget < remaining;
        budget = budget.saturating_sub(remaining);
        let scan = || Scan::new(input, start, bounded);
        let unmatched = unmatched_openings(scan());
        let Some(next) = recover_commands(input, scan(), &unmatched, &mut result) else {
            break;
        };
        start = next;
    }
    result.unclosed_blocks = unclosed_blocks(&result.commands);
    result
}

struct Lexeme {
    token: Token,
    line_head: bool,
}

struct Scan<'a> {
    lexer: Lexer<'a>,
    remaining: &'a [u8],
    significant_line: u32,
}

impl<'a> Scan<'a> {
    fn new(input: &'a [u8], start: Position, bounded: bool) -> Self {
        let rest = input.get(start.offset..).unwrap_or_default();
        let (current, remaining) = if bounded {
            (&[][..], rest)
        } else {
            (rest, &[][..])
        };
        Self {
            lexer: Lexer::resume(current, start),
            remaining,
            significant_line: 0,
        }
    }
}

impl Iterator for Scan<'_> {
    type Item = Lexeme;

    fn next(&mut self) -> Option<Lexeme> {
        loop {
            if let Some(token) = self.lexer.next() {
                let line_head = token.start.line > self.significant_line;
                if !matches!(token.kind, Kind::Space | Kind::Newline) {
                    self.significant_line = token.end.line
                        - u32::from(token.end.line > token.start.line && token.end.column == 1);
                }
                return Some(Lexeme { token, line_head });
            }
            let mut lines = self.remaining.split_inclusive(|&byte| byte == b'\n');
            let mut width = lines.next()?.len();
            for line in lines.take_while(|line| !statement_head(line)) {
                width += line.len();
            }
            let (window, remaining) = self.remaining.split_at_checked(width)?;
            self.remaining = remaining;
            self.lexer = Lexer::resume(window, self.lexer.position());
        }
    }
}

fn unmatched_openings(scan: Scan<'_>) -> Vec<usize> {
    let mut openings = Vec::new();
    for Lexeme { token, .. } in scan {
        match token.kind {
            Kind::LeftParen => openings.push(token.start.offset),
            Kind::RightParen => {
                openings.pop();
            }
            _ => {}
        }
    }
    openings
}

type Tokens<'a> = std::iter::Peekable<Scan<'a>>;

fn recover_commands(
    input: &[u8],
    scan: Scan<'_>,
    unmatched: &[usize],
    result: &mut TolerantParse,
) -> Option<Position> {
    let mut tokens = scan.peekable();
    let mut needs_newline = false;
    while let Some(Lexeme { token, line_head }) = tokens.next() {
        match token.kind {
            Kind::Space => {}
            Kind::Newline => needs_newline = false,
            Kind::BracketComment => needs_newline = true,
            Kind::Identifier => {
                if needs_newline && !line_head {
                    result.errors.push(unexpected(
                        b"Parse error.  Expected a newline, got ",
                        &token,
                    ));
                }
                if let Some(restart) = recover_command(input, &mut tokens, unmatched, token, result)
                {
                    return Some(restart);
                }
                needs_newline = true;
            }
            _ => {
                result.errors.push(unexpected(
                    b"Parse error.  Expected a command name, got ",
                    &token,
                ));
                if let Some(restart) = hidden_statement(input, &token, false) {
                    return Some(restart);
                }
            }
        }
    }
    None
}

fn recover_command(
    input: &[u8],
    tokens: &mut Tokens<'_>,
    unmatched: &[usize],
    name: Token,
    result: &mut TolerantParse,
) -> Option<Position> {
    let mut command = PartialCommandInvocation {
        lower: name.text.as_slice().to_ascii_lowercase().into(),
        name: name.text,
        line: name.start.line,
        span: name.start.offset..name.end.offset,
        parenthesis_span: None,
        args: Vec::new(),
    };
    while tokens
        .peek()
        .is_some_and(|next| next.token.kind == Kind::Space)
    {
        tokens.next();
    }
    if !tokens
        .peek()
        .is_some_and(|next| next.token.kind == Kind::LeftParen)
    {
        if tokens.peek().is_none() {
            command.span.end = input.len();
        }
        let diagnostic = tokens.peek().map_or_else(
            || {
                error(
                    name.start,
                    "Expected an opening parenthesis after the command name.",
                )
            },
            |next| unexpected(b"Parse error.  Expected \"(\", got ", &next.token),
        );
        result.errors.push(diagnostic);
        result.partial_commands.push(command);
        return None;
    }
    let opening = tokens.next()?.token;
    command.parenthesis_span = Some(opening.start.offset..opening.end.offset);

    let has_close = unmatched.binary_search(&opening.start.offset).is_err();
    let mut depth = 0usize;
    let mut separation = Separation::Space;
    while let Some(next) = tokens.peek() {
        if !has_close && next.line_head && starts_command(input, &next.token) {
            let start = next.token.start;
            let boundary = start
                .offset
                .saturating_sub(start.column.saturating_sub(1) as usize);
            finish_partial(command, name.start, boundary, result);
            return None;
        }
        let mut token = tokens.next()?.token;
        let invalid = matches!(
            token.kind,
            Kind::UnterminatedQuote | Kind::UnterminatedBracket | Kind::BadCharacter
        );
        let suspect_delimiter = !has_close && matches!(token.kind, Kind::Quoted | Kind::Bracket);
        if (invalid || suspect_delimiter)
            && let Some(restart) = hidden_statement(input, &token, !invalid)
        {
            if let Some(prefix) = input.get(token.start.offset..restart.offset)
                && let Some(clipped) = Lexer::resume(prefix, token.start).next()
            {
                token = clipped;
            }
            result.errors.push(missing_ending(&token));
            retain_incomplete_argument(input, &mut command, token);
            save_partial(command, restart.offset, result);
            return Some(restart);
        }
        let kind = match token.kind {
            Kind::Space | Kind::Newline => {
                separation = Separation::Space;
                continue;
            }
            Kind::BracketComment => {
                separation = Separation::Bracket;
                continue;
            }
            Kind::LeftParen => {
                depth += 1;
                separation = Separation::Space;
                ArgKind::Unquoted
            }
            Kind::RightParen if depth == 0 => {
                result.commands.push(CommandInvocation {
                    name: command.name,
                    lower: command.lower,
                    line: command.line,
                    close_line: token.start.line,
                    span: command.span.start..token.end.offset,
                    args: command.args,
                });
                return None;
            }
            Kind::RightParen => {
                depth -= 1;
                separation = Separation::None;
                ArgKind::Unquoted
            }
            Kind::Bracket | Kind::Quoted | Kind::Identifier | Kind::Unquoted => {
                let kind = match token.kind {
                    Kind::Bracket => ArgKind::Bracket,
                    Kind::Quoted => ArgKind::Quoted,
                    _ => ArgKind::Unquoted,
                };
                match check_separation(separation, kind, token.start, &mut result.warnings) {
                    Ok(next) => separation = next,
                    Err(error) => {
                        result.errors.push(error);
                        separation = if kind == ArgKind::Bracket {
                            Separation::Bracket
                        } else {
                            Separation::None
                        };
                    }
                }
                kind
            }
            _ => {
                result.errors.push(missing_ending(&token));
                let end = token.end.offset;
                retain_incomplete_argument(input, &mut command, token);
                save_partial(command, end, result);
                return None;
            }
        };
        command.args.push(argument(kind, token));
    }
    finish_partial(command, name.start, input.len(), result);
    None
}

fn finish_partial(
    command: PartialCommandInvocation,
    start: Position,
    end: usize,
    result: &mut TolerantParse,
) {
    result
        .errors
        .push(error(start, "Expected a closing parenthesis."));
    save_partial(command, end, result);
}

fn save_partial(mut command: PartialCommandInvocation, end: usize, result: &mut TolerantParse) {
    command.span.end = end;
    if let Some(span) = &mut command.parenthesis_span {
        span.end = end;
    }
    result.partial_commands.push(command);
}

fn missing_ending(token: &Token) -> ParseError {
    unexpected(
        b"Parse error.  Function missing ending \")\".  Instead found ",
        token,
    )
}

fn retain_incomplete_argument(input: &[u8], command: &mut PartialCommandInvocation, token: Token) {
    let kind = match token.kind {
        Kind::UnterminatedQuote => ArgKind::Quoted,
        Kind::UnterminatedBracket if input.get(token.start.offset) != Some(&b'#') => {
            ArgKind::Bracket
        }
        _ => return,
    };
    command.args.push(argument(kind, token));
}

fn argument(kind: ArgKind, token: Token) -> Argument {
    Argument {
        kind,
        text: token.text,
        line: token.start.line,
        span: token.start.offset..token.end.offset,
    }
}

fn starts_command(input: &[u8], token: &Token) -> bool {
    token.kind == Kind::Identifier
        && input
            .get(token.end.offset..)
            .is_some_and(has_opening_parenthesis)
}

fn has_opening_parenthesis(input: &[u8]) -> bool {
    input.iter().copied().find(|&byte| !horizontal_space(byte)) == Some(b'(')
}

fn horizontal_space(byte: u8) -> bool {
    matches!(byte, b' ' | b'\t' | b'\r')
}

fn statement_head(line: &[u8]) -> bool {
    let mut code = line
        .iter()
        .copied()
        .skip_while(|&byte| horizontal_space(byte));
    code.next().is_some_and(identifier_start)
        && code
            .find(|&byte| !identifier_byte(byte))
            .is_some_and(|byte| {
                byte == b'('
                    || (horizontal_space(byte)
                        && code.find(|&byte| !horizontal_space(byte)) == Some(b'('))
            })
}

fn hidden_statement(input: &[u8], token: &Token, check_closing_line: bool) -> Option<Position> {
    if token.start.line == token.end.line {
        return None;
    }
    let bytes = input.get(token.start.offset..token.end.offset)?;
    if check_closing_line
        && !bytes
            .rsplit(|&byte| byte == b'\n')
            .next()
            .is_some_and(statement_head)
    {
        return None;
    }
    let mut lines = bytes.split_inclusive(|&byte| byte == b'\n');
    let mut offset = token.start.offset + lines.next()?.len();
    let mut line = token.start.line;
    for bytes in lines {
        line = line.saturating_add(1);
        if statement_head(bytes) {
            return Some(Position {
                offset,
                line,
                column: 1,
            });
        }
        offset += bytes.len();
    }
    None
}

fn unclosed_blocks(commands: &[CommandInvocation]) -> Vec<usize> {
    let names = [
        b"if".as_slice(),
        b"foreach",
        b"while",
        b"function",
        b"macro",
        b"block",
    ];
    let mut stack = Vec::new();
    let mut counts = [0usize; 6];
    let mut unclosed = Vec::new();
    for (index, command) in commands.iter().enumerate() {
        let name = command.lower.as_slice();
        if let Some(closer) = name.strip_prefix(b"end") {
            let Some(kind) = names.iter().position(|&name| name == closer) else {
                continue;
            };
            if counts.get(kind).copied().unwrap_or_default() == 0 {
                continue;
            }
            while let Some((open, index)) = stack.pop() {
                if let Some(count) = counts.get_mut(open) {
                    *count -= 1;
                }
                if open == kind {
                    break;
                }
                unclosed.push(index);
            }
        } else if let Some(kind) = names.iter().position(|&opener| opener == name) {
            stack.push((kind, index));
            if let Some(count) = counts.get_mut(kind) {
                *count += 1;
            }
        }
    }
    unclosed.extend(stack.into_iter().map(|(_, index)| index));
    unclosed.sort_unstable();
    unclosed
}
