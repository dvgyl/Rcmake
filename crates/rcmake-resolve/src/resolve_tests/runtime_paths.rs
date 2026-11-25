//! Runtime paths in link and install recipes.

use super::*;
use rcmake_base::ByteSlice;
use rcmake_model::GeneratorKind;

fn fixture(system: &str) -> Result<(ProjectModel, PureHost), Box<dyn std::error::Error>> {
    let base = if cfg!(windows) { "C:/work" } else { "/work" };
    let source = format!("{base}/src");
    let build = format!("{base}/build");
    let mut model = project_with_roots(&source, &build);
    model.generator = Some(GeneratorKind::Ninja);
    model
        .global
        .props
        .set("CMAKE_SYSTEM_NAME".into(), system.into());
    let host = PureHost::new(Path::new(base).into(), Path::new(base).join("cmake"));
    for name in ["main.c", "lib.c", "wait.f90"] {
        host.seed_file(&Path::new(&source).join(name), b"")?;
    }
    Ok((model, host))
}

fn install(model: &mut ProjectModel, id: TargetId) {
    model.add_install(InstallRule {
        dir: DirId::ROOT,
        backtrace: Backtrace::default(),
        kind: InstallKind::Targets {
            targets: vec![id],
            export: None,
        },
        options: InstallOptions {
            destination: Some("lib".into()),
            component: Some("Runtime".into()),
            optional: true,
            exclude_from_all: true,
            ..Default::default()
        },
    });
}

fn recipe(result: &GenerationResult, target: &[u8]) -> Result<String, Box<dyn std::error::Error>> {
    let graph = result.graphs.first().ok_or("Missing graph")?;
    let target = graph
        .targets
        .iter()
        .find(|entry| entry.name == target)
        .ok_or("Missing target")?;
    let command = target
        .steps
        .iter()
        .find_map(|index| {
            if let Some(BuildStep::Link { invocations, .. }) = graph.steps.get(*index) {
                invocations.first()
            } else {
                None
            }
        })
        .ok_or("Missing link recipe")?;
    Ok(command.shell.to_string())
}

#[test]
fn elf_install_reserves_runtime_path_space_and_checks_the_installed_file() -> TestResult {
    for (build_path, install_path, expected_old) in [
        ("/short", "/longer/runtime/path", "/short::::::::::::::"),
        ("", "/lib", "::::"),
        ("/long", "", "/long:"),
        ("/same", "/same", "/same:"),
        ("", "", ""),
    ] {
        let (mut model, host) = fixture("Linux")?;
        for (name, value) in [
            ("CMAKE_EXECUTABLE_FORMAT", "ELF"),
            ("CMAKE_SHARED_LIBRARY_RUNTIME_C_FLAG_SEP", ":"),
            ("CMAKE_EXECUTABLE_RUNTIME_C_FLAG", "-Wl,-rpath,"),
            ("CMAKE_EXECUTABLE_RUNTIME_C_FLAG_SEP", ":"),
        ] {
            model.global.props.set(name.into(), value.into());
        }
        let mut app = target("app", TargetKind::Executable, &["main.c"]);
        app.props.set("BUILD_RPATH".into(), build_path.into());
        app.props.set("INSTALL_RPATH".into(), install_path.into());
        let id = model.add_target(app)?;
        install(&mut model, id);
        let result = resolve(&model, &host, &[])?;
        let link = recipe(&result, b"app")?;
        if expected_old.is_empty() {
            assert!(!link.contains("-rpath"), "{link}");
            assert_eq!(result.install.actions.len(), 1);
        } else {
            assert!(
                link.contains(&format!("-Wl,-rpath,{expected_old}")),
                "{link}"
            );
            let [check, copy, change] = result.install.actions.as_slice() else {
                return Err("Expected check, copy, and change actions.".into());
            };
            let (InstallActionKind::Code(check_code), InstallActionKind::Code(change_code)) =
                (&check.kind, &change.kind)
            else {
                return Err("Expected runtime path code.".into());
            };
            assert!(check_code.contains_str("file(RPATH_CHECK"));
            assert!(change_code.contains_str(format!("OLD_RPATH \"{expected_old}\"")));
            assert!(change_code.contains_str(format!("NEW_RPATH \"{install_path}\"")));
            assert!(change_code.contains_str("$ENV{DESTDIR}${CMAKE_INSTALL_PREFIX}/lib/app"));
            for action in [check, copy, change] {
                assert_eq!(action.component, "Runtime");
                assert!(action.optional && action.exclude_from_all);
            }
        }
    }
    Ok(())
}

