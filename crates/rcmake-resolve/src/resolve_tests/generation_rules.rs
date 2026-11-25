//! CMake generation rules for definitions, `EXCLUDE_FROM_ALL`, link directories,
//! imported stub libraries, custom command outputs, and imported include paths.

use super::*;

fn roots() -> (String, String) {
    let root = if cfg!(windows) { "C:/work" } else { "/work" };
    (format!("{root}/src"), format!("{root}/out"))
}

fn seeded(source: &str, files: &[&str]) -> Result<PureHost, Box<dyn std::error::Error>> {
    let host = PureHost::default();
    for file in files {
        host.seed_file(Path::new(&format!("{source}/{file}")), b"")?;
    }
    Ok(host)
}

#[test]
fn strips_a_leading_define_flag_after_evaluation() -> TestResult {
    let (source, build) = roots();
    let mut model = project_with_roots(&source, &build);
    let mut lib = target("lib", TargetKind::StaticLibrary, &["a.c"]);
    for value in ["$<1:-DGENEX>", "PLAIN", "-DPLAIN"] {
        lib.usage.push(UsageField::CompileDefinitions, entry(value));
    }
    model
        .dir_mut(DirId::ROOT)
        .source_props
        .entry(format!("{source}/a.c").into())
        .or_default()
        .set("COMPILE_DEFINITIONS".into(), "$<1:-DFROM_SOURCE>".into());
    model.add_target(lib)?;
    let result = resolve(&model, &seeded(&source, &["a.c"])?, &[])?;
    let defines: Vec<_> = result.targets[0].compile_groups[0]
        .defines
        .iter()
        .map(|define| define.value.clone())
        .collect();
    assert_eq!(defines, ["GENEX", "PLAIN", "FROM_SOURCE"]);
    Ok(())
}

#[test]
fn evaluates_exclude_from_all_for_each_configuration() -> TestResult {
    let (source, build) = roots();
    let mut model = project_with_roots(&source, &build);
    let mut tool = target("tool", TargetKind::Executable, &["main.c"]);
    tool.props
        .set("EXCLUDE_FROM_ALL".into(), "$<NOT:$<CONFIG:Release>>".into());
    model.add_target(tool)?;
    let configs = [ConfigId("Release".into()), ConfigId("Debug".into())];
    let result = resolve(&model, &seeded(&source, &["main.c"])?, &configs)?;
    let excluded = |config: &str| {
        result
            .targets
            .iter()
            .find(|target| target.config.0 == config)
            .map(|target| target.metadata.exclude_from_all)
    };
    assert_eq!(excluded("Release"), Some(false));
    assert_eq!(excluded("Debug"), Some(true));
    Ok(())
}

/// Return the library path fragments of the link step of `name`.
fn library_paths(result: &GenerationResult, name: &str) -> Vec<BString> {
    result
        .targets
        .iter()
        .filter(|target| target.metadata.name == name)
        .filter_map(|target| target.link.as_ref())
        .flat_map(|link| &link.fragments)
        .filter(|fragment| fragment.role == LinkFragmentRole::LibraryPath)
        .map(|fragment| fragment.text.value.clone())
        .collect()
}

#[test]
fn cmp0099_controls_link_directories_of_private_static_dependencies() -> TestResult {
    for status in [PolicyStatus::Old, PolicyStatus::New] {
        let (source, build) = roots();
        let mut model = project_with_roots(&source, &build);
        let mut iface = target("iface", TargetKind::InterfaceLibrary, &[]);
        iface.usage.push(
            UsageField::InterfaceLinkDirectories,
            entry("/dir_interface"),
        );
        model.add_target(iface)?;
        let mut lib = target("lib", TargetKind::StaticLibrary, &["lib.c"]);
        lib.usage.link_libraries = Some(vec![link_item("iface")]);
        lib.usage.interface_link_libraries = Some(vec![link_item("$<LINK_ONLY:iface>")]);
        model.add_target(lib)?;
        let mut exe = target("exe", TargetKind::Executable, &["exe.c"]);
        exe.usage.link_libraries = Some(vec![link_item("lib")]);
        exe.policies.policies.set(PolicyId::new(99), status)?;
        model.add_target(exe)?;
        let result = resolve(&model, &seeded(&source, &["lib.c", "exe.c"])?, &[])?;
        let found = library_paths(&result, "exe")
            .iter()
            .any(|path| path.windows(13).any(|part| part == b"dir_interface"));
        assert_eq!(found, status == PolicyStatus::New, "{status:?}");
    }
    Ok(())
}

