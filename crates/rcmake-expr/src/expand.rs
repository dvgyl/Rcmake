use rcmake_base::{
    BString, ByteSlice as _,
    limits::Limits,
    list::{self, Empty},
};
use rcmake_syntax::{ArgKind, Argument};

use crate::{ArgumentBudget, ExpandEnv, ExpandError, ExpandedArg};

const MAX_DEPTH: usize = 256;
const LINE_VAR: &[u8] = b"CMAKE_CURRENT_LIST_LINE";

/// Expand each argument. Split unquoted arguments into list elements and drop empty
/// elements. Keep quoted arguments as one element. Copy bracket arguments verbatim.
///
/// The arguments already in `out` belong to the same command call. Each argument is
/// reserved in [`ExpandEnv::argument_budget`] before it is copied. On an error, `out`
/// keeps only the arguments that it held before the call.
pub fn expand_args(
    args: &[Argument],
    env: &mut dyn ExpandEnv,
    out: &mut Vec<ExpandedArg>,
) -> Result<(), ExpandError> {
    let mut budget = env.argument_budget();
    budget.used_arguments = out.len();
    let start = out.len();
    let result = expand_into(args, env, out, &mut budget);
    if result.is_err() {
        out.truncate(start);
    }
    result
}

fn expand_into(
    args: &[Argument],
    env: &mut dyn ExpandEnv,
    out: &mut Vec<ExpandedArg>,
    budget: &mut ArgumentBudget,
) -> Result<(), ExpandError> {
    let max_value = Limits::default().max_value_bytes;
    for arg in args {
        let expanded = |value| ExpandedArg {
            value,
            kind: arg.kind,
            line: arg.line,
        };
        if arg.kind == ArgKind::Bracket {
            budget.take(arg.text.len())?;
            out.push(expanded(arg.text.clone()));
            continue;
        }
        if arg.kind == ArgKind::Unquoted
            && let Some(name) = simple_var_name(&arg.text)
            && let Some(remaining) = max_value.checked_sub(name.len())
        {
            if let Some(value) = env.var_ref(name) {
                if value.len() > remaining {
                    return Err(ExpandError::resource_limit(
                        "Variable expansion exceeded the value size limit.",
                    ));
                }
                if value.is_empty() {
                    continue;
                }
                if value.find_byte(b';').is_none() {
                    budget.take(value.len())?;
                    out.push(expanded(BString::from(value.into_owned())));
                } else {
                    for element in list::iter(&value, Empty::Drop) {
                        budget.take(element.len())?;
                        out.push(expanded(BString::from(element.into_owned())));
                    }
                }
            } else {
                env.missing_reference(name);
            }
            continue;
        }
        let value = if let Some(value) = try_expand_simple_var(&arg.text, env) {
            value?
        } else {
            expand(&arg.text, arg.line, env, Mode::ListfileArgument)?
        };
        if arg.kind == ArgKind::Unquoted {
            if value.is_empty() {
                continue;
            }
            if value.find_byte(b';').is_none() {
                budget.take(value.len())?;
                out.push(expanded(value));
            } else {
                for element in list::iter(&value, Empty::Drop) {
                    budget.take(element.len())?;
                    out.push(expanded(BString::from(element.into_owned())));
                }
            }
        } else {
            budget.take(value.len())?;
            out.push(expanded(value));
        }
    }
    Ok(())
}

fn simple_var_name(src: &[u8]) -> Option<&[u8]> {
    let name = src.strip_prefix(b"${")?.strip_suffix(b"}")?;
    if name.is_empty() || name == LINE_VAR || !name.iter().all(|&byte| name_byte(byte)) {
        return None;
    }
    Some(name)
}

/// Fast path for an exact `${NAME}` reference in argument expansion.
fn try_expand_simple_var(
    src: &[u8],
    env: &mut dyn ExpandEnv,
) -> Option<Result<BString, ExpandError>> {
    let name = simple_var_name(src)?;
    let max = Limits::default().max_value_bytes;
    let remaining = max.checked_sub(name.len())?;
    let value = if let Some(value) = env.var_ref(name) {
        if value.len() > remaining {
            return Some(Err(ExpandError::resource_limit(
                "Variable expansion exceeded the value size limit.",
            )));
        }
        BString::from(value.into_owned())
    } else {
        env.missing_reference(name);
        BString::default()
    };
    Some(Ok(value))
}

/// Expand variables and escape sequences in a byte string. Keep list separators.
pub fn expand_string(src: &[u8], env: &mut dyn ExpandEnv) -> Result<BString, ExpandError> {
    if let Some(value) = try_expand_simple_var(src, env) {
        return value;
    }
    expand(src, env.current_line(), env, Mode::Argument)
}

