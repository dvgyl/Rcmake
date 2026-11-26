use rcmake_base::{BString, ByteSlice};
use rcmake_eval::{CmdError, CmdResult, Cx};
use rcmake_json::{JsonError, ReadOptions, Value, parse, write_compact, write_styled};

use super::{append, check_size, set, trim_start};

pub(super) fn command(cx: &mut Cx<'_>, args: &[&[u8]]) -> CmdResult {
    let [out, args @ ..] = args else {
        return Err(CmdError::status(
            "sub-command JSON missing out-var argument.",
        ));
    };
    let (error_out, args) = match args {
        [b"ERROR_VARIABLE", error, rest @ ..] => (Some(*error), rest),
        [b"ERROR_VARIABLE"] => {
            return Err(CmdError::status(
                "sub-command JSON missing error-var argument.",
            ));
        }
        _ => (None, args),
    };
    match evaluate(cx, args) {
        Ok(value) => {
            if let Some(error_out) = error_out {
                cx.set_var(error_out, "NOTFOUND".into());
            }
            set(cx, out, value)
        }
        Err(error) => {
            let mut missing = Vec::new();
            if let Some(path) = error.path {
                for element in path {
                    append(cx, &mut missing, element)?;
                    append(cx, &mut missing, b"-")?;
                }
                if path.is_empty() {
                    append(cx, &mut missing, b"-")?;
                }
            }
            append(cx, &mut missing, b"NOTFOUND")?;
            cx.set_var(out, missing.into());
            if let Some(error_out) = error_out {
                set(cx, error_out, error.message)
            } else {
                Err(CmdError::status(format!(
                    "sub-command JSON {}.",
                    error.message
                )))
            }
        }
    }
}

struct Failure<'a> {
    path: Option<&'a [&'a [u8]]>,
    message: String,
}

impl<'a> Failure<'a> {
    fn new(message: impl Into<String>) -> Self {
        Self {
            path: None,
            message: message.into(),
        }
    }

    fn at(mut self, path: &'a [&'a [u8]]) -> Self {
        self.path = Some(path);
        self
    }
}

type Result<'a, T> = std::result::Result<T, Failure<'a>>;

