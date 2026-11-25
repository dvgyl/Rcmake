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
                code.contains_str(r#"NEW_RPATH "\${ORIGIN}/installed:/platform""#),
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