#[test]
fn darwin_install_name_uses_each_prefix_context() -> TestResult {
    let (mut model, host) = fixture("Darwin")?;
    model
        .global
        .props
        .set("CMAKE_INSTALL_PREFIX".into(), "/configured".into());
    let mut lib = target("driver", TargetKind::SharedLibrary, &["lib.c"]);
    lib.props
        .set("BUILD_WITH_INSTALL_NAME_DIR".into(), "ON".into());
    lib.props.set(
        "INSTALL_NAME_DIR".into(),
        "$<$<CONFIG:Debug>:$<INSTALL_PREFIX>/sdk>".into(),
    );
    let id = model.add_target(lib)?;
    install(&mut model, id);
    let mut app = target("app", TargetKind::Executable, &["main.c"]);
    app.usage.link_libraries = Some(vec![link_item("driver")]);
    let app = model.add_target(app)?;
    install(&mut model, app);
    let result = resolve(&model, &host, &[ConfigId("Debug".into())])?;
    let link = recipe(&result, b"driver")?;
    assert!(link.contains("/configured/sdk/libdriver.dylib"), "{link}");
    assert!(result.install.actions.iter().any(|action| matches!(&action.kind,
        InstallActionKind::Code(code) if code.contains_str("-id \"${CMAKE_INSTALL_PREFIX}/sdk/libdriver.dylib\""))));
    assert!(result.install.actions.iter().any(|action| matches!(&action.kind,
        InstallActionKind::Code(code) if code.contains_str("-change /configured/sdk/libdriver.dylib \"${CMAKE_INSTALL_PREFIX}/sdk/libdriver.dylib\"")
            || code.contains_str("-change \"/configured/sdk/libdriver.dylib\" \"${CMAKE_INSTALL_PREFIX}/sdk/libdriver.dylib\""))));
    Ok(())
}

#[test]
fn runtime_paths_exclude_import_stubs_without_a_runtime_location() -> TestResult {
    let (mut model, host) = fixture("Darwin")?;
    let build = &model.dir(DirId::ROOT).binary_dir;
    let stub = format!("{build}/stub/libdriver.dylib");
    let mut imported = target("driver", TargetKind::SharedLibrary, &[]);
    imported.origin = TargetOrigin::Imported(ImportedVisibility::Local);
    imported
        .props
        .set("IMPORTED_IMPLIB".into(), stub.as_str().into());
    model.add_target(imported)?;
    let mut app = target("app", TargetKind::Executable, &["main.c"]);
    app.usage.link_libraries = Some(vec![link_item("driver")]);
    model.add_target(app)?;
    let result = resolve(&model, &host, &[])?;
    let link = recipe(&result, b"app")?;
    assert!(link.contains("stub/libdriver.dylib"), "{link}");
    assert!(!link.contains("-rpath"), "{link}");
    Ok(())
}

