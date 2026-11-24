//! Core byte-string, list, truthiness, version, hash, and safety-limit primitives for Rcmake.
//!
//! CMake values are arbitrary byte strings (not necessarily UTF-8), so this crate
//! re-exports [`bstr`] types and implements CMake 4.5.1's exact list splitting,
//! boolean truthiness, arbitrary-precision version comparison, cryptographic hashes,
//! and execution limits over byte slices.

#![forbid(unsafe_code)]

pub mod hash;
pub mod limits;
pub mod list;
pub mod list_ops;
pub mod path_lex;
pub mod qt_resource;
pub mod shell;
mod source_date_epoch;
pub mod suggest;
pub mod truth;
pub mod version;

pub use bstr::{BStr, BString, ByteSlice, ByteVec};
pub use limits::Limits;
pub use source_date_epoch::parse_source_date_epoch;

/// Target CMake version implemented by Rcmake (`4.5.1`).
pub const CMAKE_VERSION_STR: &str = "4.5.1";

/// Target CMake major version (`4`).
pub const CMAKE_VERSION_MAJOR: u16 = 4;

/// Target CMake minor version (`5`).
pub const CMAKE_VERSION_MINOR: u16 = 5;

/// Target CMake patch version (`1`).
pub const CMAKE_VERSION_PATCH: u16 = 1;

/// Fold a CMake command name to ASCII lowercase, matching CMake's case-insensitive
/// command dispatch while preserving non-ASCII bytes unchanged.
#[must_use]
pub fn fold_command_name(name: &[u8]) -> BString {
    let mut out = Vec::with_capacity(name.len());
    for &b in name {
        out.push(b.to_ascii_lowercase());
    }
    BString::from(out)
}
