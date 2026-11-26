//! JSON helpers for configure journal records.
//!
//! Byte strings use a JSON string when they are valid UTF-8 and an array of
//! byte values otherwise, so every byte survives a round trip. Native paths use
//! the platform byte form: UTF-16LE units on Windows and raw bytes elsewhere.

use std::path::{Path, PathBuf};

use rcmake_base::BString;
use rcmake_json::{FieldError, FromJson, Object, Value, array, deny_unknown, object, req_value};

/// Encode raw bytes without loss.
pub(super) fn bytes(raw: &[u8]) -> Value {
    if core::str::from_utf8(raw).is_ok() {
        Value::String(BString::from(raw))
    } else {
        Value::array_from(raw.iter().map(|byte| Value::UInt(u64::from(*byte))))
    }
}

/// Decode bytes written by [`bytes`].
pub(super) fn from_bytes(value: &Value) -> Result<Vec<u8>, FieldError> {
    match value {
        Value::String(text) => Ok(text.to_vec()),
        Value::Array(items) => items.iter().map(u8::from_json).collect(),
        _ => Err(FieldError::expected("byte string", value)),
    }
}

/// Encode optional raw bytes as `null` or [`bytes`].
pub(super) fn opt_bytes(raw: Option<&[u8]>) -> Value {
    raw.map_or(Value::Null, bytes)
}

/// Decode optional raw bytes written by [`opt_bytes`].
pub(super) fn from_opt_bytes(value: &Value) -> Result<Option<Vec<u8>>, FieldError> {
    match value {
        Value::Null => Ok(None),
        _ => from_bytes(value).map(Some),
    }
}

/// Encode a list of byte strings.
pub(super) fn bytes_list(items: &[Vec<u8>]) -> Value {
    Value::array_from(items.iter().map(Vec::as_slice).map(bytes))
}

/// Decode a list written by [`bytes_list`].
pub(super) fn from_bytes_list(value: &Value) -> Result<Vec<Vec<u8>>, FieldError> {
    from_list(value, from_bytes)
}

/// Encode name and value pairs as two-element arrays.
pub(super) fn byte_pairs(items: &[(Vec<u8>, Vec<u8>)]) -> Value {
    list(items, |(name, value)| pair(bytes(name), bytes(value)))
}

/// Decode pairs written by [`byte_pairs`].
pub(super) fn from_byte_pairs(value: &Value) -> Result<Vec<(Vec<u8>, Vec<u8>)>, FieldError> {
    from_list(value, |item| {
        let (name, value) = from_pair(item)?;
        Ok((from_bytes(name)?, from_bytes(value)?))
    })
}

/// Encode a native path in the platform byte form.
pub(super) fn path(path: &Path) -> Value {
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        let raw: Vec<u8> = path
            .as_os_str()
            .encode_wide()
            .flat_map(u16::to_le_bytes)
            .collect();
        bytes(&raw)
    }
    #[cfg(not(windows))]
    {
        bytes(path.as_os_str().as_encoded_bytes())
    }
}

/// Decode a native path written by [`path`].
pub(super) fn from_path(value: &Value) -> Result<PathBuf, FieldError> {
    let raw = from_bytes(value)?;
    #[cfg(windows)]
    {
        use std::ffi::OsString;
        use std::os::windows::ffi::OsStringExt;
        let (units, rest) = raw.as_chunks::<2>();
        if !rest.is_empty() {
            return Err(FieldError::invalid("Invalid Windows path encoding."));
        }
        let wide: Vec<u16> = units.iter().copied().map(u16::from_le_bytes).collect();
        Ok(OsString::from_wide(&wide).into())
    }
    #[cfg(unix)]
    {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt;
        Ok(OsString::from_vec(raw).into())
    }
    #[cfg(not(any(unix, windows)))]
    {
        String::from_utf8(raw)
            .map(PathBuf::from)
            .map_err(|_| FieldError::invalid("Path is not valid UTF-8."))
    }
}

/// Encode each item with `encode` into a JSON array.
pub(super) fn list<T>(items: &[T], encode: impl Fn(&T) -> Value) -> Value {
    Value::array_from(items.iter().map(encode))
}