#[test]
fn build_only_dependencies_do_not_add_fortran_runtime_arguments() -> TestResult {
    for linked in [false, true] {
        let (mut model, host) = fixture("Linux")?;
        for (name, value) in [
            ("CMAKE_Fortran_COMPILER", "gfortran"),
            (
                "CMAKE_Fortran_PREPROCESS_SOURCE",
                "<CMAKE_Fortran_COMPILER> -E <SOURCE> -o <PREPROCESSED_SOURCE>",
            ),
            ("CMAKE_Fortran_IMPLICIT_LINK_LIBRARIES", "gfortran"),
            (
                "CMAKE_Fortran_IMPLICIT_LINK_DIRECTORIES",
                "/fortran/runtime",
            ),
        ] {
            model.global.props.set(name.into(), value.into());
        }
        model.add_target(target("wait", TargetKind::StaticLibrary, &["wait.f90"]))?;
        let mut app = target("app", TargetKind::Executable, &["main.c"]);
        app.props.set("LINKER_LANGUAGE".into(), "C".into());
        if linked {
            app.usage.link_libraries = Some(vec![link_item("wait")]);
        } else {
            app.utility_deps.push(entry("wait"));
        }
        let id = model.add_target(app)?;
        let result = resolve(&model, &host, &[])?;
        assert!(
            result
                .targets
                .iter()
                .any(|target| target.id == id && target.dependencies.len() == 1)
        );
        let link = recipe(&result, b"app")?;
        assert_eq!(link.contains("-lgfortran"), linked, "{link}");
        assert_eq!(link.contains("/fortran/runtime"), linked, "{link}");
    }
    Ok(())
}

#[test]
fn imported_runtime_installs_select_wrappers_and_the_mapped_soname() -> TestResult {
    for (system, kind, location, installed) in [
        (
            "Darwin",
            TargetKind::Executable,
            "/opt/Tool.app/Contents/MacOS/Tool",
            "/opt/Tool.app",
        ),
        (
            "Darwin",
            TargetKind::SharedLibrary,
            "/opt/Kit.framework/Versions/A/Kit",
            "/opt/Kit.framework",
        ),
        (
            "Darwin",
            TargetKind::ModuleLibrary,
            "/opt/Plugin.bundle/Contents/MacOS/Plugin",
            "/opt/Plugin.bundle",
        ),
        (
            "Linux",
            TargetKind::SharedLibrary,
            "/opt/libKit.so.1.2",
            "/opt/libKit.so.1",
        ),
    ] {
        let (mut model, host) = fixture(system)?;
        let mut imported = target("item", kind, &[]);
        imported.origin = TargetOrigin::Imported(ImportedVisibility::Local);
        for (key, value) in [
            ("MAP_IMPORTED_CONFIG_DEBUG", "RELEASE"),
            ("IMPORTED_LOCATION_RELEASE", location),
            ("IMPORTED_SONAME_RELEASE", "libKit.so.1"),
            ("IMPORTED_SONAME_DEBUG", "wrong.so"),
        ] {
            imported.props.set(key.into(), value.into());
        }
        model.add_target(imported)?;
        let index = model.add_install(InstallRule {
            dir: DirId::ROOT,
            kind: InstallKind::Files(vec![entry("$<TARGET_FILE:item>")]),
            backtrace: Backtrace::default(),
            options: InstallOptions {
                destination: Some("lib/$<CONFIG>".into()),
                configurations: vec!["Debug".into()],
                component: Some("Runtime".into()),
                optional: true,
                ..Default::default()
            },
        });
        for (arg, value) in ["IMPORTED_RUNTIME_ARTIFACTS", "item"]
            .into_iter()
            .enumerate()
        {
            model.global.props.set(
                format!("_RCMAKE_INSTALL_{index}_ARG{arg}").into(),
                value.into(),
            );
        }
        let result = resolve(
            &model,
            &host,
            &[ConfigId("Debug".into()), ConfigId("Release".into())],
        )?;
        let [action] = result.install.actions.as_slice() else {
            return Err("Expected one import action.".into());
        };
        match &action.kind {
            InstallActionKind::Directory { dirs, options } if system == "Darwin" => {
                assert_eq!(dirs, &[installed]);
                assert!(
                    options
                        .iter()
                        .any(|value| value == "USE_SOURCE_PERMISSIONS")
                );
            }
            InstallActionKind::Files {
                file_type, files, ..
            } if system == "Linux" => {
                assert_eq!(file_type, "SHARED_LIBRARY");
                assert_eq!(files, &[location, installed]);
            }
            kind => return Err(format!("Unexpected install action {kind:?}").into()),
        }
        assert_eq!(action.destination, "lib/Debug");
        assert_eq!(action.configurations, ["Debug"]);
        assert!(action.optional);
    }
    Ok(())
}