#[test]
fn links_an_imported_stub_library_from_imported_implib() -> TestResult {
    let (source, build) = roots();
    let mut model = project_with_roots(&source, &build);
    let mut stub = target("stub", TargetKind::SharedLibrary, &[]);
    stub.origin = TargetOrigin::Imported(ImportedVisibility::Local);
    stub.props
        .set("IMPORTED_IMPLIB".into(), "/pkg/libstub.dylib".into());
    model.add_target(stub)?;
    let mut app = target("app", TargetKind::Executable, &["main.c"]);
    app.usage.link_libraries = Some(vec![link_item("stub")]);
    model.add_target(app)?;
    let result = resolve(&model, &seeded(&source, &["main.c"])?, &[])?;
    let app = result
        .targets
        .iter()
        .find(|target| target.metadata.name == "app")
        .and_then(|target| target.link.as_ref())
        .ok_or("Expected a link step.")?;
    let libraries: Vec<_> = app
        .fragments
        .iter()
        .filter(|fragment| fragment.role == LinkFragmentRole::Libraries)
        .map(|fragment| fragment.text.value.clone())
        .collect();
    assert_eq!(libraries, ["/pkg/libstub.dylib"]);
    Ok(())
}

#[test]
fn reports_bad_outputs_of_every_custom_command_at_generation() -> TestResult {
    use rcmake_model::{CustomCommand, CustomCommandKind};

    let (source, build) = roots();
    let mut model = project_with_roots(&source, &build);
    for (line, output) in [(3, "a<"), (4, "$<1:b>>")] {
        let mut command = CustomCommand::new(
            DirId::ROOT,
            CustomCommandKind::Output,
            backtrace(line),
            Default::default(),
        );
        command.outputs.push(entry(&format!("{build}/{output}")));
        model.add_command(command)?;
    }
    let host = PureHost::default();
    let Err(error) = resolve(&model, &host, &[]) else {
        return Err("Expected output errors.".into());
    };
    let text = error.to_string();
    let first = text
        .find("OUTPUT containing a \"<\" is not allowed.")
        .ok_or("Expected the first output error.")?;
    let second = text
        .find("OUTPUT containing a \">\" is not allowed.")
        .ok_or("Expected the second output error.")?;
    assert!(first < second, "{text}");
    Ok(())
}

#[test]
fn rejects_missing_include_directories_of_imported_targets() -> TestResult {
    let (source, build) = roots();
    let mut model = project_with_roots(&source, &build);
    let mut imported = target("imported", TargetKind::UnknownLibrary, &[]);
    imported.origin = TargetOrigin::Imported(ImportedVisibility::Local);
    imported.props.set(
        "IMPORTED_LOCATION".into(),
        format!("{source}/libimported.a").into(),
    );
    imported.usage.push(
        UsageField::InterfaceIncludeDirectories,
        entry(&format!("{source}/does/not/exist")),
    );
    model.add_target(imported)?;
    let mut app = target("app", TargetKind::Executable, &["main.c"]);
    app.usage.link_libraries = Some(vec![link_item("imported")]);
    model.add_target(app)?;
    let host = seeded(&source, &["main.c", "libimported.a"])?;
    let Err(error) = resolve(&model, &host, &[]) else {
        return Err("Expected a missing include directory error.".into());
    };
    assert!(
        error
            .to_string()
            .contains("Imported target \"imported\" includes non-existent path"),
        "{error}"
    );
    Ok(())
}