/// Decode each element of a JSON array with `decode`.
pub(super) fn from_list<T>(
    value: &Value,
    decode: impl Fn(&Value) -> Result<T, FieldError>,
) -> Result<Vec<T>, FieldError> {
    array(value)?.iter().map(decode).collect()
}

/// Encode two values as a two-element array.
pub(super) fn pair(first: Value, second: Value) -> Value {
    Value::Array(vec![first, second])
}

/// Split a two-element array written by [`pair`].
pub(super) fn from_pair(value: &Value) -> Result<(&Value, &Value), FieldError> {
    match array(value)? {
        [first, second] => Ok((first, second)),
        items => Err(FieldError::invalid(format!(
            "expected array of length 2, found {}",
            items.len()
        ))),
    }
}

/// Decode the required member `key` of `obj` with `decode`.
pub(super) fn field<T>(
    obj: &Object,
    key: &str,
    decode: impl FnOnce(&Value) -> Result<T, FieldError>,
) -> Result<T, FieldError> {
    decode(req_value(obj, key)?).map_err(|err| err.in_field(key))
}

/// Borrow the members of an object that has only the `allowed` members.
pub(super) fn fields<'a>(value: &'a Value, allowed: &[&str]) -> Result<&'a Object, FieldError> {
    let obj = object(value)?;
    deny_unknown(obj, allowed)?;
    Ok(obj)
}

/// Encode an enum variant that has data as a single-member object.
pub(super) fn variant(name: &str, data: Value) -> Value {
    Value::object_from([(name, data)])
}

/// Decode an enum variant written as a string (no data) or by [`variant`].
///
/// `decode` gets the variant name and its data. Errors name the variant.
pub(super) fn decode_variant<T>(
    value: &Value,
    decode: impl FnOnce(&str, Option<&Value>) -> Result<T, FieldError>,
) -> Result<T, FieldError> {
    let (name, data) = match value {
        Value::String(name) => (name.as_slice(), None),
        Value::Object(members) if members.len() == 1 => members
            .iter()
            .next()
            .map(|(name, data)| (name.as_slice(), Some(data)))
            .ok_or_else(|| FieldError::expected("enum variant", value))?,
        _ => return Err(FieldError::expected("enum variant", value)),
    };
    let name = core::str::from_utf8(name).map_err(|_| unknown_variant(name))?;
    decode(name, data).map_err(|err| err.in_field(name))
}

/// Get the data of a variant that must have data.
pub(super) fn variant_data<'a>(
    name: &str,
    data: Option<&'a Value>,
) -> Result<&'a Value, FieldError> {
    data.ok_or_else(|| unknown_variant(name))
}

/// Borrow the members of a variant whose data is an object with only the
/// `allowed` members.
pub(super) fn variant_fields<'a>(
    name: &str,
    data: Option<&'a Value>,
    allowed: &[&str],
) -> Result<&'a Object, FieldError> {
    fields(variant_data(name, data)?, allowed)
}

/// Report a variant name that the decoder does not know, or a known variant
/// with the wrong data shape.
pub(super) fn unknown_variant(name: impl AsRef<[u8]>) -> FieldError {
    FieldError::invalid(format!(
        "unknown variant '{}'",
        String::from_utf8_lossy(name.as_ref())
    ))
}

/// Encode `value` as compact JSON text, decode the text, and require that the
/// decoded value encodes to the same text.
#[cfg(test)]
pub(super) fn round_trip<T: rcmake_json::ToJson + FromJson>(
    value: &T,
) -> Result<T, Box<dyn std::error::Error>> {
    let options = rcmake_json::ReadOptions {
        allow_comments: false,
        dup_keys_last_wins: false,
    };
    let text = rcmake_json::write_compact(&value.to_json());
    let decoded = T::from_json(&rcmake_json::parse(&text, &options)?)?;
    assert_eq!(rcmake_json::write_compact(&decoded.to_json()), text);
    Ok(decoded)
}

#[cfg(test)]
#[path = "codec_tests.rs"]
mod tests;