#[test]
fn elf_link_paths_respect_origin_and_the_install_path_switch() -> TestResult {
    for build_with_install in [false, true] {
        let (mut model, host) = fixture("Linux")?;
        for (key, value) in [
            ("CMAKE_EXECUTABLE_FORMAT", "ELF"),
            ("CMAKE_SHARED_LIBRARY_RUNTIME_C_FLAG_SEP", ":"),
            ("CMAKE_EXECUTABLE_RUNTIME_C_FLAG", "-Wl,-rpath,"),
            ("CMAKE_EXECUTABLE_RUNTIME_C_FLAG_SEP", ":"),
            (
                "CMAKE_C_USE_IMPLICIT_LINK_DIRECTORIES_IN_RUNTIME_PATH",
                "ON",
            ),
            ("CMAKE_C_IMPLICIT_LINK_DIRECTORIES", "/toolchain/runtime"),
            ("CMAKE_PLATFORM_REQUIRED_RUNTIME_PATH", "/platform"),
        ] {
            model.global.props.set(key.into(), value.into());
        }
        let mut lib = target("driver", TargetKind::SharedLibrary, &["lib.c"]);
        lib.props
            .set("LIBRARY_OUTPUT_DIRECTORY".into(), "lib".into());
        model.add_target(lib)?;
        let mut app = target("app", TargetKind::Executable, &["main.c"]);
        app.usage.link_libraries = Some(vec![link_item("driver")]);
        for (key, value) in [
            ("BUILD_RPATH", "/explicit"),
            ("BUILD_RPATH_USE_ORIGIN", "ON"),
            ("INSTALL_RPATH", "${ORIGIN}/installed"),
            ("INSTALL_REMOVE_ENVIRONMENT_RPATH", "ON"),
            (
                "BUILD_WITH_INSTALL_RPATH",
                if build_with_install { "ON" } else { "OFF" },
            ),
        ] {
            app.props.set(key.into(), value.into());
        }
        app.policies
            .policies
            .set(PolicyId::new(95), PolicyStatus::New)?;
        let id = model.add_target(app)?;
        install(&mut model, id);
        let output = resolve(&model, &host, &[])?;
        let link = recipe(&output, b"app")?;
        if build_with_install {
            assert!(link.contains("${ORIGIN}/installed:/platform"), "{link}");
            assert!(!link.contains("/toolchain/runtime"), "{link}");
            assert_eq!(output.install.actions.len(), 1);
        } else {
            assert!(
                link.contains("/explicit:$ORIGIN/lib:/toolchain/runtime:/platform:"),
                "{link}"
            );
            let code = output
                .install
                .actions
                .iter()
                .find_map(|action| match &action.kind {
                    InstallActionKind::Code(code) if code.contains_str("file(RPATH_CHANGE") => {
                        Some(code)
                    }
                    _ => None,
                })
                .ok_or("Missing install runtime path change.")?;
            assert!(
                code.contains_str("NEW_RPATH [[${ORIGIN}/installed:/platform]]"),
                "{code}"
            );
            assert!(
                code.contains_str("INSTALL_REMOVE_ENVIRONMENT_RPATH"),
                "{code}"
            );
        }
    }
    Ok(())
}

