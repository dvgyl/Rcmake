use std::{collections::BTreeMap, io::Write};

use rcmake_base::BString;

use crate::Value;

const ARRAY_MARGIN: usize = 74;

/// Serialize a JSON value on one line with sorted object keys.
/// The result ends at the last JSON token.
#[must_use]
pub fn write_compact(v: &Value) -> BString {
    write(v, b"")
}

/// Serialize a JSON value in CMake's `string(JSON)` format with sorted object keys.
///
/// Use two spaces for CMake's indentation, or an empty indent for compact output.
/// The result ends at the last JSON token.
#[must_use]
pub fn write_styled(v: &Value, indent: &str) -> BString {
    write(v, indent.as_bytes())
}

fn write(value: &Value, indent: &[u8]) -> BString {
    let mut writer = Writer {
        out: Vec::new(),
        indent,
    };
    writer.value(value, 0, false);
    BString::from(writer.out)
}

struct Writer<'a> {
    out: Vec<u8>,
    indent: &'a [u8],
}

impl Writer<'_> {
    fn value(&mut self, value: &Value, depth: usize, after_key: bool) {
        match value {
            Value::Array(items) if !items.is_empty() => self.array(items, depth, after_key),
            Value::Object(members) if !members.is_empty() => self.object(members, depth, after_key),
            value => self.atomic_value(value),
        }
    }

    fn atomic_value(&mut self, value: &Value) {
        match value {
            Value::Null => self.out.extend_from_slice(b"null"),
            Value::Bool(true) => self.out.extend_from_slice(b"true"),
            Value::Bool(false) => self.out.extend_from_slice(b"false"),
            Value::Int(n) => self.out.extend_from_slice(n.to_string().as_bytes()),
            Value::UInt(n) => self.out.extend_from_slice(n.to_string().as_bytes()),
            Value::Real(n) => write_real(*n, &mut self.out),
            Value::String(s) => write_string(s.as_slice(), &mut self.out),
            Value::Array(_) => self.out.extend_from_slice(b"[]"),
            Value::Object(_) => self.out.extend_from_slice(b"{}"),
        }
    }

    fn array(&mut self, items: &[Value], depth: usize, after_key: bool) {
        if !self.indent.is_empty() && self.inline_array(items) {
            return;
        }
        if after_key {
            self.newline(depth);
        }
        self.out.push(b'[');
        for (index, item) in items.iter().enumerate() {
            self.start_item(index, depth + 1);
            self.value(item, depth + 1, false);
        }
        self.newline(depth);
        self.out.push(b']');
    }

    fn inline_array(&mut self, items: &[Value]) -> bool {
        if items.len().saturating_mul(3) >= ARRAY_MARGIN
            || items.iter().any(|item| match item {
                Value::Array(children) => !children.is_empty(),
                Value::Object(members) => !members.is_empty(),
                Value::String(text) => text.len() >= ARRAY_MARGIN,
                _ => false,
            })
        {
            return false;
        }

        let start = self.out.len();
        self.out.extend_from_slice(b"[ ");
        for (index, item) in items.iter().enumerate() {
            if index != 0 {
                self.out.extend_from_slice(b", ");
            }
            self.atomic_value(item);
            if self.out.len() - start + 2 >= ARRAY_MARGIN {
                self.out.truncate(start);
                return false;
            }
        }
        self.out.extend_from_slice(b" ]");
        true
    }

    fn object(&mut self, members: &BTreeMap<BString, Value>, depth: usize, after_key: bool) {
        if after_key {
            self.newline(depth);
        }
        self.out.push(b'{');
        for (index, (key, value)) in members.iter().enumerate() {
            self.start_item(index, depth + 1);
            write_string(key.as_slice(), &mut self.out);
            if self.indent.is_empty() {
                self.out.push(b':');
            } else {
                self.out.extend_from_slice(b" : ");
            }
            self.value(value, depth + 1, true);
        }
        self.newline(depth);
        self.out.push(b'}');
    }

    fn start_item(&mut self, index: usize, depth: usize) {
        if index != 0 {
            self.out.push(b',');
        }
        self.newline(depth);
    }

    fn newline(&mut self, depth: usize) {
        if !self.indent.is_empty() {
            self.out.push(b'\n');
            for _ in 0..depth {
                self.out.extend_from_slice(self.indent);
            }
        }
    }
}

