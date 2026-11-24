//! Recover listfile syntax for editors.

use core::cell::Cell;
use core::ops::Range;

use rcmake_base::BString;
use rcmake_diag::{Diagnostic, SourceId, source::SourceKind};

use crate::lexer::{Kind, Lexer, Position, Token, identifier_byte, identifier_start};
use crate::parser::{Separation, check_separation, error, unexpected};
use crate::{ArgKind, Argument, CommandInvocation, ParseError};

const MAX_INPUT_BYTES: usize = 2 * 1024 * 1024;
const MAX_TOKENS: usize = 128 * 1024;
// Each output token can add an argument or invocation and up to two diagnostics.
const MAX_OUTPUT_TOKENS: usize = 64 * 1024;
const MAX_NESTING: usize = 4 * 1024;

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
    /// To find partial block openers, read [`Self::partial_commands`].
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
    /// Unexpanded arguments in source order.
    pub args: Vec<Argument>,
    /// Index into [`Self::args`] of an argument with an unclosed quote or bracket.
    /// If present, this is the final argument. `None` means all argument delimiters are closed.
    pub unclosed_argument: Option<usize>,
}

/// Parse an editor buffer and retain as much syntax as possible.
///
/// Spans refer to the original bytes, including in buffers with CRLF or a UTF-8 BOM.
/// Flow-control errors do not prevent syntax recovery. For an invocation with no
/// matching closing parenthesis, a command-shaped line can start a new invocation.
/// Recovery limits repeat scans. It returns a diagnostic if the buffer exceeds
/// the editor limits for bytes, tokens, nesting, or returned syntax.
///
/// Use [`crate::parse_list_file`] to validate a listfile before execution.
#[must_use]
pub fn parse_tolerant(source: SourceId, input: &[u8]) -> TolerantParse {
    let mut result = empty_parse(source);
    if input.len() > MAX_INPUT_BYTES {
        result.errors.push(limit_error("editor input size"));
        return result;
    }
    let start = match Lexer::new(input, SourceKind::File) {
        Ok(lexer) => lexer.position(),
        Err(error) => {
            result.errors.push(error);
            return result;
        }
    };
    if let Err(error) = recover(input, start, &mut result) {
        result = empty_parse(source);
        result.errors.push(error);
    }
    result
}

fn empty_parse(source: SourceId) -> TolerantParse {
    TolerantParse {
        source,
        commands: Vec::new(),
        partial_commands: Vec::new(),
        unclosed_blocks: Vec::new(),
        errors: Vec::new(),
        warnings: Vec::new(),
    }
}

fn recover(
    input: &[u8],
    mut start: Position,
    result: &mut TolerantParse,
) -> Result<(), ParseError> {
    let heads = statement_heads(input, start)?;
    let mut remaining_heads = heads.as_slice();
    let output_left = Cell::new(MAX_OUTPUT_TOKENS);
    let mut budget = input.len() * 3;
    loop {
        let remaining = input.len() - start.offset;
        let fallback = (budget < remaining).then(|| DelimiterEnds::new(input));
        budget = budget.saturating_sub(remaining);
        let scan = |output| Scan::new(input, start, remaining_heads, fallback.as_ref(), output);
        let unmatched = unmatched_openings(scan(None))?;
        let next = recover_commands(input, scan(Some(&output_left)), &unmatched, result);
        if output_left.get() == 0 {
            return Err(limit_error("editor output"));
        }
        let Some(next) = next else {
            break;
        };
        start = next;
        while let Some((head, rest)) = remaining_heads.split_first() {
            if head.start.offset > start.offset {
                break;
            }
            remaining_heads = rest;
        }
    }
    result.unclosed_blocks = unclosed_blocks(&result.commands)?;
    Ok(())
}

fn limit_error(limit: &str) -> ParseError {
    ParseError {
        line: 1,
        column: 0,
        command: None,
        message: format!("Listfile exceeds the {limit} limit.").into(),
    }
}

struct StatementHead {
    start: Position,
    opening: usize,
}

struct Lexeme<'a> {
    token: Token,
    line_head: bool,
    hidden_heads: &'a [StatementHead],
}