#[cfg(unix)]
#[test]
fn darwin_rpath_follows_the_install_name_of_an_import_through_a_static_library() -> TestResult {
    for (soname, expected) in [
        ("@rpath/libvendor.dylib", true),
        ("/sdk/libvendor.dylib", false),
    ] {
        let (mut model, host) = fixture("Darwin")?;
        model
            .global
            .props
            .set("CMAKE_PLATFORM_HAS_INSTALLNAME".into(), "ON".into());
        let mut vendor = target("vendor", TargetKind::SharedLibrary, &[]);
        vendor.origin = TargetOrigin::Imported(ImportedVisibility::Local);
        vendor.props.set(
            "IMPORTED_LOCATION".into(),
            "/sdk with space/libvendor.dylib".into(),
        );
        vendor.props.set("IMPORTED_SONAME".into(), soname.into());
        model.add_target(vendor)?;
        let mut mid = target("mid", TargetKind::StaticLibrary, &["lib.c"]);
        mid.usage.link_libraries = Some(vec![link_item("vendor")]);
        mid.usage.interface_link_libraries = Some(vec![link_item("$<LINK_ONLY:vendor>")]);
        model.add_target(mid)?;
        let mut app = target("app", TargetKind::Executable, &["main.c"]);
        app.usage.link_libraries = Some(vec![link_item("mid")]);
        model.add_target(app)?;
        let result = resolve(&model, &host, &[])?;
        let link = recipe(&result, b"app")?;
        assert_eq!(link.contains("-Wl,-rpath,"), expected, "{link}");
        if expected {
            assert!(link.contains("-Wl,-rpath,'/sdk with space'"), "{link}");
        }
    }
    Ok(())
}

#[test]
fn darwin_rpath_recognizes_the_linker_name_of_a_versioned_library() -> TestResult {
    let (mut model, host) = fixture("Darwin")?;
    model.generator = Some(GeneratorKind::UnixMakefiles);
    model
        .global
        .props
        .set("CMAKE_PLATFORM_HAS_INSTALLNAME".into(), "ON".into());
    let mut library = target("driver", TargetKind::SharedLibrary, &["lib.c"]);
    library.props.set("VERSION".into(), "1.2".into());
    library.props.set("SOVERSION".into(), "1".into());
    model.add_target(library)?;
    let mut app = target("app", TargetKind::Executable, &["main.c"]);
    app.usage.link_libraries = Some(vec![link_item("driver")]);
    model.add_target(app)?;
    let result = resolve(&model, &host, &[])?;
    let link = recipe(&result, b"app")?;
    assert!(
        link.contains("-Wl,-rpath,") && link.ends_with("/libdriver.dylib"),
        "{link}"
    );
    Ok(())
}

#[cfg(unix)]
#[test]
fn elf_runtime_paths_exclude_implicit_directories_and_include_private_shared_dependencies()
-> TestResult {
    let (mut model, host) = fixture("Linux")?;
    for (name, value) in [
        ("CMAKE_EXECUTABLE_FORMAT", "ELF"),
        ("CMAKE_SHARED_LIBRARY_RUNTIME_C_FLAG_SEP", ":"),
        ("CMAKE_C_IMPLICIT_LINK_DIRECTORIES", "/usr/lib/example/"),
        ("CMAKE_SHARED_LIBRARY_RPATH_LINK_C_FLAG", "-Wl,-rpath-link,"),
    ] {
        model.global.props.set(name.into(), value.into());
    }
    for (name, file) in [
        ("implicit", "/usr/lib/example/libimplicit.so"),
        ("indirect", "/opt/vendor/lib/libindirect.so.1"),
    ] {
        let mut library = target(name, TargetKind::SharedLibrary, &[]);
        library.origin = TargetOrigin::Imported(ImportedVisibility::Local);
        library.props.set("IMPORTED_LOCATION".into(), file.into());
        model.add_target(library)?;
    }
    let mut library = target("direct", TargetKind::SharedLibrary, &["lib.c"]);
    library.usage.link_libraries = Some(vec![link_item("indirect")]);
    model.add_target(library)?;
    let mut app = target("app", TargetKind::Executable, &["main.c"]);
    app.usage.link_libraries = Some(vec![link_item("direct"), link_item("implicit")]);
    app.props
        .set("INSTALL_RPATH_USE_LINK_PATH".into(), "ON".into());
    app.props.set("BUILD_RPATH_USE_ORIGIN".into(), "ON".into());
    let id = model.add_target(app)?;
    install(&mut model, id);
    let result = resolve(&model, &host, &[])?;
    let link = recipe(&result, b"app")?;
    assert!(link.contains("-Wl,-rpath-link,/opt/vendor/lib"), "{link}");
    assert!(link.contains("$ORIGIN:/opt/vendor/lib"), "{link}");
    assert!(!link.contains("-Wl,-rpath,/usr"), "{link}");
    assert!(result.install.actions.iter().any(|action| {
        matches!(&action.kind, InstallActionKind::Code(code)
            if code.contains_str("NEW_RPATH") && code.contains_str("/opt/vendor/lib")
                && !code.contains_str("/usr/lib/example"))
    }));
    Ok(())
}