#[test]
fn ignores_missing_include_directories_of_imported_file_sets() -> TestResult {
    let (source, build) = roots();
    let mut model = project_with_roots(&source, &build);
    let missing = format!("{source}/srcs");
    let mut imported = target("imported", TargetKind::UnknownLibrary, &[]);
    imported.origin = TargetOrigin::Imported(ImportedVisibility::Local);
    imported.props.set(
        "IMPORTED_LOCATION".into(),
        format!("{source}/libimported.a").into(),
    );
    let imported = model.add_target(imported)?;
    let mut props = rcmake_model::PropertyMap::default();
    props.set(
        "INTERFACE_INCLUDE_DIRECTORIES".into(),
        missing.as_str().into(),
    );
    model.add_file_set(FileSet {
        target: imported,
        name: "SOURCES".into(),
        kind: FileSetKind::Sources,
        scope: UsageScope::Interface,
        base_dirs: Vec::new(),
        files: Vec::new(),
        props,
    })?;
    let mut app = target("app", TargetKind::Executable, &["main.c"]);
    app.usage.link_libraries = Some(vec![link_item("imported")]);
    model.add_target(app)?;
    let host = seeded(&source, &["main.c", "libimported.a"])?;
    resolve(&model, &host, &[])?;
    let target = model
        .target_mut(imported)
        .ok_or("Missing imported target")?;
    target
        .usage
        .push(UsageField::InterfaceIncludeDirectories, entry(&missing));
    assert!(resolve(&model, &host, &[]).is_err());
    assert!(
        String::from_utf8_lossy(&host.stream_bytes(rcmake_host::Stream::Stderr))
            .contains("includes non-existent path")
    );
    Ok(())
}

#[test]
fn linker_type_selects_flags_or_reports_an_unknown_type() -> TestResult {
    let (source, build) = roots();
    for (linker_type, expected) in [("LLD", Ok("-fuse-ld=lld")), ("FOO", Err("FOO"))] {
        let mut model = project_with_roots(&source, &build);
        model
            .dir_mut(DirId::ROOT)
            .vars
            .insert("CMAKE_C_USING_LINKER_LLD".into(), "-fuse-ld=lld".into());
        let mut app = target("app", TargetKind::Executable, &["main.c"]);
        app.props.set("LINKER_TYPE".into(), linker_type.into());
        model.add_target(app)?;
        let result = resolve(&model, &seeded(&source, &["main.c"])?, &[]);
        match (expected, result) {
            (Ok(flag), Ok(result)) => {
                let BuildStep::Link { invocations, .. } =
                    link_build_step(&result.graphs[0], "app")?
                else {
                    return Err("Expected a link step.".into());
                };
                assert!(
                    invocations.iter().any(|invocation| invocation
                        .argv
                        .iter()
                        .any(|arg| arg == flag)
                        || invocation
                            .shell
                            .windows(flag.len())
                            .any(|part| part == flag.as_bytes())),
                    "{invocations:?}"
                );
            }
            (Err(name), Err(error)) => assert!(
                error
                    .to_string()
                    .contains(&format!("LINKER_TYPE '{name}' is unknown or not supported")),
                "{error}"
            ),
            (expected, result) => {
                return Err(format!(
                    "{linker_type}: expected {expected:?}, got {:?}",
                    result.is_ok()
                )
                .into());
            }
        }
    }
    Ok(())
}

#[test]
fn rejects_literal_link_tags_in_link_properties() -> TestResult {
    let (source, build) = roots();
    let mut model = project_with_roots(&source, &build);
    let mut lib = target("lib", TargetKind::SharedLibrary, &["lib.c"]);
    lib.usage.link_libraries = Some(vec![link_item("<LINK_LIBRARY:feat>")]);
    model.add_target(lib)?;
    let Err(error) = resolve(&model, &seeded(&source, &["lib.c"])?, &[]) else {
        return Err("Expected a link tag error.".into());
    };
    assert!(
        error
            .to_string()
            .contains("Property LINK_LIBRARIES contains the invalid item \"<LINK_LIBRARY:feat>\""),
        "{error}"
    );
    Ok(())
}

#[test]
fn full_paths_follow_the_cmake_host_rules() {
    assert!(crate::file_is_full_path(b"/src/include"));
    assert!(!crate::file_is_full_path(b"include"));
    assert!(!crate::file_is_full_path(b"foo/bar"));
    assert!(crate::file_is_full_path(b"/work/src/include"));
    assert_eq!(crate::file_is_full_path(b"C:foo"), cfg!(windows));
    assert!(!crate::file_is_full_path(b""));
    assert_eq!(crate::file_is_full_path(b"C:/src"), cfg!(windows));
    assert_eq!(crate::file_is_full_path(b"~/src"), !cfg!(windows));
}

