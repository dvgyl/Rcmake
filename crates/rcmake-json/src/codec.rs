//! Typed JSON encoding, decoding, and object field extractors over [`Value`].

use std::collections::{BTreeMap, BTreeSet};

use rcmake_base::BString;
use thiserror::Error;

use crate::Value;

/// Sorted JSON object map keyed by raw byte strings.
pub type Object = BTreeMap<BString, Value>;

/// Error returned when decoding a typed value from a JSON [`Value`].
#[derive(Clone, Debug, PartialEq, Eq, Error)]
pub enum FieldError {
    /// A required object member was missing.
    #[error("missing required field '{field}'")]
    Missing {
        /// Name of the missing field.
        field: String,
    },
    /// An object contained an unexpected member.
    #[error("unknown field '{field}'")]
    Unknown {
        /// Name of the unexpected field.
        field: String,
    },
    /// A JSON value did not have the expected type.
    #[error("expected {expected}, found {actual}")]
    Type {
        /// Expected JSON type name.
        expected: &'static str,
        /// Actual JSON type name.
        actual: &'static str,
    },
    /// A numeric value did not fit in the target integer type.
    #[error("integer out of range for {expected}")]
    Range {
        /// Target Rust integer type name.
        expected: &'static str,
    },
    /// A value failed domain validation or conversion.
    #[error("{message}")]
    Invalid {
        /// Human-readable description of the validation failure.
        message: String,
    },
    /// A nested object field failed to decode.
    #[error("field '{field}': {source}")]
    Field {
        /// Name of the field that failed.
        field: String,
        /// Underlying decode error.
        #[source]
        source: Box<Self>,
    },
}

impl FieldError {
    /// Construct a missing-field error.
    #[must_use]
    pub fn missing(field: impl Into<String>) -> Self {
        Self::Missing {
            field: field.into(),
        }
    }

    /// Construct an unknown-field error.
    #[must_use]
    pub fn unknown(field: impl Into<String>) -> Self {
        Self::Unknown {
            field: field.into(),
        }
    }

    /// Construct a type-mismatch error against `value`'s actual JSON type.
    #[must_use]
    pub const fn expected(expected: &'static str, value: &Value) -> Self {
        Self::Type {
            expected,
            actual: value.type_name(),
        }
    }

    /// Construct an integer-range error for `expected`.
    #[must_use]
    pub const fn range(expected: &'static str) -> Self {
        Self::Range { expected }
    }

    /// Construct a domain validation error.
    #[must_use]
    pub fn invalid(message: impl Into<String>) -> Self {
        Self::Invalid {
            message: message.into(),
        }
    }

    /// Attach an enclosing object field name to this error.
    #[must_use]
    pub fn in_field(self, field: impl Into<String>) -> Self {
        Self::Field {
            field: field.into(),
            source: Box::new(self),
        }
    }
}

/// Convert a Rust value into a JSON [`Value`].
pub trait ToJson {
    /// Encode this value as a [`Value`].
    fn to_json(&self) -> Value;
}

/// Decode a Rust value from a JSON [`Value`].
pub trait FromJson: Sized {
    /// Decode a value from `value`.
    ///
    /// # Errors
    ///
    /// Returns a [`FieldError`] when `value` has the wrong JSON type, overflows
    /// the target type, or fails validation.
    fn from_json(value: &Value) -> Result<Self, FieldError>;
}

/// Look up a required raw [`Value`] member in `obj`.
///
/// # Errors
///
/// Returns [`FieldError::Missing`] if `key` is absent.
pub fn req_value<'a>(obj: &'a Object, key: &str) -> Result<&'a Value, FieldError> {
    obj.get(key.as_bytes())
        .ok_or_else(|| FieldError::missing(key))
}

/// Decode a required typed member from `obj`.
///
/// # Errors
///
/// Returns [`FieldError::Missing`] if `key` is absent, or [`FieldError::Field`]
/// wrapping the decode failure if the value cannot be decoded as `T`.
pub fn req<T: FromJson>(obj: &Object, key: &str) -> Result<T, FieldError> {
    let value = req_value(obj, key)?;
    T::from_json(value).map_err(|err| err.in_field(key))
}

/// Decode an optional typed member from `obj`.
///
/// Returns `Ok(None)` when `key` is absent or maps to [`Value::Null`].
///
/// # Errors
///
/// Returns [`FieldError::Field`] wrapping the decode failure if a non-null
/// value cannot be decoded as `T`.
pub fn opt<T: FromJson>(obj: &Object, key: &str) -> Result<Option<T>, FieldError> {
    match obj.get(key.as_bytes()) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => T::from_json(value)
            .map(Some)
            .map_err(|err| err.in_field(key)),
    }
}

