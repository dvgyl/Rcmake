// SPDX-License-Identifier: MIT OR Apache-2.0
//! Xcode file types and build settings for Fortran and assembly.

#![forbid(unsafe_code)]

use std::collections::BTreeSet;

use rcmake_base::{BString, list, path_lex::Lexical};
use rcmake_gen_common::GenError;

use crate::{
    object::{Attrs, Objects, Reference, attrs},
    phases, text,
};

/// Return the PBX file type for a Fortran or assembly source.
///
/// An empty language permits extension detection outside a compile group.
/// Apply explicit file type overrides before calling this helper.
pub(crate) fn file_type(path: &[u8], language: &[u8]) -> Option<&'static str> {
    let (_, extension) = Lexical::new(path).stem_extension(true);
    let extension = extension.to_ascii_lowercase();
    match language {
        b"Fortran" => Some("sourcecode.fortran.f90"),
        b"ASM" => Some("sourcecode.asm"),
        b"" => match extension.as_slice() {
            b".f" | b".for" | b".ftn" | b".f77" | b".fpp" | b".f90" | b".f95" | b".f03"
            | b".f08" | b".f18" | b".f23" => Some("sourcecode.fortran.f90"),
            b".s" => Some("sourcecode.asm"),
            _ => None,
        },
        _ => None,
    }
}

/// Set the Intel Fortran plug-in's compiler flags and source format.
///
/// Supply module search paths in compiler search order and Fortran flags
/// with their existing shell quoting.
pub(crate) fn add_fortran_settings(
    settings: &mut Attrs,
    module_search_paths: &[BString],
    compiler_flags: &str,
    format: Option<&[u8]>,
) {
    let mut flags = Vec::new();
    if !compiler_flags.is_empty() {
        flags.push(compiler_flags.to_owned());
    }
    let mut seen = BTreeSet::new();
    for path in module_search_paths
        .iter()
        .filter(|path| !path.is_empty() && seen.insert(*path))
    {
        flags.push(format!("-I{}", phases::quote(&text(path))));
    }
    if !flags.is_empty() {
        settings.insert("IFORT_OTHER_FLAGS".into(), flags.join(" ").into());
    }
    if let Some(format) = format_name(format) {
        settings.insert("IFORT_LANG_SRCFMT".into(), format.into());
    }
}

/// Add a build file for the target's PBXSourcesBuildPhase.
///
/// Pass the target-specific key and already-quoted per-source compiler flags.
///
/// # Errors
///
/// Returns an error if the object identifier is already in use.
pub(crate) fn source_build_file(
    objects: &mut Objects,
    key: &str,
    file: &Reference,
    compiler_flags: &str,
) -> Result<Reference, GenError> {
    let settings = if compiler_flags.is_empty() {
        Attrs::default()
    } else {
        attrs([("COMPILER_FLAGS", compiler_flags.into())])
    };
    phases::build_file(objects, key, "Sources", file, settings)
}

/// Add a Fortran build file with its source-level format override.
///
/// Pass the source's format property and its shell-quoted compiler flags.
///
/// # Errors
///
/// Returns an error if the object identifier is already in use.
pub(crate) fn fortran_build_file(
    objects: &mut Objects,
    key: &str,
    file: &Reference,
    format: Option<&[u8]>,
    compiler_flags: &str,
) -> Result<Reference, GenError> {
    let flags = match format_name(format) {
        Some(format) if compiler_flags.is_empty() => format!("-{format}"),
        Some(format) => format!("-{format} {compiler_flags}"),
        None => compiler_flags.to_owned(),
    };
    source_build_file(objects, key, file, &flags)
}

fn format_name(format: Option<&[u8]>) -> Option<&'static str> {
    list::split(format?, list::Empty::Drop)
        .iter()
        .rev()
        .find_map(|entry| match entry.as_slice() {
            b"FIXED" => Some("fixed"),
            b"FREE" => Some("free"),
            _ => None,
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::object::{Isa, Value};

    #[test]
    fn file_types_match_the_fortran_and_asm_mappings() {
        assert_eq!(
            file_type(b"legacy.F", b"Fortran"),
            Some("sourcecode.fortran.f90")
        );
        assert_eq!(
            file_type(b"module.F90", b""),
            Some("sourcecode.fortran.f90")
        );
        assert_eq!(file_type(b"source.S", b"ASM"), Some("sourcecode.asm"));
        assert_eq!(file_type(b"kernel.asm", b"ASM"), Some("sourcecode.asm"));
        assert_eq!(file_type(b"kernel.asm", b"ASM_NASM"), None);
        assert_eq!(file_type(b"legacy.f", b"C"), None);
    }

    #[test]
    fn fortran_settings_preserve_module_search_order() {
        let mut settings = Attrs::default();
        add_fortran_settings(
            &mut settings,
            &[
                "generated".into(),
                "source modules".into(),
                "generated".into(),
            ],
            "-O2",
            Some(b"FIXED"),
        );
        assert!(matches!(
            settings.get("IFORT_OTHER_FLAGS"),
            Some(Value::Text(value)) if value == "-O2 -Igenerated -I'source modules'"
        ));
        assert!(
            matches!(settings.get("IFORT_LANG_SRCFMT"), Some(Value::Text(value)) if value == "fixed")
        );
    }

    #[test]
    fn fortran_build_file_keeps_flags_in_the_sources_phase() -> Result<(), GenError> {
        let mut objects = Objects::default();
        let source = objects.add(
            Isa::PBXFileReference,
            "input.f90",
            "input.f90",
            Attrs::default(),
        )?;
        let input = fortran_build_file(
            &mut objects,
            "target:input.f90",
            &source,
            Some(b"FREE;FIXED;unknown"),
            "-O2",
        )?;
        let phase = phases::files(
            &mut objects,
            Isa::PBXSourcesBuildPhase,
            "target:sources",
            "Sources",
            vec![input.clone()],
        )?;
        let build_file = objects.entries.get(&("PBXBuildFile", input.id));
        assert!(matches!(
            build_file.and_then(|file| file.attrs.get("settings")),
            Some(Value::Dict(settings)) if matches!(settings.get("COMPILER_FLAGS"), Some(Value::Text(value)) if value == "-fixed -O2")
        ));
        let phase = objects.entries.get(&("PBXSourcesBuildPhase", phase.id));
        assert!(matches!(
            phase.and_then(|phase| phase.attrs.get("files")),
            Some(Value::List(files)) if matches!(files.as_slice(), [Value::Ref(reference)] if reference.comment == "input.f90 in Sources")
        ));
        Ok(())
    }
}