#[test]
fn reports_invalid_exclude_from_all_expression() -> TestResult {
    let (source, build) = roots();
    let mut model = project_with_roots(&source, &build);
    let mut tool = target("tool", TargetKind::Executable, &["main.c"]);
    tool.props
        .set("EXCLUDE_FROM_ALL".into(), "$<NO_SUCH_EXPRESSION>".into());
    model.add_target(tool)?;
    let error = resolve(&model, &seeded(&source, &["main.c"])?, &[])
        .err()
        .ok_or("Expected an expression error.")?;
    assert!(error.to_string().contains("NO_SUCH_EXPRESSION"), "{error}");
    Ok(())
}

#[test]
fn normalizes_evaluated_windows_usage_paths() -> TestResult {
    let (source, build) = roots();
    let mut model = project_with_roots(&source, &build);
    model
        .global
        .props
        .set("CMAKE_SYSTEM_NAME".into(), "Windows".into());
    let mut iface = target("iface", TargetKind::InterfaceLibrary, &[]);
    iface.usage.push(
        UsageField::InterfaceIncludeDirectories,
        entry(r"$<1:C:\SDK\include>"),
    );
    iface.usage.push(
        UsageField::InterfaceLinkDirectories,
        entry(r"$<1:C:\SDK\lib>"),
    );
    model.add_target(iface)?;
    let mut exe = target("exe", TargetKind::Executable, &["main.c"]);
    exe.usage.link_libraries = Some(vec![link_item("iface")]);
    model.add_target(exe)?;
    let result = resolve(&model, &seeded(&source, &["main.c"])?, &[])?;
    assert_eq!(library_paths(&result, "exe"), ["-LC:/SDK/lib"]);
    let includes = result
        .targets
        .iter()
        .find(|target| target.metadata.name == "exe")
        .and_then(|target| target.compile_groups.first())
        .ok_or("Missing compile group.")?;
    assert!(
        includes
            .includes
            .iter()
            .any(|entry| entry.path.value == "C:/SDK/include")
    );
    Ok(())
}

#[test]
fn relative_interface_link_directories_fail_under_every_cmp0081_setting() -> TestResult {
    let (source, build) = roots();
    for status in [PolicyStatus::Old, PolicyStatus::Warn, PolicyStatus::New] {
        let mut model = project_with_roots(&source, &build);
        let mut iface = target("iface", TargetKind::InterfaceLibrary, &[]);
        iface
            .usage
            .push(UsageField::InterfaceLinkDirectories, entry("$<1:relative>"));
        model.add_target(iface)?;
        let mut exe = target("exe", TargetKind::Executable, &["main.c"]);
        exe.usage.link_libraries = Some(vec![link_item("iface")]);
        exe.policies.policies.set(PolicyId::new(81), status)?;
        model.add_target(exe)?;
        let host = seeded(&source, &["main.c"])?;
        assert!(resolve(&model, &host, &[]).is_err());
        let text = host.stream_bytes(rcmake_host::Stream::Stderr).to_string();
        assert!(
            text.contains("Target \"iface\" contains relative path"),
            "{text}"
        );
    }
    Ok(())
}

#[test]
fn selects_direct_linker_in_the_link_language_context() -> TestResult {
    let (source, build) = roots();
    let mut model = project_with_roots(&source, &build);
    for (name, value) in [
        ("CMAKE_C_LINK_MODE", "LINKER"),
        ("CMAKE_C_USING_LINKER_custom", "/tools/custom linker"),
        (
            "CMAKE_C_LINK_EXECUTABLE",
            "<CMAKE_LINKER> <OBJECTS> -o <TARGET>",
        ),
    ] {
        model.global.props.set(name.into(), value.into());
    }
    let mut exe = target("exe", TargetKind::Executable, &["main.c"]);
    exe.props
        .set("LINKER_TYPE".into(), "$<$<LINK_LANGUAGE:C>:custom>".into());
    model.add_target(exe)?;
    let result = resolve(&model, &seeded(&source, &["main.c"])?, &[])?;
    assert!(
        result
            .graphs
            .iter()
            .flat_map(|graph| &graph.steps)
            .any(|step| matches!(step,
        BuildStep::Link { invocations, .. } if invocations.iter().any(|command| command.argv.first()
            .is_some_and(|path| path == "/tools/custom linker"))))
    );
    Ok(())
}

