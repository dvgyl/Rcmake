use super::*;
use rcmake_host::Stream;

#[test]
fn object_name_context_error_uses_one_paragraph() {
    let mut model = project();
    model
        .dir_mut(DirId::ROOT)
        .source_props
        .entry("/src/objlib_lib.c".into())
        .or_default()
        .set("OBJECT_NAME".into(), "$<CONFIG>/objlib_lib.c".into());
    let error = crate::sources::custom_object_name(&model, DirId::ROOT, b"/src/objlib_lib.c")
        .expect_err("OBJECT_NAME must be independent of the configuration.");
    let formatted = rcmake_diag::format_paragraphs(error.to_string().as_bytes(), 2, 77);
    assert!(
        formatted
            .windows(b"is not supported\n  ($<CONFIG>/objlib_lib.c).".len())
            .any(|part| part == b"is not supported\n  ($<CONFIG>/objlib_lib.c)."),
        "{formatted}"
    );
}

#[test]
fn relative_generated_source_base_follows_cmp0070() -> TestResult {
    for (policy, source) in [
        ("Old", "/src/generated.c"),
        ("Warn", "/src/generated.c"),
        ("New", "/out/generated.c"),
    ] {
        let mut model = project();
        for (key, value) in [
            ("_RCMAKE_FILE_GENERATE_COUNT", "1"),
            ("_RCMAKE_FILE_GENERATE_0_DIRECTORY", "0"),
            ("_RCMAKE_FILE_GENERATE_0_ARG1", "generated.c"),
            ("_RCMAKE_FILE_GENERATE_0_CMP0070", policy),
        ] {
            model.global.props.set(key.into(), value.into());
        }
        model.add_target(target("app", TargetKind::Executable, &[source]))?;
        let host = PureHost::new("/elsewhere".into(), "/tools/cmake".into());
        let result = resolve(&model, &host, &[])?;
        assert_eq!(result.targets[0].compile_units[0].source.value, source);
    }
    Ok(())
}

#[test]
fn missing_source_discards_only_its_own_entry() -> TestResult {
    for (line, no_sources) in [(8, true), (9, false)] {
        let mut model = project();
        let mut app = target("app", TargetKind::Executable, &["main.c"]);
        app.sources
            .push(BT::new("missing.c".into(), backtrace(line)));
        model.add_target(app)?;
        let host = PureHost::default();
        host.seed_file(Path::new("/src/main.c"), b"")?;
        assert!(resolve_with_options(&model, &host, &[], &ResolveOptions::default()).is_err());
        let stderr = host.stream_bytes(Stream::Stderr).to_string();
        assert!(stderr.contains("Cannot find source file"), "{stderr}");
        assert_eq!(
            stderr.contains("No SOURCES given to target: app"),
            no_sources,
            "{stderr}"
        );
    }
    Ok(())
}

#[test]
fn standard_link_directories_follow_target_directories() -> TestResult {
    let mut model = project();
    model.cache.insert(
        "CMAKE_C_STANDARD_LINK_DIRECTORIES".into(),
        CacheEntry {
            value: "/standard;/another".into(),
            ty: CacheType::String,
            help: BString::default(),
            advanced: false,
            props: Default::default(),
        },
    );
    model
        .global
        .props
        .set("CMAKE_LIBRARY_PATH_TERMINATOR".into(), "@".into());
    for (name, kind) in [
        ("app", TargetKind::Executable),
        ("archive", TargetKind::StaticLibrary),
    ] {
        let mut raw = target(name, kind, &["main.c"]);
        raw.usage
            .push(UsageField::LinkDirectories, entry("/target"));
        model.add_target(raw)?;
    }
    let host = PureHost::default();
    host.seed_file(Path::new("/src/main.c"), b"")?;
    let result = resolve(&model, &host, &[])?;
    let link_paths = |name| {
        result
            .targets
            .iter()
            .filter(|target| target.metadata.name == name)
            .filter_map(|target| target.link.as_ref())
            .flat_map(|link| &link.fragments)
            .filter(|fragment| fragment.role == LinkFragmentRole::LibraryPath)
            .map(|fragment| fragment.text.value.clone())
            .collect::<Vec<_>>()
    };
    assert_eq!(
        link_paths("app"),
        ["-L/target", "-L/standard@", "-L/another@"]
    );
    assert_eq!(link_paths("archive"), ["-L/target"]);
    Ok(())
}

