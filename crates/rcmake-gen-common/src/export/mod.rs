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

use crate::{GenError, quote, quoted_content, write_error, write_if_changed};

pub mod cps;
mod generate;
pub(crate) mod provenance;
pub use generate::{
    FileGenerateContext, FileGenerateInput, FileGenerateNewline, FileGeneratePermissions,
    FileGenerateRequest, write_generated_files,
};

const IMPORT_PREFIX: &[u8] = b"${_IMPORT_PREFIX}";
const IMPORT_VERSION: &[u8] = b"set(CMAKE_IMPORT_FILE_VERSION 1)\n";
const END_IMPORT_VERSION: &[u8] = b"unset(CMAKE_IMPORT_FILE_VERSION)\n";

/// Render the main export file. For an install export, assume the file goes in the
/// root of the install prefix. The main file loads any installed configuration files.
pub fn render(plan: &ExportPlan) -> Result<BString, GenError> {
    render_main(plan, b"", None)
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
    let main = render_main(plan, install_dest.unwrap_or_default(), install_prefix)?;
    let path = out_dir.join(to_native(&plan.file));
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

fn render_main(
    plan: &ExportPlan,
    install_dest: &[u8],
    install_prefix: Option<&[u8]>,
) -> Result<BString, GenError> {
    if !Lexical::new(&plan.file).filename().ends_with(b".cmake") {
        return Err(output_error(
            &plan.file,
            "expected a .cmake export file name",
        ));
    }
    let mut out = BString::from(
        br#"if(CMAKE_VERSION VERSION_LESS "3.0")
  message(FATAL_ERROR "CMake 3.0 or later is required.")
endif()
cmake_policy(PUSH)
cmake_policy(VERSION 3.0...4.3)
"#,
    );
    out.extend_from_slice(IMPORT_VERSION);
    render_guard(&mut out, plan);
    if plan.install {
        render_prefix(&mut out, plan, install_dest, install_prefix)?;
    }
    for target in &plan.targets {
        render_target(&mut out, plan, target)?;
    }
    if plan.install {
        render_installed_configurations(&mut out, export_stem(plan));
    } else {
        for target in &plan.targets {
            for (config, properties) in &target.config_properties {
                if !properties.is_empty() {
                    render_config_target(&mut out, plan, &config_label(config), target, config);
                }
            }
        }
    }
    out.extend_from_slice(END_IMPORT_VERSION);
    out.extend_from_slice(b"cmake_policy(POP)\n");
    Ok(out)
}

fn render_guard(out: &mut BString, plan: &ExportPlan) {
    out.extend_from_slice(
        br#"set(_cmake_targets_defined "")
set(_cmake_targets_not_defined "")
foreach(_cmake_expected_target IN ITEMS"#,
    );
    for target in &plan.targets {
        out.push(b' ');
        quote(out, &qualified_name(plan, target));
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
    if to_native(destination).is_absolute() {
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
    let normalized = Lexical::new(destination).normalize();
    for component in Lexical::new(&normalized).components() {
        if component == b"." {
            continue;
        }
        if component == b".." {
            return Err(output_error(
                &plan.file,
                "the export destination is outside the install prefix",
            ));
        }
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
    match kind {
        b"EXECUTABLE" => {
            out.extend_from_slice(b"add_executable(");
            quote(out, &name);
        }
        b"STATIC" | b"SHARED" | b"MODULE" | b"INTERFACE" | b"OBJECT" | b"UNKNOWN" => {
            out.extend_from_slice(b"add_library(");
            quote(out, &name);
            out.push(b' ');
            out.extend_from_slice(kind);
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
    out.extend_from_slice(b" IMPORTED)\n");
    if !target.properties.is_empty() {
        start_properties(out, &name);
        for (property, value) in &target.properties {
            render_property(out, property, value, plan.install);
        }
        out.extend_from_slice(b")\n");
    }
    out.push(b'\n');
    Ok(())
}

type ConfigTargets<'a> = Vec<(&'a ExportedTarget, &'a BString)>;

fn configurations(plan: &ExportPlan) -> BTreeMap<BString, ConfigTargets<'_>> {
    let mut configs = BTreeMap::<BString, ConfigTargets<'_>>::new();
    for target in &plan.targets {
        for (config, properties) in &target.config_properties {
            if !properties.is_empty() {
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
    quote(out, &name);
    out.extend_from_slice(b" APPEND PROPERTY IMPORTED_CONFIGURATIONS ");
    quote(out, config);
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
                render_relative_path(out, value, false);
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
                    render_relative_path(out, &path, true);
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

    if !paths.is_empty() {
        out.extend_from_slice(b"list(APPEND _cmake_import_check_targets ");
        quote(out, &name);
        out.extend_from_slice(b")\nlist(APPEND ");
        quote(
            out,
            &[b"_cmake_import_check_files_for_", name.as_slice()].concat(),
        );
        for path in paths {
            out.extend_from_slice(b" \"");
            render_relative_path(out, &path, true);
            out.push(b'"');
        }
        out.extend_from_slice(b")\n");
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
      message(FATAL_ERROR "The imported target \"${_cmake_target}\" references the file \"${_cmake_file}\", but this file does not exist.")
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
    quote(out, name);
    out.extend_from_slice(b" PROPERTIES\n");
}

fn start_property(out: &mut BString, key: &[u8]) {
    out.extend_from_slice(b"  ");
    quote(out, key);
    out.extend_from_slice(b" \"");
}

fn render_property(out: &mut BString, key: &[u8], value: &[u8], install: bool) {
    start_property(out, key);
    quote_export_content(out, value, install, false);
    out.extend_from_slice(b"\"\n");
}

fn render_relative_path(out: &mut BString, path: &[u8], list_element: bool) {
    if !path.is_empty()
        && !to_native(path).is_absolute()
        && !path.starts_with(b"$<")
        && !path.starts_with(IMPORT_PREFIX)
    {
        out.extend_from_slice(IMPORT_PREFIX);
        out.push(b'/');
    }
    quote_export_content(out, path, true, list_element);
}

fn quote_export_content(out: &mut BString, mut value: &[u8], install: bool, list_element: bool) {
    while let Some((byte, rest)) = value.split_first() {
        if install
            && let Some(after_prefix) = value
                .strip_prefix(b"$<INSTALL_PREFIX>")
                .or_else(|| value.strip_prefix(IMPORT_PREFIX))
        {
            out.extend_from_slice(IMPORT_PREFIX);
            value = after_prefix;
            continue;
        }
        if *byte == b'$' && rest.starts_with(b"<") {
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

pub(crate) fn write_output(host: &dyn Host, path: &Path, content: &[u8]) -> Result<(), GenError> {
    if let Some(parent) = path.parent() {
        host.create_dir_all(parent)
            .map_err(|error| write_error(parent, error))?;
    }
    write_if_changed(host, path, content)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use rcmake_host::{
        CommandSpec, Input, NativeHost, Output, ProcessResult, RunRequest, path::from_native,
    };
    use rcmake_host::{PureHost, ReadRange};
    #[cfg(unix)]
    use std::time::Duration;

    fn export_plan(install: bool) -> ExportPlan {
        let location = |path: &str| -> BString {
            if install {
                path.into()
            } else {
                format!("/build/{path}").into()
            }
        };
        let mut plan = ExportPlan {
            namespace: "fmt::".into(),
            file: "fmt-targets.cmake".into(),
            install,
            ..ExportPlan::default()
        };
        let mut static_lib = ExportedTarget {
            name: "fmt".into(),
            kind: "STATIC".into(),
            ..ExportedTarget::default()
        };
        static_lib.config_properties.insert(
            "".into(),
            [
                (
                    "IMPORTED_LOCATION_NOCONFIG".into(),
                    location("lib/libfmt.a"),
                ),
                ("IMPORTED_LINK_INTERFACE_LANGUAGES".into(), "CXX".into()),
            ]
            .into(),
        );
        let include = if install {
            "$<INSTALL_PREFIX>/include;${_IMPORT_PREFIX}/include"
        } else {
            "/build/include"
        };
        static_lib
            .properties
            .insert("INTERFACE_INCLUDE_DIRECTORIES".into(), include.into());
        static_lib.properties.insert(
            "INTERFACE_LINK_LIBRARIES".into(),
            "$<$<CONFIG:Release>:fmt::headers>".into(),
        );
        plan.targets.push(static_lib);
        plan.targets.push(ExportedTarget {
            name: "shared".into(),
            kind: "SHARED".into(),
            config_properties: [(
                "Release".into(),
                [
                    ("IMPORTED_LOCATION".into(), location("lib/libfmt.so.2")),
                    ("IMPORTED_SONAME".into(), "libfmt.so.2".into()),
                ]
                .into(),
            )]
            .into(),
            ..ExportedTarget::default()
        });
        plan.targets.push(ExportedTarget {
            name: "headers".into(),
            kind: "INTERFACE".into(),
            ..ExportedTarget::default()
        });
        plan.targets.push(ExportedTarget {
            name: "tool".into(),
            kind: "EXECUTABLE".into(),
            config_properties: [(
                "release".into(),
                [("IMPORTED_LOCATION".into(), location("bin/tool;driver"))].into(),
            )]
            .into(),
            ..ExportedTarget::default()
        });
        plan
    }

    fn read_text(host: &dyn Host, path: &Path) -> String {
        String::from_utf8(host.read(path, ReadRange::All { limit: 32_768 }).unwrap()).unwrap()
    }

    #[test]
    fn install_export_writes_relocatable_configuration_files() {
        let host = PureHost::default();
        let out = Path::new("/work/export");
        let plan = export_plan(true);
        let files = write_export_plan(&plan, out, Some(b"lib//./cmake/fmt/"), &host).unwrap();
        assert_eq!(
            files,
            [
                out.join("fmt-targets.cmake"),
                out.join("fmt-targets-noconfig.cmake"),
                out.join("fmt-targets-release.cmake"),
            ]
        );
        let main = read_text(&host, &out.join("fmt-targets.cmake"));
        assert_eq!(
            main.matches("get_filename_component(_IMPORT_PREFIX ")
                .count(),
            4
        );
        assert!(main.contains("add_library(\"fmt::headers\" INTERFACE IMPORTED)"));
        assert!(main.contains("add_executable(\"fmt::tool\" IMPORTED)"));
        assert!(main.contains("${_IMPORT_PREFIX}/include;${_IMPORT_PREFIX}/include"));
        assert!(main.contains("$<$<CONFIG:Release>:fmt::headers>"));
        assert!(main.contains("${CMAKE_CURRENT_LIST_DIR}/fmt-targets-*.cmake"));
        let no_config = read_text(&host, &out.join("fmt-targets-noconfig.cmake"));
        assert!(
            no_config.contains("\"IMPORTED_LOCATION_NOCONFIG\" \"${_IMPORT_PREFIX}/lib/libfmt.a\"")
        );
        assert!(no_config.contains("\"IMPORTED_LINK_INTERFACE_LANGUAGES_NOCONFIG\" \"CXX\""));
        let release = read_text(&host, &out.join("fmt-targets-release.cmake"));
        assert!(release.contains("\"IMPORTED_SONAME_RELEASE\" \"libfmt.so.2\""));
        assert!(release.contains(
            "\"_cmake_import_check_files_for_fmt::tool\" \"${_IMPORT_PREFIX}/bin/tool\\\\;driver\""
        ));
    }

    #[test]
    fn build_export_is_self_contained() {
        let host = PureHost::default();
        let out = Path::new("/work/export");
        let plan = export_plan(false);
        let path = out.join("fmt-targets.cmake");
        assert_eq!(
            write_export_plan(&plan, out, None, &host).unwrap(),
            std::slice::from_ref(&path)
        );
        let script = read_text(&host, &path);
        assert!(script.contains("add_library(\"fmt::fmt\" STATIC IMPORTED)"));
        assert!(script.contains("add_library(\"fmt::shared\" SHARED IMPORTED)"));
        assert!(script.contains("\"IMPORTED_LOCATION_NOCONFIG\" \"/build/lib/libfmt.a\""));
        assert!(script.contains("\"IMPORTED_LOCATION_RELEASE\" \"/build/lib/libfmt.so.2\""));
        assert!(!script.contains("_IMPORT_PREFIX"));
        assert!(!script.contains("file(GLOB"));
    }

    #[cfg(unix)]
    struct TestDirectory<'a> {
        root: PathBuf,
        host: &'a NativeHost,
    }

    #[cfg(unix)]
    impl Drop for TestDirectory<'_> {
        fn drop(&mut self) {
            let _ = self.host.remove_dir_all(&self.root);
        }
    }

    #[cfg(unix)]
    #[test]
    fn cmake_oracle_imports_both_exports() {
        let host = NativeHost::new().unwrap();
        let Some(oracle) = host.env_get(b"RCMAKE_TEST_REFERENCE_CMAKE") else {
            return;
        };
        let root = PathBuf::from(format!("/tmp/rcmake-export-test-{}", host.system().pid));
        host.create_dir_all(&root).unwrap();
        let workspace = TestDirectory { root, host: &host };
        let root = &workspace.root;
        for install in [true, false] {
            let project = root.join(if install { "install" } else { "build" });
            let destination = "lib/cmake/fmt";
            let package = project.join("relocated prefix");
            let export_dir = if install {
                package.join(destination)
            } else {
                package.clone()
            };
            let plan = export_plan(install);
            write_export_plan(&plan, &export_dir, Some(destination.as_bytes()), &host).unwrap();
            if install {
                for name in ["lib/libfmt.a", "lib/libfmt.so.2", "bin/tool;driver"] {
                    write_output(&host, &package.join(name), b"").unwrap();
                }
                host.create_dir_all(&package.join("include")).unwrap();
            }

            let export_path = export_dir.join("fmt-targets.cmake");
            let mut script =
                BString::from("cmake_minimum_required(VERSION 4.5)\nproject(import_test NONE)\n");
            for _ in 0..2 {
                script.extend_from_slice(b"include(");
                quote(&mut script, &from_native(&export_path));
                script.extend_from_slice(b")\n");
            }
            script.extend_from_slice(
                br#"
get_target_property(config fmt::fmt IMPORTED_CONFIGURATIONS)
get_target_property(location fmt::fmt IMPORTED_LOCATION_NOCONFIG)
get_target_property(soname fmt::shared IMPORTED_SONAME_RELEASE)
get_target_property(headers fmt::headers TYPE)
get_target_property(tool fmt::tool IMPORTED_LOCATION_RELEASE)
get_target_property(links fmt::fmt INTERFACE_LINK_LIBRARIES)
if(NOT config STREQUAL "NOCONFIG"
   OR NOT soname STREQUAL "libfmt.so.2"
   OR NOT headers STREQUAL "INTERFACE_LIBRARY"
   OR NOT links STREQUAL "$<$<CONFIG:Release>:fmt::headers>"
   OR DEFINED _IMPORT_PREFIX OR DEFINED _cmake_import_check_targets)
  message(FATAL_ERROR "The imported properties or scope are incorrect.")
endif()
if(NOT location STREQUAL "#,
            );
            let prefix = if install {
                package.as_path()
            } else {
                Path::new("/build")
            };
            quote(&mut script, &from_native(&prefix.join("lib/libfmt.a")));
            script.extend_from_slice(b" OR NOT tool STREQUAL ");
            quote(&mut script, &from_native(&prefix.join("bin/tool;driver")));
            script.extend_from_slice(
                b")\n  message(FATAL_ERROR \"The imported paths are incorrect.\")\nendif()\n",
            );
            write_output(&host, &project.join("CMakeLists.txt"), &script).unwrap();

            let result = host
                .run(
                    &RunRequest {
                        pipeline: vec![CommandSpec {
                            program: oracle.clone(),
                            args: ["-S", ".", "-B", "out", "-G", "Unix Makefiles"]
                                .into_iter()
                                .map(BString::from)
                                .collect(),
                        }],
                        cwd: project,
                        env: host.env_all(),
                        stdin: Input::Null,
                        stdout: Output::Capture,
                        stderr: Output::Capture,
                        timeout: Some(Duration::from_secs(20)),
                        tee: false,
                    },
                    &mut |_, _| {},
                )
                .unwrap();
            assert_eq!(
                result.results,
                [ProcessResult::Exited(0)],
                "{}\n{}",
                String::from_utf8_lossy(&result.stdout),
                String::from_utf8_lossy(&result.stderr)
            );
        }
    }
}
