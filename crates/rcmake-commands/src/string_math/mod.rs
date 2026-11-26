//! The `string` and `math` commands.

#![forbid(unsafe_code)]

use rcmake_base::{BString, ByteSlice, hash::HashAlgo};
use rcmake_eval::{CmdError, CmdFlags, CmdResult, Cx, Flow, Registry};
use rcmake_expr::ExpandedArg;

mod generate;
mod json;
mod math;
mod regex;

pub(crate) use generate::configure_text;

#[cfg(test)]
mod tests;

pub(crate) fn register(registry: &mut Registry) {
    let flags = CmdFlags { scriptable: true };
    registry.add("string", string, flags);
    registry.add("math", math::command, flags);
}

fn string(cx: &mut Cx<'_>, args: &[ExpandedArg]) -> CmdResult {
    let args: Vec<&[u8]> = args.iter().map(|arg| arg.value.as_slice()).collect();
    let Some((&mode, args)) = args.split_first() else {
        return Err(CmdError::status(
            "must be called with at least one argument.",
        ));
    };
    match mode {
        b"FIND" => find(cx, args),
        b"REPLACE" => replace(cx, args),
        b"REGEX" => regex::command(cx, args),
        b"APPEND" | b"PREPEND" | b"CONCAT" | b"JOIN" => join(cx, mode, args),
        b"TOLOWER" | b"TOUPPER" | b"LENGTH" | b"STRIP" | b"GENEX_STRIP" | b"MAKE_C_IDENTIFIER"
        | b"HEX" => transform(cx, mode, args),
        b"SUBSTRING" => substring(cx, args),
        b"REPEAT" => repeat(cx, args),
        b"COMPARE" => compare(cx, args),
        b"ASCII" => generate::ascii(cx, args),
        b"CONFIGURE" => generate::configure(cx, args),
        b"RANDOM" => generate::random(cx, args),
        b"TIMESTAMP" => generate::timestamp(cx, args),
        b"UUID" => generate::uuid(cx, args),
        b"JSON" => json::command(cx, args),
        _ => {
            let mode_name = mode.to_str_lossy();
            let Some(algo) = HashAlgo::parse(mode) else {
                return Err(CmdError::status(format!(
                    "does not recognize sub-command {mode_name}"
                )));
            };
            let [out, input] = args else {
                return Err(CmdError::status(format!(
                    "{mode_name} requires an output variable and an input string"
                )));
            };
            set(cx, out, algo.hex_digest(input))
        }
    }
}

fn set(cx: &mut Cx<'_>, out: &[u8], value: impl Into<BString>) -> CmdResult {
    let value = value.into();
    check_size(cx, value.len())?;
    cx.set_var(out, value);
    Ok(Flow::Normal)
}

fn check_size(cx: &Cx<'_>, size: usize) -> Result<(), CmdError> {
    if size > cx.limits().max_value_bytes {
        return Err(CmdError::status("result exceeds the value size limit."));
    }
    Ok(())
}

fn append(cx: &Cx<'_>, out: &mut Vec<u8>, bytes: &[u8]) -> Result<(), CmdError> {
    check_size(cx, out.len().saturating_add(bytes.len()))?;
    out.extend_from_slice(bytes);
    Ok(())
}

fn concat(cx: &Cx<'_>, args: &[&[u8]]) -> Result<Vec<u8>, CmdError> {
    let mut output = Vec::new();
    for arg in args {
        append(cx, &mut output, arg)?;
    }
    Ok(output)
}

fn find(cx: &mut Cx<'_>, args: &[&[u8]]) -> CmdResult {
    let [input, needle, out, rest @ ..] = args else {
        return Err(CmdError::status(
            "sub-command FIND requires 3 or 4 parameters.",
        ));
    };
    let offset = match rest {
        [] => input.find(needle),
        [b"REVERSE"] => input.rfind(needle),
        [_] => return Err(CmdError::status("sub-command FIND: unknown last parameter")),
        _ => {
            return Err(CmdError::status(
                "sub-command FIND requires 3 or 4 parameters.",
            ));
        }
    };
    set(
        cx,
        out,
        offset.map_or_else(|| "-1".to_owned(), |n| n.to_string()),
    )
}

