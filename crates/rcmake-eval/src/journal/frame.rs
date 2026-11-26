//! Size limits and bounded compact JSON streaming for configure journal records.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use rcmake_base::BString;
use rcmake_json::{Value, write_compact};

/// Maximum JSON payload size for a configure snapshot.
pub(super) const MAX_FRAME_BYTES: usize = 16 * 1024 * 1024;

/// Reason bounded compact JSON encoding stopped before completing.
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum EncodeError {
    /// Output would exceed the remaining byte budget.
    #[error("Encoded JSON exceeds the remaining byte budget.")]
    Budget,
    /// A JSON string or key contained invalid UTF-8 bytes.
    #[error("JSON string or object key is not valid UTF-8.")]
    InvalidUtf8,
}

/// Stream compact JSON bytes directly while enforcing a hard byte budget.
///
/// Encoding writes directly without building an intermediate JSON value tree. When measuring
/// sizes without an output buffer, encoding allocates no output buffer, except for the small
/// bounded UTF-16LE scratch buffer used for Windows paths and short formatting buffers for floats.
pub struct JsonSink<'a> {
    output: Option<&'a mut Vec<u8>>,
    left: usize,
}

/// Encode a journal record or snapshot model directly as compact JSON under a hard byte budget.
pub trait EncodeJson {
    /// Stream this value into `sink` without exceeding its remaining byte budget.
    fn encode(&self, sink: &mut JsonSink<'_>) -> Result<(), EncodeError>;
}

/// Measure the compact JSON size of `value`, stopping immediately if it exceeds `limit` bytes.
pub(super) fn encoded_size(value: &(impl EncodeJson + ?Sized), limit: usize) -> Option<usize> {
    let mut sink = JsonSink {
        output: None,
        left: limit,
    };
    value.encode(&mut sink).ok()?;
    Some(limit - sink.left)
}

/// Append the compact JSON of `value` to `output`, rolling back and returning `None`
/// when encoding fails or exceeds `limit` bytes.
#[cfg(test)]
pub(super) fn write_json(
    value: &(impl EncodeJson + ?Sized),
    output: &mut Vec<u8>,
    limit: usize,
) -> Option<()> {
    write_with(output, limit, |sink| value.encode(sink))
}

/// Stream compact JSON produced by `write` into `output`, rolling back on failure.
pub(super) fn write_with(
    output: &mut Vec<u8>,
    limit: usize,
    write: impl FnOnce(&mut JsonSink<'_>) -> Result<(), EncodeError>,
) -> Option<()> {
    let start = output.len();
    let mut sink = JsonSink {
        output: Some(output),
        left: limit,
    };
    if write(&mut sink).is_err() {
        if let Some(output) = sink.output {
            output.truncate(start);
        }
        return None;
    }
    Some(())
}

/// Streaming compact JSON object writer. Fields must be written in strictly sorted key order.
pub struct ObjectWriter<'a, 'b> {
    sink: &'a mut JsonSink<'b>,
    count: usize,
    #[cfg(debug_assertions)]
    last_key: Option<&'static str>,
}

impl ObjectWriter<'_, '_> {
    /// Write one sorted object member using `value` to stream its JSON payload.
    pub fn field(
        &mut self,
        key: &'static str,
        value: impl FnOnce(&mut JsonSink<'_>) -> Result<(), EncodeError>,
    ) -> Result<(), EncodeError> {
        #[cfg(debug_assertions)]
        {
            debug_assert!(
                self.last_key.is_none_or(|prev| prev < key),
                "journal JSON object keys must be strictly sorted: '{key}'"
            );
            self.last_key = Some(key);
        }
        if self.count != 0 {
            self.sink.push_byte(b',')?;
        }
        self.count += 1;
        self.sink.str(key)?;
        self.sink.push_byte(b':')?;
        value(self.sink)
    }

    fn finish(self) -> Result<(), EncodeError> {
        self.sink.push_byte(b'}')
    }
}

impl JsonSink<'_> {
    fn reserve(&mut self, len: usize) -> Result<(), EncodeError> {
        self.left = self.left.checked_sub(len).ok_or(EncodeError::Budget)?;
        Ok(())
    }

    fn check_array_min(&self, len: usize) -> Result<(), EncodeError> {
        let min_len = if len == 0 {
            2
        } else {
            len.saturating_mul(2).saturating_add(1)
        };
        if min_len > self.left {
            return Err(EncodeError::Budget);
        }
        Ok(())
    }

    fn check_object_min(&self, len: usize) -> Result<(), EncodeError> {
        let min_len = if len == 0 {
            2
        } else {
            len.saturating_mul(5).saturating_add(1)
        };
        if min_len > self.left {
            return Err(EncodeError::Budget);
        }
        Ok(())
    }

    fn push_byte(&mut self, byte: u8) -> Result<(), EncodeError> {
        self.reserve(1)?;
        if let Some(output) = &mut self.output {
            output.push(byte);
        }
        Ok(())
    }

    fn push_slice(&mut self, bytes: &[u8]) -> Result<(), EncodeError> {
        self.reserve(bytes.len())?;
        if let Some(output) = &mut self.output {
            output.extend_from_slice(bytes);
        }
        Ok(())
    }

    /// Encode JSON `null`.
    pub fn null(&mut self) -> Result<(), EncodeError> {
        self.push_slice(b"null")
    }

    /// Encode a JSON boolean.
    pub fn bool(&mut self, value: bool) -> Result<(), EncodeError> {
        self.push_slice(if value { b"true" } else { b"false" })
    }

    /// Encode an unsigned 64-bit integer.
    pub fn u64(&mut self, mut value: u64) -> Result<(), EncodeError> {
        let mut buf = [0_u8; 20];
        let mut index = buf.len();
        loop {
            index -= 1;
            if let Some(slot) = buf.get_mut(index) {
                *slot = b'0' + (value % 10) as u8;
            }
            value /= 10;
            if value == 0 {
                break;
            }
        }
        self.push_slice(buf.get(index..).unwrap_or_default())
    }

    /// Encode a signed 64-bit integer.
    pub fn i64(&mut self, value: i64) -> Result<(), EncodeError> {
        if value < 0 {
            self.push_byte(b'-')?;
        }
        self.u64(value.unsigned_abs())
    }

    /// Encode a floating-point number using compact `jsoncpp` formatting.
    pub fn f64(&mut self, value: f64) -> Result<(), EncodeError> {
        let formatted = write_compact(&Value::Real(value));
        self.push_slice(formatted.as_slice())
    }

    /// Encode a UTF-8 JSON string with `jsoncpp`-compatible escaping.
    pub fn str(&mut self, text: &str) -> Result<(), EncodeError> {
        // Every UTF-8 byte encodes to at least one JSON byte, plus the surrounding quotes.
        if text.len().saturating_add(2) > self.left {
            return Err(EncodeError::Budget);
        }
        self.push_byte(b'"')?;
        for ch in text.chars() {
            match ch {
                '"' => self.push_slice(b"\\\"")?,
                '\\' => self.push_slice(b"\\\\")?,
                '\x08' => self.push_slice(b"\\b")?,
                '\x0c' => self.push_slice(b"\\f")?,
                '\n' => self.push_slice(b"\\n")?,
                '\r' => self.push_slice(b"\\r")?,
                '\t' => self.push_slice(b"\\t")?,
                '\x00'..='\x1f' => self.write_hex_escape(ch as u32)?,
                ' '..='\x7f' => self.push_byte(ch as u8)?,
                _ => {
                    let point = ch as u32;
                    if point < 0x10000 {
                        self.write_hex_escape(point)?;
                    } else {
                        let pair = point - 0x10000;
                        self.write_hex_escape(0xd800 + ((pair >> 10) & 0x3ff))?;
                        self.write_hex_escape(0xdc00 + (pair & 0x3ff))?;
                    }
                }
            }
        }
        self.push_byte(b'"')
    }

    fn utf8_bytes(&mut self, raw: &[u8]) -> Result<(), EncodeError> {
        if raw.len().saturating_add(2) > self.left {
            return Err(EncodeError::Budget);
        }
        let text = core::str::from_utf8(raw).map_err(|_| EncodeError::InvalidUtf8)?;
        self.str(text)
    }

    fn write_hex_escape(&mut self, point: u32) -> Result<(), EncodeError> {
        let buf = [
            b'\\',
            b'u',
            hex_digit(((point >> 12) & 0x0f) as u8),
            hex_digit(((point >> 8) & 0x0f) as u8),
            hex_digit(((point >> 4) & 0x0f) as u8),
            hex_digit((point & 0x0f) as u8),
        ];
        self.push_slice(&buf)
    }

    /// Encode raw bytes without loss: a JSON string when valid UTF-8, or a JSON byte array
    /// otherwise. Rejects oversized inputs before allocating or iterating past the budget.
    pub(super) fn bytes(&mut self, raw: &[u8]) -> Result<(), EncodeError> {
        if raw.len().saturating_add(2) > self.left {
            return Err(EncodeError::Budget);
        }
        if let Ok(text) = core::str::from_utf8(raw) {
            return self.str(text);
        }
        // Invalid UTF-8 is non-empty and encodes as `[d0,d1,...]`, taking at least 2*len + 1 bytes.
        if raw.len().saturating_mul(2).saturating_add(1) > self.left {
            return Err(EncodeError::Budget);
        }
        self.push_byte(b'[')?;
        for (index, &byte) in raw.iter().enumerate() {
            if index != 0 {
                self.push_byte(b',')?;
            }
            self.u64(u64::from(byte))?;
        }
        self.push_byte(b']')
    }

    pub(super) fn opt_bytes(&mut self, raw: Option<&[u8]>) -> Result<(), EncodeError> {
        match raw {
            Some(raw) => self.bytes(raw),
            None => self.null(),
        }
    }

    pub(super) fn bytes_list(&mut self, items: &[Vec<u8>]) -> Result<(), EncodeError> {
        self.array(items, |sink, item| sink.bytes(item))
    }

    pub(super) fn byte_pairs(&mut self, items: &[(Vec<u8>, Vec<u8>)]) -> Result<(), EncodeError> {
        self.array(items, |sink, (name, value)| {
            sink.pair(|s| s.bytes(name), |s| s.bytes(value))
        })
    }

    /// Encode a native path in the platform byte form within the remaining budget.
    pub(super) fn path(&mut self, path: &Path) -> Result<(), EncodeError> {
        #[cfg(windows)]
        {
            use std::os::windows::ffi::OsStrExt;
            let max_units = self.left.saturating_sub(2) / 2;
            let mut raw = Vec::new();
            for (count, unit) in path.as_os_str().encode_wide().enumerate() {
                if count >= max_units {
                    return Err(EncodeError::Budget);
                }
                raw.extend_from_slice(&unit.to_le_bytes());
            }
            self.bytes(&raw)
        }
        #[cfg(not(windows))]
        {
            self.bytes(path.as_os_str().as_encoded_bytes())
        }
    }

    /// Encode a 32-byte digest as a 64-character lowercase hexadecimal JSON string.
    pub(super) fn hex_digest(&mut self, bytes: &[u8; 32]) -> Result<(), EncodeError> {
        self.reserve(66)?;
        if let Some(output) = &mut self.output {
            output.push(b'"');
            for &byte in bytes {
                output.push(hex_digit(byte >> 4));
                output.push(hex_digit(byte & 0x0f));
            }
            output.push(b'"');
        }
        Ok(())
    }

    /// Encode a JSON object whose members are streamed in strictly sorted key order.
    pub fn object(
        &mut self,
        write: impl FnOnce(&mut ObjectWriter<'_, '_>) -> Result<(), EncodeError>,
    ) -> Result<(), EncodeError> {
        self.push_byte(b'{')?;
        let mut obj = ObjectWriter {
            sink: self,
            count: 0,
            #[cfg(debug_assertions)]
            last_key: None,
        };
        write(&mut obj)?;
        obj.finish()
    }

    pub(super) fn variant(
        &mut self,
        name: &'static str,
        write: impl FnOnce(&mut JsonSink<'_>) -> Result<(), EncodeError>,
    ) -> Result<(), EncodeError> {
        self.object(|obj| obj.field(name, write))
    }

    /// Encode a slice as a JSON array by streaming each element directly into this sink.
    pub fn array<T>(
        &mut self,
        items: &[T],
        mut write: impl FnMut(&mut JsonSink<'_>, &T) -> Result<(), EncodeError>,
    ) -> Result<(), EncodeError> {
        self.check_array_min(items.len())?;
        self.push_byte(b'[')?;
        for (index, item) in items.iter().enumerate() {
            if index != 0 {
                self.push_byte(b',')?;
            }
            write(self, item)?;
        }
        self.push_byte(b']')
    }

    /// Encode a two-element JSON array.
    pub fn pair(
        &mut self,
        first: impl FnOnce(&mut JsonSink<'_>) -> Result<(), EncodeError>,
        second: impl FnOnce(&mut JsonSink<'_>) -> Result<(), EncodeError>,
    ) -> Result<(), EncodeError> {
        self.push_byte(b'[')?;
        first(self)?;
        self.push_byte(b',')?;
        second(self)?;
        self.push_byte(b']')
    }

    /// Encode an arbitrary [`Value`] compactly while enforcing the remaining budget and
    /// rejecting invalid UTF-8 strings or keys rather than corrupting them lossily.
    pub fn value(&mut self, value: &Value) -> Result<(), EncodeError> {
        match value {
            Value::Null => self.null(),
            Value::Bool(flag) => self.bool(*flag),
            Value::Int(number) => self.i64(*number),
            Value::UInt(number) => self.u64(*number),
            Value::Real(number) => self.f64(*number),
            Value::String(text) => self.utf8_bytes(text.as_slice()),
            Value::Array(items) => self.array(items, |sink, item| sink.value(item)),
            Value::Object(members) => members.encode(self),
        }
    }
}

fn hex_digit(nibble: u8) -> u8 {
    match nibble & 0x0f {
        n @ 0..=9 => b'0' + n,
        n => b'a' + (n - 10),
    }
}

impl EncodeJson for () {
    fn encode(&self, sink: &mut JsonSink<'_>) -> Result<(), EncodeError> {
        sink.null()
    }
}

impl EncodeJson for bool {
    fn encode(&self, sink: &mut JsonSink<'_>) -> Result<(), EncodeError> {
        sink.bool(*self)
    }
}

impl EncodeJson for u8 {
    fn encode(&self, sink: &mut JsonSink<'_>) -> Result<(), EncodeError> {
        sink.u64(u64::from(*self))
    }
}

impl EncodeJson for u16 {
    fn encode(&self, sink: &mut JsonSink<'_>) -> Result<(), EncodeError> {
        sink.u64(u64::from(*self))
    }
}

impl EncodeJson for u32 {
    fn encode(&self, sink: &mut JsonSink<'_>) -> Result<(), EncodeError> {
        sink.u64(u64::from(*self))
    }
}

impl EncodeJson for u64 {
    fn encode(&self, sink: &mut JsonSink<'_>) -> Result<(), EncodeError> {
        sink.u64(*self)
    }
}

impl EncodeJson for usize {
    fn encode(&self, sink: &mut JsonSink<'_>) -> Result<(), EncodeError> {
        sink.u64(u64::try_from(*self).unwrap_or(u64::MAX))
    }
}

impl EncodeJson for i8 {
    fn encode(&self, sink: &mut JsonSink<'_>) -> Result<(), EncodeError> {
        sink.i64(i64::from(*self))
    }
}

impl EncodeJson for i16 {
    fn encode(&self, sink: &mut JsonSink<'_>) -> Result<(), EncodeError> {
        sink.i64(i64::from(*self))
    }
}

impl EncodeJson for i32 {
    fn encode(&self, sink: &mut JsonSink<'_>) -> Result<(), EncodeError> {
        sink.i64(i64::from(*self))
    }
}

impl EncodeJson for i64 {
    fn encode(&self, sink: &mut JsonSink<'_>) -> Result<(), EncodeError> {
        sink.i64(*self)
    }
}

impl EncodeJson for isize {
    fn encode(&self, sink: &mut JsonSink<'_>) -> Result<(), EncodeError> {
        sink.i64(i64::try_from(*self).unwrap_or(if *self < 0 { i64::MIN } else { i64::MAX }))
    }
}

impl EncodeJson for f64 {
    fn encode(&self, sink: &mut JsonSink<'_>) -> Result<(), EncodeError> {
        sink.f64(*self)
    }
}

impl EncodeJson for str {
    fn encode(&self, sink: &mut JsonSink<'_>) -> Result<(), EncodeError> {
        sink.str(self)
    }
}

impl EncodeJson for String {
    fn encode(&self, sink: &mut JsonSink<'_>) -> Result<(), EncodeError> {
        sink.str(self)
    }
}

impl EncodeJson for BString {
    fn encode(&self, sink: &mut JsonSink<'_>) -> Result<(), EncodeError> {
        sink.utf8_bytes(self.as_slice())
    }
}

impl EncodeJson for Value {
    fn encode(&self, sink: &mut JsonSink<'_>) -> Result<(), EncodeError> {
        sink.value(self)
    }
}

impl<T: EncodeJson + ?Sized> EncodeJson for &T {
    fn encode(&self, sink: &mut JsonSink<'_>) -> Result<(), EncodeError> {
        (**self).encode(sink)
    }
}

impl<T: EncodeJson + ?Sized> EncodeJson for Box<T> {
    fn encode(&self, sink: &mut JsonSink<'_>) -> Result<(), EncodeError> {
        (**self).encode(sink)
    }
}

impl<T: EncodeJson> EncodeJson for Option<T> {
    fn encode(&self, sink: &mut JsonSink<'_>) -> Result<(), EncodeError> {
        match self {
            Some(value) => value.encode(sink),
            None => sink.null(),
        }
    }
}

impl<T: EncodeJson> EncodeJson for [T] {
    fn encode(&self, sink: &mut JsonSink<'_>) -> Result<(), EncodeError> {
        sink.array(self, |s, item| item.encode(s))
    }
}

impl<T: EncodeJson> EncodeJson for Vec<T> {
    fn encode(&self, sink: &mut JsonSink<'_>) -> Result<(), EncodeError> {
        self.as_slice().encode(sink)
    }
}

impl<T: EncodeJson, const N: usize> EncodeJson for [T; N] {
    fn encode(&self, sink: &mut JsonSink<'_>) -> Result<(), EncodeError> {
        self.as_slice().encode(sink)
    }
}

impl<T: EncodeJson> EncodeJson for BTreeSet<T> {
    fn encode(&self, sink: &mut JsonSink<'_>) -> Result<(), EncodeError> {
        sink.check_array_min(self.len())?;
        sink.push_byte(b'[')?;
        for (index, item) in self.iter().enumerate() {
            if index != 0 {
                sink.push_byte(b',')?;
            }
            item.encode(sink)?;
        }
        sink.push_byte(b']')
    }
}

impl<T: EncodeJson> EncodeJson for BTreeMap<String, T> {
    fn encode(&self, sink: &mut JsonSink<'_>) -> Result<(), EncodeError> {
        sink.check_object_min(self.len())?;
        sink.push_byte(b'{')?;
        for (index, (key, value)) in self.iter().enumerate() {
            if index != 0 {
                sink.push_byte(b',')?;
            }
            sink.str(key)?;
            sink.push_byte(b':')?;
            value.encode(sink)?;
        }
        sink.push_byte(b'}')
    }
}

impl<T: EncodeJson> EncodeJson for BTreeMap<BString, T> {
    fn encode(&self, sink: &mut JsonSink<'_>) -> Result<(), EncodeError> {
        sink.check_object_min(self.len())?;
        sink.push_byte(b'{')?;
        for (index, (key, value)) in self.iter().enumerate() {
            if index != 0 {
                sink.push_byte(b',')?;
            }
            sink.utf8_bytes(key.as_slice())?;
            sink.push_byte(b':')?;
            value.encode(sink)?;
        }
        sink.push_byte(b'}')
    }
}

impl<A: EncodeJson, B: EncodeJson> EncodeJson for (A, B) {
    fn encode(&self, sink: &mut JsonSink<'_>) -> Result<(), EncodeError> {
        sink.pair(|s| self.0.encode(s), |s| self.1.encode(s))
    }
}