/// Decode an optional typed member from `obj`, falling back to `default` only
/// when `key` is absent.
///
/// Unlike [`opt`], an explicit JSON `null` is passed to `T::from_json` and
/// rejected unless `T` accepts `null`.
///
/// # Errors
///
/// Returns [`FieldError::Field`] wrapping the decode failure if `key` is
/// present and cannot be decoded as `T`.
pub fn opt_or_else<T: FromJson>(
    obj: &Object,
    key: &str,
    default: impl FnOnce() -> T,
) -> Result<T, FieldError> {
    match obj.get(key.as_bytes()) {
        None => Ok(default()),
        Some(value) => T::from_json(value).map_err(|err| err.in_field(key)),
    }
}

/// Reject any member in `obj` whose key is not listed in `allowed`.
///
/// # Errors
///
/// Returns [`FieldError::Unknown`] for the first unexpected or non-UTF-8 key
/// in sorted byte order.
pub fn deny_unknown(obj: &Object, allowed: &[&str]) -> Result<(), FieldError> {
    for key in obj.keys() {
        let is_allowed =
            core::str::from_utf8(key.as_slice()).is_ok_and(|name| allowed.contains(&name));
        if !is_allowed {
            return Err(FieldError::unknown(String::from_utf8_lossy(key.as_slice())));
        }
    }
    Ok(())
}

/// Extract an unsigned 64-bit integer from `value`.
///
/// # Errors
///
/// Returns [`FieldError::Type`] if `value` is not an integer, or
/// [`FieldError::Range`] if it is negative.
pub fn u64(value: &Value) -> Result<core::primitive::u64, FieldError> {
    match value {
        Value::UInt(n) => Ok(*n),
        Value::Int(n) => core::primitive::u64::try_from(*n).map_err(|_| FieldError::range("u64")),
        _ => Err(FieldError::expected("u64", value)),
    }
}

/// Extract a signed 64-bit integer from `value`.
///
/// # Errors
///
/// Returns [`FieldError::Type`] if `value` is not an integer, or
/// [`FieldError::Range`] if it exceeds `i64::MAX`.
pub fn i64(value: &Value) -> Result<core::primitive::i64, FieldError> {
    match value {
        Value::Int(n) => Ok(*n),
        Value::UInt(n) => core::primitive::i64::try_from(*n).map_err(|_| FieldError::range("i64")),
        _ => Err(FieldError::expected("i64", value)),
    }
}

/// Extract a pointer-sized unsigned integer from `value`.
///
/// # Errors
///
/// Returns [`FieldError::Type`] if `value` is not an integer, or
/// [`FieldError::Range`] if it is negative or overflows `usize`.
pub fn usize(value: &Value) -> Result<core::primitive::usize, FieldError> {
    match value {
        Value::UInt(n) => {
            core::primitive::usize::try_from(*n).map_err(|_| FieldError::range("usize"))
        }
        Value::Int(n) => {
            core::primitive::usize::try_from(*n).map_err(|_| FieldError::range("usize"))
        }
        _ => Err(FieldError::expected("usize", value)),
    }
}

/// Extract a boolean from `value`.
///
/// # Errors
///
/// Returns [`FieldError::Type`] if `value` is not a [`Value::Bool`].
pub fn bool(value: &Value) -> Result<core::primitive::bool, FieldError> {
    value
        .as_bool()
        .ok_or_else(|| FieldError::expected("boolean", value))
}

/// Extract a UTF-8 string slice from `value`.
///
/// # Errors
///
/// Returns [`FieldError::Type`] if `value` is not a [`Value::String`], or
/// [`FieldError::Invalid`] if the string bytes are not valid UTF-8.
pub fn str(value: &Value) -> Result<&core::primitive::str, FieldError> {
    let raw = bytes(value)?;
    core::str::from_utf8(raw).map_err(|_| FieldError::invalid("string is not valid UTF-8"))
}

/// Extract the raw bytes of a JSON string from `value`.
///
/// # Errors
///
/// Returns [`FieldError::Type`] if `value` is not a [`Value::String`].
pub fn bytes(value: &Value) -> Result<&[u8], FieldError> {
    value
        .as_bytes()
        .ok_or_else(|| FieldError::expected("string", value))
}

