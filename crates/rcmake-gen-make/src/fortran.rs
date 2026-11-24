// SPDX-License-Identifier: MIT OR Apache-2.0
//! Unix Makefiles dependency inputs and rules for Fortran.

#![forbid(unsafe_code)]

use std::{collections::BTreeSet, path::Path};

use rcmake_base::{BString, path_lex::Lexical, shell::quote_always};
use rcmake_gen_common::GenError;
use rcmake_host::Host;
use rcmake_resolve::DependInfo;

use crate::{
    Word, build_path, check_make_path, make_recipe_text, make_word, phony, rule, write_makefile,
};

/// Inputs for the per-target dependency scan.
pub(crate) struct DependencyRule<'a> {
    /// The top-level source directory.
    pub(crate) source_dir: &'a [u8],
    /// The target's source directory.
    pub(crate) target_source_dir: &'a [u8],
    /// The top-level build directory.
    pub(crate) build_dir: &'a [u8],
    /// The target's build directory.
    pub(crate) target_build_dir: &'a [u8],
    /// The path of the target's DependInfo.cmake.
    pub(crate) depend_info: &'a [u8],
    /// A phony goal that Makefile2 runs before the build goal.
    pub(crate) goal: &'a [u8],
}

/// Additional inputs for the target dependency file.
#[derive(Default)]
pub(crate) struct DependencyFiles<'a> {
    /// Support directories with module summaries to forward.
    pub(crate) forward_target_dirs: &'a [BString],
    /// Native depfile records in source, object, compiler, depfile order.
    pub(crate) native_depfiles: &'a [BString],
    /// Whether the compiler builds its intrinsic modules.
    pub(crate) building_intrinsic_modules: bool,
}

/// Write the target's Fortran dependency information using the host.
///
/// # Errors
///
/// Returns an error if the data is for another language or the host cannot
/// write the file.
pub(crate) fn write_depend_info(
    host: &dyn Host,
    build_dir: &Path,
    file: &[u8],
    info: &DependInfo,
    files: &DependencyFiles<'_>,
) -> Result<(), GenError> {
    write_makefile(host, build_dir, file, &depend_info(info, files)?)
}

fn depend_info(info: &DependInfo, files: &DependencyFiles<'_>) -> Result<Vec<u8>, GenError> {
    let DependInfo::Fortran(info) = info else {
        return Err(GenError::Invalid(
            "Expected Fortran dependency information.".into(),
        ));
    };
    let mut out =
        b"set(CMAKE_DEPENDS_IN_PROJECT_ONLY OFF)\nset(CMAKE_DEPENDS_LANGUAGES \"Fortran\")\n"
            .to_vec();
    out.extend_from_slice(b"set(CMAKE_DEPENDS_CHECK_Fortran\n");
    for source in &info.sources {
        let absolute = Lexical::new(&info.dir_top_bld).append(&source.object);
        out.extend_from_slice(b"  ");
        cmake_argument(&mut out, &source.source, true);
        out.push(b' ');
        cmake_argument(&mut out, &absolute, true);
        out.push(b'\n');
    }
    out.extend_from_slice(b")\n");
    let linked = info
        .linked_target_dirs
        .iter()
        .map(|dir| Lexical::new(dir).append(b"DependInfo.cmake"))
        .collect::<Vec<_>>();
    let forwarded: Vec<_> = files
        .forward_target_dirs
        .iter()
        .map(|dir| Lexical::new(dir).append(b"DependInfo.cmake"))
        .collect();
    for (name, values) in [
        (
            b"CMAKE_Fortran_TARGET_INCLUDE_PATH".as_slice(),
            info.include_dirs.as_slice(),
        ),
        (b"CMAKE_Fortran_TARGET_LINKED_INFO_FILES", linked.as_slice()),
        (
            b"CMAKE_Fortran_TARGET_FORWARD_LINKED_INFO_FILES",
            forwarded.as_slice(),
        ),
        (b"CMAKE_DEPENDS_DEPENDENCY_FILES", files.native_depfiles),
    ] {
        out.extend_from_slice(b"set(");
        out.extend_from_slice(name);
        for value in values {
            out.extend_from_slice(b"\n  ");
            cmake_argument(&mut out, value, true);
        }
        out.extend_from_slice(b"\n)\n");
    }
    let module_dir = info
        .module_dir
        .as_ref()
        .map_or(b"".as_slice(), |path| path.as_slice());
    for (name, value) in [
        (
            b"CMAKE_Fortran_COMPILER_ID".as_slice(),
            info.compiler_id.as_slice(),
        ),
        (
            b"CMAKE_Fortran_SUBMODULE_SEP",
            info.submodule_sep.as_slice(),
        ),
        (
            b"CMAKE_Fortran_SUBMODULE_EXT",
            info.submodule_ext.as_slice(),
        ),
        (b"CMAKE_Fortran_TARGET_MODULE_DIR", module_dir),
    ] {
        out.extend_from_slice(b"set(");
        out.extend_from_slice(name);
        out.push(b' ');
        cmake_argument(&mut out, value, false);
        out.extend_from_slice(b")\n");
    }
    if files.building_intrinsic_modules {
        out.extend_from_slice(b"set(CMAKE_Fortran_TARGET_BUILDING_INTRINSIC_MODULES ON)\n");
    }
    Ok(out)
}