fn evaluate<'a>(cx: &Cx<'_>, args: &'a [&'a [u8]]) -> Result<'a, BString> {
    let [mode, args @ ..] = args else {
        return Err(Failure::new("missing mode argument"));
    };
    match *mode {
        b"GET" | b"GET_RAW" | b"TYPE" | b"MEMBER" | b"LENGTH" | b"ARRAY_SPLIT" | b"REMOVE"
        | b"SET" | b"EQUAL" | b"PARTIAL_EQUAL" | b"STRING_ENCODE" => {}
        _ => {
            return Err(Failure::new(format!(
                "got an invalid mode '{}', expected one of GET, GET_RAW, TYPE, MEMBER, LENGTH, ARRAY_SPLIT, REMOVE, SET, EQUAL, PARTIAL_EQUAL, STRING_ENCODE",
                mode.to_str_lossy()
            )));
        }
    }
    let [input, path @ ..] = args else {
        return Err(Failure::new("missing json string argument"));
    };
    if *mode == b"STRING_ENCODE" {
        return Ok(write_compact(&Value::string(*input)));
    }

    let mut root = read(input)?;
    let output = match *mode {
        b"EQUAL" | b"PARTIAL_EQUAL" => {
            let second = path
                .first()
                .ok_or_else(|| Failure::new("missing second json string argument"))?;
            let right = read(second)?;
            let equal = if *mode == b"EQUAL" {
                root == right
            } else {
                partial_equal(&root, &right, &mut 1_000_000)?
            };
            boolean(equal).into()
        }
        b"SET" => {
            let (value, path) = path
                .split_last()
                .ok_or_else(|| Failure::new("missing new value remove"))?;
            let (key, parent) = path
                .split_last()
                .ok_or_else(|| Failure::new("missing member name to add"))?;
            let new = read(value)?;
            match lookup(&mut root, parent)? {
                Value::Object(map) => {
                    map.insert(BString::from(*key), new);
                }
                Value::Array(items) => {
                    let pos =
                        index(key, Some(u32::MAX as usize)).map_err(|error| error.at(path))?;
                    if let Some(item) = items.get_mut(pos) {
                        *item = new;
                    } else {
                        items.push(new);
                    }
                }
                value => return Err(wrong_type("SET", "OBJECT or ARRAY", value)),
            }
            styled(&root)
        }
        b"REMOVE" => {
            let (key, parent) = path
                .split_last()
                .ok_or_else(|| Failure::new("missing member or index to remove"))?;
            match lookup(&mut root, parent)? {
                Value::Object(map) => {
                    map.remove(*key);
                }
                Value::Array(items) => {
                    let pos = index(key, Some(items.len())).map_err(|error| error.at(path))?;
                    items.remove(pos);
                }
                value => return Err(wrong_type("REMOVE", "ARRAY or OBJECT", value).at(parent)),
            }
            styled(&root)
        }
        b"MEMBER" => {
            let (key, parent) = path
                .split_last()
                .ok_or_else(|| Failure::new("missing member index"))?;
            match lookup(&mut root, parent)? {
                Value::Object(map) => {
                    let pos = index(key, Some(map.len())).map_err(|error| error.at(path))?;
                    map.keys().nth(pos).cloned().unwrap_or_default()
                }
                value => return Err(wrong_type("MEMBER", "OBJECT", value).at(parent)),
            }
        }
        _ => {
            let value = lookup(&mut root, path)?;
            match *mode {
                b"GET" => match value {
                    Value::Null => BString::default(),
                    Value::Bool(flag) => boolean(*flag).into(),
                    Value::String(text) => text.clone(),
                    _ => styled(value),
                },
                b"GET_RAW" => styled(value),
                b"TYPE" => type_name(value).into(),
                b"LENGTH" => match value {
                    Value::Object(map) => map.len().to_string().into(),
                    Value::Array(items) => items.len().to_string().into(),
                    _ => return Err(wrong_type("LENGTH", "ARRAY or OBJECT", value).at(path)),
                },
                b"ARRAY_SPLIT" => match value {
                    Value::Array(items) => array_split(cx, items)?,
                    _ => return Err(wrong_type("ARRAY_SPLIT", "ARRAY", value).at(path)),
                },
                _ => return Err(Failure::new("invalid JSON mode")),
            }
        }
    };
    Ok(output)
}

fn read<'a>(source: &[u8]) -> Result<'a, Value> {
    const OPTIONS: ReadOptions = ReadOptions {
        allow_comments: true,
        dup_keys_last_wins: true,
    };
    let source = source.strip_prefix(b"\xef\xbb\xbf").unwrap_or(source);
    if source.is_empty() {
        return Err(Failure::new(
            "failed parsing json string:\nA JSON document cannot be empty",
        ));
    }
    if let Ok(value) = parse(source, &OPTIONS) {
        return Ok(value);
    }
    let normalized = normalize(source);
    let err = match parse(&normalized, &OPTIONS) {
        Ok(value) => return Ok(value),
        Err(error) => error,
    };
    let line = error_line(source, &err);
    Err(Failure::new(format!(
        "failed parsing json string:\n{}\n{}\n{}^",
        err.message,
        line.to_str_lossy(),
        " ".repeat(err.column.saturating_sub(1) as usize),
    )))
}

fn error_line<'a>(source: &'a [u8], error: &JsonError) -> &'a [u8] {
    let mut start = 0;
    for _ in 1..error.line {
        let tail = source.get(start..).unwrap_or_default();
        let Some(end) = tail.iter().position(|&byte| matches!(byte, b'\r' | b'\n')) else {
            break;
        };
        start += end + 1;
        if tail
            .get(end..)
            .is_some_and(|rest| rest.starts_with(b"\r\n"))
        {
            start += 1;
        }
    }
    let tail = source.get(start..).unwrap_or_default();
    let end = tail
        .iter()
        .position(|&byte| matches!(byte, b'\r' | b'\n'))
        .unwrap_or(tail.len());
    tail.get(..end).unwrap_or_default()
}