#[test]
fn named_runtime_dependency_install_scans_between_the_surrounding_rules() -> TestResult {
    use rcmake_model::{RuntimeDependencyFilters, RuntimeDependencyInstall};

    let (mut model, host) = fixture("Linux")?;
    let app = model.add_target(target("app", TargetKind::Executable, &["main.c"]))?;
    install(&mut model, app);
    for (key, arg) in ["TARGETS", "app", "RUNTIME_DEPENDENCY_SET", "sdk"]
        .into_iter()
        .enumerate()
    {
        model
            .global
            .props
            .set(format!("_RCMAKE_INSTALL_0_ARG{key}").into(), arg.into());
    }
    model
        .runtime_dependency_installs
        .push(RuntimeDependencyInstall {
            dir: DirId::ROOT,
            position: 1,
            name: "sdk".into(),
            backtrace: Backtrace::default(),
            library: InstallOptions {
                destination: Some("vendor lib".into()),
                component: Some("Vendor".into()),
                ..Default::default()
            },
            framework: None,
            filters: RuntimeDependencyFilters {
                directories: vec!["/vendor path".into()],
                post_exclude_regexes: vec!["^/usr/".into()],
                ..Default::default()
            },
        });
    model.add_install(InstallRule {
        dir: DirId::ROOT,
        backtrace: Backtrace::default(),
        kind: InstallKind::Code("message(STATUS after)".into()),
        options: InstallOptions::default(),
    });
    let result = resolve(&model, &host, &[])?;
    let [app, scan, copy, after] = result.install.actions.as_slice() else {
        return Err("Expected four install actions.".into());
    };
    assert!(matches!(app.kind, InstallActionKind::Files { .. }));
    let (InstallActionKind::Code(scan_code), InstallActionKind::Code(copy_code)) =
        (&scan.kind, &copy.kind)
    else {
        return Err("Expected runtime dependency code.".into());
    };
    assert_eq!(scan.component, "Vendor");
    assert_eq!(copy.component, "Vendor");
    assert!(scan_code.contains_str("file(GET_RUNTIME_DEPENDENCIES"));
    assert!(scan_code.contains_str("EXECUTABLES\n"));
    assert!(scan_code.contains_str("/build/app"));
    assert!(scan_code.contains_str("DIRECTORIES\n    \"/vendor path\""));
    assert!(scan_code.contains_str("POST_EXCLUDE_REGEXES\n    \"^/usr/\""));
    assert!(copy_code.contains_str("${CMAKE_INSTALL_PREFIX}/vendor lib"));
    assert!(copy_code.contains_str("FOLLOW_SYMLINK_CHAIN"));
    assert_eq!(
        after.kind,
        InstallActionKind::Code("message(STATUS after)".into())
    );
    Ok(())
}