fn replace(cx: &mut Cx<'_>, args: &[&[u8]]) -> CmdResult {
    let [needle, replacement, out, inputs @ ..] = args else {
        return Err(CmdError::status(
            "sub-command REPLACE requires at least four arguments.",
        ));
    };
    if inputs.is_empty() {
        return Err(CmdError::status(
            "sub-command REPLACE requires at least four arguments.",
        ));
    }
    let input = concat(cx, inputs)?;
    if needle.is_empty() {
        return set(cx, out, input);
    }

    let mut result = Vec::new();
    let mut start = 0;
    for pos in input.find_iter(needle) {
        append(cx, &mut result, input.get(start..pos).unwrap_or_default())?;
        append(cx, &mut result, replacement)?;
        start = pos + needle.len();
    }
    append(cx, &mut result, input.get(start..).unwrap_or_default())?;
    set(cx, out, result)
}

fn join(cx: &mut Cx<'_>, mode: &[u8], args: &[&[u8]]) -> CmdResult {
    if mode == b"JOIN" {
        let [glue, out, inputs @ ..] = args else {
            return Err(CmdError::status(
                "sub-command JOIN requires at least two arguments.",
            ));
        };
        let mut result = Vec::new();
        for (index, input) in inputs.iter().enumerate() {
            if index != 0 {
                append(cx, &mut result, glue)?;
            }
            append(cx, &mut result, input)?;
        }
        return set(cx, out, result);
    }
    let [out, inputs @ ..] = args else {
        return Err(CmdError::status(format!(
            "sub-command {} requires at least one argument.",
            mode.to_str_lossy()
        )));
    };
    if mode != b"CONCAT" && inputs.is_empty() {
        return Ok(Flow::Normal);
    }
    let mut result = concat(cx, inputs)?;
    if mode == b"APPEND" {
        let mut prefix = Vec::from(cx.var(out).unwrap_or_default());
        append(cx, &mut prefix, &result)?;
        result = prefix;
    } else if mode == b"PREPEND" {
        append(cx, &mut result, &cx.var_or_empty(out))?;
    }
    set(cx, out, result)
}

fn transform(cx: &mut Cx<'_>, mode: &[u8], args: &[&[u8]]) -> CmdResult {
    if matches!(mode, b"TOLOWER" | b"TOUPPER") {
        let [input, out, ..] = args else {
            return Err(CmdError::status(if args.is_empty() {
                "no input string specified"
            } else {
                "no output variable specified"
            }));
        };
        return set(
            cx,
            out,
            if mode == b"TOLOWER" {
                input.to_ascii_lowercase()
            } else {
                input.to_ascii_uppercase()
            },
        );
    }

    let [input, out] = args else {
        return Err(CmdError::status(if mode == b"HEX" {
            "Incorrect number of arguments".to_owned()
        } else {
            format!(
                "sub-command {} requires two arguments.",
                mode.to_str_lossy()
            )
        }));
    };
    match mode {
        b"LENGTH" => set(cx, out, input.len().to_string()),
        b"STRIP" => set(cx, out, trim_start(input).trim_end_with(is_space)),
        b"GENEX_STRIP" => set(cx, out, drop_empty_elements(strip_genex(input))),
        b"MAKE_C_IDENTIFIER" => {
            if input.first().is_some_and(u8::is_ascii_digit) {
                check_size(cx, input.len().saturating_add(1))?;
            }
            set(cx, out, make_c_identifier(input))
        }
        b"HEX" => {
            check_size(cx, input.len().saturating_mul(2))?;
            set(cx, out, hex(input))
        }
        _ => Err(CmdError::status("invalid string transformation.")),
    }
}

pub(crate) fn make_c_identifier(input: &[u8]) -> Vec<u8> {
    let mut result = Vec::with_capacity(input.len());
    if input.first().is_some_and(u8::is_ascii_digit) {
        result.push(b'_');
    }
    result.extend(input.iter().map(|&byte| {
        if byte.is_ascii_alphanumeric() {
            byte
        } else {
            b'_'
        }
    }));
    result
}

fn strip_genex(input: &[u8]) -> Vec<u8> {
    let mut remaining = input;
    let mut result = Vec::new();
    while let Some(start) = remaining.find(b"$<") {
        let Some((prefix, expression)) = remaining.split_at_checked(start) else {
            break;
        };
        result.extend_from_slice(prefix);
        remaining = expression;
        let mut depth = 0;
        let mut previous = 0;
        let mut end = None;
        for (index, &byte) in expression.iter().enumerate() {
            match byte {
                b'<' if previous == b'$' => depth += 1,
                b'>' => {
                    depth -= 1;
                    if depth == 0 {
                        end = Some(index + 1);
                        break;
                    }
                }
                _ => {}
            }
            previous = byte;
        }
        let Some(tail) = end.and_then(|end| expression.get(end..)) else {
            break;
        };
        remaining = tail;
    }
    result.extend_from_slice(remaining);
    result
}