/// CMake permits a trailing comma, leading zeroes, and input after the first value.
/// Keep byte offsets for diagnostics and let the shared parser validate each value.
fn normalize(source: &[u8]) -> Vec<u8> {
    let mut normalized = source.to_vec();
    let mut pos = 0;
    let mut depth = 0usize;
    let mut previous = None;
    let mut comma = None;
    while let Some(tail) = source.get(pos..) {
        let Some(&byte) = tail.first() else { break };
        match tail {
            [b' ' | b'\t' | b'\r' | b'\n', ..] => {
                pos += 1;
                continue;
            }
            [b'/', b'/', rest @ ..] => {
                pos += 2 + rest
                    .iter()
                    .position(|&byte| matches!(byte, b'\r' | b'\n'))
                    .unwrap_or(rest.len());
                continue;
            }
            [b'/', b'*', rest @ ..] => {
                let Some(end) = rest.find(b"*/") else { break };
                pos += 4 + end;
                continue;
            }
            _ => {}
        }

        let end = match byte {
            b'{' | b'[' => {
                depth += 1;
                1
            }
            b'}' | b']' => {
                if let Some(index) = comma
                    && let Some(byte) = normalized.get_mut(index)
                {
                    *byte = b' ';
                }
                depth = depth.saturating_sub(1);
                1
            }
            b'"' => {
                let mut escaped = false;
                let mut chars = tail.iter().skip(1);
                let mut len = 1;
                for &byte in &mut chars {
                    len += 1;
                    if !escaped && byte == b'"' {
                        break;
                    }
                    escaped = !escaped && byte == b'\\';
                }
                len
            }
            b'-' | b'0'..=b'9' => {
                let sign = usize::from(byte == b'-');
                let digits = tail.get(sign..).unwrap_or_default();
                let zeroes = digits.iter().take_while(|&&byte| byte == b'0').count();
                let integer_len = digits
                    .iter()
                    .take_while(|byte| byte.is_ascii_digit())
                    .count();
                let remove = zeroes.min(integer_len.saturating_sub(1));
                if remove != 0 {
                    if let Some(prefix) = normalized.get_mut(pos..pos + remove) {
                        prefix.fill(b' ');
                    }
                    if sign == 1
                        && let Some(sign) = normalized.get_mut(pos + remove)
                    {
                        *sign = b'-';
                    }
                }
                number_length(tail, sign + integer_len)
            }
            b't' if tail.starts_with(b"true") => 4,
            b'n' if tail.starts_with(b"null") => 4,
            b'f' if tail.starts_with(b"false") => 5,
            _ => 1,
        };
        comma = (byte == b','
            && previous.is_some_and(|byte| !matches!(byte, b',' | b'[' | b'{' | b':')))
        .then_some(pos);
        previous = Some(byte);
        pos += end;
        if depth == 0 {
            normalized.truncate(pos);
            break;
        }
    }
    normalized
}

fn number_length(mut tail: &[u8], integer_end: usize) -> usize {
    let start = tail.len();
    tail = tail.get(integer_end..).unwrap_or_default();
    if let Some(fraction) = tail.strip_prefix(b".") {
        let digits = fraction
            .iter()
            .take_while(|byte| byte.is_ascii_digit())
            .count();
        tail = fraction.get(digits..).unwrap_or_default();
    }
    if let [b'e' | b'E', exponent @ ..] = tail {
        let unsigned = match exponent {
            [b'+' | b'-', rest @ ..] => rest,
            _ => exponent,
        };
        let digits = unsigned
            .iter()
            .take_while(|byte| byte.is_ascii_digit())
            .count();
        tail = unsigned.get(digits..).unwrap_or_default();
    }
    start - tail.len()
}

