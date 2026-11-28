//! Write target exports and evaluate files during generation.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use rcmake_base::{
    BString,
    list::{self, Empty},
    path_lex::Lexical,
};
use rcmake_host::{Host, path::to_native};
use rcmake_resolve::{ExportPlan, ExportedTarget};

use crate::{GenError, quote, quoted_content, write_output};

mod android_mk;
pub mod cps;
mod generate;
mod modules;
pub(crate) mod provenance;
mod xcframework;
pub use generate::{
    FileGenerateContext, FileGenerateInput, FileGenerateNewline, FileGeneratePermissions,
    FileGenerateRequest, write_generated_files,
};
pub use modules::{ModuleExportJob, write_module_exports};

const IMPORT_PREFIX: &[u8] = b"${_IMPORT_PREFIX}";
const IMPORT_LIBRARY_SUFFIX: &[u8] = b"${CMAKE_IMPORT_LIBRARY_SUFFIX}";
const IMPORT_VERSION: &[u8] = b"set(CMAKE_IMPORT_FILE_VERSION 1)\n";
const END_IMPORT_VERSION: &[u8] = b"unset(CMAKE_IMPORT_FILE_VERSION)\n";

/// Render the main export file. For an install export, assume the file goes in the
/// root of the install prefix. The main file loads any installed configuration files.
pub fn render(plan: &ExportPlan) -> Result<BString, GenError> {
    if plan.android {
        return Ok(android_mk::render(plan, b""));
    }
    render_main(plan, b"", None, b"")
}

/// Write the main export file and return its path first. For an install export,
/// also write a file for each configuration with imported properties.
/// With no `install_dest`, the install export goes in the root of the install prefix.
/// For an absolute install destination, use [`write_export_plan_with_prefix`].
pub fn write_export_plan(
    plan: &ExportPlan,
    out_dir: &Path,
    install_dest: Option<&[u8]>,
    host: &dyn Host,
) -> Result<Vec<PathBuf>, GenError> {
    write_export_plan_with_prefix(plan, out_dir, install_dest, None, host)
}

/// Write an export plan with the configured install prefix. A relative install
/// destination uses the location of the installed export file to find the prefix.
/// An absolute install destination uses the supplied `install_prefix`.
pub fn write_export_plan_with_prefix(
    plan: &ExportPlan,
    out_dir: &Path,
    install_dest: Option<&[u8]>,
    install_prefix: Option<&[u8]>,
    host: &dyn Host,
) -> Result<Vec<PathBuf>, GenError> {
    let destination = install_dest.unwrap_or_default();
    let path = export_output_directory(plan, out_dir, destination).join(to_native(&plan.file));
    if plan.android {
        let content = android_mk::render(plan, destination);
        if plan.append {
            if let Some(parent) = path.parent() {
                host.create_dir_all(parent)
                    .map_err(|error| crate::write_error(parent, error))?;
            }
            host.write(&path, &content, rcmake_host::WriteMode::Append)
                .map_err(|error| crate::write_error(&path, error))?;
        } else {
            write_output(host, &path, &content)?;
        }
        return Ok(vec![path]);
    }
    let main = render_main(plan, destination, install_prefix, b"")?;
    let mut files = vec![(path.clone(), main)];
    if plan.install {
        for (config, targets) in configurations(plan) {
            if config.iter().any(|byte| matches!(byte, b'/' | b'\\' | 0)) {
                return Err(output_error(
                    &plan.file,
                    format!("invalid export configuration name \"{config}\""),
                ));
            }
            let name = [
                export_stem(plan),
                b"-",
                config.to_ascii_lowercase().as_slice(),
                b".cmake",
            ]
            .concat();
            let mut out = BString::from(IMPORT_VERSION);
            for (target, source_config) in targets {
                render_config_target(&mut out, plan, &config, target, source_config);
            }
            out.extend_from_slice(END_IMPORT_VERSION);
            files.push((path.with_file_name(to_native(&name)), out));
        }
    }

    for (path, content) in files.iter().skip(1).chain(files.first()) {
        write_output(host, path, content)?;
    }
    Ok(files.into_iter().map(|(path, _)| path).collect())
}

