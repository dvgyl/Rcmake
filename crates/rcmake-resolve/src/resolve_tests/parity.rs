//! Generation diagnostics that follow the CMake 4.5.1 RunCMake suites.

use super::*;
use rcmake_host::Stream;

fn fixture() -> Result<(ProjectModel, PureHost), Box<dyn std::error::Error>> {
    let root = if cfg!(windows) { "C:/work" } else { "/work" };
    let host = PureHost::new(Path::new(root).to_path_buf(), Path::new(root).join("cmake"));
    let source = format!("{root}/src");
    host.seed_file(Path::new(&format!("{source}/main.c")), b"")?;
    let model = project_with_roots(&source, &format!("{root}/build"));
    Ok((model, host))
}

fn stderr(host: &PureHost) -> String {
    String::from_utf8_lossy(&host.stream_bytes(Stream::Stderr)).into_owned()
}

#[test]
fn own_relative_include_directory_names_the_consumer() -> TestResult {
    let (mut model, host) = fixture()?;
    let mut app = target("app", TargetKind::Executable, &["main.c"]);
    app.usage
        .push(UsageField::IncludeDirectories, entry("relative"));
    model.add_target(app)?;
    assert!(matches!(
        resolve(&model, &host, &[]),
        Err(ResolveError::Reported(_))
    ));
    let text = stderr(&host);
    assert!(
        text.contains("Found relative path while evaluating include directories of \"app\":"),
        "{text}"
    );
    assert!(text.contains("\"relative\""), "{text}");
    Ok(())
}

#[test]
fn evaluation_errors_of_each_target_are_reported() -> TestResult {
    let (mut model, host) = fixture()?;
    for name in ["first", "second"] {
        let mut lib = target(name, TargetKind::StaticLibrary, &["main.c"]);
        lib.usage.push(
            UsageField::IncludeDirectories,
            entry(&format!(
                "$<TARGET_PROPERTY:{name}_missing/x,INCLUDE_DIRECTORIES>"
            )),
        );
        model.add_target(lib)?;
    }
    assert!(matches!(
        resolve(&model, &host, &[]),
        Err(ResolveError::Reported(_))
    ));
    let text = stderr(&host);
    assert!(text.contains("first_missing/x"), "{text}");
    assert!(text.contains("second_missing/x"), "{text}");
    Ok(())
}

#[test]
fn invalid_language_standard_is_an_error_at_the_target() -> TestResult {
    let (mut model, host) = fixture()?;
    let mut app = target("app", TargetKind::Executable, &["main.c"]);
    app.props.set("C_STANDARD".into(), "bad".into());
    model.add_target(app)?;
    let error = resolve(&model, &host, &[]).expect_err("The standard is invalid.");
    let text = error.to_string();
    assert!(text.starts_with("CMake Error at "), "{text}");
    assert!(
        text.contains("C_STANDARD is set to invalid value 'bad'"),
        "{text}"
    );
    Ok(())
}

#[test]
fn relative_link_directory_follows_cmp0081() -> TestResult {
    for status in [PolicyStatus::New, PolicyStatus::Warn, PolicyStatus::Old] {
        let (mut model, host) = fixture()?;
        let mut app = target("app", TargetKind::Executable, &["main.c"]);
        app.usage.push(UsageField::LinkDirectories, entry("../lib"));
        app.policies.policies.set(PolicyId::new(81), status)?;
        model.add_target(app)?;
        let result = resolve(&model, &host, &[]);
        assert_eq!(result.is_err(), status == PolicyStatus::New);
        let text = stderr(&host);
        let found = "Found relative path while evaluating link directories of \"app\":";
        assert_eq!(text.contains(found), status != PolicyStatus::Old, "{text}");
        assert_eq!(
            text.contains("Policy CMP0081 is not set"),
            status == PolicyStatus::Warn,
            "{text}"
        );
    }
    Ok(())
}

#[test]
fn missing_link_targets_are_reported_in_target_order() -> TestResult {
    let (mut model, host) = fixture()?;
    let mut iface = target("iface", TargetKind::InterfaceLibrary, &[]);
    iface.usage.interface_link_libraries = Some(vec![link_item("config::iface")]);
    model.add_target(iface)?;
    let mut lib = target("impl", TargetKind::StaticLibrary, &["main.c"]);
    lib.usage.link_libraries = Some(vec![link_item("config::impl"), link_item("iface")]);
    model.add_target(lib)?;
    assert!(matches!(
        resolve(&model, &host, &[]),
        Err(ResolveError::Reported(_))
    ));
    let text = stderr(&host);
    let interface = text
        .find("The link interface of target \"iface\" contains:")
        .ok_or("missing interface error")?;
    let implementation = text
        .find("Target \"impl\" links to:")
        .ok_or("missing link error")?;
    assert!(interface < implementation, "{text}");
    assert!(
        text.contains("  * An ALIAS target is missing.\n\n"),
        "{text}"
    );
    Ok(())
}

