use std::path::Path;
use std::sync::Arc;

use rcmake_eval::{Interp, InterpConfig, RunMode};
use rcmake_host::{PureHost, Stream};

use super::*;

use crate::test_support::TestResult;

fn configure(
    code: &str,
    expected: i32,
) -> std::result::Result<(Arc<PureHost>, Interp), Box<dyn std::error::Error>> {
    configure_in(Path::new("/work"), code, expected)
}

fn configure_in(
    work: &Path,
    code: &str,
    expected: i32,
) -> std::result::Result<(Arc<PureHost>, Interp), Box<dyn std::error::Error>> {
    let host = Arc::new(PureHost::new(work.to_path_buf(), work.join("cmake")));
    let mut interp = Interp::new(
        InterpConfig::new(
            host.clone(),
            RunMode::Configure,
            work.join("data"),
            work.join("cmake"),
            work.to_path_buf(),
        ),
        crate::register_all,
    );
    for name in ["core", "api"] {
        interp.cx().project().add_target(rcmake_model::Target::new(
            name.into(),
            TargetKind::InterfaceLibrary,
            rcmake_model::TargetOrigin::Normal,
            rcmake_model::DirId::ROOT,
            Default::default(),
            Default::default(),
        ))?;
    }
    let source = work.join("src");
    host.seed_file(&source.join("CMakeLists.txt"), code.as_bytes())?;
    let version =
        rcmake_base::version::Version::parse_strict(b"4.5").ok_or("invalid test version")?;
    interp.cx().set_cmake_minimum_required(b"4.5", &version)?;
    assert_eq!(
        interp
            .run_configure(&source, &work.join("build"), &[])
            .exit_code,
        expected,
        "{}",
        host.stream_bytes(Stream::Stderr)
    );
    Ok((host, interp))
}

#[test]
fn install_records_rules_and_export_membership() -> TestResult {
    let headers = if cfg!(windows) {
        "C:/opt/headers"
    } else {
        "/opt/headers"
    };
    let (_, mut interp) = configure(
        &format!(
            r#"
set(CMAKE_INSTALL_INCLUDEDIR headers)
install(TARGETS core EXPORT sdk DESTINATION lib COMPONENT Run
        ARCHIVE DESTINATION archives NAMELINK_SKIP FILE_SET HEADERS DESTINATION headers
        INCLUDES DESTINATION ./include include2 {headers})
install(TARGETS api EXPORT sdk)
install(FILES f TYPE INCLUDE PERMISSIONS OWNER_READ GROUP_READ RENAME file OPTIONAL)
install(PROGRAMS tool DESTINATION bin CONFIGURATIONS Debug)
install(DIRECTORY data/ DESTINATION share DIRECTORY_PERMISSIONS OWNER_READ
        USE_SOURCE_PERMISSIONS MESSAGE_NEVER FILES_MATCHING PATTERN "*.txt"
        REGEX "private" EXCLUDE)
install(SCRIPT hooks.cmake ALL_COMPONENTS EXCLUDE_FROM_ALL)
install(CODE "message(STATUS hi)" COMPONENT Docs)
install(EXPORT sdk DESTINATION lib/cmake NAMESPACE sdk:: FILE pkg.cmake
        EXPORT_LINK_INTERFACE_LIBRARIES CXX_MODULES_DIRECTORY bmi)
export(EXPORT sdk NAMESPACE sdk::)
export(TARGETS core FILE core.cmake)
export(TARGETS api APPEND FILE core.cmake EXPORT_LINK_INTERFACE_LIBRARIES)
export(PACKAGE Ignored)
set(CMAKE_EXPORT_PACKAGE_REGISTRY ON)
export(PACKAGE Enabled)
"#
        ),
        0,
    )?;
    let mut cx = interp.cx();
    let model = cx.project();
    let [
        target_rule,
        _,
        files,
        programs,
        directory,
        script,
        code,
        export,
    ] = model.installs.as_slice()
    else {
        return Err("incorrect install rule count".into());
    };
    let sdk = model
        .exports
        .get(b"sdk".as_slice())
        .ok_or("missing export set")?;
    let core = model
        .find_target(rcmake_model::DirId::ROOT, b"core")
        .ok_or("missing core target")?;
    let api = model
        .find_target(rcmake_model::DirId::ROOT, b"api")
        .ok_or("missing api target")?;
    assert_eq!(sdk.targets, [core, api]);
    assert_eq!(sdk.install_rules, [0, 1]);
    assert_eq!(target_rule.options.destination, Some("lib".into()));
    assert_eq!(target_rule.options.component, Some("Run".into()));
    assert_eq!(
        target_rule.options.includes_destination,
        ["./include", "include2", headers]
    );
    assert_eq!(files.options.destination, Some("headers".into()));
    assert_eq!(files.options.rename, Some("file".into()));
    assert!(files.options.optional);
    assert!(
        matches!(&files.kind, InstallKind::Files(paths) if paths.first().is_some_and(|path| path.value == b"/work/src/f"))
    );
    assert_eq!(programs.options.configurations, [BString::from("Debug")]);
    assert!(
        matches!(&directory.kind, InstallKind::Directory(paths) if paths.first().is_some_and(|path| path.value == b"/work/src/data/"))
    );
    assert!(matches!(&script.kind, InstallKind::Script(path) if path == b"/work/src/hooks.cmake"));
    assert!(script.options.exclude_from_all);
    assert_eq!(code.options.component, Some("Docs".into()));
    assert!(
        matches!(&export.kind, InstallKind::Export { set, namespace, file: Some(file), .. } if set == b"sdk" && namespace == b"sdk::" && file == b"pkg.cmake")
    );
    for (key, value) in [
        ("_RCMAKE_INSTALL_0_ARG8", "ARCHIVE"),
        ("_RCMAKE_BUILD_EXPORT_COUNT", "3"),
        ("_RCMAKE_PACKAGE_EXPORT_COUNT", "1"),
        ("_RCMAKE_PACKAGE_EXPORT_0_ARG1", "Enabled"),
    ] {
        assert_eq!(
            model.global.props.get(key.as_bytes()),
            Some(&BString::from(value))
        );
    }
    Ok(())
}