fn lookup<'a, 'v>(mut value: &'v mut Value, path: &'a [&'a [u8]]) -> Result<'a, &'v mut Value> {
    for (i, key) in path.iter().enumerate() {
        let traversed = path.get(..=i).unwrap_or_default();
        value =
            match value {
                Value::Object(map) => map.get_mut(*key).ok_or_else(|| {
                    Failure::new(format!(
                        "member '{}' not found",
                        traversed.join(&b' ').to_str_lossy()
                    ))
                    .at(traversed)
                })?,
                Value::Array(items) => {
                    let i = index(key, Some(items.len())).map_err(|error| error.at(traversed))?;
                    items
                        .get_mut(i)
                        .ok_or_else(|| Failure::new("invalid array index").at(traversed))?
                }
                scalar => return Err(Failure::new(format!(
                    "invalid path '{}', need element of OBJECT or ARRAY type to lookup '{}' got {}",
                    traversed.join(&b' ').to_str_lossy(),
                    key.to_str_lossy(),
                    type_name(scalar)
                ))
                .at(traversed)),
            };
    }
    Ok(value)
}

fn index<'a>(key: &[u8], length: Option<usize>) -> Result<'a, usize> {
    let pos = std::str::from_utf8(trim_start(key))
        .ok()
        .and_then(|key| key.parse::<u64>().ok())
        .map(|n| n as u32 as usize)
        .ok_or_else(|| {
            Failure::new(format!(
                "expected an array index, got: '{}'",
                key.to_str_lossy()
            ))
        })?;
    if let Some(length) = length
        && pos >= length
    {
        return Err(Failure::new(format!(
            "expected an index less than {length} got '{}'",
            key.to_str_lossy()
        )));
    }
    Ok(pos)
}

fn boolean(flag: bool) -> &'static str {
    if flag { "ON" } else { "OFF" }
}

fn styled(value: &Value) -> BString {
    write_styled(value, "  ")
}

fn type_name(value: &Value) -> &'static str {
    match value {
        Value::Null => "NULL",
        Value::Bool(_) => "BOOLEAN",
        Value::Int(_) | Value::UInt(_) | Value::Real(_) => "NUMBER",
        Value::String(_) => "STRING",
        Value::Array(_) => "ARRAY",
        Value::Object(_) => "OBJECT",
    }
}

fn wrong_type<'a>(mode: &str, expected: &str, value: &Value) -> Failure<'a> {
    Failure::new(format!(
        "{mode} needs to be called with an element of type {expected}, got {}",
        type_name(value)
    ))
}

fn array_split<'a>(cx: &Cx<'_>, values: &[Value]) -> Result<'a, BString> {
    let mut output = Vec::new();
    let mut quoted = false;
    let mut escaped = false;
    for (i, value) in values.iter().enumerate() {
        if i != 0 {
            output.push(b';');
        }
        for &byte in write_compact(value).as_slice() {
            let encoded = match byte {
                b';' => b"\\;".as_slice(),
                b'[' if quoted => b"\\u005B".as_slice(),
                b']' if quoted => b"\\u005D".as_slice(),
                _ => std::slice::from_ref(&byte),
            };
            output.extend_from_slice(encoded);
            if byte == b'"' && !escaped {
                quoted = !quoted;
            }
            escaped = byte == b'\\' && !escaped;
        }
        check_size(cx, output.len())
            .map_err(|_| Failure::new("result exceeds the value size limit"))?;
    }
    Ok(output.into())
}

fn partial_equal<'a>(pattern: &Value, actual: &Value, remaining: &mut u32) -> Result<'a, bool> {
    *remaining = remaining
        .checked_sub(1)
        .ok_or_else(|| Failure::new("partial equality exceeded the comparison limit"))?;
    match (pattern, actual) {
        (Value::Object(pattern), Value::Object(actual)) => {
            for (key, value) in pattern {
                let Some(other) = actual.get(key) else {
                    return Ok(false);
                };
                if !partial_equal(value, other, remaining)? {
                    return Ok(false);
                }
            }
            Ok(true)
        }
        (Value::Array(pattern), Value::Array(actual)) => {
            if pattern.len() > actual.len() {
                return Ok(false);
            }
            let mut used = vec![false; actual.len()];
            for value in pattern {
                let mut matched = false;
                for (other, used) in actual.iter().zip(&mut used) {
                    if !*used && partial_equal(value, other, remaining)? {
                        *used = true;
                        matched = true;
                        break;
                    }
                }
                if !matched {
                    return Ok(false);
                }
            }
            Ok(true)
        }
        _ => Ok(pattern == actual),
    }
}
