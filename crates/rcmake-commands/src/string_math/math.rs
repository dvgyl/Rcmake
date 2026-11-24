use rcmake_base::ByteSlice;
use rcmake_diag::{DiagCategory, Effect, MessageKind};
use rcmake_eval::{CmdError, CmdResult, Cx};
use rcmake_expr::ExpandedArg;

use super::{set, trim_start};

pub(super) fn command(cx: &mut Cx<'_>, args: &[ExpandedArg]) -> CmdResult {
    let args: Vec<&[u8]> = args.iter().map(|arg| arg.value.as_slice()).collect();
    match args.as_slice() {
        [b"EXPR", args @ ..] => expression(cx, args),
        [mode @ (b"INCREMENT" | b"DECREMENT"), args @ ..] => adjust(cx, mode, args),
        [mode, ..] => Err(CmdError::status(format!(
            "does not recognize sub-command {}",
            mode.to_str_lossy()
        ))),
        [] => Err(CmdError::status(
            "must be called with at least one argument.",
        )),
    }
}

fn expression(cx: &mut Cx<'_>, args: &[&[u8]]) -> CmdResult {
    let (out, source, hex) = match args {
        [out, source] | [out, source, b"OUTPUT_FORMAT", b"DECIMAL"] => (*out, *source, false),
        [out, source, b"OUTPUT_FORMAT", b"HEXADECIMAL"] => (*out, *source, true),
        [_, _, b"OUTPUT_FORMAT", value] => {
            return Err(CmdError::status(format!(
                "sub-command EXPR value \"{}\" for option \"OUTPUT_FORMAT\" is invalid.",
                value.to_str_lossy()
            )));
        }
        [_, _, option, _] => {
            return Err(CmdError::status(format!(
                "sub-command EXPR option \"{}\" is unknown.",
                option.to_str_lossy()
            )));
        }
        _ => return Err(CmdError::status("EXPR called with incorrect arguments.")),
    };

    let mut parser = Parser {
        input: source,
        offset: 0,
        token: Token::End,
        depth_limit: cx.limits().max_recursion.min(256),
        warnings: String::new(),
        warning_limit: cx.limits().max_value_bytes,
    };
    parser.advance();
    let result = parser.parse(1, 0).and_then(|value| {
        if parser.token == Token::End {
            Ok(value)
        } else {
            Err(parser.syntax(", expecting end of file"))
        }
    });
    let value = result.map_err(|error| {
        let (action, detail) = match error {
            ParseError::Syntax(detail) => ("parse", detail),
            ParseError::Evaluate(detail) => ("evaluate", detail.to_owned()),
        };
        CmdError::status(format!(
            "cannot {action} the expression: \"{}\": {detail}.",
            source.to_str_lossy()
        ))
    })?;
    if !parser.warnings.is_empty() {
        parser.warnings.push('\n');
        warn(cx, parser.warnings);
    }
    set(
        cx,
        out,
        if hex {
            format!("0x{value:x}")
        } else {
            value.to_string()
        },
    )
}

fn adjust(cx: &mut Cx<'_>, mode: &[u8], args: &[&[u8]]) -> CmdResult {
    let name = mode.to_str_lossy();
    let [out] = args else {
        return Err(CmdError::status(format!(
            "sub-command {name} wrong number of arguments"
        )));
    };
    let value = cx.var(out).ok_or_else(|| {
        CmdError::status(format!(
            "sub-command {name} variable \"{}\" is not defined",
            out.to_str_lossy()
        ))
    })?;
    let n: i64 = std::str::from_utf8(trim_start(&value))
        .ok()
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| {
            CmdError::status(format!(
                "sub-command {name} value \"{}\" is not a valid integer",
                value.to_str_lossy()
            ))
        })?;
    let (delta, action) = if mode == b"INCREMENT" {
        (1, "incrementing")
    } else {
        (-1, "decrementing")
    };
    let (value, overflow) = n.overflowing_add(delta);
    if overflow {
        warn(
            cx,
            format!("signed integer overflow while {action}:\n  {n}"),
        );
    }
    set(cx, out, value.to_string())
}

