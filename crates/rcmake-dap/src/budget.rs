//! Exact sizes of compact JSON text.
//!
//! The adapter checks the size of a response against its byte budget before it copies
//! data into the response, and the outbound queue checks the size of a message before it
//! serializes the message. The sizes match `rcmake_json::write_compact`, including its
//! escapes: a control byte or a byte of invalid UTF-8 becomes six bytes (`\u0001`,
//! `�`), and a character outside the Basic Multilingual Plane becomes twelve.

use rcmake_json::Value;

/// Most bytes in the JSON text of one response body.
pub(crate) const MAX_RESPONSE_BYTES: usize = 8 * 1024 * 1024;

/// Most bytes of one `Value::Real` (`-1.2345678901234567e+308` has 24). The adapter
/// does not send real numbers, so this upper bound is the only inexact size.
const MAX_REAL_BYTES: usize = 32;

/// Bytes of the escape for one character of a string.
const ESCAPE: usize = 6;

/// Bytes of the JSON string (with its quotes) for the concatenation of `bytes`.
///
/// Return `None` as soon as the size is more than `limit`, without reading the remaining
/// bytes.
pub(crate) fn string_len<I>(bytes: I, limit: usize) -> Option<usize>
where
    I: Iterator<Item = u8> + Clone,
{
    let mut bytes = bytes;
    let mut len: usize = 2;
    while let Some(byte) = bytes.next() {
        if len > limit {
            return None;
        }
        len = len.saturating_add(match byte {
            b'"' | b'\\' | 0x08 | 0x0c | b'\n' | b'\r' | b'\t' => 2,
            0x00..=0x1f => ESCAPE,
            0x20..=0x7f => 1,
            _ => non_ascii_len(byte, &mut bytes),
        });
    }
    (len <= limit).then_some(len)
}

/// [`string_len`] for one byte string.
pub(crate) fn bytes_len(bytes: &[u8], limit: usize) -> Option<usize> {
    string_len(bytes.iter().copied(), limit)
}

/// Bytes of the escape for the character that starts with the non-ASCII byte `first`.
/// Consume its continuation bytes from `bytes` as the writer does: the writer takes
/// the number of bytes that `first` gives if that many remain, and otherwise only
/// `first`.
fn non_ascii_len<I: Iterator<Item = u8> + Clone>(first: u8, bytes: &mut I) -> usize {
    let (count, mask) = match first {
        0x80..=0xdf => (1, 0x1f),
        0xe0..=0xef => (2, 0x0f),
        0xf0..=0xf7 => (3, 0x07),
        _ => return ESCAPE,
    };
    let mut ahead = bytes.clone();
    let mut point = u32::from(first & mask);
    for _ in 0..count {
        let Some(next) = ahead.next() else {
            return ESCAPE;
        };
        point = (point << 6) | u32::from(next & 0x3f);
    }
    *bytes = ahead;
    // Only a valid four-byte sequence gives a point outside the Basic Multilingual Plane,
    // which the writer sends as a surrogate pair. Every other point, and the replacement
    // character for a malformed sequence, is one escape.
    if point >= 0x1_0000 {
        2 * ESCAPE
    } else {
        ESCAPE
    }
}

/// Bytes of the compact JSON text of `value`. The sum saturates at `usize::MAX`.
pub(crate) fn value_len(value: &Value) -> usize {
    match value {
        Value::Null | Value::Bool(true) => 4,
        Value::Bool(false) => 5,
        Value::Int(n) => usize::from(*n < 0).saturating_add(digits(n.unsigned_abs())),
        Value::UInt(n) => digits(*n),
        Value::Real(_) => MAX_REAL_BYTES,
        Value::String(text) => bytes_len(text.as_slice(), usize::MAX).unwrap_or(usize::MAX),
        Value::Array(items) => items.iter().fold(separators(items.len()), |sum, item| {
            sum.saturating_add(value_len(item))
        }),
        Value::Object(members) => {
            members
                .iter()
                .fold(separators(members.len()), |sum, (key, item)| {
                    let key = bytes_len(key.as_slice(), usize::MAX).unwrap_or(usize::MAX);
                    // The `:` after the key.
                    sum.saturating_add(key)
                        .saturating_add(1)
                        .saturating_add(value_len(item))
                })
        }
    }
}