#[derive(Clone, Copy)]
pub(crate) enum Mode {
    Argument,
    ListfileArgument,
    Template {
        at_only: bool,
        escape_quotes: bool,
        file: bool,
    },
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
    let max_bytes = Limits::default().max_value_bytes;
    let is_plain = match mode {
        Mode::Argument | Mode::ListfileArgument => src.find_byteset(b"$\\").is_none(),
        Mode::Template { at_only: true, .. } => src.find_byte(b'@').is_none(),
        Mode::Template { at_only: false, .. } => src.find_byteset(b"$@").is_none(),
    };
    if is_plain && src.len() <= max_bytes {
        return Ok(BString::from(src));
    }
    let mut scanner = Scanner {
        source: src,
        remaining: src,
        line,
        env,
        mode,
        remaining_bytes: max_bytes,
    };
    scanner.scan(None, 0).map(BString::from)
}

struct Scanner<'a, 'env> {
    source: &'a [u8],
    remaining: &'a [u8],
    line: u32,
    env: &'env mut dyn ExpandEnv,
    mode: Mode,
    remaining_bytes: usize,
}

impl<'a> Scanner<'a, '_> {
    fn scan(&mut self, reference: Option<Reference>, depth: usize) -> Result<Vec<u8>, ExpandError> {
        if depth > MAX_DEPTH {
            return Err(ExpandError::resource_limit(
                "Variable expansion exceeded the nesting limit.",
            ));
        }

        let mut output = if reference.is_none() {
            Vec::with_capacity(self.remaining.len().min(self.remaining_bytes))
        } else {
            Vec::new()
        };
        let outer_specials: Option<&[u8]> = if reference.is_none() {
            Some(match self.mode {
                Mode::Argument | Mode::ListfileArgument => b"$\\\n",
                Mode::Template { at_only: true, .. } => b"@\n",
                Mode::Template { at_only: false, .. } => b"$@\n",
            })
        } else {
            None
        };
        while !self.remaining.is_empty() {
            if let Some(specials) = outer_specials {
                match self.remaining.find_byteset(specials) {
                    Some(0) => {}
                    Some(pos) => {
                        let (plain, rest) = self.remaining.split_at(pos);
                        self.remaining = rest;
                        self.append(&mut output, plain)?;
                    }
                    None => {
                        let plain = self.remaining;
                        self.remaining = b"";
                        self.append(&mut output, plain)?;
                        break;
                    }
                }
            }
            let Some((&byte, rest)) = self.remaining.split_first() else {
                break;
            };
            self.remaining = rest;
            match byte {
                b'}' if reference.is_some() => return Ok(output),
                b'\\' if matches!(self.mode, Mode::Argument | Mode::ListfileArgument) => {
                    let escape = self.escape(reference.is_some())?;
                    self.append(&mut output, escape)?;
                }
                b'\\' => self.append(&mut output, b"\\")?,
                b'@' if matches!(self.mode, Mode::Template { .. }) => {
                    if let Some((name, rest)) = delimited_name(rest, b'@') {
                        self.remaining = rest;
                        self.append_lookup(&mut output, Reference::Normal, name, self.line)?;
                    } else {
                        self.literal(&mut output, byte, reference.is_some())?;
                    }
                }
                b'$' if !matches!(self.mode, Mode::Template { at_only: true, .. }) => {
                    if let Some((reference, rest)) = self.reference_start(rest)? {
                        self.remaining = rest;
                        let line = self.line;
                        if depth < MAX_DEPTH
                            && let Some((name, rest)) = delimited_name(rest, b'}')
                            && name.len() <= self.remaining_bytes
                        {
                            self.remaining = rest;
                            self.remaining_bytes -= name.len();
                            self.append_lookup(&mut output, reference, name, line)?;
                        } else {
                            let name = self.scan(Some(reference), depth + 1)?;
                            self.append_lookup(&mut output, reference, &name, line)?;
                        }
                    } else {
                        self.append(&mut output, b"$")?;
                    }
                }
                _ => self.literal(&mut output, byte, reference.is_some())?,
            }
        }
        if reference.is_some() {
            return Err(self.syntax_error("There is an unterminated variable reference."));
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
            return Err(self.syntax_error(message));
        }
        self.append(output, &[byte])
    }

    fn escape(&mut self, in_reference: bool) -> Result<&'a [u8], ExpandError> {
        let Some((byte, rest)) = self.remaining.split_first() else {
            return Err(self.syntax_error("Invalid character escape '\\' (at end of input)."));
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
                Err(self.syntax_error(message))
            }
            _ => Ok(core::slice::from_ref(byte)),
        }
    }

    fn reference_start(
        &mut self,
        src: &'a [u8],
    ) -> Result<Option<(Reference, &'a [u8])>, ExpandError> {
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
            message.extend_from_slice(
                b"{{} is not supported.  Only ${}, $ENV{}, and $CACHE{} are allowed.",
            );
            return Err(self.syntax_error(message));
        }
        Ok(None)
    }

    fn syntax_error(&mut self, message: impl Into<BString>) -> ExpandError {
        let message = message.into();
        if matches!(
            self.mode,
            Mode::ListfileArgument | Mode::Template { file: true, .. }
        ) && let Some(path) = self.env.current_list_file().filter(|path| !path.is_empty())
        {
            let mut body = b"Syntax error in cmake code at\n  ".to_vec();
            body.extend_from_slice(&path);
            body.push(b':');
            body.extend_from_slice(self.line.to_string().as_bytes());
            body.extend_from_slice(b"\nwhen parsing string\n  ");
            body.extend_from_slice(self.source);
            body.push(b'\n');
            body.extend_from_slice(&message);
            return error(body);
        }
        error(message)
    }

    fn append_lookup(
        &mut self,
        output: &mut Vec<u8>,
        reference: Reference,
        name: &[u8],
        line: u32,
    ) -> Result<(), ExpandError> {
        let mode = self.mode;
        match reference {
            Reference::Normal if name == LINE_VAR => {
                if let Some(value) = self.env.var_ref(name) {
                    return append_value_bytes(output, &value, mode, &mut self.remaining_bytes);
                }
                let text = line.to_string();
                return append_value_bytes(
                    output,
                    text.as_bytes(),
                    mode,
                    &mut self.remaining_bytes,
                );
            }
            Reference::Normal => {
                if let Some(value) = self.env.var_ref(name) {
                    return append_value_bytes(output, &value, mode, &mut self.remaining_bytes);
                }
            }
            Reference::Cache => {
                if let Some(value) = self.env.cache_var_ref(name) {
                    return append_value_bytes(output, &value, mode, &mut self.remaining_bytes);
                }
            }
            Reference::Environment => {
                if let Some(value) = self.env.env_var(name) {
                    return append_value_bytes(output, &value, mode, &mut self.remaining_bytes);
                }
            }
        }
        self.env.missing_reference(name);
        Ok(())
    }

    fn append(&mut self, output: &mut Vec<u8>, bytes: &[u8]) -> Result<(), ExpandError> {
        append_bytes(output, bytes, &mut self.remaining_bytes)
    }
}