fn warn(cx: &mut Cx<'_>, detail: String) {
    cx.issue(
        MessageKind::AuthorWarning,
        Effect::None,
        Some(DiagCategory::Author),
        detail.into(),
    );
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Token {
    End,
    Number(Option<i64>),
    Open,
    Close,
    Not,
    Or,
    Xor,
    And,
    Left,
    Right,
    Plus,
    Minus,
    Times,
    Divide,
    Modulo,
}

impl Token {
    fn precedence(self) -> u8 {
        match self {
            Self::Or => 1,
            Self::Xor => 2,
            Self::And => 3,
            Self::Left | Self::Right => 4,
            Self::Plus | Self::Minus => 5,
            Self::Times | Self::Divide | Self::Modulo => 6,
            _ => 0,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::End => "end of file",
            Self::Number(_) => "exp_NUMBER",
            Self::Open => "exp_OPENPARENT",
            Self::Close => "exp_CLOSEPARENT",
            Self::Not => "exp_NOT",
            Self::Or => "exp_OR",
            Self::Xor => "exp_XOR",
            Self::And => "exp_AND",
            Self::Left => "exp_SHIFTLEFT",
            Self::Right => "exp_SHIFTRIGHT",
            Self::Plus => "exp_PLUS",
            Self::Minus => "exp_MINUS",
            Self::Times => "exp_TIMES",
            Self::Divide => "exp_DIVIDE",
            Self::Modulo => "exp_MOD",
        }
    }
}

enum ParseError {
    Syntax(String),
    Evaluate(&'static str),
}

struct Parser<'a> {
    input: &'a [u8],
    offset: usize,
    token: Token,
    depth_limit: u32,
    warnings: String,
    warning_limit: usize,
}

impl Parser<'_> {
    fn advance(&mut self) {
        while let Some((&byte, tail)) = self.input.split_first() {
            self.input = tail;
            self.offset += 1;
            self.token = match byte {
                b' ' | b'\t' | b'\n' => continue,
                b'(' => Token::Open,
                b')' => Token::Close,
                b'~' => Token::Not,
                b'|' => Token::Or,
                b'^' => Token::Xor,
                b'&' => Token::And,
                b'<' if self.take(b'<') => Token::Left,
                b'>' if self.take(b'>') => Token::Right,
                b'+' => Token::Plus,
                b'-' => Token::Minus,
                b'*' => Token::Times,
                b'/' => Token::Divide,
                b'%' => Token::Modulo,
                b'0'..=b'9' => self.number(byte),
                _ => {
                    self.warning(format!(
                        "Unexpected character in expression at position {}: {}",
                        self.offset,
                        std::slice::from_ref(&byte).to_str_lossy()
                    ));
                    continue;
                }
            };
            return;
        }
        self.token = Token::End;
    }

    fn take(&mut self, byte: u8) -> bool {
        let Some(tail) = self.input.strip_prefix(&[byte]) else {
            return false;
        };
        self.input = tail;
        self.offset += 1;
        true
    }

    fn number(&mut self, first: u8) -> Token {
        let hex = first == b'0'
            && matches!(self.input, [b'x' | b'X', digit, ..] if digit.is_ascii_hexdigit());
        let (radix, mut value) = if hex {
            self.input = self.input.get(1..).unwrap_or_default();
            self.offset += 1;
            (16, Some(0_i64))
        } else {
            (10, Some(i64::from(first - b'0')))
        };
        while let Some((&byte, rest)) = self.input.split_first() {
            let Some(digit) = char::from(byte).to_digit(radix) else {
                break;
            };
            value = value.and_then(|n| {
                n.checked_mul(i64::from(radix))?
                    .checked_add(i64::from(digit))
            });
            self.input = rest;
            self.offset += 1;
        }
        Token::Number(value)
    }

    fn warning(&mut self, warning: String) {
        if self
            .warnings
            .len()
            .saturating_add(warning.len())
            .saturating_add(2)
            > self.warning_limit
        {
            return;
        }
        if !self.warnings.is_empty() {
            self.warnings.push('\n');
        }
        self.warnings.push_str(&warning);
    }

    fn syntax(&self, expected: &str) -> ParseError {
        ParseError::Syntax(format!(
            "syntax error, unexpected {}{expected} ({})",
            self.token.name(),
            self.offset
        ))
    }

    fn parse(&mut self, precedence: u8, depth: u32) -> Result<i64, ParseError> {
        if depth > self.depth_limit {
            return Err(ParseError::Evaluate(
                "expression nesting exceeds the recursion limit",
            ));
        }
        let mut left = match self.token {
            Token::Number(n) => {
                self.advance();
                n.ok_or(ParseError::Evaluate("a numeric value is out of range"))?
            }
            Token::Open => {
                self.advance();
                let value = self.parse(1, depth + 1)?;
                if self.token != Token::Close {
                    return Err(self.syntax(", expecting exp_CLOSEPARENT or exp_OR"));
                }
                self.advance();
                value
            }
            operator @ (Token::Plus | Token::Minus | Token::Not) => {
                self.advance();
                let value = self.parse(7, depth + 1)?;
                match operator {
                    Token::Minus => {
                        if value == i64::MIN {
                            self.warning(format!("signed integer cannot negate:\n  {value}"));
                        }
                        value.wrapping_neg()
                    }
                    Token::Not => !value,
                    _ => value,
                }
            }
            _ => return Err(self.syntax("")),
        };
        while self.token.precedence() >= precedence {
            let operator = self.token;
            self.advance();
            let right = self.parse(operator.precedence() + 1, depth + 1)?;
            left = self.apply(operator, left, right)?;
        }
        Ok(left)
    }

    fn apply(&mut self, op: Token, left: i64, right: i64) -> Result<i64, ParseError> {
        match op {
            Token::Or => Ok(left | right),
            Token::Xor => Ok(left ^ right),
            Token::And => Ok(left & right),
            Token::Plus => Ok(self.arithmetic(left.overflowing_add(right), left, "+", right)),
            Token::Minus => Ok(self.arithmetic(left.overflowing_sub(right), left, "-", right)),
            Token::Times => Ok(self.arithmetic(left.overflowing_mul(right), left, "*", right)),
            Token::Divide if right == 0 => Err(ParseError::Evaluate("divide by zero")),
            Token::Modulo if right == 0 => Err(ParseError::Evaluate("modulo by zero")),
            Token::Divide => left
                .checked_div(right)
                .ok_or(ParseError::Evaluate("signed integer overflow in division")),
            Token::Modulo => left
                .checked_rem(right)
                .ok_or(ParseError::Evaluate("signed integer overflow in modulo")),
            Token::Left | Token::Right => Ok(self.shift(op, left, right)),
            _ => Err(self.syntax("")),
        }
    }

    fn arithmetic(
        &mut self,
        (value, overflow): (i64, bool),
        left: i64,
        op: &str,
        right: i64,
    ) -> i64 {
        if overflow {
            self.warning(format!(
                "signed integer overflow in:\n  {left} {op} {right}"
            ));
        }
        value
    }

    fn shift(&mut self, op: Token, left: i64, right: i64) -> i64 {
        let symbol = if op == Token::Left { "<<" } else { ">>" };
        let operand = format!("{left} {symbol} {right}");
        if op == Token::Left && left < 0 {
            self.warning(format!("left shift of negative value in:\n  {operand}"));
        }
        if right < 0 {
            self.warning(format!("shift exponent is negative in:\n  {operand}"));
        } else if right >= i64::BITS.into() {
            self.warning(format!("shift exponent is too large in:\n  {operand}"));
        }
        if op == Token::Left {
            left.wrapping_shl(right as u32)
        } else {
            left.wrapping_shr(right as u32)
        }
    }
}