struct Scan<'a> {
    input: &'a [u8],
    lexer: Lexer<'a>,
    heads: &'a [StatementHead],
    fallback: Option<&'a DelimiterEnds>,
    output_left: Option<&'a Cell<usize>>,
    significant_line: u32,
}

impl<'a> Scan<'a> {
    fn new(
        input: &'a [u8],
        start: Position,
        heads: &'a [StatementHead],
        fallback: Option<&'a DelimiterEnds>,
        output_left: Option<&'a Cell<usize>>,
    ) -> Self {
        let rest = input.get(start.offset..).unwrap_or_default();
        Self {
            input,
            lexer: Lexer::resume(rest, start),
            heads,
            fallback,
            output_left,
            significant_line: 0,
        }
    }

    fn clip(&mut self, start: Position, boundary: Position) -> Option<Token> {
        let token = clipped_token(self.input, start, boundary)?;
        self.lexer = Lexer::resume(self.input.get(boundary.offset..)?, boundary);
        Some(token)
    }
}

impl<'a> Iterator for Scan<'a> {
    type Item = Lexeme<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.output_left.is_some_and(|left| left.get() == 0) {
            return None;
        }
        let start = self.lexer.position();
        while let Some((head, rest)) = self.heads.split_first() {
            if head.start.offset > start.offset {
                break;
            }
            self.heads = rest;
        }
        let mut token = if let Some(ends) = self.fallback
            && let Some(head) = self.heads.first()
            && ends.is_unterminated(self.input, start.offset)
        {
            self.clip(start, head.start)?
        } else {
            self.lexer.next()?
        };
        let mut hidden_heads = &[] as &[StatementHead];
        let mut following_head = None;
        if matches!(
            token.kind,
            Kind::Quoted | Kind::UnterminatedQuote | Kind::UnterminatedBracket
        ) && token.start.line != token.end.line
        {
            let count = self
                .heads
                .iter()
                .take_while(|head| head.start.offset < token.end.offset)
                .count();
            hidden_heads = self.heads.get(..count).unwrap_or_default();
            following_head = self.heads.get(count);
        }
        if self.fallback.is_some() {
            // After the repeat-scan budget, repair quotes in place and keep bracket tokens intact.
            if token.kind == Kind::Quoted
                && let Some(boundary) = hidden_quote(self.input, &token, hidden_heads)
                && !quote_can_end_here(self.input, &token, following_head)
            {
                token = self.clip(start, boundary)?;
            }
            hidden_heads = &[];
        }
        let line_head = token.start.line > self.significant_line;
        if !matches!(token.kind, Kind::Space | Kind::Newline) {
            self.significant_line = token.end.line
                - u32::from(token.end.line > token.start.line && token.end.column == 1);
        }
        if let Some(left) = self.output_left {
            left.set(left.get() - 1);
        }
        Some(Lexeme {
            token,
            line_head,
            hidden_heads,
        })
    }
}

fn unmatched_openings(scan: Scan<'_>) -> Result<Vec<usize>, ParseError> {
    let mut openings = Vec::new();
    for (index, Lexeme { token, .. }) in scan.enumerate() {
        if index >= MAX_TOKENS {
            return Err(limit_error("editor token count"));
        }
        match token.kind {
            Kind::LeftParen => {
                if openings.len() == MAX_NESTING {
                    return Err(limit_error("editor parenthesis nesting"));
                }
                openings.push(token.start.offset);
            }
            Kind::RightParen => {
                openings.pop();
            }
            _ => {}
        }
    }
    Ok(openings)
}

type Tokens<'a> = std::iter::Peekable<Scan<'a>>;

fn recover_commands(
    input: &[u8],
    scan: Scan<'_>,
    mut unmatched: &[usize],
    result: &mut TolerantParse,
) -> Option<Position> {
    let mut tokens = scan.peekable();
    let mut needs_newline = false;
    while let Some(Lexeme {
        token,
        line_head,
        hidden_heads,
    }) = tokens.next()
    {
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
                if let Some(restart) =
                    recover_command(input, &mut tokens, &mut unmatched, token, result)
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
                if matches!(
                    token.kind,
                    Kind::UnterminatedQuote | Kind::UnterminatedBracket
                ) && let Some(head) = hidden_heads.first()
                {
                    return Some(head.start);
                }
            }
        }
    }
    None
}