fn append_value_bytes(
    output: &mut Vec<u8>,
    value: &[u8],
    mode: Mode,
    remaining_bytes: &mut usize,
) -> Result<(), ExpandError> {
    if matches!(
        mode,
        Mode::Template {
            escape_quotes: true,
            ..
        }
    ) && value.find_byte(b'"').is_some()
    {
        for part in value.split_inclusive(|&byte| byte == b'"') {
            if let Some(prefix) = part.strip_suffix(b"\"") {
                append_bytes(output, prefix, remaining_bytes)?;
                append_bytes(output, b"\\\"", remaining_bytes)?;
            } else {
                append_bytes(output, part, remaining_bytes)?;
            }
        }
        Ok(())
    } else {
        append_bytes(output, value, remaining_bytes)
    }
}

fn append_bytes(
    output: &mut Vec<u8>,
    bytes: &[u8],
    remaining_bytes: &mut usize,
) -> Result<(), ExpandError> {
    *remaining_bytes = remaining_bytes.checked_sub(bytes.len()).ok_or_else(|| {
        ExpandError::resource_limit("Variable expansion exceeded the value size limit.")
    })?;
    output.extend_from_slice(bytes);
    Ok(())
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

pub(crate) fn error(message: impl Into<BString>) -> ExpandError {
    ExpandError::syntax(message)
}

#[cfg(test)]
mod expand_tests {
    use std::borrow::Cow;
    use std::collections::BTreeMap;

    use super::*;
    use crate::ExpandEnv;

    struct BorrowEnv {
        vars: BTreeMap<BString, BString>,
        var_clones: usize,
        var_borrows: usize,
    }

    impl ExpandEnv for BorrowEnv {
        fn var(&mut self, name: &[u8]) -> Option<BString> {
            self.var_clones += 1;
            self.vars.get(name).cloned()
        }

        fn var_ref(&mut self, name: &[u8]) -> Option<Cow<'_, [u8]>> {
            self.var_borrows += 1;
            self.vars.get(name).map(|v| Cow::Borrowed(v.as_slice()))
        }

        fn cache_var(&mut self, _name: &[u8]) -> Option<BString> {
            None
        }

        fn env_var(&mut self, _name: &[u8]) -> Option<BString> {
            None
        }

        fn current_line(&self) -> u32 {
            10
        }
    }

    #[test]
    fn expand_uses_borrowed_lookups_and_bulk_literal_scanning() {
        let mut env = BorrowEnv {
            vars: BTreeMap::from([
                ("A".into(), "alpha".into()),
                ("B".into(), "beta".into()),
                ("LIST".into(), "one;two;three".into()),
            ]),
            var_clones: 0,
            var_borrows: 0,
        };
        let result = expand_string(
            b"prefix_${A}_middle\nline_${CMAKE_CURRENT_LIST_LINE}_${B}_\\t_suffix",
            &mut env,
        );
        assert_eq!(
            result.ok(),
            Some(BString::from("prefix_alpha_middle\nline_11_beta_\t_suffix"))
        );
        assert_eq!(env.var_clones, 0);
        assert_eq!(env.var_borrows, 3);

        let mut out = Vec::new();
        let arg = Argument {
            kind: ArgKind::Unquoted,
            text: "${LIST}".into(),
            line: 1,
            span: 0..7,
        };
        assert!(expand_args(&[arg], &mut env, &mut out).is_ok());
        assert_eq!(out.len(), 3);
        assert_eq!(env.var_clones, 0);
    }
}