fn export_output_directory(plan: &ExportPlan, out_dir: &Path, destination: &[u8]) -> PathBuf {
    if plan.install
        && out_dir
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.parse::<usize>().is_ok())
        && let Some(parent) = out_dir
            .parent()
            .filter(|parent| parent.ends_with("CMakeFiles/Export"))
    {
        return parent.join(rcmake_base::hash::HashAlgo::Md5.hex_digest(destination));
    }
    out_dir.to_owned()
}

fn render_main(
    plan: &ExportPlan,
    install_dest: &[u8],
    install_prefix: Option<&[u8]>,
    module_properties: &[u8],
) -> Result<BString, GenError> {
    if !Lexical::new(&plan.file).filename().ends_with(b".cmake") {
        return Err(output_error(
            &plan.file,
            "expected a .cmake export file name",
        ));
    }
    let mut out = BString::from(
        br#"# Generated by CMake

if("${CMAKE_MAJOR_VERSION}.${CMAKE_MINOR_VERSION}" LESS 2.8)
   message(FATAL_ERROR "CMake >= 2.8.3 required")
endif()
if(CMAKE_VERSION VERSION_LESS "2.8.3")
   message(FATAL_ERROR "CMake >= 2.8.3 required")
endif()
cmake_policy(PUSH)
cmake_policy(VERSION 2.8.3...4.3)
#----------------------------------------------------------------
# Generated CMake target import file.
#----------------------------------------------------------------

# Commands may need to know the format version.
"#,
    );
    out.extend_from_slice(IMPORT_VERSION);
    render_guard(&mut out, plan);
    render_package_dependencies(&mut out, plan);
    if plan.install {
        render_prefix(&mut out, plan, install_dest, install_prefix)?;
    }
    for target in &plan.targets {
        render_target(&mut out, plan, target)?;
    }
    out.extend_from_slice(module_properties);
    if plan.install {
        render_installed_configurations(&mut out, export_stem(plan));
    } else {
        for target in &plan.targets {
            for (config, properties) in &target.config_properties {
                if !properties.is_empty() || target.xcframework_locations.contains_key(config) {
                    render_config_target(&mut out, plan, &config_label(config), target, config);
                }
            }
        }
    }
    out.extend_from_slice(END_IMPORT_VERSION);
    out.extend_from_slice(b"cmake_policy(POP)\n");
    Ok(out)
}

fn render_package_dependencies(out: &mut BString, plan: &ExportPlan) {
    if plan.package_dependencies.is_empty() {
        return;
    }
    out.extend_from_slice(
        b"include(CMakeFindDependencyMacro)\nif(DEFINED CMAKE_FIND_PACKAGE_NAME)\n  set(_cmake_unwind_arg UNWIND_INCLUDE)\nendif()\n\n"
    );
    for (name, args) in &plan.package_dependencies {
        out.extend_from_slice(b"__find_dependency_no_return(");
        let plain = !name.is_empty()
            && name
                .iter()
                .all(|byte| byte.is_ascii_alphanumeric() || b"_.+-".contains(byte));
        if plain {
            out.extend_from_slice(name);
        } else {
            crate::script_argument(out, name);
        }
        for arg in args {
            out.push(b' ');
            crate::script_argument(out, arg);
        }
        out.extend_from_slice(b" ${_cmake_unwind_arg})\nif(NOT ");
        if plain {
            out.extend_from_slice(name);
            out.extend_from_slice(b"_FOUND");
        } else {
            out.extend_from_slice(b"\"${");
            quoted_content(out, name);
            out.extend_from_slice(b"_FOUND}\"");
        }
        out.extend_from_slice(
            b")\n  unset(_cmake_unwind_arg)\n  cmake_policy(POP)\n  return()\nendif()\n\n",
        );
    }
    out.extend_from_slice(b"unset(_cmake_unwind_arg)\n\n");
}

