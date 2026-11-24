// SPDX-License-Identifier: MIT OR Apache-2.0
//! Xcode file types and build settings for Fortran and assembly.

#![forbid(unsafe_code)]

use std::collections::BTreeSet;

use rcmake_base::{BString, path_lex::Lexical};
use rcmake_gen_common::GenError;
use rcmake_resolve::FortranTargetInfo;

use crate::{
    object::{Attrs, Objects, Reference, attrs},
    phases, text,
};

/// Return the PBX file type for a Fortran or assembly source.
///
/// Supply the evaluated source format or the target default. An explicit
/// language takes precedence over the extension. An empty language permits
/// extension detection for a file reference that is outside a compile group.
pub(crate) fn file_type(
    path: &[u8],
    language: &[u8],
    fortran_format: Option<&[u8]>,
) -> Option<&'static str> {
    let (_, extension) = Lexical::new(path).stem_extension(true);
    let extension = extension.to_ascii_lowercase();
    let extension = extension.as_slice();
    let fixed = matches!(extension, b".f" | b".for" | b".ftn" | b".f77" | b".fpp");
    let free = matches!(
        extension,
        b".f90" | b".f95" | b".f03" | b".f08" | b".f18" | b".f23"
    );
    if language == b"Fortran" || (language.is_empty() && (fixed || free)) {
        let format = format_name(fortran_format);
        return Some(if format.map_or(fixed, |format| format == "fixed") {
            "sourcecode.fortran.f77"
        } else {
            "sourcecode.fortran.f90"
        });
    }
    match language {
        b"ASM_NASM" | b"ASM_MASM" => Some("sourcecode.asm.asm"),
        b"ASM" | b"ASM-ATT" | b"ASM_MARMASM" | b"ASM_MASM64" | b"ASM_ARMASM" => {
            Some(if extension == b".asm" {
                "sourcecode.asm.asm"
            } else {
                "sourcecode.asm"
            })
        }
        b"" => match extension {
            b".s" => Some("sourcecode.asm"),
            b".asm" | b".nasm" => Some("sourcecode.asm.asm"),
            _ => None,
        },
        _ => None,
    }
}

/// Set the Intel Fortran plug-in's compiler flags and source format.
///
/// The plug-in receives the module output directory through its compiler
/// options. Supply module search paths in the order the compiler must use.
/// Supply other Fortran flags with their existing shell quoting.
pub(crate) fn add_fortran_settings(
    settings: &mut Attrs,
    info: &FortranTargetInfo,
    module_search_paths: &[BString],
    compiler_flags: &str,
    format: Option<&[u8]>,
) {
    let mut flags = Vec::new();
    if !compiler_flags.is_empty() {
        flags.push(compiler_flags.to_owned());
    }
    if let Some(module_dir) = &info.module_dir {
        flags.push(format!("-module {}", phases::quote(&text(module_dir))));
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

/// Add an assembler build file for the target's PBXSourcesBuildPhase.
///
/// Pass the target-specific key and already-quoted per-source compiler flags.
///
/// # Errors
///
/// Returns an error if the object identifier is already in use.
pub(crate) fn assembler_build_file(
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

fn format_name(format: Option<&[u8]>) -> Option<&'static str> {
    let format = format?;
    if format.eq_ignore_ascii_case(b"fixed") {
        Some("fixed")
    } else if format.eq_ignore_ascii_case(b"free") {
        Some("free")
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::object::{Isa, Value};

    #[test]
    fn file_types_respect_source_format_and_assembly_dialect() {
        assert_eq!(
            file_type(b"legacy.F", b"Fortran", None),
            Some("sourcecode.fortran.f77")
        );
        assert_eq!(
            file_type(b"legacy.F", b"Fortran", Some(b"FREE")),
            Some("sourcecode.fortran.f90")
        );
        assert_eq!(
            file_type(b"module.F90", b"", None),
            Some("sourcecode.fortran.f90")
        );
        assert_eq!(file_type(b"source.S", b"ASM", None), Some("sourcecode.asm"));
        assert_eq!(
            file_type(b"kernel.asm", b"ASM_NASM", None),
            Some("sourcecode.asm.asm")
        );
        assert_eq!(file_type(b"legacy.f", b"C", None), None);
    }

    #[test]
    fn fortran_settings_preserve_module_search_order() {
        let info = FortranTargetInfo {
            module_dir: Some("module files".into()),
            ..FortranTargetInfo::default()
        };
        let mut settings = Attrs::default();
        add_fortran_settings(
            &mut settings,
            &info,
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
            Some(Value::Text(value)) if value == "-O2 -module 'module files' -Igenerated -I'source modules'"
        ));
        assert!(
            matches!(settings.get("IFORT_LANG_SRCFMT"), Some(Value::Text(value)) if value == "fixed")
        );
    }

    #[test]
    fn assembler_build_file_keeps_flags_in_the_sources_phase() -> Result<(), GenError> {
        let mut objects = Objects::default();
        let source = objects.add(
            Isa::PBXFileReference,
            "asm/input.S",
            "input.S",
            Attrs::default(),
        )?;
        let input = assembler_build_file(&mut objects, "target:asm/input.S", &source, "-DASM=1")?;
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
            Some(Value::Dict(settings)) if matches!(settings.get("COMPILER_FLAGS"), Some(Value::Text(value)) if value == "-DASM=1")
        ));
        let phase = objects.entries.get(&("PBXSourcesBuildPhase", phase.id));
        assert!(matches!(
            phase.and_then(|phase| phase.attrs.get("files")),
            Some(Value::List(files)) if matches!(files.as_slice(), [Value::Ref(reference)] if reference.comment == "input.S in Sources")
        ));
        Ok(())
    }
}