#[test]
fn runtime_install_reaches_shared_dependencies_through_private_static_links() -> TestResult {
    let mut model = project();
    model
        .global
        .props
        .set("CMAKE_SYSTEM_NAME".into(), "Linux".into());
    let leaf = target("leaf", TargetKind::SharedLibrary, &["leaf.c"]);
    let leaf = model.add_target(leaf)?;
    let mut shared = target("shared", TargetKind::SharedLibrary, &["shared.c"]);
    for (key, value) in [
        ("VERSION", "2.1"),
        ("SOVERSION", "2"),
        ("PUBLIC_HEADER", "shared.h"),
        ("BUILD_RPATH", "/old"),
        ("INSTALL_RPATH", "/long/new/rpath"),
    ] {
        shared.props.set(key.into(), value.into());
    }
    shared.usage.link_libraries = Some(vec![link_item("leaf")]);
    let shared = model.add_target(shared)?;
    let mut bridge = target("bridge", TargetKind::StaticLibrary, &["bridge.c"]);
    bridge.usage.link_libraries = Some(vec![link_item("shared")]);
    bridge.usage.interface_link_libraries = Some(vec![link_item("$<LINK_ONLY:shared>")]);
    let bridge = model.add_target(bridge)?;
    let mut app = target("app", TargetKind::Executable, &["main.c"]);
    app.usage.link_libraries = Some(vec![link_item("$<$<CONFIG:Debug>:bridge>")]);
    let app = model.add_target(app)?;
    let rule = model.add_install(InstallRule {
        dir: DirId::ROOT,
        backtrace: backtrace(14),
        kind: InstallKind::Targets {
            targets: vec![app],
            export: None,
        },
        options: InstallOptions::default(),
    });
    for (index, value) in [
        "TARGETS",
        "app",
        "RUNTIME_DEPENDENCY_TARGETS",
        "LIBRARY",
        "DESTINATION",
        "runtime-libs",
        "COMPONENT",
        "Runtime",
        "NAMELINK_COMPONENT",
        "Development",
    ]
    .into_iter()
    .enumerate()
    {
        model.global.props.set(
            format!("_RCMAKE_INSTALL_{rule}_ARG{index}").into(),
            value.into(),
        );
    }
    let host = PureHost::default();
    for file in ["main.c", "bridge.c", "shared.c", "leaf.c"] {
        host.seed_file(&Path::new("/src").join(file), b"")?;
    }
    let result = resolve(
        &model,
        &host,
        &[ConfigId("Debug".into()), ConfigId("Release".into())],
    )?;
    let members: Vec<_> = result
        .install
        .target_installers
        .iter()
        .filter(|installer| installer.destination == "runtime-libs")
        .collect();
    assert_eq!(
        members
            .iter()
            .map(|installer| installer.target)
            .collect::<Vec<_>>(),
        [shared, leaf]
    );
    for member in members {
        assert!(member.config.matches(b"Debug"));
        assert_eq!(member.component, "Runtime");
        assert!(!member.import_library);
        assert_ne!(member.namelink, NamelinkMode::Only);
        assert_ne!(member.target, bridge);
    }
    assert!(result.install.actions.iter().all(|action| {
        match &action.kind {
            InstallActionKind::Files { files, .. } => files
                .iter()
                .all(|file| !file.ends_with(b"/shared.h") && !file.ends_with(b"/libshared.so")),
            _ => true,
        }
    }));
    Ok(())
}

#[test]
fn install_include_destination_error_names_target_and_directory() -> TestResult {
    let mut model = project();
    let id = model.add_target(target("api", TargetKind::InterfaceLibrary, &[]))?;
    let rule = model.add_install(InstallRule {
        dir: DirId::ROOT,
        backtrace: backtrace(6),
        kind: InstallKind::Targets {
            targets: vec![id],
            export: Some("api".into()),
        },
        options: InstallOptions {
            includes_destination: vec!["include/$<CONFIG>".into()],
            ..InstallOptions::default()
        },
    });
    model.exports.insert(
        "api".into(),
        ExportSet {
            name: "api".into(),
            targets: vec![id],
            install_rules: vec![rule],
        },
    );
    model.add_install(InstallRule {
        dir: DirId::ROOT,
        backtrace: backtrace(7),
        kind: InstallKind::Export {
            set: "api".into(),
            namespace: "".into(),
            file: None,
            cxx_modules_directory: None,
        },
        options: InstallOptions {
            destination: Some("lib/cmake".into()),
            ..InstallOptions::default()
        },
    });
    let error =
        resolve(&model, &PureHost::default(), &[]).expect_err("Context-sensitive destination");
    let message = error.to_string();
    for text in [
        "CMake Error in CMakeLists.txt:",
        "Target \"api\" is installed with INCLUDES DESTINATION",
        "target_include_directories instead.",
    ] {
        assert!(message.contains(text), "{message}");
    }
    Ok(())
}

#[test]
fn executable_link_flags_include_export_flags_in_cmake_order() -> TestResult {
    let mut model = project();
    for (name, value) in [
        ("CMAKE_SYSTEM_NAME", "Linux"),
        ("CMAKE_EXE_EXPORTS_C_FLAG", "-export"),
        ("CMAKE_SHARED_BUILD_C_FLAGS", "-shared-build"),
        ("CMAKE_SHARED_LIBRARY_LINK_C_FLAGS", "-shared-link"),
        ("BUILD_SHARED_LIBS", "ON"),
        ("TARGET_SUPPORTS_SHARED_LIBS", "TRUE"),
    ] {
        model.global.props.set(name.into(), value.into());
    }
    for (name, exports) in [("app", "ON"), ("other", "OFF")] {
        let mut raw = target(name, TargetKind::Executable, &["main.c"]);
        raw.props.set("ENABLE_EXPORTS".into(), exports.into());
        raw.props.set("LINK_FLAGS".into(), "-user-flag".into());
        model.add_target(raw)?;
    }
    let host = PureHost::default();
    host.seed_file(Path::new("/src/main.c"), b"")?;
    let result = resolve(&model, &host, &[])?;
    for (name, flags) in [
        (
            "app",
            b"-export -shared-build -shared-link -user-flag".as_slice(),
        ),
        ("other", b"-shared-build -user-flag"),
    ] {
        let BuildStep::Link { invocations, .. } = link_build_step(&result.graphs[0], name)? else {
            return Err("Missing link step".into());
        };
        assert!(
            invocations[0]
                .shell
                .windows(flags.len())
                .any(|part| part == flags),
            "{invocations:?}"
        );
    }
    Ok(())
}
