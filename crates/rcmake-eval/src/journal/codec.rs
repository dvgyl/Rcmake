//! JSON decode helpers for configure journal records.
//!
//! Byte strings use a JSON string when they are valid UTF-8 and an array of
//! byte values otherwise, so every byte survives a round trip. Native paths use
//! the platform byte form: UTF-16LE units on Windows and raw bytes elsewhere.

use std::path::PathBuf;

use rcmake_json::{FieldError, FromJson, Object, Value, array, deny_unknown, object, req_value};

/// Decode bytes written by [`super::frame::JsonSink::bytes`].
pub(super) fn from_bytes(value: &Value) -> Result<Vec<u8>, FieldError> {
    match value {
        Value::String(text) => Ok(text.to_vec()),
        Value::Array(items) => items.iter().map(u8::from_json).collect(),
        _ => Err(FieldError::expected("byte string", value)),
    }
}

/// Decode optional raw bytes written by [`super::frame::JsonSink::opt_bytes`].
pub(super) fn from_opt_bytes(value: &Value) -> Result<Option<Vec<u8>>, FieldError> {
    match value {
        Value::Null => Ok(None),
        _ => from_bytes(value).map(Some),
    }
}

/// Decode a list written by [`super::frame::JsonSink::bytes_list`].
pub(super) fn from_bytes_list(value: &Value) -> Result<Vec<Vec<u8>>, FieldError> {
    from_list(value, from_bytes)
}

/// Name-and-value byte pairs stored in journal records.
pub(super) type BytePairs = Vec<(Vec<u8>, Vec<u8>)>;

/// Decode pairs written by [`super::frame::JsonSink::byte_pairs`].
pub(super) fn from_byte_pairs(value: &Value) -> Result<BytePairs, FieldError> {
    from_list(value, |item| {
        let (name, value) = from_pair(item)?;
        Ok((from_bytes(name)?, from_bytes(value)?))
    })
}

/// Decode a native path written by [`super::frame::JsonSink::path`].
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

/// Decode each element of a JSON array with `decode`.
pub(super) fn from_list<T>(
    value: &Value,
    decode: impl Fn(&Value) -> Result<T, FieldError>,
) -> Result<Vec<T>, FieldError> {
    array(value)?.iter().map(decode).collect()
}

/// Split a two-element array written by [`super::frame::JsonSink::pair`].
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

/// Decode an enum variant written as a string (no data) or single-member object.
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

/// Encode `value` as compact JSON text, verify `encoded_size`, decode the text,
/// and require that the decoded value encodes to the same text.
#[cfg(test)]
pub(super) fn round_trip<T: super::frame::EncodeJson + FromJson>(
    value: &T,
) -> Result<T, Box<dyn std::error::Error>> {
    use super::frame::{MAX_FRAME_BYTES, encoded_size, write_json};

    let options = rcmake_json::ReadOptions {
        allow_comments: false,
        dup_keys_last_wins: false,
    };
    let mut text = Vec::new();
    write_json(value, &mut text, MAX_FRAME_BYTES).ok_or("encode failed")?;
    assert_eq!(encoded_size(value, MAX_FRAME_BYTES), Some(text.len()));
    let decoded = T::from_json(&rcmake_json::parse(&text, &options)?)?;
    let mut reencoded = Vec::new();
    write_json(&decoded, &mut reencoded, MAX_FRAME_BYTES).ok_or("re-encode failed")?;
    assert_eq!(reencoded, text);
    Ok(decoded)
}

#[cfg(test)]
#[path = "codec_tests.rs"]
mod tests;