fn recover_command(
    input: &[u8],
    tokens: &mut Tokens<'_>,
    unmatched: &mut &[usize],
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
        unclosed_argument: None,
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

    while let Some((offset, rest)) = unmatched.split_first() {
        if *offset >= opening.start.offset {
            break;
        }
        *unmatched = rest;
    }
    let has_close = unmatched.first() != Some(&opening.start.offset);
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
        let Lexeme {
            token,
            hidden_heads,
            ..
        } = tokens.next()?;
        let restart = match token.kind {
            Kind::UnterminatedQuote | Kind::UnterminatedBracket => {
                hidden_heads.first().map(|head| head.start)
            }
            Kind::Quoted if !has_close => hidden_quote(input, &token, hidden_heads),
            _ => None,
        };
        if let Some(restart) = restart {
            if let Some(clipped) = clipped_token(input, token.start, restart) {
                result.errors.push(missing_ending(&clipped));
                retain_incomplete_argument(input, &mut command, clipped);
            }
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
    command.unclosed_argument = Some(command.args.len());
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

fn statement_heads(input: &[u8], mut start: Position) -> Result<Vec<StatementHead>, ParseError> {
    let mut heads = Vec::new();
    let rest = input.get(start.offset..).unwrap_or_default();
    for line in rest.split_inclusive(|&byte| byte == b'\n') {
        if let Some(opening) = statement_opening(line) {
            if heads.len() >= MAX_TOKENS {
                return Err(limit_error("editor command head count"));
            }
            heads.push(StatementHead {
                start,
                opening: start.offset + opening,
            });
        }
        start.offset += line.len();
        start.line += 1;
        start.column = 1;
    }
    Ok(heads)
}

fn statement_opening(line: &[u8]) -> Option<usize> {
    let mut bytes = line.iter().copied().enumerate();
    let (_, first) = bytes.find(|&(_, byte)| !horizontal_space(byte))?;
    if !identifier_start(first) {
        return None;
    }
    let (offset, byte) = bytes.find(|&(_, byte)| !identifier_byte(byte))?;
    if byte == b'(' {
        Some(offset)
    } else if horizontal_space(byte) {
        let (offset, byte) = bytes.find(|&(_, byte)| !horizontal_space(byte))?;
        (byte == b'(').then_some(offset)
    } else {
        None
    }
}

fn clipped_token(input: &[u8], start: Position, boundary: Position) -> Option<Token> {
    Lexer::resume(input.get(start.offset..boundary.offset)?, start).next()
}

// A later command can supply the apparent closing quote of an open argument.
// A complete command inside the text cannot supply that delimiter.
fn hidden_quote(input: &[u8], token: &Token, heads: &[StatementHead]) -> Option<Position> {
    let first = heads.first()?.start;
    let quote = token.end.offset.checked_sub(1)?;
    let mut lexer = Lexer::resume(input.get(first.offset..quote)?, first);
    let mut heads = heads.iter().peekable();
    let mut command = None;
    let mut depth = 0;
    while let Some(token) = lexer.next() {
        while heads
            .peek()
            .is_some_and(|head| head.opening < token.start.offset)
        {
            heads.next();
        }
        match token.kind {
            Kind::LeftParen if depth != 0 => depth += 1,
            Kind::LeftParen
                if heads
                    .peek()
                    .is_some_and(|head| head.opening == token.start.offset) =>
            {
                command = heads.next().map(|head| head.start);
                depth = 1;
            }
            Kind::RightParen if depth != 0 => {
                depth -= 1;
                if depth == 0 {
                    command = None;
                }
            }
            Kind::UnterminatedBracket => return command,
            Kind::UnterminatedQuote => return None,
            _ => {}
        }
    }
    command
}

fn quote_can_end_here(input: &[u8], token: &Token, next_head: Option<&StatementHead>) -> bool {
    let end = next_head.map_or(input.len(), |head| head.start.offset);
    let rest = input.get(token.end.offset..end).unwrap_or_default();
    // Stop at the next quote. Each lookahead scans a different part of the buffer.
    let width = rest
        .iter()
        .position(|&byte| byte == b'"')
        .unwrap_or(rest.len());
    let before_quote = rest.get(..width).unwrap_or_default();
    if before_quote.iter().all(u8::is_ascii_whitespace) {
        return if width < rest.len() {
            width != 0
        } else {
            next_head.is_none()
        };
    }
    if let Some(after_quote) = rest.get(width + 1..)
        && before_quote.last().is_some_and(u8::is_ascii_whitespace)
        && after_quote
            .iter()
            .copied()
            .find(|&byte| !horizontal_space(byte))
            != Some(b')')
    {
        return true;
    }
    let mut lexer = Lexer::resume(before_quote, token.end);
    let mut depth = 0;
    while let Some(token) = lexer.next() {
        match token.kind {
            Kind::LeftParen => depth += 1,
            Kind::RightParen if depth == 0 => return true,
            Kind::RightParen => depth -= 1,
            _ => {}
        }
    }
    false
}

// The last closer for each bracket width is sufficient to detect an open bracket.
// Check it before asking the lexer to read to the end of a damaged buffer.
struct DelimiterEnds {
    quote: usize,
    brackets: Vec<usize>,
}

impl DelimiterEnds {
    fn new(input: &[u8]) -> Self {
        let mut ends = Self {
            quote: 0,
            brackets: Vec::new(),
        };
        let mut escaped = false;
        for (offset, &byte) in input.iter().enumerate() {
            if byte == b'"' && !escaped {
                ends.quote = offset;
            }
            escaped = byte == b'\\' && !escaped;
            if byte == b']'
                && let Some(rest) = input.get(offset + 1..)
                && let Some(width) = bracket_width(rest, b']')
            {
                if ends.brackets.len() <= width {
                    ends.brackets.resize(width + 1, 0);
                }
                if let Some(closer) = ends.brackets.get_mut(width) {
                    *closer = offset;
                }
            }
        }
        ends
    }

    fn is_unterminated(&self, input: &[u8], offset: usize) -> bool {
        match input.get(offset..) {
            Some([b'"', ..]) => self.quote <= offset,
            Some([b'#', b'[', rest @ ..] | [b'[', rest @ ..]) => bracket_width(rest, b'[')
                .is_some_and(|width| {
                    self.brackets
                        .get(width)
                        .is_none_or(|&closer| closer <= offset)
                }),
            _ => false,
        }
    }
}

fn bracket_width(input: &[u8], delimiter: u8) -> Option<usize> {
    let width = input.iter().take_while(|&&byte| byte == b'=').count();
    (input.get(width) == Some(&delimiter)).then_some(width)
}

fn unclosed_blocks(commands: &[CommandInvocation]) -> Result<Vec<usize>, ParseError> {
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
    let mut openers = Vec::new();
    for (index, command) in commands.iter().enumerate() {
        let name = command.lower.as_slice();
        if let Some(closer) = name.strip_prefix(b"end") {
            let Some(kind) = names.iter().position(|&name| name == closer) else {
                continue;
            };
            if counts.get(kind).copied().unwrap_or_default() == 0 {
                continue;
            }
            while let Some((open, slot)) = stack.pop() {
                if let Some(count) = counts.get_mut(open) {
                    *count -= 1;
                }
                if open == kind {
                    if let Some(opener) = openers.get_mut(slot) {
                        *opener = None;
                    }
                    break;
                }
            }
        } else if let Some(kind) = names.iter().position(|&opener| opener == name) {
            if stack.len() == MAX_NESTING {
                return Err(limit_error("editor block nesting"));
            }
            stack.push((kind, openers.len()));
            openers.push(Some(index));
            if let Some(count) = counts.get_mut(kind) {
                *count += 1;
            }
        }
    }
    Ok(openers.into_iter().flatten().collect())
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use rcmake_diag::source::SourceMap;

    use super::*;

    fn parse(input: &[u8]) -> Result<TolerantParse, &'static str> {
        let source = SourceMap::default()
            .add("CMakeLists.txt".into(), SourceKind::File, Arc::from([]))
            .ok_or("No source identifier")?;
        Ok(parse_tolerant(source, input))
    }

    #[test]
    fn editor_limits_bound_dense_input_and_output() -> Result<(), &'static str> {
        let input = vec![b'('; MAX_INPUT_BYTES + 1];
        let result = parse(&input)?;
        assert_eq!(result.errors.len(), 1);
        assert!(result.errors.first().is_some_and(|error| {
            error.message.as_slice() == b"Listfile exceeds the editor input size limit."
        }));

        let input = vec![b'('; MAX_NESTING + 1];
        let result = parse(&input)?;
        assert!(result.errors.first().is_some_and(|error| {
            error.message.as_slice() == b"Listfile exceeds the editor parenthesis nesting limit."
        }));

        let input = b"!\n".repeat(MAX_TOKENS / 2 + 1);
        let result = parse(&input)?;
        assert!(result.errors.first().is_some_and(|error| {
            error.message.as_slice() == b"Listfile exceeds the editor token count limit."
        }));

        let input = b"set()\n".repeat(MAX_OUTPUT_TOKENS / 4 + 1);
        let result = parse(&input)?;
        assert!(result.commands.is_empty());
        assert!(result.errors.first().is_some_and(|error| {
            error.message.as_slice() == b"Listfile exceeds the editor output limit."
        }));
        Ok(())
    }

    #[test]
    fn open_quote_can_hide_a_multiline_command() -> Result<(), &'static str> {
        let result = parse(b"message(\"oops\nset(value\n  \"ok\")\nmessage(done)")?;
        assert_eq!(result.commands.len(), 2);
        assert_eq!(result.partial_commands.len(), 1);
        assert!(result.commands.first().is_some_and(|command| {
            command.name.as_slice() == b"set"
                && command
                    .args
                    .last()
                    .is_some_and(|arg| arg.text.as_slice() == b"ok")
        }));
        assert!(result.partial_commands.first().is_some_and(|command| {
            command.unclosed_argument == Some(0)
                && command
                    .args
                    .last()
                    .is_some_and(|arg| arg.text.as_slice() == b"oops\n")
        }));
        Ok(())
    }

    #[test]
    fn recovery_keeps_balanced_brackets_after_repeated_open_quotes() -> Result<(), &'static str> {
        let mut input = b"message(\"oops\n".repeat(20);
        input.extend_from_slice(
            b"set(value [=[\nfake(\"text\")\n]=])\nmessage(\"line\nfake(\nmore\" tail)\n",
        );
        let result = parse(&input)?;
        assert_eq!(result.partial_commands.len(), 20);
        assert_eq!(result.commands.len(), 2);
        assert!(result.commands.first().is_some_and(|command| {
            command.name.as_slice() == b"set"
                && command.args.last().is_some_and(|arg| {
                    arg.kind == ArgKind::Bracket && arg.text.as_slice() == b"fake(\"text\")\n"
                })
        }));
        assert!(result.commands.last().is_some_and(|command| {
            command
                .args
                .first()
                .is_some_and(|arg| arg.text.as_slice() == b"line\nfake(\nmore")
        }));
        Ok(())
    }

    #[test]
    fn partial_arguments_record_open_delimiters() -> Result<(), &'static str> {
        let open = parse(b"set(value \"valid\" [=[still open")?;
        assert_eq!(
            open.partial_commands
                .first()
                .and_then(|command| command.unclosed_argument),
            Some(2)
        );

        let closed = parse(b"set(value \"valid\" [=[closed]=]")?;
        assert!(closed.partial_commands.first().is_some_and(|command| {
            command.args.len() == 3 && command.unclosed_argument.is_none()
        }));
        Ok(())
    }

    #[test]
    fn recovery_reports_open_blocks_in_source_order() -> Result<(), &'static str> {
        let mut input = b"set(x)\n".repeat(2_000);
        input.extend_from_slice(
            b"while()\nif()\nblock()\nendif()\nforeach()\nendwhile()\nmacro()\n",
        );
        input.extend_from_slice(&b"if(\n".repeat(2_000));
        let result = parse(&input)?;
        assert_eq!(result.unclosed_blocks, [2_002, 2_004, 2_006]);
        assert_eq!(result.partial_commands.len(), 2_000);
        Ok(())
    }
}
