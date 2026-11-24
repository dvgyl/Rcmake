use std::path::Path;
use std::sync::Arc;

use rcmake_eval::{Interp, InterpConfig, RunMode};
use rcmake_host::{PureHost, Stream};

use super::*;

type TestResult = std::result::Result<(), Box<dyn std::error::Error>>;

fn configure(
    code: &str,
    expected: i32,
) -> std::result::Result<(Arc<PureHost>, Interp), Box<dyn std::error::Error>> {
    let host = Arc::new(PureHost::default());
    let mut interp = Interp::new(
        InterpConfig {
            host: host.clone(),
            mode: RunMode::Configure,
            cmake_root: "/cmake".into(),
            cmake_command: "/cmake/bin/cmake".into(),
            cwd: "/work".into(),
            limits: Default::default(),
            diag: Default::default(),
            defines: Vec::new(),
            script_args: Vec::new(),
        },
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
    let source = Path::new("/work/src");
    host.seed_file(&source.join("CMakeLists.txt"), code.as_bytes())?;
    let version =
        rcmake_base::version::Version::parse_strict(b"4.5").ok_or("invalid test version")?;
    interp.cx().set_cmake_minimum_required(b"4.5", &version)?;
    assert_eq!(
        interp
            .run_configure(source, Path::new("/work/build"), &[])
            .exit_code,
        expected,
        "{}",
        host.stream_bytes(Stream::Stderr)
    );
    Ok((host, interp))
}

#[test]
fn install_records_rules_and_export_membership() -> TestResult {
    let (_, mut interp) = configure(
        r#"
set(CMAKE_INSTALL_INCLUDEDIR headers)
install(TARGETS core EXPORT sdk DESTINATION lib COMPONENT Run
        ARCHIVE DESTINATION archives NAMELINK_SKIP FILE_SET HEADERS DESTINATION headers
        INCLUDES DESTINATION ./include include2 /opt/headers)
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
"#,
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
        ["./include", "include2", "/opt/headers"]
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
        (2, "./lib/../$<CONFIG>/"),
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
fn install_and_export_reject_invalid_argument_sets() -> TestResult {
    for (code, expected) in [
        (
            "install(FILES a b DESTINATION bin RENAME c)",
            "more than one file",
        ),
        ("install(TARGETS absent EXPORT set)", "does not exist"),
        (
            "install(TARGETS core RUNTIME NAMELINK_SKIP)",
            "outside LIBRARY or ARCHIVE",
        ),
        (
            "install(DIRECTORY data DESTINATION d TYPE DATA)",
            "mutually exclusive",
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
    let (host, _) = configure(
        r#"
cmake_diagnostic(SET CMD_INSTALL_ABSOLUTE_DESTINATION WARN)
install(TARGETS core ARCHIVE DESTINATION /absolute/archives)
install(FILES one DESTINATION relative)
"#,
        0,
    )?;
    let stderr = host.stream_bytes(Stream::Stderr).to_string();
    assert!(stderr.contains("/absolute/archives"), "{stderr}");
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
