//! `jsoncpp`-compatible JSON value model, parser, and writers for Rcmake.
//!
//! CMake relies on `jsoncpp` for `string(JSON)`, `cmake -E capabilities`, Presets v13,
//! File-API v2.12, and SARIF output. Using a dedicated byte-aware JSON crate keeps
//! key ordering (`BTreeMap`), comment handling, duplicate-key behavior, and formatting
//! consistent across the entire workspace.

#![forbid(unsafe_code)]

use std::collections::BTreeMap;

use rcmake_base::BString;
use thiserror::Error;

/// A JSON value following `jsoncpp`'s type distinctions (including signed vs unsigned
/// integers and sorted object keys).
#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    /// JSON `null`.
    Null,
    /// JSON boolean.
    Bool(bool),
    /// Signed 64-bit integer.
    Int(i64),
    /// Unsigned 64-bit integer.
    UInt(u64),
    /// Floating-point number.
    Real(f64),
    /// JSON string stored as raw bytes.
    String(BString),
    /// Ordered JSON array.
    Array(Vec<Value>),
    /// Sorted JSON object (alphabetical bytewise key order, matching `jsoncpp`).
    Object(BTreeMap<BString, Value>),
}

impl Value {
    /// Create a JSON string value from UTF-8 or byte content.
    #[must_use]
    pub fn string(s: impl Into<BString>) -> Self {
        Self::String(s.into())
    }

    /// Create an empty JSON object.
    #[must_use]
    pub fn object() -> Self {
        Self::Object(BTreeMap::new())
    }
}

/// Options controlling `jsoncpp`-compatible JSON parsing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReadOptions {
    /// Allow C/C++ `//` and `/* ... */` comments.
    pub allow_comments: bool,
    /// When `true`, duplicate object keys overwrite earlier values; when `false`,
    /// duplicate keys produce a parse error.
    pub dup_keys_last_wins: bool,
}

impl Default for ReadOptions {
    fn default() -> Self {
        Self {
            allow_comments: true,
            dup_keys_last_wins: true,
        }
    }
}

/// Error returned when parsing JSON fails.
#[derive(Clone, Debug, PartialEq, Eq, Error)]
#[error("* Line {line}, Column {column}\n  {message}")]
pub struct JsonError {
    /// 1-based line number of the error.
    pub line: u32,
    /// 1-based column number of the error.
    pub column: u32,
    /// Human-readable diagnostic message.
    pub message: String,
}

/// Serialize `v` as compact JSON with alphabetically sorted object keys.
#[must_use]
pub fn write_compact(v: &Value) -> BString {
    let mut out = Vec::new();
    write_value(v, None, 0, &mut out);
    BString::from(out)
}

/// Serialize `v` as pretty-printed JSON with `indent` per nesting level and sorted keys.
#[must_use]
pub fn write_styled(v: &Value, indent: &str) -> BString {
    let mut out = Vec::new();
    write_value(v, Some(indent.as_bytes()), 0, &mut out);
    out.push(b'\n');
    BString::from(out)
}

fn write_value(v: &Value, indent: Option<&[u8]>, depth: usize, out: &mut Vec<u8>) {
    match v {
        Value::Null => out.extend_from_slice(b"null"),
        Value::Bool(true) => out.extend_from_slice(b"true"),
        Value::Bool(false) => out.extend_from_slice(b"false"),
        Value::Int(n) => out.extend_from_slice(n.to_string().as_bytes()),
        Value::UInt(n) => out.extend_from_slice(n.to_string().as_bytes()),
        Value::Real(r) => {
            if r.is_finite() {
                out.extend_from_slice(r.to_string().as_bytes());
            } else {
                out.extend_from_slice(b"null");
            }
        }
        Value::String(s) => write_escaped_string(s.as_slice(), out),
        Value::Array(items) => {
            if items.is_empty() {
                out.extend_from_slice(b"[]");
                return;
            }
            out.push(b'[');
            for (idx, item) in items.iter().enumerate() {
                if idx > 0 {
                    out.push(b',');
                }
                if let Some(ind) = indent {
                    out.push(b'\n');
                    for _ in 0..=depth {
                        out.extend_from_slice(ind);
                    }
                }
                write_value(item, indent, depth + 1, out);
            }
            if let Some(ind) = indent {
                out.push(b'\n');
                for _ in 0..depth {
                    out.extend_from_slice(ind);
                }
            }
            out.push(b']');
        }
        Value::Object(map) => {
            if map.is_empty() {
                out.extend_from_slice(b"{}");
                return;
            }
            out.push(b'{');
            for (idx, (k, val)) in map.iter().enumerate() {
                if idx > 0 {
                    out.push(b',');
                }
                if let Some(ind) = indent {
                    out.push(b'\n');
                    for _ in 0..=depth {
                        out.extend_from_slice(ind);
                    }
                }
                write_escaped_string(k.as_slice(), out);
                if indent.is_some() {
                    out.extend_from_slice(b" : ");
                } else {
                    out.push(b':');
                }
                write_value(val, indent, depth + 1, out);
            }
            if let Some(ind) = indent {
                out.push(b'\n');
                for _ in 0..depth {
                    out.extend_from_slice(ind);
                }
            }
            out.push(b'}');
        }
    }
}

fn write_escaped_string(bytes: &[u8], out: &mut Vec<u8>) {
    out.push(b'"');
    for &b in bytes {
        match b {
            b'"' => out.extend_from_slice(b"\\\""),
            b'\\' => out.extend_from_slice(b"\\\\"),
            0x08 => out.extend_from_slice(b"\\b"),
            0x0C => out.extend_from_slice(b"\\f"),
            b'\n' => out.extend_from_slice(b"\\n"),
            b'\r' => out.extend_from_slice(b"\\r"),
            b'\t' => out.extend_from_slice(b"\\t"),
            c if c < 0x20 => {
                out.extend_from_slice(format!("\\u{c:04x}").as_bytes());
            }
            c => out.push(c),
        }
    }
    out.push(b'"');
}

/// Parse a JSON byte slice using `options`.
///
/// Full `jsoncpp`-exact parsing is implemented in Milestone 1 Wave 1; this Milestone 0
/// placeholder rejects non-empty input cleanly without panicking.
pub fn parse(_src: &[u8], _options: &ReadOptions) -> Result<Value, JsonError> {
    Err(JsonError {
        line: 1,
        column: 1,
        message: "JSON parser not yet implemented (scheduled for M1-W1)".to_owned(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compact_json_sorts_keys() {
        let mut obj = BTreeMap::new();
        obj.insert(BString::from("z"), Value::Int(1));
        obj.insert(BString::from("a"), Value::string("hi\n"));
        let out = write_compact(&Value::Object(obj));
        assert_eq!(out, BString::from(r#"{"a":"hi\n","z":1}"#));
    }
}
