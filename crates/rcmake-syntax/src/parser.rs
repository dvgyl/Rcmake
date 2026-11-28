//! Parse command invocations and validate the order of flow-control commands.

use std::sync::Arc;

use rcmake_base::{BString, Limits};
use rcmake_diag::{DiagCategory, Diagnostic, Effect, Location, MessageKind, source::SourceKind};

use crate::lexer::{Kind, Lexer, Position, Token};
use crate::{ArgKind, Argument, CommandInvocation, ParseError};

const WHITESPACE_MESSAGE: &str = "Argument not separated from preceding token by whitespace.";

pub(super) fn parse(
    src: &[u8],
    kind: SourceKind,
    limits: &Limits,
    warnings: &mut Vec<Diagnostic>,
) -> Result<Arc<[CommandInvocation]>, ParseError> {
    let over_limit = match kind {
        SourceKind::File => src.len() as u64 > limits.max_file_read,
        SourceKind::VirtualEval => src.len() > limits.max_value_bytes,
    };
    if over_limit {
        return Err(ParseError {
            line: 1,
            column: 0,
            command: None,
            message: "Listfile exceeds the input size limit.".into(),
        });
    }

    let mut lexer = Lexer::new(src, kind)?;
    let mut commands = Vec::new();
    let mut need_newline = false;
    while let Some(token) = lexer.next() {
        match token.kind {
            Kind::Space => {}
            Kind::Newline => need_newline = false,
            Kind::BracketComment => need_newline = true,
            Kind::Identifier if need_newline => {
                return Err(unexpected(
                    b"Parse error.  Expected a newline, got ",
                    &token,
                ));
            }
            Kind::Identifier => {
                commands.push(parse_command(&mut lexer, token, warnings)?);
                need_newline = true;
            }
            _ => {
                return Err(unexpected(
                    b"Parse error.  Expected a command name, got ",
                    &token,
                ));
            }
        }
    }

    validate_flow_control(&commands)?;

    Ok(commands.into())
}

fn parse_command(
    lexer: &mut Lexer<'_>,
    name: Token,
    warnings: &mut Vec<Diagnostic>,
) -> Result<CommandInvocation, ParseError> {
    let opening = loop {
        match lexer.next() {
            Some(token) if token.kind == Kind::Space => continue,
            Some(token) => break token,
            None => {
                return Err(error(
                    name.start,
                    "Unexpected end of file.\nParse error.  Function missing opening \"(\".",
                ));
            }
        }
    };
    if opening.kind != Kind::LeftParen {
        return Err(unexpected(b"Parse error.  Expected \"(\", got ", &opening));
    }

    let mut args = Vec::new();
    let mut depth = 0;
    let mut separation = Separation::Space;
    while let Some(token) = lexer.next() {
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
                let lower = name.text.as_slice().to_ascii_lowercase().into();
                return Ok(CommandInvocation {
                    name: name.text,
                    lower,
                    line: name.start.line,
                    close_line: token.start.line,
                    span: name.start.offset..token.end.offset,
                    args,
                });
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
                separation = check_separation(separation, kind, token.start, warnings)?;
                kind
            }
            _ => {
                return Err(unexpected(
                    b"Parse error.  Function missing ending \")\".  Instead found ",
                    &token,
                ));
            }
        };
        args.push(Argument {
            kind,
            text: token.text,
            line: token.start.line,
            span: token.start.offset..token.end.offset,
        });
    }
    Err(error(
        name.start,
        "Parse error.  Function missing ending \")\".  End of file reached.",
    ))
}

#[derive(PartialEq, Eq)]
pub(super) enum Separation {
    Space,
    None,
    Bracket,
}

pub(super) fn check_separation(
    preceding: Separation,
    kind: ArgKind,
    position: Position,
    warnings: &mut Vec<Diagnostic>,
) -> Result<Separation, ParseError> {
    let column = position.column;
    if preceding != Separation::Space {
        if preceding == Separation::Bracket || kind == ArgKind::Bracket {
            return Err(error(
                position,
                format!("Syntax Error in cmake code at column {column}\n{WHITESPACE_MESSAGE}"),
            ));
        }
        warnings.push(Diagnostic {
            annotations: None,
            kind: MessageKind::AuthorWarning,
            effect: Effect::None,
            category: Some(DiagCategory::Author),
            policy: None,
            location: Location::FileLine {
                file: Arc::from([]),
                line: position.line,
            },
            body: format!("Syntax Warning in cmake code at column {column}\n{WHITESPACE_MESSAGE}")
                .into(),
        });
    }
    Ok(if kind == ArgKind::Bracket {
        Separation::Bracket
    } else {
        Separation::None
    })
}

pub(super) fn unexpected(prefix: &[u8], token: &Token) -> ParseError {
    let mut message = prefix.to_vec();
    message.extend_from_slice(token.kind.description());
    message.extend_from_slice(b" with text \"");
    message.extend_from_slice(token.text.as_slice());
    message.extend_from_slice(b"\".");

    let position = if token.start.line == token.end.line {
        token.start
    } else {
        token.end
    };
    error(position, message)
}

pub(super) fn error(position: Position, message: impl Into<BString>) -> ParseError {
    ParseError {
        line: position.line,
        column: position.column,
        command: None,
        message: message.into(),
    }
}

pub(super) fn validate_flow_control(commands: &[CommandInvocation]) -> Result<(), ParseError> {
    let mut stack: Vec<(&CommandInvocation, bool)> = Vec::new();
    for command in commands {
        let name = command.lower.as_slice();
        match name {
            b"if" | b"foreach" | b"while" | b"function" | b"macro" | b"block" => {
                stack.push((command, false));
            }
            b"elseif" | b"else" => match stack.last_mut() {
                Some((opener, seen_else)) if opener.lower.as_slice() == b"if" && !*seen_else => {
                    *seen_else = name == b"else";
                }
                _ => return Err(nesting_error(command)),
            },
            b"endif" | b"endforeach" | b"endwhile" | b"endfunction" | b"endmacro" | b"endblock" => {
                let expected = name.strip_prefix(b"end");
                if !stack
                    .pop()
                    .is_some_and(|(opener, _)| Some(opener.lower.as_slice()) == expected)
                {
                    return Err(nesting_error(command));
                }
            }
            _ => {}
        }
    }
    if let Some((command, _)) = stack.last() {
        return Err(nesting_error(command));
    }
    Ok(())
}

fn nesting_error(command: &CommandInvocation) -> ParseError {
    ParseError {
        line: command.line,
        column: 0,
        command: Some(command.name.clone()),
        message: "Flow control statements are not properly nested.".into(),
    }
}
