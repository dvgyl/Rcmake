use super::*;
#[cfg(unix)]
use rcmake_host::{
    CommandSpec, Input, NativeHost, Output, ProcessResult, RunRequest, path::from_native,
};
use rcmake_host::{PureHost, ReadRange};
#[cfg(unix)]
use std::time::Duration;

#[test]
fn install_export_uses_the_target_path_dialect() -> Result<(), Box<dyn std::error::Error>> {
    for (windows, absolute, relative) in [
        (true, "D:/sdk/lib/pkg.lib", "lib/pkg.dll"),
        (false, "/sdk/lib/pkg.so", "D:/pkg.so"),
    ] {
        let plan = ExportPlan {
            file: "pkg.cmake".into(),
            install: true,
            windows_paths: Some(windows),
            targets: vec![ExportedTarget {
                name: "pkg".into(),
                kind: "SHARED".into(),
                config_properties: [(
                    "Release".into(),
                    [
                        ("IMPORTED_LOCATION".into(), absolute.into()),
                        ("IMPORTED_IMPLIB".into(), relative.into()),
                    ]
                    .into(),
                )]
                .into(),
                ..Default::default()
            }],
            ..Default::default()
        };
        let host = PureHost::default();
        let dir = if cfg!(windows) { "C:/out" } else { "/out" };
        let files = write_export_plan_with_prefix(
            &plan,
            Path::new(dir),
            Some(absolute.as_bytes()),
            Some(b"/prefix"),
            &host,
        )?;
        let mut files = files.iter();
        let range = ReadRange::All { limit: 32_768 };
        let main = String::from_utf8(host.read(files.next().ok_or("main file")?, range)?)?;
        let config = String::from_utf8(host.read(files.next().ok_or("config file")?, range)?)?;
        assert!(main.contains("set(_IMPORT_PREFIX \"/prefix\")"));
        assert!(config.contains(&format!("IMPORTED_LOCATION_RELEASE \"{absolute}\"")));
        assert!(config.contains(&format!(
            "IMPORTED_IMPLIB_RELEASE \"${{_IMPORT_PREFIX}}/{relative}\""
        )));
    }
    Ok(())
}

#[test]
fn build_export_keeps_import_variables() {
    let mut out = BString::default();
    render_property(
        &mut out,
        b"IMPORTED_IMPLIB_RELEASE",
        b"${_IMPORT_PREFIX}/lib/pkg${CMAKE_IMPORT_LIBRARY_SUFFIX}${OTHER}",
        false,
    );
    assert_eq!(
        out,
        r#"  IMPORTED_IMPLIB_RELEASE "${_IMPORT_PREFIX}/lib/pkg${CMAKE_IMPORT_LIBRARY_SUFFIX}\${OTHER}"
"#
    );
}