#[cfg(unix)]
#[test]
fn elf_links_private_import_stubs_without_adding_them_to_the_runtime_path() -> TestResult {
    let (mut model, host) = fixture("Linux")?;
    for (name, value) in [
        ("CMAKE_EXECUTABLE_FORMAT", "ELF"),
        ("CMAKE_C_COMPILER_LINKER_ID", "GNU"),
        ("CMAKE_SHARED_LIBRARY_RPATH_LINK_C_FLAG", "-Wl,-rpath-link,"),
    ] {
        model.global.props.set(name.into(), value.into());
    }
    let mut stub = target("stub", TargetKind::SharedLibrary, &[]);
    stub.origin = TargetOrigin::Imported(ImportedVisibility::Local);
    stub.props.set(
        "IMPORTED_IMPLIB".into(),
        "/opt/vendor/stubs/libstub.so".into(),
    );
    model.add_target(stub)?;
    let mut library = target("direct", TargetKind::SharedLibrary, &["lib.c"]);
    library.usage.link_libraries = Some(vec![link_item("stub")]);
    model.add_target(library)?;
    let mut app = target("app", TargetKind::Executable, &["main.c"]);
    app.usage.link_libraries = Some(vec![link_item("direct")]);
    model.add_target(app)?;
    let result = resolve(&model, &host, &[])?;
    let link = recipe(&result, b"app")?;
    assert!(link.contains(" /opt/vendor/stubs/libstub.so"), "{link}");
    assert!(!link.contains("-Wl,-rpath,/opt/vendor/stubs"), "{link}");
    Ok(())
}

#[cfg(unix)]
#[test]
fn runtime_path_starts_with_non_implicit_user_link_directories() -> TestResult {
    for (system, separator, expected) in [
        ("Linux", ":", "-Wl,-rpath,/first:/second"),
        ("Darwin", "", "-Wl,-rpath,/first -Wl,-rpath,/second"),
    ] {
        let (mut model, host) = fixture(system)?;
        for (key, value) in [
            ("CMAKE_EXECUTABLE_RUNTIME_C_FLAG", "-Wl,-rpath,"),
            ("CMAKE_EXECUTABLE_RUNTIME_C_FLAG_SEP", separator),
            ("CMAKE_C_IMPLICIT_LINK_DIRECTORIES", "/system"),
            ("CMAKE_C_STANDARD_LINK_DIRECTORIES", "/standard"),
        ] {
            model.global.props.set(key.into(), value.into());
        }
        let mut app = target("app", TargetKind::Executable, &["main.c"]);
        app.usage.push(
            UsageField::LinkDirectories,
            entry("/first;/system;$<1:/second>"),
        );
        model.add_target(app)?;
        let result = resolve(&model, &host, &[])?;
        let link = recipe(&result, b"app")?;
        assert!(link.contains(expected), "{link}");
        assert!(!link.contains("-rpath,/system"), "{link}");
        assert!(!link.contains("-rpath,/standard"), "{link}");
    }
    Ok(())
}

#[cfg(unix)]
#[test]
fn private_raw_shared_library_does_not_add_consumer_runtime_paths() -> TestResult {
    let (mut model, host) = fixture("Linux")?;
    model.global.props.set(
        "CMAKE_SHARED_LIBRARY_RPATH_LINK_C_FLAG".into(),
        "-Wl,-rpath-link,".into(),
    );
    let mut library = target("driver", TargetKind::SharedLibrary, &["lib.c"]);
    let mut private = link_item("/opt/x/lib/libz.so");
    private.scope = Some(rcmake_model::UsageScope::Private);
    library.usage.link_libraries = Some(vec![private]);
    model.add_target(library)?;
    let mut app = target("app", TargetKind::Executable, &["main.c"]);
    app.usage.link_libraries = Some(vec![link_item("driver")]);
    model.add_target(app)?;
    let result = resolve(&model, &host, &[])?;
    let driver = recipe(&result, b"driver")?;
    let app = recipe(&result, b"app")?;
    assert!(driver.contains("-rpath,/opt/x/lib"), "{driver}");
    assert!(!app.contains("/opt/x/lib"), "{app}");
    Ok(())
}
