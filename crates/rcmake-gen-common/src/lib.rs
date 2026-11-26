//! Generator-independent outputs shared by the Ninja and Makefile generators (ADR-009).
//!
//! Each module owns one output format.

#![forbid(unsafe_code)]

use std::path::Path;

use rcmake_base::BString;
use rcmake_genex::{EvalContext, PreprocessMode};
use rcmake_host::{Host, HostError, path::from_native};

pub mod compile_commands;
pub mod configure_log;
pub mod cpack_properties;
pub mod ctest_file;
pub mod export;
pub mod fileapi;
pub mod glob;
pub mod ide;
pub mod install_script;
pub mod link_response;
pub mod sbom;

pub use install_script::write_supplementary_outputs;

#[cfg(test)]
mod package_install_tests;
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
    /// The output data is invalid.
    #[error("{0}")]
    Invalid(BString),
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

/// Create the parent directory and update the file when its contents change.
///
/// # Errors
///
/// Return the path and host error if either operation fails.
pub fn write_output(host: &dyn Host, path: &Path, content: &[u8]) -> Result<(), GenError> {
    if let Some(parent) = path.parent() {
        host.create_dir_all(parent)
            .map_err(|error| write_error(parent, error))?;
    }
    write_if_changed(host, path, content)
}

fn write_error(path: &Path, err: HostError) -> GenError {
    GenError::Write {
        path: from_native(path),
        message: err.to_string().into(),
    }
}

use rcmake_base::shell::{cmake_quote as quote, cmake_quoted_content as quoted_content};

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

fn script_argument(out: &mut BString, value: &[u8]) {
    if !value
        .iter()
        .any(|&byte| matches!(byte, b'"' | b'\\' | b'$'))
    {
        quote(out, value);
        return;
    }
    bracket_argument(out, value);
}

fn bracket_argument(out: &mut BString, value: &[u8]) {
    // Leave room for any closing delimiter inside the argument or at its end.
    let width = value
        .split(|&byte| byte == b']')
        .skip(1)
        .map(|part| part.iter().take_while(|&&byte| byte == b'=').count() + 1)
        .max()
        .unwrap_or_default();
    let delimiter = std::iter::repeat_n(b'=', width);
    out.push(b'[');
    out.extend(delimiter.clone());
    out.push(b'[');
    if value.starts_with(b"\n") {
        out.push(b'\n');
    }
    out.extend_from_slice(value);
    out.push(b']');
    out.extend(delimiter);
    out.push(b']');
}

fn genex(
    resolver: &mut rcmake_resolve::usage::UsageResolver<'_>,
    dir: rcmake_model::DirId,
    context: &EvalContext,
    value: &[u8],
    mode: Option<PreprocessMode>,
) -> Result<BString, GenError> {
    if value.len() <= rcmake_base::Limits::default().max_value_bytes
        && rcmake_genex::find_genex(value).is_none()
    {
        return Ok(value.into());
    }
    let mut evaluate = || -> Result<BString, rcmake_genex::GenexError> {
        let ast = rcmake_genex::parse(value)?;
        let ast = match mode {
            Some(mode) => rcmake_genex::strip(&ast, mode),
            None => ast,
        };
        rcmake_genex::evaluate(&ast, context, &mut resolver.host(dir)).map(|(value, _)| value)
    };
    evaluate().map_err(|error| GenError::Invalid(error.to_string().into()))
}
