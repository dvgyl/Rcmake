//! Ninja `ninja_dyndep_version = 1.0` writer and the `cmake -E cmake_ninja_dyndep`
//! request.

use rcmake_base::{BString, path_lex::Lexical};

use crate::collate::{CollationInput, collate};
use crate::json::invalid;
use crate::{ModmapFormat, ModulesError, P1689File, TargetDependInfo};

/// File header that identifies Ninja dyndep version 1.0.
pub const HEADER: &[u8] = b"ninja_dyndep_version = 1.0\n";

/// Arguments of `cmake -E cmake_ninja_dyndep --tdi=<file> --lang=<lang> --dd=<file>
/// [--modmapfmt=<fmt>] [<file.ddi>...]`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DyndepRequest {
    /// `--tdi=`: path of `CXXDependInfo.json`.
    pub tdi: BString,
    /// `--lang=`: language (`CXX`).
    pub lang: BString,
    /// `--dd=`: path of the dyndep file to write.
    pub dd: BString,
    /// `--modmapfmt=`.
    pub modmap_format: Option<ModmapFormat>,
    /// `.ddi` file paths.
    pub ddis: Vec<BString>,
}

/// Write a dyndep file for the scan results with no linked targets.
///
/// # Errors
/// Returns an error for invalid scan results or conflicting module providers.
pub fn write_dyndep(ddis: &[P1689File], module_dir: &[u8]) -> Result<BString, ModulesError> {
    let output = collate(&CollationInput {
        tdi: TargetDependInfo {
            module_dir: module_dir.into(),
            ..TargetDependInfo::default()
        },
        ddis: ddis.to_vec(),
        ..CollationInput::default()
    })?;
    Ok(output.dyndep)
}

pub(crate) fn ninja_path(path: &[u8], top: &[u8]) -> BString {
    if top.is_empty() {
        return path.into();
    }
    let top = top.strip_suffix(b"/").unwrap_or(top);
    if let Some(relative) = path
        .strip_prefix(top)
        .and_then(|rest| rest.strip_prefix(b"/"))
    {
        return relative.into();
    }
    path.into()
}

pub(crate) fn rule_path(path: &[u8], directory: Option<&BString>) -> BString {
    directory
        .filter(|dir| !dir.is_empty())
        .map_or_else(|| path.into(), |dir| Lexical::new(dir).append(path))
}

pub(crate) fn edge<'a>(
    out: &mut BString,
    primary: &[u8],
    provided: impl Iterator<Item = &'a [u8]>,
    required: impl Iterator<Item = &'a [u8]>,
    restat: bool,
) -> Result<(), ModulesError> {
    out.extend_from_slice(b"build ");
    escape(out, primary)?;
    implicit(out, provided.filter(|path| *path != primary))?;
    out.extend_from_slice(b": dyndep");
    implicit(out, required)?;
    out.push(b'\n');
    if restat {
        out.extend_from_slice(b"  restat = 1\n");
    }
    out.push(b'\n');
    Ok(())
}

fn implicit<'a>(
    out: &mut BString,
    mut paths: impl Iterator<Item = &'a [u8]>,
) -> Result<(), ModulesError> {
    if let Some(path) = paths.next() {
        out.extend_from_slice(b" | ");
        escape(out, path)?;
        for path in paths {
            out.push(b' ');
            escape(out, path)?;
        }
    }
    Ok(())
}

fn escape(out: &mut BString, path: &[u8]) -> Result<(), ModulesError> {
    for &byte in path {
        match byte {
            b'\r' | b'\n' | 0 => return Err(invalid("Invalid character in a Ninja path")),
            b'$' | b' ' | b':' => out.push(b'$'),
            _ => {}
        }
        out.push(byte);
    }
    Ok(())
}