#[test]
fn renders_namespace_qualified_sibling_links() -> Result<(), Box<dyn std::error::Error>> {
    use rcmake_model::{
        BT, DirId, ExportSet, InstallKind, InstallOptions, InstallRule, LinkItem, ProjectModel,
        Target, TargetKind, TargetOrigin,
    };

    let mut model = ProjectModel::default();
    model.set_root_dirs("/src".into(), "/build".into());
    let interface = |name: &str| {
        Target::new(
            name.into(),
            TargetKind::InterfaceLibrary,
            TargetOrigin::Normal,
            DirId::ROOT,
            Default::default(),
            Default::default(),
        )
    };
    let mut dependency = interface("dep");
    dependency.props.set("EXPORT_NAME".into(), "renamed".into());
    let dependency = model.add_target(dependency)?;
    model.add_alias(DirId::ROOT, "Local::dep".into(), dependency)?;
    let mut consumer = interface("consumer");
    consumer.usage.interface_link_libraries = Some(vec![LinkItem {
        opens_lookup_scope: false,
        closes_lookup_scope: false,
        scope: None,
        text: BT::new(
            concat!(
                "dep;$<LINK_ONLY:Local::dep>;",
                "$<$<CONFIG:Debug>:$<TARGET_NAME:dep>>;",
                "$<$<CONFIG:Debug>:dep>;pthread"
            )
            .into(),
            Default::default(),
        ),
        lookup_dir: DirId::ROOT,
    }]);
    let consumer = model.add_target(consumer)?;
    model.exports.insert(
        "sample".into(),
        ExportSet {
            name: "sample".into(),
            targets: vec![consumer, dependency],
            install_rules: Vec::new(),
        },
    );
    model.add_install(InstallRule {
        dir: DirId::ROOT,
        backtrace: Default::default(),
        kind: InstallKind::Export {
            set: "sample".into(),
            namespace: "Install::".into(),
            file: None,
            cxx_modules_directory: None,
        },
        options: InstallOptions {
            destination: Some("lib/cmake".into()),
            ..Default::default()
        },
    });
    for (index, arg) in [
        "EXPORT",
        "sample",
        "NAMESPACE",
        "Build::",
        "FILE",
        "targets.cmake",
    ]
    .into_iter()
    .enumerate()
    {
        model.global.props.set(
            format!("_RCMAKE_BUILD_EXPORT_0_ARG{index}").into(),
            arg.into(),
        );
    }
    model
        .global
        .props
        .set("_RCMAKE_BUILD_EXPORT_COUNT".into(), "1".into());
    model
        .global
        .props
        .set("_RCMAKE_BUILD_EXPORT_0_DIRECTORY".into(), "0".into());
    let result = rcmake_resolve::resolve(&model, &PureHost::default(), &[Default::default()])?;
    assert_eq!(result.exports.len(), 2);
    for (plan, prefix) in result.exports.iter().zip(["Install::", "Build::"]) {
        let name = format!("{prefix}renamed");
        let main = render_main(plan, b"lib/cmake", None, b"")?;
        let expected = format!(
            "INTERFACE_LINK_LIBRARIES \"{name};\\$<LINK_ONLY:{name}>;\\$<\\$<CONFIG:Debug>:{name}>;\\$<\\$<CONFIG:Debug>:dep>;pthread\""
        );
        assert!(String::from_utf8_lossy(&main).contains(&expected), "{main}");
    }
    Ok(())
}

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
    // CMake goes up once per non-empty destination component, `.` included.
    assert_eq!(
        main.matches("get_filename_component(_IMPORT_PREFIX ")
            .count(),
        5
    );
    assert!(main.contains("add_library(fmt::headers INTERFACE IMPORTED)"));
    assert!(main.contains("add_executable(fmt::tool IMPORTED)"));
    assert!(main.contains("${_IMPORT_PREFIX}/include;${_IMPORT_PREFIX}/include"));
    // Exported target properties escape every `$` of a generator expression.
    assert!(main.contains(r"\$<\$<CONFIG:Release>:fmt::headers>"));
    assert!(main.contains("${CMAKE_CURRENT_LIST_DIR}/fmt-targets-*.cmake"));
    let no_config = read_text(&host, &out.join("fmt-targets-noconfig.cmake"));
    assert!(no_config.contains("IMPORTED_LOCATION_NOCONFIG \"${_IMPORT_PREFIX}/lib/libfmt.a\""));
    assert!(no_config.contains("IMPORTED_LINK_INTERFACE_LANGUAGES_NOCONFIG \"CXX\""));
    let release = read_text(&host, &out.join("fmt-targets-release.cmake"));
    assert!(release.contains("IMPORTED_SONAME_RELEASE \"libfmt.so.2\""));
    assert!(release.contains(
        "_cmake_import_check_files_for_fmt::tool \"${_IMPORT_PREFIX}/bin/tool\\\\;driver\""
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
    assert!(script.contains("add_library(fmt::fmt STATIC IMPORTED)"));
    assert!(script.contains("add_library(fmt::shared SHARED IMPORTED)"));
    assert!(script.contains("IMPORTED_LOCATION_NOCONFIG \"/build/lib/libfmt.a\""));
    assert!(script.contains("IMPORTED_LOCATION_RELEASE \"/build/lib/libfmt.so.2\""));
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