#[test]
fn target_installers_keep_platform_artifact_groups() -> TestResult {
    let (source, build) = roots();
    for system in ["Linux", "Windows"] {
        let mut model = project_with_roots(&source, &build);
        model
            .global
            .props
            .set("CMAKE_SYSTEM_NAME".into(), system.into());
        let mut library = target("example", TargetKind::SharedLibrary, &["main.c"]);
        library.props.set("VERSION".into(), "1.2.3".into());
        library.props.set("SOVERSION".into(), "1".into());
        let id = model.add_target(library)?;
        let rule = model.add_install(InstallRule {
            dir: DirId::ROOT,
            backtrace: backtrace(10),
            kind: InstallKind::Targets {
                targets: vec![id],
                export: None,
            },
            options: InstallOptions::default(),
        });
        let result = resolve(&model, &seeded(&source, &["main.c"])?, &[])?;
        let installers = &result.install.target_installers;
        let [first, second] = installers.as_slice() else {
            return Err(
                format!("Expected two target installers on {system}, got {installers:?}").into(),
            );
        };
        assert_eq!((first.rule, first.target), (rule, id));
        if system == "Windows" {
            assert!(first.import_library);
            assert!(!second.import_library);
            assert_eq!(first.destination, "lib");
            assert_eq!(second.destination, "bin");
            assert_eq!(first.namelink, NamelinkMode::None);
            assert_eq!(second.namelink, NamelinkMode::None);
        } else {
            assert_eq!(first.namelink, NamelinkMode::Skip);
            assert_eq!(second.namelink, NamelinkMode::Only);
            let names = |installer: &TargetInstaller| {
                installer
                    .paths
                    .iter()
                    .map(|(_, to)| to.clone())
                    .collect::<Vec<_>>()
            };
            assert_eq!(names(first), ["libexample.so.1.2.3", "libexample.so.1"]);
            assert_eq!(names(second), ["libexample.so"]);
        }
    }
    Ok(())
}

#[cfg(unix)]
#[test]
fn ninja_transformed_depfile_hashes_the_absolute_input_and_creates_its_directory() -> TestResult {
    use rcmake_base::hash::HashAlgo;
    use rcmake_model::{CustomCommand, CustomCommandKind, GeneratorKind};

    let mut model = project();
    model.generator = Some(GeneratorKind::Ninja);
    let mut policies = rcmake_diag::PolicyMap::default();
    policies.set(PolicyId::new(116), PolicyStatus::New)?;
    let mut command = CustomCommand::new(
        DirId::ROOT,
        CustomCommandKind::Output,
        Backtrace::default(),
        policies,
    );
    command.outputs.push(entry("generated.c"));
    command.depfile = Some("generated.d".into());
    command.verbatim = true;
    command.command_lines.push(vec![
        "echo".into(),
        r#"-DDET_NAME="det""#.into(),
        "two words".into(),
    ]);
    model.add_command(command)?;
    model.add_target(target(
        "generated",
        TargetKind::ObjectLibrary,
        &["generated.c"],
    ))?;
    let host = PureHost::default();
    let result = resolve(&model, &host, &[])?;
    let custom = result
        .graphs
        .iter()
        .flat_map(|graph| &graph.steps)
        .find_map(|step| {
            if let BuildStep::Custom(custom) = step {
                Some(custom)
            } else {
                None
            }
        })
        .ok_or("Missing custom command.")?;
    let hash = HashAlgo::Sha256.hex_digest(b"/out/generated.d");
    let expected = format!("CMakeFiles/d/{hash}.d");
    assert_eq!(
        custom.depfile.as_ref().map(|path| path.as_slice()),
        Some(expected.as_bytes())
    );
    host.read_dir(Path::new("/out/CMakeFiles/d"))?;
    let [command, transform] = custom.invocations.as_slice() else {
        return Err("Expected the command and depfile transform.".into());
    };
    assert_eq!(
        command.shell,
        br#"echo -DDET_NAME=\"det\" "two words""#.as_slice()
    );
    assert_eq!(command.working_dir, transform.working_dir);
    Ok(())
}