#[test]
fn old_cmp0108_drops_a_self_link_through_an_alias() -> TestResult {
    let (mut model, host) = fixture()?;
    let mut lib = target("foo", TargetKind::SharedLibrary, &["main.c"]);
    lib.policies
        .policies
        .set(PolicyId::new(108), PolicyStatus::Old)?;
    lib.usage.link_libraries = Some(vec![link_item("Bar::foo")]);
    let id = model.add_target(lib)?;
    model.add_alias(DirId::ROOT, "Bar::foo".into(), id)?;
    resolve(&model, &host, &[])?;
    Ok(())
}

#[test]
fn framework_feature_names_a_framework_target_by_bundle() -> TestResult {
    let (mut model, host) = fixture()?;
    model.generator = Some(rcmake_model::GeneratorKind::Ninja);
    for (name, value) in [
        (
            "CMAKE_C_LINK_LIBRARY_USING_FRAMEWORK",
            "-framework <LIBRARY>",
        ),
        ("CMAKE_C_LINK_LIBRARY_USING_FRAMEWORK_SUPPORTED", "TRUE"),
        ("CMAKE_SHARED_LIBRARY_RUNTIME_C_FLAG", "-Wl,-rpath,"),
        ("CMAKE_SKIP_BUILD_RPATH", "OFF"),
    ] {
        model.global.props.set(name.into(), value.into());
    }
    let mut framework = target("Kit", TargetKind::SharedLibrary, &["main.c"]);
    framework.props.set("FRAMEWORK".into(), "TRUE".into());
    model.add_target(framework)?;
    let mut app = target("app", TargetKind::Executable, &["main.c"]);
    app.usage.link_libraries = Some(vec![link_item("$<LINK_LIBRARY:FRAMEWORK,Kit>")]);
    model.add_target(app)?;
    let result = resolve(&model, &host, &[])?;
    let BuildStep::Link {
        invocations,
        inputs,
        ..
    } = link_build_step(&result.graphs[0], "app")?
    else {
        return Err("Expected a link command.".into());
    };
    let argv: Vec<_> = invocations
        .iter()
        .flat_map(|invocation| invocation.argv.iter())
        .collect();
    let at = argv
        .iter()
        .position(|arg| *arg == "-framework")
        .ok_or("missing -framework")?;
    assert_eq!(
        argv.get(at + 1).map(|arg| arg.as_slice()),
        Some(b"Kit".as_slice())
    );
    // The link step still depends on the framework binary.
    assert!(
        inputs
            .iter()
            .any(|input| input.ends_with(b"Kit.framework/Versions/A/Kit")),
        "{inputs:?}"
    );
    Ok(())
}

#[test]
fn file_set_membership_follows_cmp0211_at_generation() -> TestResult {
    for status in [PolicyStatus::New, PolicyStatus::Warn, PolicyStatus::Old] {
        let (mut model, host) = fixture()?;
        let mut lib = target("lib", TargetKind::StaticLibrary, &["main.c"]);
        lib.policies.policies.set(PolicyId::new(211), status)?;
        let id = model.add_target(lib)?;
        for name in ["s1", "s2"] {
            model.add_file_set(rcmake_model::FileSet {
                target: id,
                name: name.into(),
                kind: rcmake_model::FileSetKind::Sources,
                scope: rcmake_model::UsageScope::Private,
                base_dirs: Vec::new(),
                files: vec![entry("main.c")],
                props: Default::default(),
            })?;
        }
        let result = resolve(&model, &host, &[]);
        assert_eq!(result.is_err(), status == PolicyStatus::New);
        let text = stderr(&host);
        // The file set and the SOURCES entry both conflict with s1.
        assert_eq!(
            text.matches("already belongs to file set \"s1\".").count(),
            if status == PolicyStatus::Old { 0 } else { 2 },
            "{text}"
        );
        assert_eq!(
            text.contains("Policy CMP0211 is not set"),
            status == PolicyStatus::Warn
        );
    }
    Ok(())
}

#[test]
fn cmp0128_optional_warning_names_the_old_behavior() -> TestResult {
    for (standard, extensions, expected) in [
        (Some("17"), None, "unnecessary flags for"),
        (None, Some("OFF"), "be disabled."),
    ] {
        let (mut model, host) = fixture()?;
        for (name, value) in [
            ("CMAKE_C_STANDARD_DEFAULT", "17"),
            ("CMAKE_C_EXTENSIONS_DEFAULT", "ON"),
            ("CMAKE_POLICY_WARNING_CMP0128", "ON"),
        ] {
            model.global.props.set(name.into(), value.into());
        }
        let mut lib = target("lib", TargetKind::StaticLibrary, &["main.c"]);
        // CMake also warns when the project sets the policy to OLD.
        lib.policies
            .policies
            .set(PolicyId::new(128), PolicyStatus::Old)?;
        if let Some(standard) = standard {
            lib.props.set("C_STANDARD".into(), standard.into());
        }
        if let Some(extensions) = extensions {
            lib.props.set("C_EXTENSIONS".into(), extensions.into());
        }
        model.add_target(lib)?;
        resolve(&model, &host, &[])?;
        let text = stderr(&host);
        assert!(
            text.contains("Policy CMP0128 is not set: Selection of language standard"),
            "{text}"
        );
        assert!(text.contains(expected), "{text}");
    }
    Ok(())
}