fn drop_empty_elements(input: Vec<u8>) -> Vec<u8> {
    let mut result = Vec::new();
    let mut start = 0;
    let mut depth = 0_isize;
    let mut previous = None;
    for (index, &byte) in input.iter().enumerate() {
        match byte {
            b'[' => depth += 1,
            b']' => depth -= 1,
            b';' if depth == 0 && previous != Some(b'\\') => {
                if start < index {
                    rcmake_base::list::push(
                        &mut result,
                        input.get(start..index).unwrap_or_default(),
                    );
                }
                start = index + 1;
            }
            _ => {}
        }
        previous = Some(byte);
    }
    if start < input.len() {
        rcmake_base::list::push(&mut result, input.get(start..).unwrap_or_default());
    }
    result
}

fn hex(input: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut result = String::with_capacity(input.len() * 2);
    for byte in input {
        let _ = write!(result, "{byte:02x}");
    }
    result
}

fn is_space(c: char) -> bool {
    matches!(c, ' ' | '\t' | '\r' | '\n' | '\u{b}' | '\u{c}')
}

fn trim_start(bytes: &[u8]) -> &[u8] {
    bytes.trim_start_with(is_space)
}

fn decimal_prefix(bytes: &[u8]) -> i64 {
    let bytes = trim_start(bytes);
    let (sign, digits) = match bytes.split_first() {
        Some((b'-' | b'+', rest)) => (1, rest),
        _ => (0, bytes),
    };
    let length = sign + digits.iter().take_while(|b| b.is_ascii_digit()).count();
    bytes
        .get(..length)
        .and_then(|text| std::str::from_utf8(text).ok())
        .and_then(|text| text.parse().ok())
        .unwrap_or_else(|| {
            if length <= sign {
                0
            } else if bytes.starts_with(b"-") {
                i64::MIN
            } else {
                i64::MAX
            }
        })
}

fn substring(cx: &mut Cx<'_>, args: &[&[u8]]) -> CmdResult {
    let [input, begin, length, out] = args else {
        return Err(CmdError::status(
            "sub-command SUBSTRING requires four arguments.",
        ));
    };
    let begin = decimal_prefix(begin) as i32;
    let length = decimal_prefix(length) as i32;
    let invalid_begin = || {
        CmdError::status(format!(
            "begin index: {begin} is out of range 0 - {}",
            input.len()
        ))
    };
    let start = usize::try_from(begin).map_err(|_| invalid_begin())?;
    let suffix = input.get(start..).ok_or_else(invalid_begin)?;
    if length < -1 {
        return Err(CmdError::status(format!(
            "end index: {length} should be -1 or greater"
        )));
    }
    let result = if length == -1 {
        suffix
    } else {
        suffix.get(..length as usize).unwrap_or(suffix)
    };
    set(cx, out, result)
}

fn repeat(cx: &mut Cx<'_>, args: &[&[u8]]) -> CmdResult {
    let [input, count, out] = args else {
        return Err(crate::command_error(
            cx,
            "sub-command REPEAT requires three arguments.",
        ));
    };
    let count = std::str::from_utf8(trim_start(count))
        .ok()
        .and_then(|s| s.parse::<usize>().ok())
        .ok_or_else(|| crate::command_error(cx, "repeat count is not a positive number."))?;
    if input.is_empty() {
        return set(cx, out, "");
    }
    check_size(cx, input.len().saturating_mul(count))?;
    set(cx, out, input.repeat(count))
}

fn compare(cx: &mut Cx<'_>, args: &[&[u8]]) -> CmdResult {
    let [mode, a, b, out, ..] = args else {
        return Err(CmdError::status(
            "sub-command COMPARE requires at least four arguments.",
        ));
    };
    let result = match *mode {
        b"LESS" => a < b,
        b"GREATER" => a > b,
        b"EQUAL" => a == b,
        b"NOTEQUAL" => a != b,
        b"LESS_EQUAL" => a <= b,
        b"GREATER_EQUAL" => a >= b,
        _ => {
            return Err(CmdError::status(format!(
                "sub-command COMPARE does not recognize mode {}",
                mode.to_str_lossy()
            )));
        }
    };
    set(cx, out, if result { "1" } else { "0" })
}
