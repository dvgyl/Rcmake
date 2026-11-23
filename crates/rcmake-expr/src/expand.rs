use rcmake_base::{
    BString,
    limits::Limits,
    list::{Empty, split_into},
};
use rcmake_syntax::{ArgKind, Argument};

use crate::{ExpandEnv, ExpandError, ExpandedArg};

const MAX_DEPTH: usize = 256;
const LINE_VAR: &[u8] = b"CMAKE_CURRENT_LIST_LINE";

/// Expand each argument. Split unquoted arguments into list elements and drop empty
/// elements. Keep quoted arguments as one element. Copy bracket arguments verbatim.
pub fn expand_args(
    args: &[Argument],
    env: &mut dyn ExpandEnv,
    out: &mut Vec<ExpandedArg>,
) -> Result<(), ExpandError> {
    let mut elements = Vec::new();
    let mut expanded_args = Vec::new();
    for arg in args {
        let value = if arg.kind == ArgKind::Bracket {
            arg.text.clone()
        } else {
            expand(&arg.text, arg.line, env, Mode::Argument)?
        };
        let expanded = |value| ExpandedArg {
            value,
            kind: arg.kind,
            line: arg.line,
        };
        if arg.kind == ArgKind::Unquoted {
            split_into(&value, Empty::Drop, &mut elements);
            expanded_args.extend(elements.drain(..).map(expanded));
        } else {
            expanded_args.push(expanded(value));
        }
    }
    out.extend(expanded_args);
    Ok(())
}

/// Expand variables and escape sequences in a byte string. Keep list separators.
pub fn expand_string(src: &[u8], env: &mut dyn ExpandEnv) -> Result<BString, ExpandError> {
    expand(src, env.current_line(), env, Mode::Argument)
}

#[derive(Clone, Copy)]
pub(crate) enum Mode {
    Argument,
    Template { at_only: bool, escape_quotes: bool },
}

#[derive(Clone, Copy)]
enum Reference {
    Normal,
    Cache,
    Environment,
}

pub(crate) fn expand(
    src: &[u8],
    line: u32,
    env: &mut dyn ExpandEnv,
    mode: Mode,
) -> Result<BString, ExpandError> {
    let mut scanner = Scanner {
        remaining: src,
        line,
        env,
        mode,
        remaining_bytes: Limits::default().max_value_bytes,
    };
    scanner.scan(None, 0).map(BString::from)
}

struct Scanner<'a, 'env> {
    remaining: &'a [u8],
    line: u32,
    env: &'env mut dyn ExpandEnv,
    mode: Mode,
    remaining_bytes: usize,
}

