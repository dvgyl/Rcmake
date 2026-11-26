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

mod read;
mod write;

pub use read::parse;
pub use write::{write_compact, write_pretty_utf8, write_string, write_styled};

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

    /// Collect object entries in byte order by key. The last value for a key wins.
    pub fn object_from<K: Into<BString>>(entries: impl IntoIterator<Item = (K, Self)>) -> Self {
        Self::Object(
            entries
                .into_iter()
                .map(|(key, value)| (key.into(), value))
                .collect(),
        )
    }

    /// Collect array elements in iterator order.
    pub fn array_from(entries: impl IntoIterator<Item = Self>) -> Self {
        Self::Array(entries.into_iter().collect())
    }

    /// Look up an object member by its key.
    pub fn get(&self, key: impl AsRef<[u8]>) -> Option<&Self> {
        self.as_object()?.get(key.as_ref())
    }

    /// Borrow a string value as UTF-8. Return `None` for invalid UTF-8.
    #[must_use]
    pub fn as_str(&self) -> Option<&str> {
        core::str::from_utf8(self.as_bytes()?).ok()
    }

    /// Borrow the bytes of a string value.
    #[must_use]
    pub fn as_bytes(&self) -> Option<&[u8]> {
        match self {
            Self::String(value) => Some(value),
            _ => None,
        }
    }

    /// Borrow the elements of an array.
    #[must_use]
    pub fn as_array(&self) -> Option<&[Self]> {
        match self {
            Self::Array(value) => Some(value),
            _ => None,
        }
    }

    /// Borrow the entries of an object.
    #[must_use]
    pub fn as_object(&self) -> Option<&BTreeMap<BString, Self>> {
        match self {
            Self::Object(value) => Some(value),
            _ => None,
        }
    }

    /// Return an integer that fits in a signed 64-bit value.
    #[must_use]
    pub fn as_i64(&self) -> Option<i64> {
        match self {
            Self::Int(value) => Some(*value),
            Self::UInt(value) => i64::try_from(*value).ok(),
            _ => None,
        }
    }

    /// Return a Boolean value.
    #[must_use]
    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Self::Bool(value) => Some(*value),
            _ => None,
        }
    }
}

/// Options controlling `jsoncpp`-compatible JSON parsing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReadOptions {
    /// Skip C/C++ `//` and `/* ... */` comments during parsing.
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

#[cfg(test)]
mod tests;