fn render_guard(out: &mut BString, plan: &ExportPlan) {
    out.extend_from_slice(
        br#"set(_cmake_targets_defined "")
set(_cmake_targets_not_defined "")
foreach(_cmake_expected_target IN ITEMS"#,
    );
    for target in &plan.targets {
        out.push(b' ');
        target_argument(out, &qualified_name(plan, target));
    }
    out.extend_from_slice(
        br#")
  if(TARGET "${_cmake_expected_target}")
    list(APPEND _cmake_targets_defined "${_cmake_expected_target}")
  else()
    list(APPEND _cmake_targets_not_defined "${_cmake_expected_target}")
  endif()
endforeach()
unset(_cmake_expected_target)
if(_cmake_targets_not_defined STREQUAL "")
  unset(_cmake_targets_defined)
  unset(_cmake_targets_not_defined)
  unset(CMAKE_IMPORT_FILE_VERSION)
  cmake_policy(POP)
  return()
endif()
if(NOT _cmake_targets_defined STREQUAL "")
  message(FATAL_ERROR "Some (but not all) targets in this export set were already defined.\nTargets Defined: ${_cmake_targets_defined}\nTargets not yet defined: ${_cmake_targets_not_defined}")
endif()
unset(_cmake_targets_defined)
unset(_cmake_targets_not_defined)

"#,
    );
}

fn render_prefix(
    out: &mut BString,
    plan: &ExportPlan,
    destination: &[u8],
    prefix: Option<&[u8]>,
) -> Result<(), GenError> {
    if is_absolute(plan, destination) {
        let prefix = prefix.ok_or_else(|| {
            output_error(
                &plan.file,
                "an absolute export destination requires the configured install prefix",
            )
        })?;
        out.extend_from_slice(b"set(_IMPORT_PREFIX ");
        quote(out, if prefix == b"/" { b"" } else { prefix });
        out.extend_from_slice(b")\n");
        return Ok(());
    }

    out.extend_from_slice(
        b"get_filename_component(_IMPORT_PREFIX \"${CMAKE_CURRENT_LIST_FILE}\" PATH)\n",
    );
    // CMake goes up one level for each non-empty destination component, which includes
    // `.` and `..` components of a destination that is not normalized (`CMP0177`).
    let levels = destination
        .split(|&byte| {
            byte == b'/' || (plan.windows_paths.unwrap_or(cfg!(windows)) && byte == b'\\')
        })
        .filter(|component| !component.is_empty())
        .count();
    for _ in 0..levels {
        out.extend_from_slice(
            b"get_filename_component(_IMPORT_PREFIX \"${_IMPORT_PREFIX}\" PATH)\n",
        );
    }
    out.extend_from_slice(
        b"if(_IMPORT_PREFIX STREQUAL \"/\")\n  set(_IMPORT_PREFIX \"\")\nendif()\n\n",
    );
    Ok(())
}

fn render_target(
    out: &mut BString,
    plan: &ExportPlan,
    target: &ExportedTarget,
) -> Result<(), GenError> {
    let name = qualified_name(plan, target);
    let kind = target.kind.as_slice();
    let symbolic = kind == b"INTERFACE"
        && target
            .properties
            .get(b"SYMBOLIC".as_slice())
            .is_some_and(|value| rcmake_base::truth::is_on(value));
    match (kind, symbolic) {
        (b"INTERFACE", true) => {
            out.extend_from_slice(
                b"if(CMAKE_VERSION VERSION_GREATER_EQUAL \"4.2.0\")\n  add_library(",
            );
            target_argument(out, &name);
            out.extend_from_slice(b" INTERFACE SYMBOLIC IMPORTED)\nelseif(CMAKE_VERSION VERSION_GREATER_EQUAL \"3.2.0\")\n  add_library(");
            target_argument(out, &name);
            out.extend_from_slice(b" INTERFACE IMPORTED)\n  set_target_properties(");
            target_argument(out, &name);
            out.extend_from_slice(b" PROPERTIES SYMBOLIC TRUE)\nendif()\n");
        }
        (b"EXECUTABLE", _) => {
            out.extend_from_slice(b"add_executable(");
            target_argument(out, &name);
            out.extend_from_slice(b" IMPORTED)\n");
        }
        (b"STATIC" | b"SHARED" | b"MODULE" | b"INTERFACE" | b"OBJECT" | b"UNKNOWN", _) => {
            out.extend_from_slice(b"add_library(");
            target_argument(out, &name);
            out.push(b' ');
            out.extend_from_slice(kind);
            out.extend_from_slice(b" IMPORTED)\n");
        }
        _ => {
            return Err(output_error(
                &plan.file,
                format!(
                    "invalid export target kind \"{}\" for \"{name}\"",
                    target.kind
                ),
            ));
        }
    }
    let mut properties = target
        .properties
        .iter()
        .filter(|(property, _)| {
            !(symbolic && property.as_slice() == b"SYMBOLIC")
                && !computed_file_set_property(property)
        })
        .collect::<Vec<_>>();
    if !properties.is_empty() {
        properties.sort_unstable_by_key(|(key, _)| *key);
        start_properties(out, &name);
        for (property, value) in properties {
            render_property(out, property, value, plan.install);
        }
        out.extend_from_slice(b")\n");
    }
    render_source_sets(out, &name, &target.source_sets, plan.install);
    out.push(b'\n');
    Ok(())
}

fn computed_file_set_property(property: &[u8]) -> bool {
    let name = property.strip_prefix(b"INTERFACE_").unwrap_or(property);
    matches!(
        name,
        b"FILE_SET_TYPES" | b"HEADER_SETS" | b"SOURCE_SETS" | b"CXX_MODULE_SETS"
    ) || name
        .strip_prefix(b"FILE_SETS_")
        .is_some_and(|kind| rcmake_model::FileSetKind::from_name(kind).is_some())
}

fn render_source_sets(
    out: &mut BString,
    target: &[u8],
    sets: &[rcmake_resolve::ExportedSourceSet],
    install: bool,
) {
    use rcmake_model::FileSetKind::{CxxModules, Headers, Sources};
    for kind in [Headers, Sources, CxxModules] {
        let sets: Vec<_> = sets.iter().filter(|set| set.kind == kind).collect();
        if sets.is_empty() {
            continue;
        }
        let language = sets
            .iter()
            .any(|set| set.properties.contains_key(b"LANGUAGE".as_slice()));
        let version: &[u8] = if language
            || sets
                .iter()
                .any(|set| set.name == b"SYCL" || set.name == b"SYCL_HEADERS")
        {
            b"4.5.0"
        } else {
            match kind {
                Headers => b"3.23.0",
                Sources => b"4.4.0",
                CxxModules => b"3.28.0",
            }
        };
        out.extend_from_slice(b"if(NOT CMAKE_VERSION VERSION_LESS ");
        quote(out, version);
        out.extend_from_slice(b")\n  target_sources(");
        target_argument(out, target);
        for set in &sets {
            out.extend_from_slice(b"\n    INTERFACE\n      FILE_SET ");
            quote(out, &set.name);
            out.extend_from_slice(b"\n      TYPE ");
            quote(out, kind.as_str().as_bytes());
            for (key, entries) in [
                (b"\n      BASE_DIRS".as_slice(), &set.bases),
                (b"\n      FILES", &set.files),
            ] {
                if !entries.is_empty() {
                    out.extend_from_slice(key);
                    for value in entries {
                        out.push(b' ');
                        source_set_value(out, value, install, true);
                    }
                }
            }
        }
        out.extend_from_slice(b"\n  )");
        for set in &sets {
            for (key, value) in &set.properties {
                out.extend_from_slice(b"\n\n  set_property(FILE_SET ");
                quote(out, &set.name);
                out.extend_from_slice(b" TARGET ");
                target_argument(out, target);
                out.extend_from_slice(b"\n    PROPERTY ");
                target_argument(out, key);
                out.push(b' ');
                source_set_value(out, value, install, false);
                out.extend_from_slice(b"\n  )");
            }
        }
        out.extend_from_slice(b"\nelse()\n");
        if kind == Headers && !language {
            out.extend_from_slice(b"  set_property(TARGET ");
            target_argument(out, target);
            out.extend_from_slice(b"\n    APPEND PROPERTY INTERFACE_INCLUDE_DIRECTORIES");
            for set in &sets {
                for base in &set.bases {
                    out.extend_from_slice(b"\n      ");
                    source_set_value(out, base, install, true);
                }
            }
            out.extend_from_slice(b"\n  )");
        } else {
            let (level, requirement) = match kind {
                Headers => (
                    "FATAL_ERROR",
                    "the 'LANGUAGE' property of a 'HEADERS' file set which is not supported",
                ),
                Sources => (
                    "FATAL_ERROR",
                    "'FILE_SET' of type 'SOURCES' which is not supported",
                ),
                CxxModules => ("AUTHOR_WARNING", "C++ modules which are not supported"),
            };
            out.extend_from_slice(format!("  message({level} ").as_bytes());
            quote(out, format!(
                "The target '{}' cannot be imported{} because it relies on {requirement} by this CMake version.",
                BString::from(target),
                if kind == CxxModules { " properly" } else { "" },
            ).as_bytes());
            out.push(b')');
        }
        out.extend_from_slice(b"\nendif()\n\n");
    }
}

fn source_set_value(out: &mut BString, value: &[u8], install: bool, path: bool) {
    out.push(b'"');
    quote_export_content(out, value, install, path, false);
    out.push(b'"');
}

type ConfigTargets<'a> = Vec<(&'a ExportedTarget, &'a BString)>;

fn configurations(plan: &ExportPlan) -> BTreeMap<BString, ConfigTargets<'_>> {
    let mut configs = BTreeMap::<BString, ConfigTargets<'_>>::new();
    for target in &plan.targets {
        for (config, properties) in &target.config_properties {
            if !properties.is_empty() || target.xcframework_locations.contains_key(config) {
                configs
                    .entry(config_label(config))
                    .or_default()
                    .push((target, config));
            }
        }
    }
    configs
}