/// Extract a JSON array slice from `value`.
///
/// # Errors
///
/// Returns [`FieldError::Type`] if `value` is not a [`Value::Array`].
pub fn array(value: &Value) -> Result<&[Value], FieldError> {
    value
        .as_array()
        .ok_or_else(|| FieldError::expected("array", value))
}

/// Decode a JSON array containing at most `max` elements.
///
/// # Errors
///
/// Returns [`FieldError::Type`] if `value` is not a [`Value::Array`],
/// [`FieldError::Invalid`] if the array exceeds `max` elements, or the first
/// element decode error.
pub fn array_bounded<T: FromJson>(
    value: &Value,
    max: core::primitive::usize,
) -> Result<Vec<T>, FieldError> {
    let items = array(value)?;
    if items.len() > max {
        return Err(FieldError::invalid(format!(
            "array length {} exceeds limit of {max}",
            items.len()
        )));
    }
    items.iter().map(T::from_json).collect()
}

/// Extract a JSON object map from `value`.
///
/// # Errors
///
/// Returns [`FieldError::Type`] if `value` is not a [`Value::Object`].
pub fn object(value: &Value) -> Result<&Object, FieldError> {
    value
        .as_object()
        .ok_or_else(|| FieldError::expected("object", value))
}

impl ToJson for Value {
    fn to_json(&self) -> Value {
        self.clone()
    }
}

impl FromJson for Value {
    fn from_json(value: &Value) -> Result<Self, FieldError> {
        Ok(value.clone())
    }
}

impl ToJson for () {
    fn to_json(&self) -> Value {
        Value::Null
    }
}

impl FromJson for () {
    fn from_json(value: &Value) -> Result<Self, FieldError> {
        match value {
            Value::Null => Ok(()),
            _ => Err(FieldError::expected("null", value)),
        }
    }
}

impl ToJson for core::primitive::bool {
    fn to_json(&self) -> Value {
        Value::Bool(*self)
    }
}

impl FromJson for core::primitive::bool {
    fn from_json(value: &Value) -> Result<Self, FieldError> {
        bool(value)
    }
}

macro_rules! impl_unsigned_json {
    ($($ty:ty),+ $(,)?) => {
        $(
            impl ToJson for $ty {
                fn to_json(&self) -> Value {
                    Value::UInt(core::primitive::u64::try_from(*self).unwrap_or(core::primitive::u64::MAX))
                }
            }

            impl FromJson for $ty {
                fn from_json(value: &Value) -> Result<Self, FieldError> {
                    match value {
                        Value::UInt(n) => Self::try_from(*n).map_err(|_| FieldError::range(stringify!($ty))),
                        Value::Int(n) => Self::try_from(*n).map_err(|_| FieldError::range(stringify!($ty))),
                        _ => Err(FieldError::expected(stringify!($ty), value)),
                    }
                }
            }
        )+
    };
}

macro_rules! impl_signed_json {
    ($($ty:ty),+ $(,)?) => {
        $(
            impl ToJson for $ty {
                fn to_json(&self) -> Value {
                    Value::Int(core::primitive::i64::try_from(*self).unwrap_or(core::primitive::i64::MAX))
                }
            }

            impl FromJson for $ty {
                fn from_json(value: &Value) -> Result<Self, FieldError> {
                    match value {
                        Value::Int(n) => Self::try_from(*n).map_err(|_| FieldError::range(stringify!($ty))),
                        Value::UInt(n) => Self::try_from(*n).map_err(|_| FieldError::range(stringify!($ty))),
                        _ => Err(FieldError::expected(stringify!($ty), value)),
                    }
                }
            }
        )+
    };
}

impl_unsigned_json!(u8, u16, u32, core::primitive::u64, core::primitive::usize);
impl_signed_json!(i8, i16, i32, core::primitive::i64, isize);

impl ToJson for f64 {
    fn to_json(&self) -> Value {
        Value::Real(*self)
    }
}

impl FromJson for f64 {
    fn from_json(value: &Value) -> Result<Self, FieldError> {
        match value {
            Value::Real(n) => Ok(*n),
            Value::Int(n) => Ok(*n as Self),
            Value::UInt(n) => Ok(*n as Self),
            _ => Err(FieldError::expected("number", value)),
        }
    }
}

impl ToJson for core::primitive::str {
    fn to_json(&self) -> Value {
        Value::string(self)
    }
}

impl ToJson for String {
    fn to_json(&self) -> Value {
        Value::string(self.as_str())
    }
}