impl<'a> Scanner<'a, '_> {
    fn scan(&mut self, reference: Option<Reference>, depth: usize) -> Result<Vec<u8>, ExpandError> {
        if depth > MAX_DEPTH {
            return Err(error("Variable expansion exceeded the nesting limit."));
        }

        let mut output = Vec::new();
        while let Some((&byte, rest)) = self.remaining.split_first() {
            self.remaining = rest;
            match byte {
                b'}' if reference.is_some() => return Ok(output),
                b'\\' if matches!(self.mode, Mode::Argument) => {
                    let escape = self.escape(reference.is_some())?;
                    self.append(&mut output, escape)?;
                }
                b'\\' => self.append(&mut output, b"\\")?,
                b'@' if matches!(self.mode, Mode::Template { .. }) => {
                    if let Some((name, rest)) = delimited_name(rest, b'@') {
                        self.remaining = rest;
                        let value = self.lookup(Reference::Normal, name, self.line);
                        self.append_value(&mut output, &value)?;
                    } else {
                        self.literal(&mut output, byte, reference.is_some())?;
                    }
                }
                b'$' if !matches!(self.mode, Mode::Template { at_only: true, .. }) => {
                    if let Some((reference, rest)) = reference_start(rest)? {
                        self.remaining = rest;
                        let line = self.line;
                        let name = self.scan(Some(reference), depth + 1)?;
                        let value = self.lookup(reference, &name, line);
                        self.append_value(&mut output, &value)?;
                    } else {
                        self.append(&mut output, b"$")?;
                    }
                }
                _ => self.literal(&mut output, byte, reference.is_some())?,
            }
        }
        if reference.is_some() {
            return Err(error("There is an unterminated variable reference."));
        }
        Ok(output)
    }

    fn literal(
        &mut self,
        output: &mut Vec<u8>,
        byte: u8,
        in_reference: bool,
    ) -> Result<(), ExpandError> {
        // CMake accepts a source newline in a variable name and counts it in the line cursor.
        if byte == b'\n' {
            self.line = self.line.saturating_add(1);
        } else if in_reference && !name_byte(byte) {
            let mut message = b"Invalid character ('".to_vec();
            message.push(byte);
            message.extend_from_slice(b"') in a variable name: '");
            message.extend_from_slice(output);
            message.push(b'\'');
            return Err(error(message));
        }
        self.append(output, &[byte])
    }

    fn escape(&mut self, in_reference: bool) -> Result<&'a [u8], ExpandError> {
        let Some((byte, rest)) = self.remaining.split_first() else {
            return Err(error("Invalid character escape '\\' (at end of input)."));
        };
        self.remaining = rest;
        match byte {
            b't' => Ok(b"\t"),
            b'r' => Ok(b"\r"),
            b'n' => Ok(b"\n"),
            b'\n' => {
                self.line = self.line.saturating_add(1);
                Ok(b"\n")
            }
            b';' if !in_reference => Ok(b"\\;"),
            byte if byte.is_ascii_alphanumeric() => {
                let mut message = b"Invalid character escape '\\".to_vec();
                message.push(*byte);
                message.extend_from_slice(b"'.");
                Err(error(message))
            }
            _ => Ok(core::slice::from_ref(byte)),
        }
    }

    fn lookup(&mut self, reference: Reference, name: &[u8], line: u32) -> BString {
        match reference {
            Reference::Normal if name == LINE_VAR => self
                .env
                .var(name)
                .unwrap_or_else(|| line.to_string().into()),
            Reference::Normal => self.env.var(name).unwrap_or_default(),
            Reference::Cache => self.env.cache_var(name).unwrap_or_default(),
            Reference::Environment => self.env.env_var(name).unwrap_or_default(),
        }
    }

    fn append_value(&mut self, output: &mut Vec<u8>, value: &[u8]) -> Result<(), ExpandError> {
        if matches!(
            self.mode,
            Mode::Template {
                escape_quotes: true,
                ..
            }
        ) {
            for part in value.split_inclusive(|&byte| byte == b'"') {
                if let Some(prefix) = part.strip_suffix(b"\"") {
                    self.append(output, prefix)?;
                    self.append(output, b"\\\"")?;
                } else {
                    self.append(output, part)?;
                }
            }
            Ok(())
        } else {
            self.append(output, value)
        }
    }

    fn append(&mut self, output: &mut Vec<u8>, bytes: &[u8]) -> Result<(), ExpandError> {
        self.remaining_bytes = self
            .remaining_bytes
            .checked_sub(bytes.len())
            .ok_or_else(|| error("Variable expansion exceeded the value size limit."))?;
        output.extend_from_slice(bytes);
        Ok(())
    }
}

fn name_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'/' | b'.' | b'+' | b'-')
}

fn delimited_name(src: &[u8], delimiter: u8) -> Option<(&[u8], &[u8])> {
    let end = src.iter().take_while(|&&byte| name_byte(byte)).count();
    if end == 0 {
        return None;
    }
    let (name, tail) = src.split_at_checked(end)?;
    Some((name, tail.strip_prefix(&[delimiter])?))
}

fn reference_start(src: &[u8]) -> Result<Option<(Reference, &[u8])>, ExpandError> {
    for (prefix, reference) in [
        (b"{".as_slice(), Reference::Normal),
        (b"ENV{".as_slice(), Reference::Environment),
        (b"CACHE{".as_slice(), Reference::Cache),
    ] {
        if let Some(rest) = src.strip_prefix(prefix) {
            return Ok(Some((reference, rest)));
        }
    }
    if let Some((name, _)) = delimited_name(src, b'{') {
        let mut message = b"Syntax $".to_vec();
        message.extend_from_slice(name);
        // Upstream CMake 4.5.1 literally emits `{{}` here (verified empirically).
        message.extend_from_slice(
            b"{{} is not supported.  Only ${}, $ENV{}, and $CACHE{} are allowed.",
        );
        return Err(error(message));
    }
    Ok(None)
}

pub(crate) fn error(message: impl Into<BString>) -> ExpandError {
    ExpandError {
        message: message.into(),
    }
}