fn config_label(config: &[u8]) -> BString {
    if config.is_empty() {
        "NOCONFIG".into()
    } else {
        config.to_ascii_uppercase().into()
    }
}

fn render_config_target(
    out: &mut BString,
    plan: &ExportPlan,
    config: &[u8],
    target: &ExportedTarget,
    source_config: &BString,
) {
    let suffix = [b"_", config].concat();
    let name = qualified_name(plan, target);
    out.extend_from_slice(b"set_property(TARGET ");
    target_argument(out, &name);
    out.extend_from_slice(b" APPEND PROPERTY IMPORTED_CONFIGURATIONS ");
    target_argument(out, config);
    out.extend_from_slice(b")\n");
    start_properties(out, &name);
    let mut paths = Vec::new();
    for (property, value) in target
        .config_properties
        .get(source_config)
        .into_iter()
        .flatten()
    {
        let base = property.strip_suffix(suffix.as_slice()).unwrap_or(property);
        let mut key = BString::from(base);
        key.extend_from_slice(&suffix);
        if !plan.install {
            render_property(out, &key, value, false);
            continue;
        }
        match base {
            b"IMPORTED_LOCATION" | b"IMPORTED_IMPLIB" => {
                start_property(out, &key);
                render_relative_path(out, plan, value, false);
                out.extend_from_slice(b"\"\n");
                if !value.is_empty() {
                    paths.push(value.clone());
                }
            }
            b"IMPORTED_OBJECTS" => {
                start_property(out, &key);
                for (index, path) in list::split(value, Empty::Keep).into_iter().enumerate() {
                    if index != 0 {
                        out.push(b';');
                    }
                    render_relative_path(out, plan, &path, true);
                    if !path.is_empty() {
                        paths.push(path);
                    }
                }
                out.extend_from_slice(b"\"\n");
            }
            _ => render_property(out, &key, value, true),
        }
    }
    out.extend_from_slice(b")\n");

    let alternate = target.xcframework_locations.get(source_config);
    if let Some(path) = alternate {
        xcframework::render(out, plan, &name, config, path);
        if !paths.is_empty() {
            out.extend_from_slice(b"else()\n");
        }
    }
    if !paths.is_empty() {
        out.extend_from_slice(b"list(APPEND _cmake_import_check_targets ");
        target_argument(out, &name);
        out.extend_from_slice(b")\nlist(APPEND ");
        target_argument(
            out,
            &[b"_cmake_import_check_files_for_", name.as_slice()].concat(),
        );
        for path in paths {
            out.extend_from_slice(b" \"");
            render_relative_path(out, plan, &path, true);
            out.push(b'"');
        }
        out.extend_from_slice(b")\n");
    }
    if alternate.is_some() {
        out.extend_from_slice(b"endif()\nunset(_cmake_xcframework_location)\n");
    }
    out.push(b'\n');
}