impl FromJson for String {
    fn from_json(value: &Value) -> Result<Self, FieldError> {
        str(value).map(Self::from)
    }
}

impl ToJson for BString {
    fn to_json(&self) -> Value {
        Value::String(self.clone())
    }
}

impl FromJson for BString {
    fn from_json(value: &Value) -> Result<Self, FieldError> {
        bytes(value).map(Self::from)
    }
}

impl<T: ToJson + ?Sized> ToJson for &T {
    fn to_json(&self) -> Value {
        (**self).to_json()
    }
}

impl<T: ToJson + ?Sized> ToJson for Box<T> {
    fn to_json(&self) -> Value {
        (**self).to_json()
    }
}

impl<T: FromJson> FromJson for Box<T> {
    fn from_json(value: &Value) -> Result<Self, FieldError> {
        T::from_json(value).map(Self::new)
    }
}

impl<T: ToJson> ToJson for Option<T> {
    fn to_json(&self) -> Value {
        match self {
            Some(value) => value.to_json(),
            None => Value::Null,
        }
    }
}

impl<T: FromJson> FromJson for Option<T> {
    fn from_json(value: &Value) -> Result<Self, FieldError> {
        match value {
            Value::Null => Ok(None),
            _ => T::from_json(value).map(Some),
        }
    }
}

impl<T: ToJson> ToJson for [T] {
    fn to_json(&self) -> Value {
        Value::Array(self.iter().map(ToJson::to_json).collect())
    }
}

impl<T: ToJson> ToJson for Vec<T> {
    fn to_json(&self) -> Value {
        self.as_slice().to_json()
    }
}

impl<T: FromJson> FromJson for Vec<T> {
    fn from_json(value: &Value) -> Result<Self, FieldError> {
        array(value)?.iter().map(T::from_json).collect()
    }
}

impl<T: ToJson, const N: core::primitive::usize> ToJson for [T; N] {
    fn to_json(&self) -> Value {
        self.as_slice().to_json()
    }
}

impl<T: FromJson, const N: core::primitive::usize> FromJson for [T; N] {
    fn from_json(value: &Value) -> Result<Self, FieldError> {
        let items = array(value)?;
        if items.len() != N {
            return Err(FieldError::invalid(format!(
                "expected array of length {N}, found {}",
                items.len()
            )));
        }
        let decoded = items
            .iter()
            .map(T::from_json)
            .collect::<Result<Vec<T>, _>>()?;
        decoded
            .try_into()
            .map_err(|_| FieldError::invalid(format!("expected array of length {N}")))
    }
}

impl<T: ToJson> ToJson for BTreeSet<T> {
    fn to_json(&self) -> Value {
        Value::Array(self.iter().map(ToJson::to_json).collect())
    }
}

impl<T: FromJson + Ord> FromJson for BTreeSet<T> {
    fn from_json(value: &Value) -> Result<Self, FieldError> {
        array(value)?.iter().map(T::from_json).collect()
    }
}

impl<T: ToJson> ToJson for BTreeMap<String, T> {
    fn to_json(&self) -> Value {
        Value::Object(
            self.iter()
                .map(|(k, v)| (BString::from(k.as_str()), v.to_json()))
                .collect(),
        )
    }
}

impl<T: FromJson> FromJson for BTreeMap<String, T> {
    fn from_json(value: &Value) -> Result<Self, FieldError> {
        let obj = object(value)?;
        let mut out = Self::new();
        for (key, item) in obj {
            let name = core::str::from_utf8(key.as_slice())
                .map_err(|_| FieldError::invalid("object key is not valid UTF-8"))?;
            let decoded = T::from_json(item).map_err(|err| err.in_field(name))?;
            out.insert(name.to_owned(), decoded);
        }
        Ok(out)
    }
}

impl<T: ToJson> ToJson for BTreeMap<BString, T> {
    fn to_json(&self) -> Value {
        Value::Object(self.iter().map(|(k, v)| (k.clone(), v.to_json())).collect())
    }
}

impl<T: FromJson> FromJson for BTreeMap<BString, T> {
    fn from_json(value: &Value) -> Result<Self, FieldError> {
        let obj = object(value)?;
        let mut out = Self::new();
        for (key, item) in obj {
            let decoded = T::from_json(item)
                .map_err(|err| err.in_field(String::from_utf8_lossy(key.as_slice())))?;
            out.insert(key.clone(), decoded);
        }
        Ok(out)
    }
}