/// Bytes of the brackets and commas of a container with `n` items.
const fn separators(n: usize) -> usize {
    2 + n.saturating_sub(1)
}

const fn digits(mut n: u64) -> usize {
    let mut count = 1;
    while n >= 10 {
        n /= 10;
        count += 1;
    }
    count
}

#[cfg(test)]
mod tests {
    use rcmake_base::BString;

    use super::*;

    /// A deterministic xorshift generator.
    struct Noise(u64);

    impl Noise {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }

        fn bytes(&mut self) -> BString {
            let len = usize::try_from(self.next() % 24).unwrap_or(0);
            // Mostly bytes that need escapes: control, quote, and non-ASCII.
            const PICK: &[u8] =
                b"\x00\x01\x1f\x7f\"\\\n\t az\x80\xbf\xc3\xa9\xdf\xe0\xed\xef\xf0\xf4\xf7\xf8\xff";
            (0..len)
                .map(|_| PICK[usize::try_from(self.next() % PICK.len() as u64).unwrap_or(0)])
                .collect::<Vec<u8>>()
                .into()
        }

        fn value(&mut self, depth: u32) -> Value {
            match self.next() % if depth > 3 { 5 } else { 7 } {
                0 => Value::Null,
                1 => Value::Bool(self.next().is_multiple_of(2)),
                2 => Value::Int(self.next().cast_signed()),
                3 => Value::UInt(self.next()),
                4 => Value::String(self.bytes()),
                5 => Value::Array(
                    (0..self.next() % 4)
                        .map(|_| self.value(depth + 1))
                        .collect(),
                ),
                _ => {
                    let mut map = std::collections::BTreeMap::new();
                    for _ in 0..self.next() % 4 {
                        map.insert(self.bytes(), self.value(depth + 1));
                    }
                    Value::Object(map)
                }
            }
        }
    }

    #[test]
    fn sizes_match_the_compact_writer() {
        let mut noise = Noise(0x9e37_79b9_7f4a_7c15);
        for _ in 0..20_000 {
            let value = noise.value(0);
            assert_eq!(
                value_len(&value),
                rcmake_json::write_compact(&value).len(),
                "{value:?}"
            );
        }
        for n in [0, 9, 10, -1, -10, i64::MIN, i64::MAX] {
            let value = Value::Int(n);
            assert_eq!(value_len(&value), rcmake_json::write_compact(&value).len());
        }
        for n in [
            -0.0,
            1e-7,
            f64::MAX,
            f64::MIN_POSITIVE,
            f64::NAN,
            f64::INFINITY,
        ] {
            let value = Value::Real(n);
            assert!(value_len(&value) >= rcmake_json::write_compact(&value).len());
        }
    }

    #[test]
    fn concatenated_parts_match_the_joined_string() {
        // A truncated lead byte takes the bytes of the next part, as in the joined text.
        let parts: [&[u8]; 3] = [b"A\xe0", b":S", b"TRING\xf0"];
        let joined = parts.concat();
        let expected = rcmake_json::write_compact(&Value::string(joined)).len();
        let chained = parts.iter().flat_map(|part| part.iter()).copied();
        assert_eq!(string_len(chained, usize::MAX), Some(expected));
    }

    #[test]
    fn escape_expansion_is_counted_against_the_limit() {
        let control = [0x01_u8; 100];
        assert_eq!(bytes_len(&control, usize::MAX), Some(602));
        assert_eq!(bytes_len(&control, 602), Some(602));
        assert_eq!(bytes_len(&control, 601), None);
        // A lone invalid byte becomes `�`, six bytes for one.
        assert_eq!(bytes_len(&[0xff; 10], usize::MAX), Some(62));
        assert_eq!(bytes_len(b"", 1), None);
        assert_eq!(bytes_len(b"", 2), Some(2));
    }
}