fn render_installed_configurations(out: &mut BString, stem: &[u8]) {
    out.extend_from_slice(br#"file(GLOB _cmake_config_files "${CMAKE_CURRENT_LIST_DIR}/"#);
    for &byte in stem {
        if b"[]?*".contains(&byte) {
            out.extend_from_slice(&[b'[', byte, b']']);
        } else {
            quoted_content(out, &[byte]);
        }
    }
    out.extend_from_slice(
        br#"-*.cmake")
foreach(_cmake_config_file IN LISTS _cmake_config_files)
  include("${_cmake_config_file}")
endforeach()
unset(_cmake_config_file)
unset(_cmake_config_files)
unset(_IMPORT_PREFIX)

foreach(_cmake_target IN LISTS _cmake_import_check_targets)
  foreach(_cmake_file IN LISTS "_cmake_import_check_files_for_${_cmake_target}")
    if(NOT EXISTS "${_cmake_file}")
      message(FATAL_ERROR "The imported target \"${_cmake_target}\" references the file
   \"${_cmake_file}\"
but this file does not exist.  Possible reasons include:
* The file was deleted, renamed, or moved to another location.
* An install or uninstall procedure did not complete successfully.
* The installation package was faulty and contained
   \"${CMAKE_CURRENT_LIST_FILE}\"
but not all the files it references.
")
    endif()
  endforeach()
  unset("_cmake_import_check_files_for_${_cmake_target}")
endforeach()
unset(_cmake_file)
unset(_cmake_target)
unset(_cmake_import_check_targets)
"#,
    );
}

fn export_stem(plan: &ExportPlan) -> &[u8] {
    let filename = Lexical::new(&plan.file).filename();
    filename.strip_suffix(b".cmake").unwrap_or(filename)
}

fn qualified_name(plan: &ExportPlan, target: &ExportedTarget) -> BString {
    [plan.namespace.as_slice(), target.name.as_slice()]
        .concat()
        .into()
}

fn start_properties(out: &mut BString, name: &[u8]) {
    out.extend_from_slice(b"set_target_properties(");
    target_argument(out, name);
    out.extend_from_slice(b" PROPERTIES\n");
}

fn target_argument(out: &mut BString, name: &[u8]) {
    if !name.is_empty()
        && name
            .iter()
            .all(|byte| byte.is_ascii_alphanumeric() || b"_.+:-".contains(byte))
    {
        out.extend_from_slice(name);
    } else {
        quote(out, name);
    }
}

fn start_property(out: &mut BString, key: &[u8]) {
    out.extend_from_slice(b"  ");
    target_argument(out, key);
    out.extend_from_slice(b" \"");
}

fn render_property(out: &mut BString, key: &[u8], value: &[u8], install: bool) {
    if key
        .iter()
        .all(|byte| byte.is_ascii_alphanumeric() || *byte == b'_')
    {
        out.extend_from_slice(b"  ");
        out.extend_from_slice(key);
        out.extend_from_slice(b" \"");
    } else {
        start_property(out, key);
    }
    // Exported target properties escape every `$`, including `$<...>`.
    quote_export_content(out, value, install, false, true);
    out.extend_from_slice(b"\"\n");
}

fn is_absolute(plan: &ExportPlan, path: &[u8]) -> bool {
    match plan.windows_paths {
        Some(true) => {
            path.len() >= 2
                && (path.get(1) == Some(&b':') || matches!(path.first(), Some(b'\\' | b'/')))
        }
        Some(false) => matches!(path.first(), Some(b'/' | b'~')),
        None => to_native(path).is_absolute(),
    }
}

fn render_relative_path(out: &mut BString, plan: &ExportPlan, path: &[u8], list_element: bool) {
    if !path.is_empty()
        && !is_absolute(plan, path)
        && !path.starts_with(b"$<")
        && !path.starts_with(IMPORT_PREFIX)
    {
        out.extend_from_slice(IMPORT_PREFIX);
        out.push(b'/');
    }
    quote_export_content(out, path, true, list_element, false);
}

fn quote_export_content(
    out: &mut BString,
    mut value: &[u8],
    install: bool,
    list_element: bool,
    escape_genex: bool,
) {
    while let Some((byte, rest)) = value.split_first() {
        if let Some((variable, rest)) = [IMPORT_PREFIX, IMPORT_LIBRARY_SUFFIX]
            .into_iter()
            .find_map(|variable| value.strip_prefix(variable).map(|rest| (variable, rest)))
        {
            out.extend_from_slice(variable);
            value = rest;
            continue;
        }
        if install && let Some(rest) = value.strip_prefix(b"$<INSTALL_PREFIX>") {
            out.extend_from_slice(IMPORT_PREFIX);
            value = rest;
            continue;
        }
        if *byte == b'$' && rest.starts_with(b"<") && !escape_genex {
            out.push(*byte);
        } else {
            if *byte == b';' && list_element {
                quoted_content(out, b"\\");
            }
            quoted_content(out, &[*byte]);
        }
        value = rest;
    }
}

pub(crate) fn output_error(path: &[u8], message: impl Into<BString>) -> GenError {
    GenError::Write {
        path: path.into(),
        message: message.into(),
    }
}

#[cfg(test)]
mod tests;

#[cfg(test)]
#[test]
fn exports_file_sets_via_target_sources() -> Result<(), Box<dyn std::error::Error>> {
    let base = if cfg!(windows) { "C:/work" } else { "/work" };
    let plan = ExportPlan {
        file: "sdk.cmake".into(),
        namespace: "sdk::".into(),
        targets: vec![ExportedTarget {
            name: "library".into(),
            kind: "INTERFACE".into(),
            properties: [
                ("INTERFACE_HEADER_SETS".into(), "headers".into()),
                ("INTERFACE_SOURCE_SETS".into(), "source_files".into()),
                ("INTERFACE_FILE_SETS_HEADERS".into(), "headers".into()),
                ("INTERFACE_FILE_SETS_SOURCES".into(), "source_files".into()),
                ("INTERFACE_INCLUDE_DIRECTORIES".into(), base.into()),
            ]
            .into(),
            source_sets: vec![rcmake_resolve::ExportedSourceSet {
                kind: rcmake_model::FileSetKind::Headers,
                name: "headers".into(),
                bases: vec![base.into()],
                files: vec![format!("{base}/api.h").into()],
                properties: Default::default(),
            }],
            ..Default::default()
        }],
        ..Default::default()
    };

    let output = render(&plan)?;
    assert!(
        output
            .windows(b"INTERFACE_INCLUDE_DIRECTORIES".len())
            .any(|part| part == b"INTERFACE_INCLUDE_DIRECTORIES")
    );
    let output = String::from_utf8(output.into())?;
    assert!(output.contains(&format!(
        "target_sources(sdk::library\n    INTERFACE\n      FILE_SET \"headers\"\n      TYPE \"HEADERS\"\n      BASE_DIRS \"{base}\"\n      FILES \"{base}/api.h\"\n  )"
    )));
    assert!(!output.contains("INTERFACE_HEADER_SETS"));
    assert!(!output.contains("INTERFACE_SOURCE_SETS"));
    assert!(!output.contains("INTERFACE_FILE_SETS_"));
    Ok(())
}