fn cmake_argument(out: &mut Vec<u8>, word: &[u8], list_element: bool) {
    out.push(b'"');
    for &byte in word {
        match byte {
            b'\\' | b'"' | b'$' => out.extend_from_slice(&[b'\\', byte]),
            b';' if list_element => out.extend_from_slice(b"\\;"),
            b'\n' => out.extend_from_slice(b"\\n"),
            b'\r' => out.extend_from_slice(b"\\r"),
            b'\t' => out.extend_from_slice(b"\\t"),
            _ => out.push(byte),
        }
    }
    out.push(b'"');
}

/// Include the scanner's object dependencies and module stamp recipes.
///
/// Pass the actual target goal and the configured CMake executable. Native
/// compiler depfile includes in build.make remain in effect.
///
/// # Errors
///
/// Returns an error if a path or the command cannot be written in Make.
pub(crate) fn include_depend_make(
    out: &mut Vec<u8>,
    path: &[u8],
    goal: &[u8],
    cmake: &[u8],
) -> Result<(), GenError> {
    let stamp_goal = Lexical::new(Lexical::new(path).parent()).append(b"build");
    check_make_path(path)?;
    check_make_path(goal)?;
    check_make_path(&stamp_goal)?;
    if cmake.is_empty() {
        return Err(GenError::Invalid("CMAKE_COMMAND is empty.".into()));
    }
    out.extend_from_slice(b"override CMAKE_COMMAND := ");
    let mut command = Vec::new();
    make_recipe_text(&mut command, &quote_always(cmake))?;
    for byte in command {
        if byte == b'#' {
            out.push(b'\\');
        }
        out.push(byte);
    }
    out.extend_from_slice(b"\n-include ");
    make_word(out, path, Word::Input);
    out.push(b'\n');
    if stamp_goal != goal {
        rule(out, goal, [stamp_goal.as_slice()]);
    }
    // A target without providers has no stamp prerequisites.
    rule(out, &stamp_goal, []);
    Ok(())
}

/// Collect the files that the target's clean goal removes.
///
/// Pass the configured support directory, original objects and module paths
/// discovered by the Fortran scanner. Include both compiler basename cases.
pub(crate) fn clean_artifacts<'a>(
    build_dir: &Path,
    support_dir: &[u8],
    objects: &[BString],
    modules: impl IntoIterator<Item = &'a BString>,
) -> BTreeSet<BString> {
    let mut products = BTreeSet::new();
    for module in modules {
        let path = Lexical::new(module);
        let (stem, extension) = path.stem_extension(true);
        products.insert(build_path(module, build_dir));
        let uppercase = [stem.to_ascii_uppercase().as_slice(), extension].concat();
        products.insert(build_path(
            &Lexical::new(path.parent()).append(&uppercase),
            build_dir,
        ));
        let stamp = [path.filename(), b".stamp"].concat();
        products.insert(build_path(
            &Lexical::new(support_dir).append(&stamp),
            build_dir,
        ));
    }
    products.extend(
        objects
            .iter()
            .map(|object| build_path(&[object.as_slice(), b".provides.build"].concat(), build_dir)),
    );
    products
}