#[test]
fn install_records_destination_normalization_at_the_call_site() -> TestResult {
    let (_, mut interp) = configure(
        r#"
install(FILES header DESTINATION "./include/../$<CONFIG>/")
install(TARGETS core ARCHIVE DESTINATION "./lib/../$<CONFIG>/")
cmake_policy(SET CMP0177 OLD)
install(TARGETS core ARCHIVE DESTINATION "./lib/../$<CONFIG>/")
"#,
        0,
    )?;
    let mut cx = interp.cx();
    let model = cx.project();
    assert_eq!(
        model.installs[0].options.destination,
        Some("$<PATH:CMAKE_PATH,NORMALIZE,./include/../$<CONFIG>/>".into())
    );
    for (rule, expected) in [
        (1, "$<PATH:CMAKE_PATH,NORMALIZE,./lib/../$<CONFIG>/>"),
        (2, "./lib/../$<CONFIG>"),
    ] {
        assert_eq!(
            model
                .global
                .props
                .get(format!("_RCMAKE_INSTALL_{rule}_ARG4").as_bytes()),
            Some(&expected.into())
        );
    }
    Ok(())
}

#[test]
fn argument_group_destinations_drop_one_trailing_slash() -> TestResult {
    let (_, mut interp) = configure(
        r#"
install(FILES header DESTINATION share/x/)
install(DIRECTORY data DESTINATION share/d/)
cmake_policy(SET CMP0177 OLD)
install(FILES header DESTINATION share\\y//)
"#,
        0,
    )?;
    let mut cx = interp.cx();
    let destinations: Vec<_> = cx
        .project()
        .installs
        .iter()
        .map(|rule| rule.options.destination.clone())
        .collect();
    assert_eq!(
        destinations,
        [
            Some("share/x".into()),
            Some("share/d/".into()),
            Some("share/y".into())
        ]
    );
    Ok(())
}

#[test]
fn install_and_export_reject_invalid_argument_sets() -> TestResult {
    for (code, expected) in [
        (
            "install(FILES a b DESTINATION bin RENAME c)",
            "more than one file",
        ),
        ("install(TARGETS absent EXPORT set)", "does not exist"),
        (
            "install(TARGETS core RUNTIME NAMELINK_SKIP)",
            "not in LIBRARY or ARCHIVE",
        ),
        (
            "install(DIRECTORY data DESTINATION d TYPE DATA)",
            "given both TYPE and DESTINATION",
        ),
        (
            "install(CODE hi ALL_COMPONENTS COMPONENT x)",
            "mutually exclusive",
        ),
        ("export(EXPORT unknown)", "not found"),
        ("export(TARGETS core FILE output.txt)", "extension"),
        (
            "export(TARGETS core FILE a.cmake)\nexport(TARGETS core FILE a.cmake)",
            "already specified",
        ),
    ] {
        let (host, _) = configure(&format!("add_library(core INTERFACE)\n{code}"), 1)?;
        assert!(
            host.stream_bytes(Stream::Stderr)
                .to_string()
                .contains(expected),
            "{code}"
        );
    }
    Ok(())
}

#[test]
fn absolute_install_destinations_use_the_configured_diagnostic() -> TestResult {
    let destination = if cfg!(windows) {
        "C:/absolute/archives"
    } else {
        "/absolute/archives"
    };
    let (host, _) = configure(
        &format!(
            r#"
cmake_diagnostic(SET CMD_INSTALL_ABSOLUTE_DESTINATION WARN)
install(TARGETS core ARCHIVE DESTINATION {destination})
install(FILES one DESTINATION relative)
"#
        ),
        0,
    )?;
    let stderr = host.stream_bytes(Stream::Stderr).to_string();
    assert!(stderr.contains(destination), "{stderr}");
    assert!(!stderr.contains("relative"), "{stderr}");
    Ok(())
}

#[test]
fn package_info_records_metadata_and_paths() -> TestResult {
    use rcmake_model::{ExportKind, ProjectMetadata};

    let (_, mut interp) = configure(
        r#"
project(Sdk LANGUAGES NONE VERSION 2.1 COMPAT_VERSION 2.0 SPDX_LICENSE MIT
        DESCRIPTION "Example SDK" HOMEPAGE_URL https://example.org)
set(CMAKE_INSTALL_LIBDIR lib64)
install(TARGETS core EXPORT sdk)
install(PACKAGE_INFO Sdk EXPORT sdk LOWER_CASE_FILE VERSION_SCHEMA simple VERSION 3.0
        COMPAT_VERSION 2.5 DEFAULT_TARGETS core DEFAULT_CONFIGURATIONS Debug Release
        LICENSE Apache-2.0 DEFAULT_LICENSE ISC DESCRIPTION "Install SDK"
        HOMEPAGE_URL https://install.example.org CXX_MODULES_DIRECTORY modules
        PERMISSIONS OWNER_READ GROUP_READ CONFIGURATIONS Release COMPONENT Dev EXCLUDE_FROM_ALL)
export(PACKAGE_INFO Sdk EXPORT sdk)
export(PACKAGE_INFO Named EXPORT sdk PROJECT Sdk VERSION 4.0)
export(PACKAGE_INFO Sdk EXPORT sdk APPENDIX Tools LOWER_CASE_FILE DEFAULT_LICENSE ISC)
export(PACKAGE_INFO Isolated EXPORT sdk NO_PROJECT_METADATA)
export(PACKAGE_INFO Unmatched EXPORT sdk)
set(CMAKE_SYSTEM_NAME Windows)
install(PACKAGE_INFO Win EXPORT sdk LOWER_CASE_FILE)
install(PACKAGE_INFO Custom EXPORT sdk DESTINATION share/cps)
"#,
        0,
    )?;
    let mut cx = interp.cx();
    let model = cx.project();
    let [_, install, win, custom] = model.installs.as_slice() else {
        return Err("incorrect install rule count".into());
    };
    let InstallKind::PackageInfo(spec) = &install.kind else {
        return Err("missing package information".into());
    };
    assert_eq!(spec.package, "Sdk");
    assert_eq!(spec.export, "sdk");
    assert!(spec.lower_case_file);
    assert_eq!(spec.version, Some("3.0".into()));
    assert_eq!(spec.compat_version, Some("2.5".into()));
    assert_eq!(spec.version_schema, Some("simple".into()));
    assert_eq!(spec.default_targets, ["core"]);
    assert_eq!(spec.default_configurations, ["Debug", "Release"]);
    assert_eq!(spec.license, Some("Apache-2.0".into()));
    assert_eq!(spec.default_license, Some("ISC".into()));
    assert_eq!(spec.description, Some("Install SDK".into()));
    assert_eq!(
        spec.homepage_url,
        Some("https://install.example.org".into())
    );
    assert_eq!(spec.cxx_modules_directory, Some("modules".into()));
    assert_eq!(install.options.destination, Some("lib64/cps/sdk".into()));
    assert_eq!(install.options.component, Some("Dev".into()));
    assert_eq!(install.options.permissions, ["OWNER_READ", "GROUP_READ"]);
    assert_eq!(install.options.configurations, ["Release"]);
    assert!(install.options.exclude_from_all);
    assert_eq!(win.options.destination, Some("cps/win".into()));
    assert_eq!(custom.options.destination, Some("share/cps".into()));

    let [current, named, appendix, isolated, unmatched] = model.build_exports.as_slice() else {
        return Err("incorrect build export count".into());
    };
    let ExportKind::PackageInfo(current_spec) = &current.kind else {
        return Err("missing current project metadata".into());
    };
    assert_eq!(current.file, Some("/work/build/cps/Sdk/Sdk.cps".into()));
    assert_eq!(current.namespace, "Sdk::");
    assert_eq!(current_spec.version, Some("2.1".into()));
    assert_eq!(current_spec.compat_version, Some("2.0".into()));
    assert_eq!(current_spec.license, Some("MIT".into()));
    assert_eq!(current_spec.description, Some("Example SDK".into()));
    assert_eq!(
        current_spec.homepage_url,
        Some("https://example.org".into())
    );
    assert!(matches!(&named.kind, ExportKind::PackageInfo(spec)
        if spec.project == ProjectMetadata::Named("Sdk".into())
            && spec.version == Some("4.0".into()) && spec.compat_version.is_none()
            && spec.license == Some("MIT".into())));
    assert_eq!(
        appendix.file,
        Some("/work/build/cps/sdk/sdk-Tools.cps".into())
    );
    assert!(matches!(&appendix.kind, ExportKind::PackageInfo(spec)
        if spec.version.is_none() && spec.license.is_none()
            && spec.default_license == Some("ISC".into())));
    assert!(matches!(&isolated.kind, ExportKind::PackageInfo(spec)
        if spec.project == ProjectMetadata::None && spec.version.is_none()));
    assert!(matches!(&unmatched.kind, ExportKind::PackageInfo(spec) if spec.version.is_none()));
    Ok(())
}

#[test]
fn sbom_records_exports_formats_and_install_options() -> TestResult {
    use rcmake_model::{ExportKind, ProjectMetadata};

    let (_, mut interp) = configure(
        r#"
project(Sdk LANGUAGES NONE VERSION 2.1 SPDX_LICENSE MIT DESCRIPTION "SDK"
        HOMEPAGE_URL https://example.org)
install(TARGETS core EXPORT sdk)
install(TARGETS api EXPORT api)
install(SBOM Sdk EXPORTS sdk api FORMAT spdx DEFAULT_LICENSE ISC
        PACKAGE_URL https://example.org/sdk.tar.gz PERMISSIONS OWNER_READ
        CONFIGURATIONS Debug COMPONENT Supply EXCLUDE_FROM_ALL)
install(SBOM Published PROJECT Sdk EXPORTS sdk DESTINATION share/sbom
        DATA_LICENSE CC0-1.0 DESCRIPTION "Explicit SDK")
export(SBOM First PROJECT Sdk EXPORTS sdk FORMAT spdx-3.0+json)
export(SBOM Second NO_PROJECT_METADATA EXPORTS sdk api FORMAT spdx-3.0.1+json VERSION 3.0)
export(SBOM Other EXPORTS api)
"#,
        0,
    )?;
    let mut cx = interp.cx();
    let model = cx.project();
    let [_, _, install, published] = model.installs.as_slice() else {
        return Err("incorrect install rule count".into());
    };
    let InstallKind::Sbom(spec) = &install.kind else {
        return Err("missing installed SBOM".into());
    };
    assert_eq!(spec.name, "Sdk");
    assert_eq!(spec.exports, ["sdk", "api"]);
    assert_eq!(spec.version, Some("2.1".into()));
    assert_eq!(spec.data_license, Some("MIT".into()));
    assert_eq!(spec.default_license, Some("ISC".into()));
    assert_eq!(spec.description, Some("SDK".into()));
    assert_eq!(spec.homepage_url, Some("https://example.org".into()));
    assert_eq!(
        spec.package_url,
        Some("https://example.org/sdk.tar.gz".into())
    );
    assert_eq!(spec.format, Some("spdx-3.0.1+json".into()));
    assert_eq!(install.options.destination, Some("lib/sbom/Sdk".into()));
    assert_eq!(install.options.component, Some("Supply".into()));
    assert_eq!(install.options.permissions, ["OWNER_READ"]);
    assert_eq!(install.options.configurations, ["Debug"]);
    assert!(install.options.exclude_from_all);
    assert_eq!(published.options.destination, Some("share/sbom".into()));
    assert!(matches!(&published.kind, InstallKind::Sbom(spec)
        if spec.data_license == Some("CC0-1.0".into())
            && spec.description == Some("Explicit SDK".into())));
    let [first, second, default] = model.build_exports.as_slice() else {
        return Err("incorrect build export count".into());
    };
    assert_eq!(first.file, Some("/work/build/sbom/First/First".into()));
    assert!(matches!(&first.kind, ExportKind::Sbom(spec)
        if spec.version == Some("2.1".into()) && spec.format == Some("spdx-3.0.1+json".into())));
    assert!(matches!(&second.kind, ExportKind::Sbom(spec)
        if spec.project == ProjectMetadata::None && spec.data_license.is_none()
            && spec.format == Some("spdx-3.0.1+json".into()) && spec.version == Some("3.0".into())));
    assert!(matches!(&default.kind, ExportKind::Sbom(spec) if spec.format.is_none()));
    Ok(())
}

#[test]
fn metadata_commands_reject_invalid_options() -> TestResult {
    for command in ["install", "export"] {
        for (arguments, expected) in [
            ("PACKAGE_INFO", "PACKAGE_INFO: missing required value"),
            ("PACKAGE_INFO Sdk", "PACKAGE_INFO missing EXPORT."),
            ("PACKAGE_INFO Sdk EXPORT", "EXPORT: missing required value"),
            (
                "PACKAGE_INFO %sdk EXPORT sdk",
                "invalid package name \"%sdk\"",
            ),
            (
                "PACKAGE_INFO Sdk EXPORT sdk COMPAT_VERSION 1",
                "COMPAT_VERSION requires VERSION.",
            ),
            (
                "PACKAGE_INFO Sdk EXPORT sdk VERSION_SCHEMA simple",
                "VERSION_SCHEMA requires VERSION.",
            ),
            (
                "PACKAGE_INFO Sdk EXPORT sdk PROJECT Sdk NO_PROJECT_METADATA",
                "PROJECT and NO_PROJECT_METADATA are mutually exclusive.",
            ),
            (
                "PACKAGE_INFO Sdk EXPORT sdk PROJECT Missing",
                "PROJECT given unknown project name \"Missing\".",
            ),
            (
                "PACKAGE_INFO Sdk EXPORT sdk LICENSE \"\" DEFAULT_LICENSE \"\"",
                "LICENSE: empty string not allowed",
            ),
            ("SBOM Sdk", "SBOM missing EXPORTS."),
            ("SBOM Sdk EXPORTS", "EXPORTS: missing required value"),
            (
                "SBOM Sdk EXPORTS sdk FORMAT cyclonedx",
                "SBOM given invalid format \"cyclonedx\".",
            ),
            (
                "SBOM Sdk EXPORTS sdk FORMAT \"\"",
                "FORMAT: empty string not allowed",
            ),
            (
                "SBOM Sdk EXPORTS sdk PROJECT Sdk NO_PROJECT_METADATA",
                "PROJECT and NO_PROJECT_METADATA are mutually exclusive.",
            ),
        ] {
            let (host, _) = configure(
                &format!("install(TARGETS core EXPORT sdk)\n{command}({arguments})"),
                1,
            )?;
            let stderr = host.stream_bytes(Stream::Stderr).to_string();
            assert!(
                stderr.contains(expected),
                "{command}({arguments}): {stderr}"
            );
        }
        for (key, argument) in [
            ("PROJECT", "Sdk"),
            ("VERSION", "1.0"),
            ("LICENSE", "MIT"),
            ("DESCRIPTION", "About"),
            ("HOMEPAGE_URL", "https://example.org"),
            ("DEFAULT_TARGETS", "core"),
            ("DEFAULT_CONFIGURATIONS", "Debug"),
        ] {
            let (host, _) = configure(
                &format!(
                    "install(TARGETS core EXPORT sdk)\n{command}(PACKAGE_INFO Sdk EXPORT sdk APPENDIX Tools {key} {argument})"
                ),
                1,
            )?;
            let stderr = host.stream_bytes(Stream::Stderr).to_string();
            assert!(
                stderr.contains(&format!("APPENDIX and {key} are mutually exclusive.")),
                "{stderr}"
            );
        }
    }
    Ok(())
}

#[test]
fn target_file_set_and_bmi_options_are_recorded() -> TestResult {
    let (_, mut interp) = configure(
        r#"
target_sources(core INTERFACE FILE_SET HEADERS FILES core.h)
target_sources(core INTERFACE FILE_SET more TYPE HEADERS FILES more.h)
install(TARGETS core EXPORT sdk FILE_SET HEADERS DESTINATION include COMPONENT Headers
        PERMISSIONS OWNER_READ CONFIGURATIONS Release OPTIONAL EXCLUDE_FROM_ALL
        FILE_SET more DESTINATION extra
        CXX_MODULES_BMI DESTINATION bmi COMPONENT Modules
        PERMISSIONS OWNER_READ CONFIGURATIONS Debug OPTIONAL EXCLUDE_FROM_ALL)
"#,
        0,
    )?;
    let mut cx = interp.cx();
    let model = cx.project();
    assert_eq!(model.installs.len(), 1);
    let args: Vec<_> = (0..)
        .map_while(|i| {
            model
                .global
                .props
                .get(format!("_RCMAKE_INSTALL_0_ARG{i}").as_bytes())
        })
        .map(|value| value.as_slice())
        .collect();
    assert_eq!(
        args,
        [
            b"TARGETS".as_slice(),
            b"core",
            b"EXPORT",
            b"sdk",
            b"FILE_SET",
            b"HEADERS",
            b"DESTINATION",
            b"include",
            b"COMPONENT",
            b"Headers",
            b"PERMISSIONS",
            b"OWNER_READ",
            b"CONFIGURATIONS",
            b"Release",
            b"OPTIONAL",
            b"EXCLUDE_FROM_ALL",
            b"FILE_SET",
            b"more",
            b"DESTINATION",
            b"extra",
            b"CXX_MODULES_BMI",
            b"DESTINATION",
            b"bmi",
            b"COMPONENT",
            b"Modules",
            b"PERMISSIONS",
            b"OWNER_READ",
            b"CONFIGURATIONS",
            b"Debug",
            b"OPTIONAL",
            b"EXCLUDE_FROM_ALL",
        ]
    );
    let (host, _) = configure(
        "target_sources(core INTERFACE FILE_SET HEADERS FILES core.h)\ninstall(TARGETS core EXPORT sdk CXX_MODULES_BMI DESTINATION bmi)",
        1,
    )?;
    assert!(
        host.stream_bytes(Stream::Stderr)
            .to_string()
            .contains("not all of its interface")
    );
    Ok(())
}

#[test]
fn default_install_component_uses_the_value_at_each_call() -> TestResult {
    let (_, mut interp) = configure(
        r#"
install(CODE "message(no default)")
set(CMAKE_INSTALL_DEFAULT_COMPONENT_NAME Cached CACHE STRING "Default component")
install(FILES a DESTINATION .)
set(CMAKE_INSTALL_DEFAULT_COMPONENT_NAME Normal)
install(TARGETS core EXPORT sdk)
install(EXPORT sdk DESTINATION cmake)
install(PACKAGE_INFO Sdk EXPORT sdk)
install(SBOM Sdk EXPORTS sdk)
set(CMAKE_INSTALL_DEFAULT_COMPONENT_NAME Changed)
install(PROGRAMS script DESTINATION bin)
install(DIRECTORY data/ DESTINATION share COMPONENT Explicit)
set(CMAKE_INSTALL_DEFAULT_COMPONENT_NAME "")
install(SCRIPT install.cmake)
get_cmake_property(found COMPONENTS)
"#,
        0,
    )?;
    let mut cx = interp.cx();
    assert_eq!(
        cx.var_or_empty(b"found"),
        "Cached;Changed;Explicit;Normal;Unspecified"
    );
    let components: Vec<_> = cx
        .project()
        .installs
        .iter()
        .filter_map(|rule| rule.options.component.as_ref().map(|name| name.as_slice()))
        .collect();
    assert_eq!(
        components,
        [
            b"Unspecified".as_slice(),
            b"Cached",
            b"Normal",
            b"Normal",
            b"Normal",
            b"Normal",
            b"Changed",
            b"Explicit",
            b"Unspecified",
        ]
    );
    Ok(())
}

#[test]
fn metadata_commands_reject_duplicate_outputs() -> TestResult {
    for (commands, expected) in [
        (
            "export(PACKAGE_INFO Sdk EXPORT sdk LOWER_CASE_FILE)\nexport(PACKAGE_INFO sdk EXPORT sdk)",
            "command already specified for the file sdk.cps.",
        ),
        (
            "export(SBOM Sdk EXPORTS sdk)\nexport(SBOM Sdk EXPORTS sdk)",
            "SBOM command already specified for the file Sdk.",
        ),
        (
            "install(SBOM Sdk EXPORTS sdk)\ninstall(SBOM Sdk EXPORTS sdk DESTINATION lib/sbom/Sdk)",
            "SBOM command already specified for the file Sdk.",
        ),
        (
            "export(PACKAGE_INFO Sdk EXPORT missing)",
            "Export set \"missing\" not found.",
        ),
        (
            "export(SBOM Sdk EXPORTS sdk missing)",
            "Export set \"missing\" not found.",
        ),
    ] {
        let (host, _) = configure(&format!("install(TARGETS core EXPORT sdk)\n{commands}"), 1)?;
        let stderr = host.stream_bytes(Stream::Stderr).to_string();
        assert!(stderr.contains(expected), "{stderr}");
    }
    Ok(())
}

#[test]
fn records_mapped_package_info_from_install_export() -> TestResult {
    let work = Path::new(if cfg!(windows) { "C:/work" } else { "/work" });
    let (_, mut interp) = configure_in(
        work,
        r#"
set(CMAKE_EXPERIMENTAL_MAPPED_PACKAGE_INFO ababa1b5-7099-495f-a9cd-e22d38f274f2)
set(CMAKE_INSTALL_EXPORTS_AS_PACKAGE_INFO sdk:Example/l/share/cps)
set(project_version 2.3.4)
set(sdk_EXPORT_PACKAGE_INFO_VERSION @project_version@)
install(TARGETS core EXPORT sdk)
install(EXPORT sdk DESTINATION lib/cmake COMPONENT Development)
"#,
        0,
    )?;
    let mut cx = interp.cx();
    let [_, package, export] = cx.project().installs.as_slice() else {
        return Err("Expected target, package information, and export install rules.".into());
    };
    let InstallKind::PackageInfo(spec) = &package.kind else {
        return Err("The mapped package information rule is missing.".into());
    };
    assert_eq!(spec.package, "Example");
    assert_eq!(spec.export, "sdk");
    assert_eq!(
        spec.version.as_deref().map(Vec::as_slice),
        Some(b"2.3.4".as_slice())
    );
    assert!(spec.lower_case_file);
    assert_eq!(package.options.destination, Some("share/cps".into()));
    assert_eq!(package.options.component, export.options.component);
    Ok(())
}

#[test]
fn records_imported_runtime_artifact_options_and_build_dependencies() -> TestResult {
    let work = Path::new(if cfg!(windows) { "C:/work" } else { "/work" });
    let (_, mut interp) = configure_in(
        work,
        r#"
set(CMAKE_HOST_SYSTEM_NAME Linux)
add_executable(tool IMPORTED)
set_property(TARGET tool PROPERTY IMPORTED_LOCATION "${CMAKE_CURRENT_BINARY_DIR}/tool")
add_dependencies(tool core)
install(IMPORTED_RUNTIME_ARTIFACTS tool RUNTIME_DEPENDENCY_SET relocation
    RUNTIME DESTINATION tools OPTIONAL COMPONENT Runtime CONFIGURATIONS Debug)
"#,
        0,
    )?;
    let mut cx = interp.cx();
    let model = cx.project();
    let [rule] = model.installs.as_slice() else {
        return Err("Expected one imported artifact install rule.".into());
    };
    let InstallKind::Programs(inputs) = &rule.kind else {
        return Err("Expected an executable install rule.".into());
    };
    let [input] = inputs.as_slice() else {
        return Err("Expected one imported executable.".into());
    };
    assert_eq!(input.value, "$<TARGET_FILE:tool>");
    assert!(rule.options.optional);
    assert_eq!(rule.options.configurations, ["Debug"]);
    assert_eq!(rule.options.component, Some("Runtime".into()));
    assert_eq!(rule.options.destination, Some("tools".into()));
    assert_eq!(rule.options.build_depends, ["core"]);
    assert!(
        model
            .global
            .props
            .get(b"_RCMAKE_INSTALL_0_ARG3")
            .is_some_and(|value| value == b"relocation")
    );
    Ok(())
}

#[test]
fn namelink_only_rules_keep_one_export_member() -> TestResult {
    let work = Path::new(if cfg!(windows) { "C:/work" } else { "/work" });
    let (_, mut interp) = configure_in(
        work,
        r#"
add_library(shared SHARED)
install(TARGETS shared EXPORT sdk LIBRARY DESTINATION lib NAMELINK_SKIP)
install(TARGETS shared EXPORT sdk LIBRARY DESTINATION lib NAMELINK_ONLY)
install(EXPORT sdk DESTINATION lib/cmake)
"#,
        0,
    )?;
    let mut cx = interp.cx();
    let model = cx.project();
    let sdk = model
        .exports
        .get(b"sdk".as_slice())
        .ok_or("Missing SDK export.")?;
    assert_eq!(sdk.targets.len(), 1);
    assert_eq!(model.installs.len(), 3);
    Ok(())
}

#[test]
fn empty_script_component_uses_the_default_component() -> TestResult {
    let work = Path::new(if cfg!(windows) { "C:/work" } else { "/work" });
    let (_, mut interp) = configure_in(
        work,
        r#"
set(CMAKE_INSTALL_DEFAULT_COMPONENT_NAME Development)
install(CODE "message(STATUS installed)" COMPONENT EXCLUDE_FROM_ALL)
"#,
        0,
    )?;
    let mut cx = interp.cx();
    let [rule] = cx.project().installs.as_slice() else {
        return Err("Expected one install script rule.".into());
    };
    assert_eq!(rule.options.component, Some("Development".into()));
    assert!(rule.options.exclude_from_all);
    Ok(())
}

#[test]
fn type_destinations_use_cmake_defaults() -> TestResult {
    let (_, mut interp) = configure(
        r#"
install(FILES f TYPE DOC)
install(FILES f TYPE RUNSTATE)
set(CMAKE_INSTALL_LOCALSTATEDIR state)
install(FILES f TYPE RUNSTATE)
"#,
        0,
    )?;
    let mut cx = interp.cx();
    let destinations: Vec<_> = cx
        .project()
        .installs
        .iter()
        .map(|rule| rule.options.destination.clone())
        .collect();
    assert_eq!(
        destinations,
        [
            Some("share/doc".into()),
            Some("var/run".into()),
            Some("state/run".into())
        ]
    );
    Ok(())
}

#[test]
fn install_accepts_an_alias_target_and_installs_the_referenced_target() -> TestResult {
    let (host, mut interp) = configure(
        "add_library(real INTERFACE)\nadd_library(nick ALIAS real)\ninstall(TARGETS nick EXPORT set)\n",
        0,
    )?;
    assert_eq!(host.stream_bytes(Stream::Stderr).to_string(), "");
    let mut cx = interp.cx();
    let model = cx.project();
    let InstallKind::Targets { targets, .. } = &model.installs[0].kind else {
        return Err("expected a target rule".into());
    };
    let names: Vec<_> = targets
        .iter()
        .filter_map(|id| model.target(*id).map(|target| target.name.clone()))
        .collect();
    assert_eq!(names, [BString::from("real")]);
    Ok(())
}

#[test]
fn cmp0177_warns_for_each_destination_that_normalization_changes() -> TestResult {
    let (host, _) = configure(
        r#"
cmake_policy(VERSION 3.30)
install(FILES f DESTINATION a/../b)
install(FILES f DESTINATION plain)
install(FILES f DESTINATION "x/$<CONFIG>/../y")
install(TARGETS core ARCHIVE DESTINATION ./lib INCLUDES DESTINATION ./include)
"#,
        0,
    )?;
    let stderr = host.stream_bytes(Stream::Stderr).to_string();
    assert_eq!(
        stderr.matches("Policy CMP0177 is not set").count(),
        2,
        "{stderr}"
    );
    Ok(())
}

#[test]
fn headers_default_only_when_no_applicable_artifact_destination_is_set() -> TestResult {
    let (host, mut interp) = configure(
        r#"
add_library(static_lib STATIC a.c)
add_library(shared_lib SHARED a.c)
set_property(TARGET static_lib shared_lib PROPERTY PUBLIC_HEADER public.h)
set_property(TARGET static_lib shared_lib PROPERTY PRIVATE_HEADER private.h)
install(TARGETS static_lib shared_lib LIBRARY DESTINATION lib PUBLIC_HEADER PRIVATE_HEADER)
install(TARGETS static_lib PUBLIC_HEADER DESTINATION api PRIVATE_HEADER DESTINATION details)
"#,
        0,
    )?;
    let stderr = host.stream_bytes(Stream::Stderr).to_string();
    let diagnostic = stderr.split_whitespace().collect::<Vec<_>>().join(" ");
    for header in ["PUBLIC_HEADER", "PRIVATE_HEADER"] {
        assert_eq!(
            diagnostic
                .matches(&format!("has {header} files but no {header} DESTINATION"))
                .count(),
            1,
            "{stderr}"
        );
    }
    assert!(stderr.contains("Target shared_lib"), "{stderr}");
    let mut cx = interp.cx();
    let model = cx.project();
    let snapshot = |rule: usize, name: &str, header: &str| {
        let id = model.find_target(rcmake_model::DirId::ROOT, name.as_bytes())?;
        model
            .global
            .props
            .get(format!("_RCMAKE_INSTALL_{rule}_{header}_{}", id.index()).as_bytes())
            .cloned()
    };
    for (header, file) in [
        ("PUBLIC_HEADER", "public.h"),
        ("PRIVATE_HEADER", "private.h"),
    ] {
        assert_eq!(snapshot(0, "shared_lib", header), Some(BString::default()));
        assert_eq!(snapshot(0, "static_lib", header), Some(file.into()));
        assert_eq!(snapshot(1, "static_lib", header), Some(file.into()));
    }
    let manifest = rcmake_resolve::lower::lower_install_manifest(model, &[]);
    assert_eq!(
        manifest
            .actions
            .iter()
            .filter(|action| action.destination == b"include")
            .count(),
        2,
    );
    Ok(())
}

#[test]
fn runtime_dependencies_check_host_before_target_platform() -> TestResult {
    for (variables, expected) in [
        (
            "set(CMAKE_HOST_SYSTEM_NAME Unsupported)\nset(CMAKE_CROSSCOMPILING ON)",
            "is not supported on system \"Unsupported\"",
        ),
        (
            "set(CMAKE_HOST_SYSTEM_NAME Linux)\nset(CMAKE_CROSSCOMPILING ON)",
            "is not supported when cross-compiling.",
        ),
        (
            "set(CMAKE_HOST_SYSTEM_NAME Darwin)\nset(CMAKE_CROSSCOMPILING OFF)",
            "given no FRAMEWORK DESTINATION",
        ),
    ] {
        let (host, _) = configure(
            &format!(
                "set(CMAKE_SYSTEM_NAME Generic)\n{variables}\ninstall(TARGETS core RUNTIME_DEPENDENCIES)"
            ),
            1,
        )?;
        let stderr = host.stream_bytes(Stream::Stderr).to_string();
        let diagnostic = stderr.split_whitespace().collect::<Vec<_>>().join(" ");
        assert!(diagnostic.contains(expected), "{stderr}");
    }
    configure(
        r#"
        set(CMAKE_HOST_SYSTEM_NAME Darwin)
        set(CMAKE_CROSSCOMPILING OFF)
        install(TARGETS core RUNTIME_DEPENDENCIES DESTINATION lib)
        "#,
        0,
    )?;
    Ok(())
}

#[test]
fn runtime_dependency_sets_allow_one_bundle_executable() -> TestResult {
    let setup = r#"
        set(CMAKE_SYSTEM_NAME Darwin)
        set(CMAKE_HOST_SYSTEM_NAME Darwin)
        set(CMAKE_CROSSCOMPILING OFF)
        add_executable(one MACOSX_BUNDLE one.c)
        add_executable(two MACOSX_BUNDLE two.c)
        add_executable(imported IMPORTED)
        set_property(TARGET imported PROPERTY MACOSX_BUNDLE ON)
    "#;
    for rules in [
        "install(TARGETS one two RUNTIME_DEPENDENCIES DESTINATION apps)",
        "install(TARGETS one RUNTIME_DEPENDENCY_SET deps DESTINATION apps)
         install(TARGETS two RUNTIME_DEPENDENCY_SET deps DESTINATION apps)",
        "install(TARGETS one RUNTIME_DEPENDENCY_SET deps DESTINATION apps)
         install(IMPORTED_RUNTIME_ARTIFACTS imported RUNTIME_DEPENDENCY_SET deps DESTINATION apps)",
    ] {
        let (host, _) = configure(&format!("{setup}\n{rules}"), 1)?;
        let stderr = host.stream_bytes(Stream::Stderr).to_string();
        assert!(
            stderr.contains("A runtime dependency set may only have one bundle executable."),
            "{stderr}"
        );
    }
    configure(
        &format!(
            "{setup}
            install(TARGETS one RUNTIME_DEPENDENCIES DESTINATION apps)
            install(TARGETS two RUNTIME_DEPENDENCIES DESTINATION apps)
            install(TARGETS one RUNTIME_DEPENDENCY_SET one_set DESTINATION apps)
            install(TARGETS two RUNTIME_DEPENDENCY_SET two_set DESTINATION apps)"
        ),
        0,
    )?;
    Ok(())
}

#[test]
fn runtime_dependency_installs_register_components() -> TestResult {
    let (_, mut interp) = configure(
        r#"
        project(Demo LANGUAGES NONE)
        set(CMAKE_HOST_SYSTEM_NAME Darwin)
        set(CMAKE_CROSSCOMPILING OFF)
        set(CMAKE_INSTALL_DEFAULT_COMPONENT_NAME "<PROJECT_NAME>")
        add_executable(app main.c)
        install(TARGETS core RUNTIME_DEPENDENCIES
                LIBRARY COMPONENT Empty FRAMEWORK DESTINATION fw COMPONENT Empty)
        install(TARGETS app RUNTIME_DEPENDENCIES
                LIBRARY COMPONENT Libraries FRAMEWORK DESTINATION fw COMPONENT Frameworks)
        install(RUNTIME_DEPENDENCY_SET deps
                LIBRARY COMPONENT NamedLibraries FRAMEWORK COMPONENT NamedFrameworks)
        set(CMAKE_HOST_SYSTEM_NAME Windows)
        set(CMAKE_IMPORT_LIBRARY_SUFFIX .lib)
        install(RUNTIME_DEPENDENCY_SET dlls LIBRARY COMPONENT Unused RUNTIME COMPONENT Dlls)
        set(PROJECT_NAME "")
        install(FILES readme DESTINATION doc)
        "#,
        0,
    )?;
    let actual: Vec<_> = registered(interp.cx().project()).into_iter().collect();
    assert_eq!(
        actual,
        [
            "Demo",
            "Dlls",
            "Frameworks",
            "Libraries",
            "NamedFrameworks",
            "NamedLibraries",
            "Unspecified",
        ]
    );
    Ok(())
}

#[test]
fn runtime_dependency_targets_flag_is_recorded_and_rejects_export() -> TestResult {
    let (_, mut interp) = configure(
        "install(TARGETS core RUNTIME_DEPENDENCY_TARGETS DESTINATION lib)",
        0,
    )?;
    assert_eq!(
        interp.cx().global().props.get(b"_RCMAKE_INSTALL_0_ARG2"),
        Some(&BString::from("RUNTIME_DEPENDENCY_TARGETS"))
    );

    let (host, _) = configure(
        "install(TARGETS core RUNTIME_DEPENDENCY_TARGETS EXPORT sdk)",
        1,
    )?;
    let stderr = host.stream_bytes(Stream::Stderr).to_string();
    assert!(
        stderr.contains("TARGETS RUNTIME_DEPENDENCY_TARGETS cannot be used with EXPORT."),
        "{stderr}"
    );
    Ok(())
}

#[test]
fn android_mk_install_validates_options_and_registers_component() -> TestResult {
    let destination = if cfg!(windows) {
        "C:/absolute/sdk"
    } else {
        "/absolute/sdk"
    };
    let (host, mut interp) = configure(
        &format!(
            "cmake_diagnostic(SET CMD_INSTALL_ABSOLUTE_DESTINATION WARN)
            install(EXPORT_ANDROID_MK sdk DESTINATION {destination} FILE kit.mk
                    NAMESPACE sdk:: COMPONENT Dev PERMISSIONS OWNER_READ CONFIGURATIONS Debug
                    EXCLUDE_FROM_ALL EXPORT_LINK_INTERFACE_LIBRARIES)"
        ),
        0,
    )?;
    let stderr = host.stream_bytes(Stream::Stderr).to_string();
    assert_eq!(stderr.matches(destination).count(), 1, "{stderr}");
    assert_eq!(registered(interp.cx().project()), ["Dev".into()].into());
    for (args, expected) in [
        ("FILE kit.mk", "given no DESTINATION!"),
        ("DESTINATION lib WRONG", "given unknown argument \"WRONG\"."),
        (
            "DESTINATION lib FILE sub/kit.mk",
            "The FILE argument may not contain a path.",
        ),
        (
            "DESTINATION lib FILE kit.cmake",
            "must specify a name ending in \".mk\".",
        ),
    ] {
        let (host, _) = configure(&format!("install(EXPORT_ANDROID_MK sdk {args})"), 1)?;
        let stderr = host.stream_bytes(Stream::Stderr).to_string();
        assert!(
            stderr
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ")
                .contains(expected),
            "{stderr}"
        );
    }
    Ok(())
}

#[test]
fn install_registers_export_sets_before_targets() -> TestResult {
    let (_, mut interp) = configure(
        r#"
project(Example NONE)
install(EXPORT Standard DESTINATION lib)
install(EXPORT_ANDROID_MK Android DESTINATION lib)
install(PACKAGE_INFO Example EXPORT Package)
install(SBOM Example EXPORTS Document Other)
export(EXPORT Standard FILE Standard.cmake)
export(EXPORT Android FILE Android.cmake)
export(EXPORT Package FILE Package.cmake)
export(EXPORT Document FILE Document.cmake)
export(EXPORT Other FILE Other.cmake)
install(TARGETS core EXPORT Standard)
"#,
        0,
    )?;
    assert_eq!(
        interp.cx().project().exports[b"Standard".as_slice()]
            .targets
            .len(),
        1
    );
    Ok(())
}

#[test]
fn install_targets_accepts_empty_include_groups() -> TestResult {
    let (_, mut interp) = configure(
        r#"
install(TARGETS core EXPORT first INCLUDES)
install(TARGETS api EXPORT second INCLUDES ARCHIVE DESTINATION lib)
install(TARGETS core EXPORT third INCLUDES DESTINATION include extra)
"#,
        0,
    )?;
    let mut cx = interp.cx();
    let model = cx.project();
    assert_eq!(model.installs.len(), 3);
    assert!(model.installs[0].options.includes_destination.is_empty());
    assert!(model.installs[1].options.includes_destination.is_empty());
    assert_eq!(
        model.installs[2].options.includes_destination,
        [BString::from("include"), BString::from("extra")]
    );
    Ok(())
}