fn write_real(number: f64, out: &mut Vec<u8>) {
    if number.is_nan() {
        out.extend_from_slice(b"null");
        return;
    }
    if number.is_infinite() {
        if number.is_sign_negative() {
            out.push(b'-');
        }
        out.extend_from_slice(b"1e+9999");
        return;
    }

    // Match %.17g, with a decimal point to preserve the real type in fixed notation.
    let scientific = format!("{number:.16e}");
    let (mantissa, exponent) = scientific.rsplit_once('e').unwrap_or((&scientific, "0"));
    let exponent = exponent.parse::<i32>().unwrap_or_default();
    if (-4..17).contains(&exponent) {
        let precision = (16 - exponent) as usize;
        let fixed = format!("{number:.precision$}");
        let fixed = if precision == 0 {
            &fixed
        } else {
            fixed.trim_end_matches('0').trim_end_matches('.')
        };
        out.extend_from_slice(fixed.as_bytes());
        if !fixed.contains('.') {
            out.extend_from_slice(b".0");
        }
    } else {
        out.extend_from_slice(
            mantissa
                .trim_end_matches('0')
                .trim_end_matches('.')
                .as_bytes(),
        );
        let _ = write!(out, "e{exponent:+03}");
    }
}

fn write_string(mut bytes: &[u8], out: &mut Vec<u8>) {
    out.push(b'"');
    while let Some((&byte, rest)) = bytes.split_first() {
        bytes = rest;
        match byte {
            b'"' => out.extend_from_slice(b"\\\""),
            b'\\' => out.extend_from_slice(b"\\\\"),
            0x08 => out.extend_from_slice(b"\\b"),
            0x0c => out.extend_from_slice(b"\\f"),
            b'\n' => out.extend_from_slice(b"\\n"),
            b'\r' => out.extend_from_slice(b"\\r"),
            b'\t' => out.extend_from_slice(b"\\t"),
            0x00..=0x1f => write_hex(u32::from(byte), out),
            0x20..=0x7f => out.push(byte),
            _ => {
                let point = read_utf8_point(byte, &mut bytes);
                if point < 0x10000 {
                    write_hex(point, out);
                } else {
                    let pair = point - 0x10000;
                    write_hex(0xd800 + ((pair >> 10) & 0x3ff), out);
                    write_hex(0xdc00 + (pair & 0x3ff), out);
                }
            }
        }
    }
    out.push(b'"');
}

fn write_hex(point: u32, out: &mut Vec<u8>) {
    let _ = write!(out, "\\u{point:04x}");
}

fn read_utf8_point(first: u8, bytes: &mut &[u8]) -> u32 {
    const REPLACEMENT: u32 = 0xfffd;
    let (count, mask, minimum) = match first {
        0x80..=0xdf => (1, 0x1f, 0x80),
        0xe0..=0xef => (2, 0x0f, 0x800),
        0xf0..=0xf7 => (3, 0x07, 0x10000),
        _ => return REPLACEMENT,
    };
    let Some((tail, rest)) = bytes.split_at_checked(count) else {
        return REPLACEMENT;
    };
    *bytes = rest;

    // jsoncpp uses the low six bits of each continuation byte, even if it is malformed.
    let point = tail.iter().fold(u32::from(first & mask), |point, byte| {
        (point << 6) | u32::from(byte & 0x3f)
    });
    if point < minimum || (0xd800..=0xdfff).contains(&point) {
        REPLACEMENT
    } else {
        point
    }
}
