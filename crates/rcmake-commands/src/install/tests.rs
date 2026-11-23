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
        matches!(&export.kind, InstallKind::Export { set, namespace, file: Some(file) } if set == b"sdk" && namespace == b"sdk::" && file == b"pkg.cmake")
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