/// Write the scan goal. Pass prerequisites that generate sources before the scan.
///
/// Makefile2 must invoke this goal before invoking the target's build goal.
/// Each invocation parses build.make again and reads the latest stamp rules.
///
/// # Errors
///
/// Returns an error if a dependency or a command cannot be written in Make.
pub(crate) fn emit_depend(
    out: &mut Vec<u8>,
    plan: &DependencyRule<'_>,
    generated_sources: &[BString],
) -> Result<(), GenError> {
    check_make_path(plan.goal)?;
    check_make_path(plan.depend_info)?;
    for source in generated_sources {
        check_make_path(source)?;
    }
    phony(
        out,
        plan.goal,
        std::iter::once(plan.depend_info)
            .chain(generated_sources.iter().map(|source| source.as_slice())),
    );
    out.extend_from_slice(b"\t$(SILENT)$(CMAKE_COMMAND) -E cmake_depends \"Unix Makefiles\"");
    for arg in [
        plan.source_dir,
        plan.target_source_dir,
        plan.build_dir,
        plan.target_build_dir,
        plan.depend_info,
    ] {
        out.push(b' ');
        make_recipe_text(out, &quote_always(arg))?;
    }
    out.push(b'\n');
    Ok(())
}

/// Write a module directory rule and order-only prerequisites for its objects.
///
/// The directory may also contain modules from another target. Its timestamp
/// does not cause objects to compile again. Each build.make can create the
/// directory because the mkdir command succeeds if the directory exists.
///
/// # Errors
///
/// Returns an error if the directory or an object path cannot be written in Make.
pub(crate) fn emit_module_directory(
    out: &mut Vec<u8>,
    module_dir: &[u8],
    objects: &[BString],
) -> Result<(), GenError> {
    if objects.is_empty() {
        return Ok(());
    }
    check_make_path(module_dir)?;
    for object in objects.iter().collect::<BTreeSet<_>>() {
        check_make_path(object)?;
        make_word(out, object, Word::Target);
        out.extend_from_slice(b": | ");
        make_word(out, module_dir, Word::Input);
        out.push(b'\n');
    }
    rule(out, module_dir, []);
    out.extend_from_slice(b"\t$(SILENT)mkdir -p -- ");
    make_recipe_text(out, &quote_always(module_dir))?;
    out.push(b'\n');
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn depend_goal_uses_the_current_and_top_directories() -> Result<(), GenError> {
        let mut out = Vec::new();
        emit_depend(
            &mut out,
            &DependencyRule {
                source_dir: b"/source tree",
                target_source_dir: b"/source tree/sub",
                build_dir: b"/build",
                target_build_dir: b"/build/sub",
                depend_info: b"obj/DependInfo.cmake",
                goal: b"obj/depend",
            },
            &["generated $unit.f90".into()],
        )?;
        include_depend_make(&mut out, b"obj/depend.make", b"obj/all", b"/my tools/cmake")?;
        let text = String::from_utf8_lossy(&out);
        assert!(text.contains("obj/depend: obj/DependInfo.cmake generated\\ $$unit.f90\n"));
        assert!(text.contains("$(CMAKE_COMMAND) -E cmake_depends \"Unix Makefiles\" '/source tree' '/source tree/sub' '/build' '/build/sub' 'obj/DependInfo.cmake'\n"));
        assert!(text.contains("override CMAKE_COMMAND := '/my tools/cmake'\n"));
        assert!(text.contains("-include obj/depend.make\nobj/all: obj/build\nobj/build:\n"));
        Ok(())
    }

    #[test]
    fn module_directory_does_not_make_objects_out_of_date() -> Result<(), GenError> {
        let mut out = Vec::new();
        emit_module_directory(
            &mut out,
            b"module files",
            &["obj/uses.o".into(), "obj/defines.o".into()],
        )?;
        assert_eq!(
            String::from_utf8_lossy(&out),
            "obj/defines.o: | module\\ files\nobj/uses.o: | module\\ files\nmodule\\ files:\n\t$(SILENT)mkdir -p -- 'module files'\n"
        );
        Ok(())
    }

    #[test]
    fn depend_info_arguments_escape_expansions_and_list_separators() {
        let mut out = Vec::new();
        cmake_argument(&mut out, b"a;${name}/b\"c", true);
        assert_eq!(out, b"\"a\\;\\${name}/b\\\"c\"");
    }
}
