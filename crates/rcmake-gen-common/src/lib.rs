//! Generator-independent outputs shared by the Ninja and Makefile generators (ADR-009).
//!
//! Each module owns one output format.

#![forbid(unsafe_code)]

use std::path::Path;

use rcmake_base::BString;
use rcmake_host::{Host, HostError, path::from_native};

pub mod compile_commands;
pub mod configure_log;
pub mod cpack_properties;
pub mod ctest_file;
pub mod export;
pub mod fileapi;
pub mod install_script;
pub mod sbom;

#[cfg(test)]
mod tests;

/// Error of a generator or writer.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum GenError {
    /// Writing an output file failed.
    #[error("cannot write {path}: {message}")]
    Write {
        /// Output path.
        path: BString,
        /// Host error text.
        message: BString,
    },
    /// An install action refers to an export file that the caller did not supply.
    #[error("missing generated install export file for plan {0}")]
    MissingExport(usize),
    /// The output is not implemented yet.
    #[error("{0} is not implemented yet")]
    Unimplemented(&'static str),
}

/// Write `content` to `path` through `host`, replacing the file only when the content
/// changed (keeps build tool timestamps stable).
pub fn write_if_changed(host: &dyn Host, path: &Path, content: &[u8]) -> Result<(), GenError> {
    let limit = u64::try_from(content.len())
        .unwrap_or(u64::MAX)
        .saturating_add(1);
    if let Ok(old) = host.read(path, rcmake_host::ReadRange::All { limit })
        && old == content
    {
        return Ok(());
    }
    host.write(path, content, rcmake_host::WriteMode::Truncate)
        .map_err(|err| write_error(path, err))
}

fn write_error(path: &Path, err: HostError) -> GenError {
    GenError::Write {
        path: from_native(path),
        message: err.to_string().into(),
    }
}

fn quote(out: &mut BString, value: &[u8]) {
    out.push(b'"');
    quoted_content(out, value);
    out.push(b'"');
}

fn quoted_content(out: &mut BString, value: &[u8]) {
    for &byte in value {
        if matches!(byte, b'"' | b'\\' | b'$') {
            out.push(b'\\');
        }
        out.push(byte);
    }
}

fn configuration_pattern(configurations: &[BString]) -> BString {
    let mut pattern = BString::from("^(");
    for (index, config) in configurations.iter().enumerate() {
        if index != 0 {
            pattern.push(b'|');
        }
        for &byte in config.as_slice() {
            if byte.is_ascii_alphabetic() {
                pattern.extend_from_slice(&[
                    b'[',
                    byte.to_ascii_uppercase(),
                    byte.to_ascii_lowercase(),
                    b']',
                ]);
                continue;
            }
            if b".^$[]()|*+?\\".contains(&byte) {
                pattern.push(b'\\');
            }
            pattern.push(byte);
        }
    }
    pattern.extend_from_slice(b")$");
    pattern
}
