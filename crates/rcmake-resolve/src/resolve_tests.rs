use std::path::Path;
use std::sync::Arc;

use rcmake_diag::{Frame, FrameKind};
use rcmake_host::PureHost;
use rcmake_model::{
    CacheEntry, CacheType, ExportSet, ImportedVisibility, InstallOptions, InstallRule, LinkItem,
    TargetOrigin,
};

use super::*;

type TestResult = Result<(), Box<dyn std::error::Error>>;

fn project() -> ProjectModel {
    let mut model = ProjectModel::default();
    model.set_root_dirs("/src".into(), "/out".into());
    for (name, value) in [
        ("CMAKE_SYSTEM_NAME", "Darwin"),
        ("CMAKE_C_COMPILER", "/toolchain/cc"),
        ("CMAKE_C_COMPILER_ID", "AppleClang"),
        ("CMAKE_CXX_COMPILER", "/toolchain/c++"),
        (
            "CMAKE_CONFIGURE_LOG",
            "/out/CMakeFiles/CMakeConfigureLog.yaml",
        ),
    ] {
        model.global.props.set(name.into(), value.into());
    }
    model
        .dir_mut(DirId::ROOT)
        .listfiles
        .push("/src/CMakeLists.txt".into());
    model
}

fn backtrace(line: u32) -> Backtrace {
    Backtrace::default().push(Frame {
        file: Arc::from(b"/src/CMakeLists.txt".as_slice()),
        line,
        name: None,
        kind: FrameKind::Command,
    })
}

fn entry(value: &str) -> BT<BString> {
    BT::new(value.into(), backtrace(8))
}

fn target(name: &str, kind: TargetKind, sources: &[&str]) -> Target {
    let mut target = Target::new(
        name.into(),
        kind,
        TargetOrigin::Normal,
        DirId::ROOT,
        backtrace(3),
        Default::default(),
    );
    target
        .sources
        .extend(sources.iter().map(|source| entry(source)));
    target
}

fn link_item(name: &str) -> LinkItem {
    LinkItem {
        text: entry(name),
        lookup_dir: DirId::ROOT,
    }
}

#[test]
fn compile_flags_keep_shell_arguments_and_property_order() -> TestResult {
    let mut model = project();
    for (name, value) in [
        ("CMAKE_CXX_FLAGS", "-DLANGUAGE_FLAG"),
        ("CMAKE_CXX_FLAGS_DEBUG", "-g"),
        ("CMAKE_OSX_ARCHITECTURES", "arm64"),
    ] {
        model.global.props.set(name.into(), value.into());
    }
    let mut app = target(
        "app",
        TargetKind::SharedLibrary,
        &["a.cpp", "b.cpp", "c.cpp"],
    );
    app.props.set(
        "COMPILE_FLAGS".into(),
        r#"-DTARGET_TEXT="a b" -DESCAPED=a\ b"#.into(),
    );
    app.usage
        .push(UsageField::CompileOptions, entry("-DTARGET_OPTION"));
    app.usage
        .push(UsageField::CompileDefinitions, entry("TARGET_DEF"));
    model.add_target(app)?;
    let source = model
        .dir_mut(DirId::ROOT)
        .source_props
        .entry("/src/b.cpp".into())
        .or_default();
    source.set("COMPILE_FLAGS".into(), "-DSOURCE='$<CONFIG>;a b'".into());
    source.set("COMPILE_OPTIONS".into(), "-DSOURCE_OPTION".into());
    model
        .dir_mut(DirId::ROOT)
        .source_props
        .entry("/src/c.cpp".into())
        .or_default()
        .set("COMPILE_FLAGS".into(), "-DSOURCE_ONLY".into());
    let host = PureHost::default();
    for path in ["/src/a.cpp", "/src/b.cpp", "/src/c.cpp"] {
        host.seed_file(Path::new(path), b"")?;
    }

    let result = resolve(&model, &host, &[ConfigId("Debug".into())])?;
    let compile_args = result.graphs[0]
        .steps
        .iter()
        .filter_map(|step| match step {
            BuildStep::Compile { invocation, .. } => Some(&invocation.argv),
            _ => None,
        })
        .collect::<Vec<_>>();
    let [plain, with_source_flags, flags_only] = compile_args.as_slice() else {
        return Err("Expected three compile commands.".into());
    };
    let shared = [
        "-DTARGET_DEF",
        "-Dapp_EXPORTS",
        "-DLANGUAGE_FLAG",
        "-g",
        "-arch",
        "arm64",
        "-fPIC",
        "-DTARGET_TEXT=a b",
        "-DESCAPED=a b",
        "-DTARGET_OPTION",
    ];
    assert_eq!(&plain[1..=shared.len()], shared);
    assert_eq!(&with_source_flags[1..=shared.len()], shared);
    assert!(!plain.iter().any(|arg| arg.starts_with(b"-DSOURCE")));
    assert_eq!(
        &with_source_flags[shared.len() + 1..=shared.len() + 2],
        ["-DSOURCE=Debug;a b", "-DSOURCE_OPTION"],
    );
    assert_eq!(flags_only[shared.len() + 1], "-DSOURCE_ONLY");
    Ok(())
}

#[test]
fn darwin_framework_items_keep_linker_argument_boundaries() -> TestResult {
    let mut model = project();
    model.global.props.set(
        "CMAKE_PLATFORM_IMPLICIT_LINK_FRAMEWORK_DIRECTORIES".into(),
        "/SDK/System/Library/Frameworks".into(),
    );
    let mut app = target("app", TargetKind::Executable, &["main.c"]);
    app.usage.link_libraries = Some(vec![
        link_item("-framework Cocoa"),
        link_item(r#"-weak_framework "Two Words""#),
        link_item("/SDK/System/Library/Frameworks/IOKit.framework"),
        link_item("/framework dir/Local Name.framework"),
        link_item("/framework dir/Other.framework"),
    ]);
    model.add_target(app)?;
    let host = PureHost::default();
    host.seed_file(Path::new("/src/main.c"), b"")?;
    let result = resolve(&model, &host, &[])?;
    let Some(BuildStep::Link { invocations, .. }) = result.graphs[0].steps.last() else {
        return Err("Expected a link command.".into());
    };
    let [link] = invocations.as_slice() else {
        return Err("Expected one linker invocation.".into());
    };
    assert!(
        link.argv.ends_with(
            &[
                "-F/framework dir",
                "-framework",
                "Cocoa",
                "-weak_framework",
                "Two Words",
                "-framework",
                "IOKit",
                "-framework",
                "Local Name",
                "-framework",
                "Other",
            ]
            .map(BString::from),
        )
    );
    Ok(())
}

#[test]
fn resolves_build_install_and_export_for_each_config() -> TestResult {
    let mut model = project();
    model.cache.insert(
        "CMAKE_BUILD_TYPE".into(),
        CacheEntry {
            value: "Debug".into(),
            ty: CacheType::String,
            help: BString::default(),
            advanced: false,
            props: Default::default(),
        },
    );

    let mut math = target(
        "libmath",
        TargetKind::StaticLibrary,
        &["math.c", "$<$<CONFIG:Debug>:trace.c>", "math.h"],
    );
    math.props.set("PREFIX".into(), "".into());
    math.props.set("FOLDER".into(), "Libraries".into());
    math.props.set("EXPORT_NAME".into(), "math".into());
    let build_include = "$<BUILD_INTERFACE:/src/include>";
    math.usage
        .push(UsageField::IncludeDirectories, entry(build_include));
    math.usage.push(UsageField::InterfaceIncludeDirectories, entry(
        "$<BUILD_INTERFACE:/src/include>;$<INSTALL_INTERFACE:include>;$<INSTALL_INTERFACE:$<INSTALL_PREFIX>/extra>",
    ));
    math.usage.push(
        UsageField::CompileDefinitions,
        entry("MATH_PRIVATE;$<$<CONFIG:Debug>:MATH_TRACE>"),
    );
    math.usage.push(
        UsageField::CompileOptions,
        entry("$<$<COMPILE_LANGUAGE:C>:-Wall>"),
    );
    let math_id = model.add_target(math)?;
    model.add_alias(DirId::ROOT, "local::math".into(), math_id)?;

    let mut app = target("app", TargetKind::Executable, &["main.c"]);
    app.usage.link_libraries = Some(vec![
        link_item("local::math"),
        link_item("m"),
        link_item("/SDK/Frameworks/Foo.framework"),
    ]);
    app.usage.push(
        UsageField::LinkOptions,
        entry("$<$<LINK_LANG_AND_ID:C,AppleClang>:-v>"),
    );
    app.usage
        .push(UsageField::LinkDirectories, entry("/out/lib path"));
    let app_id = model.add_target(app)?;
    for (id, destination) in [(math_id, "lib"), (app_id, "bin")] {
        model.add_install(InstallRule {
            dir: DirId::ROOT,
            backtrace: backtrace(11),
            kind: InstallKind::Targets {
                targets: vec![id],
                export: Some("sdk".into()),
            },
            options: InstallOptions {
                destination: Some(destination.into()),
                includes_destination: if id == math_id {
                    [
                        "./sdk",
                        "/opt/sdk",
                        "$<INSTALL_PREFIX>/extra2",
                        "$<1:more;paths>",
                    ]
                    .map(BString::from)
                    .into()
                } else {
                    vec!["app/include".into()]
                },
                ..InstallOptions::default()
            },
        });
    }
    model.exports.insert(
        "sdk".into(),
        ExportSet {
            name: "sdk".into(),
            targets: vec![math_id, app_id],
            install_rules: vec![0, 1],
        },
    );
    model.add_install(InstallRule {
        dir: DirId::ROOT,
        backtrace: backtrace(12),
        kind: InstallKind::Export {
            set: "sdk".into(),
            namespace: "sdk::".into(),
            file: None,
            cxx_modules_directory: None,
        },
        options: InstallOptions {
            destination: Some("lib/cmake/sdk".into()),
            ..InstallOptions::default()
        },
    });
    for (key, value) in [
        ("_RCMAKE_BUILD_EXPORT_COUNT", "1"),
        ("_RCMAKE_BUILD_EXPORT_0_DIRECTORY", "0"),
        ("_RCMAKE_BUILD_EXPORT_0_ARGC", "6"),
        ("_RCMAKE_BUILD_EXPORT_0_ARG0", "EXPORT"),
        ("_RCMAKE_BUILD_EXPORT_0_ARG1", "sdk"),
        ("_RCMAKE_BUILD_EXPORT_0_ARG2", "FILE"),
        ("_RCMAKE_BUILD_EXPORT_0_ARG3", "SdkBuild.cmake"),
        ("_RCMAKE_BUILD_EXPORT_0_ARG4", "NAMESPACE"),
        ("_RCMAKE_BUILD_EXPORT_0_ARG5", "sdk::"),
    ] {
        model.global.props.set(key.into(), value.into());
    }
    let host = PureHost::default();
    for source in ["math.c", "trace.c", "math.h", "main.c"] {
        host.seed_file(&Path::new("/src").join(source), b"")?;
    }

    let debug = ConfigId("Debug".into());
    let result = resolve(&model, &host, &[])?;
    assert_eq!(result.configs, std::slice::from_ref(&debug));
    let [math, app] = result.targets.as_slice() else {
        return Err("Expected two resolved targets.".into());
    };
    assert_eq!((math.id, app.id), (math_id, app_id));
    assert_eq!(math.metadata.folder, Some("Libraries".into()));
    assert_eq!(
        math.metadata.backtrace.top().map(|frame| frame.line),
        Some(3)
    );
    assert_eq!(math.artifacts.file, Some("/out/libmath.a".into()));
    let [math_group] = math.compile_groups.as_slice() else {
        return Err("Expected one compile group for the library.".into());
    };
    assert_eq!(math_group.language, "C");
    assert_eq!(math_group.units, [0, 1]);
    assert_eq!(
        math_group.defines,
        [entry("MATH_PRIVATE"), entry("MATH_TRACE")]
    );
    assert_eq!(math_group.flags, [entry("-Wall")]);
    assert_eq!(math.compile_units[1].source, entry("/src/trace.c"));
    assert_eq!(
        math.link.as_ref().map(|link| link.kind),
        Some(LinkKind::StaticArchive)
    );
    let [app_group] = app.compile_groups.as_slice() else {
        return Err("Expected one compile group for the executable.".into());
    };
    assert!(app_group.defines.is_empty());
    assert_eq!(
        app_group.includes,
        [InterfaceInclude {
            path: entry("/src/include"),
            system: false
        }]
    );
    assert_eq!(app.dependencies, [math_id]);
    let app_link = app.link.as_ref().ok_or("Expected a link step.")?;
    assert_eq!(app_link.language, "C");
    assert_eq!(app_link.output, "app");
    assert_eq!(
        app_link
            .fragments
            .iter()
            .map(|fragment| (fragment.text.value.as_slice(), fragment.role))
            .collect::<Vec<_>>(),
        [
            (b"-v".as_slice(), LinkFragmentRole::Flags),
            (b"'-L/out/lib path'", LinkFragmentRole::LibraryPath),
            (b"-F/SDK/Frameworks", LinkFragmentRole::FrameworkPath),
            (b"/out/libmath.a", LinkFragmentRole::Libraries),
            (b"-lm", LinkFragmentRole::Libraries),
            (b"-framework Foo", LinkFragmentRole::Libraries),
        ],
    );

    let [graph] = result.graphs.as_slice() else {
        return Err("Expected one build graph.".into());
    };
    assert_eq!(graph.config, debug);
    assert_eq!(graph.targets[1].depends, [0]);
    assert!(matches!(
        &graph.steps[graph.targets[0].steps[0]],
        BuildStep::Compile { invocation, .. }
            if invocation.argv.contains(&"-DMATH_TRACE".into())
                && invocation.argv.contains(&"-I/src/include".into())
    ));
    assert!(matches!(
        &graph.steps[graph.targets[1].steps[1]],
        BuildStep::Link { inputs, invocations, .. }
            if inputs.contains(&"/out/libmath.a".into())
                && invocations.iter().any(|invocation| invocation.argv.contains(&"-L/out/lib path".into()))
    ));
    let [archive, executable, export] = result.install.actions.as_slice() else {
        return Err("Expected three install actions.".into());
    };
    assert!(
        matches!(&archive.kind, InstallActionKind::Files { file_type, files, .. }
        if file_type == "STATIC_LIBRARY" && files == &[BString::from("/out/libmath.a")])
    );
    assert_eq!(archive.destination, "lib");
    assert_eq!(executable.destination, "bin");
    assert_eq!(export.kind, InstallActionKind::Export { plan: 0 });
    assert_eq!(export.destination, "lib/cmake/sdk");

    let [install, build] = result.exports.as_slice() else {
        return Err("Expected one install export and one build export.".into());
    };
    let includes = b"INTERFACE_INCLUDE_DIRECTORIES".as_slice();
    let location = b"IMPORTED_LOCATION".as_slice();
    assert!(install.install);
    assert_eq!(install.file, "sdk.cmake");
    assert_eq!(install.namespace, "sdk::");
    assert_eq!(install.targets[0].name, "math");
    assert_eq!(
        install.targets[0].properties[includes],
        concat!(
            "${_IMPORT_PREFIX}/include;${_IMPORT_PREFIX}/extra;",
            "${_IMPORT_PREFIX}/./sdk;/opt/sdk;${_IMPORT_PREFIX}/extra2;",
            "${_IMPORT_PREFIX}/more;${_IMPORT_PREFIX}/paths"
        )
    );
    assert_eq!(
        install.targets[1].properties[includes],
        "${_IMPORT_PREFIX}/app/include"
    );
    assert_eq!(
        install.targets[0].config_properties[&debug.0][location],
        "lib/libmath.a"
    );
    assert_eq!(
        install.targets[1].config_properties[&debug.0][location],
        "bin/app"
    );
    assert!(!build.install);
    assert_eq!(build.file, "/out/SdkBuild.cmake");
    assert_eq!(build.targets[0].properties[includes], "/src/include");
    assert_eq!(
        build.targets[0].config_properties[&debug.0][location],
        "/out/libmath.a"
    );

    assert_eq!(
        result.metadata.listfiles,
        [BString::from("/src/CMakeLists.txt")]
    );
    assert_eq!(
        result.metadata.toolchains[b"C".as_slice()].compiler_path,
        "/toolchain/cc"
    );
    assert_eq!(
        result.metadata.configure_log,
        Some("/out/CMakeFiles/CMakeConfigureLog.yaml".into())
    );

    let release = ConfigId("Release".into());
    let multi = resolve(&model, &host, &[debug, release.clone()])?;
    assert_eq!(multi.graphs.len(), 2);
    let math_release = &multi.targets[2];
    assert_eq!(math_release.config, release);
    assert_eq!(math_release.compile_units.len(), 1);
    assert_eq!(
        math_release.compile_groups[0].defines,
        [entry("MATH_PRIVATE")]
    );
    assert_eq!(multi.exports[0].targets[0].config_properties.len(), 2);
    assert_eq!(multi.install.actions[1].configurations, [release.0]);
    Ok(())
}

#[test]
fn resolves_normal_definitions_before_cache_values() -> TestResult {
    let mut model = project();
    let root = model.dir_mut(DirId::ROOT);
    root.vars.extend([
        ("CMAKE_BUILD_TYPE".into(), "Debug".into()),
        ("CMAKE_C_COMPILER".into(), "/root/cc".into()),
        ("CMAKE_COMMAND".into(), "/tools/cmake".into()),
        ("CMAKE_CTEST_COMMAND".into(), "/tools/ctest".into()),
        ("CMAKE_ASM_COMPILER".into(), "/tools/as".into()),
    ]);
    let child = model.add_directory(
        DirId::ROOT,
        "/src/sub".into(),
        "/out/sub".into(),
        Default::default(),
    )?;
    model.dir_mut(child).vars.extend([
        ("CMAKE_C_COMPILER".into(), "/sub/cc".into()),
        ("CMAKE_C_COMPILER_ID".into(), "Clang".into()),
        ("CMAKE_C_FLAGS".into(), "-O1".into()),
        ("CMAKE_SYSTEM_NAME".into(), "Linux".into()),
    ]);
    for (name, value) in [
        ("CMAKE_BUILD_TYPE", "Release"),
        ("CMAKE_C_COMPILER", "/cache/cc"),
        ("CMAKE_C_COMPILER_ID", "GNU"),
        ("CMAKE_C_FLAGS", "-O2"),
        ("CMAKE_C_FLAGS_DEBUG", "-g"),
    ] {
        model.cache.insert(
            name.into(),
            CacheEntry {
                value: value.into(),
                ty: CacheType::String,
                help: BString::default(),
                advanced: false,
                props: Default::default(),
            },
        );
    }
    let mut raw = target("shared", TargetKind::SharedLibrary, &["a.c"]);
    raw.dir = child;
    raw.usage.push(
        UsageField::CompileDefinitions,
        entry("$<$<C_COMPILER_ID:Clang>:SUBDIR_CLANG>"),
    );
    model.add_target(raw)?;
    let host = PureHost::default();
    host.seed_file(Path::new("/src/sub/a.c"), b"")?;

    let result = resolve(&model, &host, &[])?;
    assert_eq!(result.configs, [ConfigId("Debug".into())]);
    assert_eq!(
        result.metadata.toolchains[b"C".as_slice()].compiler_path,
        "/root/cc"
    );
    assert_eq!(
        result.metadata.toolchains[b"ASM".as_slice()].compiler_path,
        "/tools/as"
    );
    assert_eq!(
        result
            .metadata
            .cmake_command
            .as_ref()
            .map(|value| value.as_slice()),
        Some(&b"/tools/cmake"[..])
    );
    assert_eq!(
        result
            .metadata
            .ctest_command
            .as_ref()
            .map(|value| value.as_slice()),
        Some(&b"/tools/ctest"[..])
    );
    assert_eq!(
        result.targets[0].artifacts.file,
        Some("/out/sub/libshared.so".into())
    );
    let invocation = result.graphs[0]
        .steps
        .iter()
        .find_map(|step| match step {
            BuildStep::Compile { invocation, .. } => Some(invocation),
            _ => None,
        })
        .ok_or("Expected a compile step.")?;
    for arg in ["/sub/cc", "-O1", "-g", "-DSUBDIR_CLANG"] {
        assert!(
            invocation.argv.contains(&arg.into()),
            "{:?}",
            invocation.argv
        );
    }
    Ok(())
}

#[test]
fn adds_export_symbols_only_to_the_library_compile_steps() -> TestResult {
    let mut model = project();
    let cases = [
        (
            "1my-lib",
            TargetKind::SharedLibrary,
            None,
            Some("_1my_lib_EXPORTS"),
        ),
        (
            "explicit",
            TargetKind::SharedLibrary,
            Some("ZLIB_DLL"),
            Some("ZLIB_DLL"),
        ),
        (
            "module",
            TargetKind::ModuleLibrary,
            None,
            Some("module_EXPORTS"),
        ),
        ("disabled", TargetKind::SharedLibrary, Some(""), None),
        ("static", TargetKind::StaticLibrary, Some("NOT_USED"), None),
        ("app", TargetKind::Executable, None, None),
    ];
    for (name, kind, symbol, _) in cases {
        let mut raw = target(name, kind, &["a.c"]);
        if let Some(symbol) = symbol {
            raw.props.set("DEFINE_SYMBOL".into(), symbol.into());
        }
        if name == "app" {
            raw.usage.link_libraries = Some(vec![link_item("explicit")]);
        }
        model.add_target(raw)?;
    }
    let host = PureHost::default();
    host.seed_file(Path::new("/src/a.c"), b"")?;

    let result = resolve(&model, &host, &[])?;
    let graph = &result.graphs[0];
    for (name, _, _, expected) in cases {
        let target = result
            .targets
            .iter()
            .find(|target| target.metadata.name == name)
            .ok_or("Expected a resolved target.")?;
        let defines: Vec<_> = target.compile_groups[0]
            .defines
            .iter()
            .map(|define| &define.value)
            .collect();
        assert_eq!(
            defines,
            expected.map(BString::from).iter().collect::<Vec<_>>()
        );
        let graph_target = graph
            .targets
            .iter()
            .find(|target| target.name == name)
            .ok_or("Expected a graph target.")?;
        let invocation = graph_target
            .steps
            .iter()
            .find_map(|&step| match &graph.steps[step] {
                BuildStep::Compile { invocation, .. } => Some(invocation),
                _ => None,
            })
            .ok_or("Expected a compile step.")?;
        let definitions: Vec<_> = invocation
            .argv
            .iter()
            .filter(|arg| arg.starts_with(b"-D"))
            .collect();
        assert_eq!(
            definitions,
            expected
                .map(|symbol| BString::from(format!("-D{symbol}")))
                .iter()
                .collect::<Vec<_>>()
        );
    }
    Ok(())
}

#[test]
fn checks_sources_and_resolves_generated_files_in_the_binary_directory() -> TestResult {
    let mut model = project();
    model.add_target(target(
        "app",
        TargetKind::Executable,
        &["main.c", "generated.cpp", "header.c"],
    ))?;
    let dir = model.dir_mut(DirId::ROOT);
    dir.source_props
        .entry("/out/generated.cpp".into())
        .or_default()
        .set("GENERATED".into(), "1".into());
    dir.source_props
        .entry("/src/header.c".into())
        .or_default()
        .set("HEADER_FILE_ONLY".into(), "ON".into());
    let host = PureHost::default();
    host.seed_file(Path::new("/src/header.c"), b"")?;
    assert_eq!(
        resolve(&model, &host, &[]).expect_err("The source is missing."),
        ResolveError::Message("Cannot find source file:\n\n  main.c".into())
    );
    host.seed_file(Path::new("/src/main.c"), b"")?;
    let result = resolve(&model, &host, &[])?;
    assert_eq!(result.configs, [ConfigId::default()]);
    let app = &result.targets[0];
    assert_eq!(app.compile_groups.len(), 2);
    assert_eq!(app.compile_units.len(), 2);
    assert_eq!(app.compile_units[1].source.value, "/out/generated.cpp");
    assert_eq!(app.compile_groups[1].language, "CXX");
    assert_eq!(
        app.link.as_ref().map(|link| link.language.as_slice()),
        Some(b"CXX".as_slice())
    );
    Ok(())
}

#[test]
fn keeps_local_dependency_order_and_links_imported_artifacts() -> TestResult {
    let mut model = project();
    let mut imported = target("external", TargetKind::StaticLibrary, &["missing.c"]);
    imported.origin = TargetOrigin::Imported(ImportedVisibility::Global);
    imported
        .props
        .set("IMPORTED_LOCATION".into(), "/pkg/libexternal.a".into());
    model.add_target(imported)?;
    let utility = model.add_target(target("prepare", TargetKind::Utility, &[]))?;
    let mut interface = target("headers", TargetKind::InterfaceLibrary, &[]);
    interface.usage.interface_link_libraries = Some(vec![link_item("external")]);
    let interface = model.add_target(interface)?;
    let mut app = target("app", TargetKind::Executable, &["main.c"]);
    app.props.set("EXCLUDE_FROM_ALL".into(), "ON".into());
    app.utility_deps.push(entry("prepare"));
    app.usage.link_libraries = Some(vec![link_item("headers")]);
    model.add_target(app)?;
    let host = PureHost::default();
    host.seed_file(Path::new("/src/main.c"), b"")?;
    let result = resolve(&model, &host, &[])?;
    assert_eq!(result.targets.len(), 3);
    let app = &result.targets[2];
    assert_eq!(app.dependencies, [utility, interface]);
    assert!(app.metadata.exclude_from_all);
    assert!(!result.graphs[0].targets[2].in_all);
    let link = app.link.as_ref().ok_or("Expected a link step.")?;
    assert_eq!(link.fragments[0].text.value, "/pkg/libexternal.a");
    Ok(())
}

fn link_build_step<'a>(graph: &'a BuildGraph, name: &str) -> Result<&'a BuildStep, &'static str> {
    graph
        .targets
        .iter()
        .find(|target| target.name == name)
        .into_iter()
        .flat_map(|target| &target.steps)
        .filter_map(|&index| graph.steps.get(index))
        .find(|step| matches!(step, BuildStep::Link { .. }))
        .ok_or("Expected a link step.")
}

fn assert_object_input(graph: &BuildGraph, name: &str, path: &BString) -> TestResult {
    let BuildStep::Link {
        inputs,
        invocations,
        ..
    } = link_build_step(graph, name)?
    else {
        return Err("Expected a link step.".into());
    };
    assert_eq!(inputs.iter().filter(|input| *input == path).count(), 1);
    assert_eq!(
        invocations
            .iter()
            .flat_map(|command| &command.argv)
            .filter(|arg| *arg == path)
            .count(),
        1
    );
    Ok(())
}

#[test]
fn target_objects_source_links_the_producer_object() -> TestResult {
    let mut model = project();
    model.add_target(target(
        "app",
        TargetKind::Executable,
        &["$<TARGET_OBJECTS:obj>", "main.c"],
    ))?;
    let objects_dir = model.add_directory(
        DirId::ROOT,
        "/src/lib".into(),
        "/out/lib".into(),
        backtrace(3),
    )?;
    let mut obj = target("obj", TargetKind::ObjectLibrary, &["../a.cpp"]);
    obj.dir = objects_dir;
    let obj_id = model.add_target(obj)?;
    let host = PureHost::default();
    host.seed_file(Path::new("/src/a.cpp"), b"")?;
    host.seed_file(Path::new("/src/main.c"), b"")?;

    let result = resolve(&model, &host, &[])?;
    let [app, obj] = result.targets.as_slice() else {
        return Err("Expected two resolved targets.".into());
    };
    assert_eq!(app.compile_units.len(), 1);
    assert_eq!(app.dependencies, [obj_id]);
    let graph = &result.graphs[0];
    assert_eq!(graph.targets[0].depends, [1]);
    assert_object_input(graph, "app", &obj.compile_units[0].object)
}

#[test]
fn unity_objects_use_shared_sources_and_config_specific_objects() -> TestResult {
    let mut model = project();
    model.generator = Some(rcmake_model::GeneratorKind::NinjaMultiConfig);
    let mut library = target(
        "obj",
        TargetKind::ObjectLibrary,
        &["always.c", "$<$<CONFIG:Debug>:debug.c>", "separate.c"],
    );
    library.props.set("UNITY_BUILD".into(), "ON".into());
    let object_id = model.add_target(library)?;
    model.add_target(target(
        "app",
        TargetKind::Executable,
        &["$<TARGET_OBJECTS:obj>", "main.c"],
    ))?;
    model
        .dir_mut(DirId::ROOT)
        .source_props
        .entry("/src/separate.c".into())
        .or_default()
        .set("COMPILE_OPTIONS".into(), "-DSEPARATE".into());
    let host = PureHost::default();
    for source in [
        "/src/always.c",
        "/src/debug.c",
        "/src/separate.c",
        "/src/main.c",
    ] {
        host.seed_file(Path::new(source), b"")?;
    }

    let configs = [ConfigId("Debug".into()), ConfigId("Release".into())];
    let result = resolve(&model, &host, &configs)?;
    let unity_file = "/out/CMakeFiles/obj.dir/Unity/unity_0_c.c";
    let content = host.read(
        Path::new(unity_file),
        rcmake_host::ReadRange::All { limit: 4096 },
    )?;
    let content = String::from_utf8(content)?;
    assert!(content.contains("#include \"/src/always.c\""));
    assert!(content.contains("#if defined(CMAKE_UNITY_CONFIG_DEBUG)"));
    assert!(content.contains("#include \"/src/debug.c\""));
    assert!(!content.contains("separate.c"));
    for (config, graph) in configs.iter().zip(&result.graphs) {
        let facts = result
            .targets
            .iter()
            .find(|target| target.id == object_id && target.config == *config)
            .ok_or("Missing object library.")?;
        let [unity, separate] = facts.compile_units.as_slice() else {
            return Err("Expected one unity unit and one separate unit.".into());
        };
        assert_eq!(unity.source.value, unity_file);
        assert_eq!(
            unity.object,
            format!("CMakeFiles/obj.dir/{}/Unity/unity_0_c.c.o", config.0)
        );
        let definition = format!(
            "CMAKE_UNITY_CONFIG_{}",
            config.0.to_string().to_ascii_uppercase()
        );
        assert!(
            facts.compile_groups[unity.group]
                .defines
                .iter()
                .any(|define| define.value == definition)
        );
        assert_object_input(graph, "app", &unity.object)?;
        assert_object_input(graph, "app", &separate.object)?;
    }
    Ok(())
}

#[test]
fn direct_link_to_object_library_includes_objects_and_usage() -> TestResult {
    let mut model = project();
    let mut obj = target("obj", TargetKind::ObjectLibrary, &["a.cpp"]);
    obj.usage
        .push(UsageField::InterfaceCompileDefinitions, entry("FROM_OBJ"));
    let obj_id = model.add_target(obj)?;
    let mut app = target("app", TargetKind::Executable, &["main.c"]);
    app.usage.link_libraries = Some(vec![link_item("obj")]);
    model.add_target(app)?;
    let mut archive = target("archive", TargetKind::StaticLibrary, &[]);
    archive.usage.link_libraries = Some(vec![link_item("obj")]);
    model.add_target(archive)?;
    let host = PureHost::default();
    host.seed_file(Path::new("/src/a.cpp"), b"")?;
    host.seed_file(Path::new("/src/main.c"), b"")?;

    let result = resolve(&model, &host, &[])?;
    let [obj, app, archive] = result.targets.as_slice() else {
        return Err("Expected three resolved targets.".into());
    };
    assert_eq!(app.compile_groups[0].defines, [entry("FROM_OBJ")]);
    let graph = &result.graphs[0];
    for consumer in [app, archive] {
        assert_eq!(consumer.dependencies, [obj_id]);
        let link = consumer.link.as_ref().ok_or("Expected a link step.")?;
        assert_eq!(link.language, "CXX");
        assert!(link.fragments.is_empty());
        assert_object_input(
            graph,
            &consumer.metadata.name.to_string(),
            &obj.compile_units[0].object,
        )?;
    }
    Ok(())
}

#[test]
fn archive_commands_keep_flags_and_objects_with_full_resolved_fragments() -> TestResult {
    let mut model = project();
    let library = model.add_target(target("lib", TargetKind::StaticLibrary, &["lib.c"]))?;
    let objects = model.add_target(target("obj", TargetKind::ObjectLibrary, &["obj.c"]))?;
    let mut archive = target("archive", TargetKind::StaticLibrary, &[]);
    archive.usage.link_libraries = Some(vec![link_item("lib"), link_item("obj")]);
    archive
        .usage
        .push(UsageField::LinkOptions, entry("-archive-flag"));
    archive
        .usage
        .push(UsageField::LinkDirectories, entry("/lib/path"));
    model.add_target(archive)?;
    let host = PureHost::default();
    for file in ["/src/lib.c", "/src/obj.c"] {
        host.seed_file(Path::new(file), b"")?;
    }
    let result = resolve(&model, &host, &[])?;
    let [_, obj, archive] = result.targets.as_slice() else {
        return Err("Expected three resolved targets.".into());
    };
    assert_eq!(archive.dependencies, [library, objects]);
    let link = archive.link.as_ref().ok_or("Expected an archive step.")?;
    assert_eq!(
        link.fragments
            .iter()
            .map(|fragment| fragment.role)
            .collect::<Vec<_>>(),
        [
            LinkFragmentRole::Flags,
            LinkFragmentRole::LibraryPath,
            LinkFragmentRole::Libraries
        ]
    );

    let graph = &result.graphs[0];
    assert_eq!(graph.targets[2].depends, [0, 1]);
    assert_object_input(graph, "archive", &obj.compile_units[0].object)?;
    let BuildStep::Link { step, .. } = link_build_step(graph, "archive")? else {
        return Err("Expected an archive step.".into());
    };
    assert_eq!(step.fragments, link.fragments[..1]);
    Ok(())
}

#[test]
fn interface_object_library_link_requires_explicit_objects() -> TestResult {
    let mut model = project();
    let mut obj = target("obj", TargetKind::ObjectLibrary, &["a.cpp"]);
    obj.usage
        .push(UsageField::InterfaceCompileDefinitions, entry("FROM_OBJ"));
    let obj_id = model.add_target(obj)?;
    let mut iface = target("iface", TargetKind::InterfaceLibrary, &[]);
    iface.usage.interface_link_libraries = Some(vec![link_item("obj")]);
    model.add_target(iface)?;
    let mut forward = target("forward", TargetKind::InterfaceLibrary, &[]);
    forward.usage.interface_link_libraries = Some(vec![link_item("$<TARGET_OBJECTS:obj>")]);
    model.add_target(forward)?;
    for (name, interface) in [("app", "iface"), ("explicit", "forward")] {
        let mut app = target(name, TargetKind::Executable, &["main.c"]);
        app.usage.link_libraries = Some(vec![link_item(interface)]);
        model.add_target(app)?;
    }
    let host = PureHost::default();
    host.seed_file(Path::new("/src/a.cpp"), b"")?;
    host.seed_file(Path::new("/src/main.c"), b"")?;

    let result = resolve(&model, &host, &[])?;
    let [obj, _, _, app, explicit] = result.targets.as_slice() else {
        return Err("Expected five resolved targets.".into());
    };
    let BuildStep::Link { inputs, step, .. } = link_build_step(&result.graphs[0], "app")? else {
        return Err("Expected a link step.".into());
    };
    assert_eq!(inputs, &[app.compile_units[0].object.clone()]);
    assert_eq!(step.language, "C");
    assert_eq!(app.compile_groups[0].defines, [entry("FROM_OBJ")]);
    assert!(explicit.dependencies.contains(&obj_id));
    let object = absolute_path(b"/out", &obj.compile_units[0].object);
    assert_object_input(&result.graphs[0], "explicit", &object)
}

#[test]
fn target_objects_source_supplies_the_link_language() -> TestResult {
    let mut model = project();
    let obj_id = model.add_target(target("cxx_obj", TargetKind::ObjectLibrary, &["main.cpp"]))?;
    model.add_target(target(
        "app",
        TargetKind::Executable,
        &["$<TARGET_OBJECTS:cxx_obj>"],
    ))?;
    let host = PureHost::default();
    host.seed_file(Path::new("/src/main.cpp"), b"")?;

    let result = resolve(&model, &host, &[])?;
    let [obj, app] = result.targets.as_slice() else {
        return Err("Expected two resolved targets.".into());
    };
    assert!(app.compile_units.is_empty());
    assert_eq!(app.dependencies, [obj_id]);
    assert_eq!(
        app.link.as_ref().map(|link| link.language.as_slice()),
        Some(b"CXX".as_slice())
    );
    assert_object_input(&result.graphs[0], "app", &obj.compile_units[0].object)
}

#[test]
fn external_object_source_is_linked_without_compilation() -> TestResult {
    let mut model = project();
    model.add_target(target(
        "app",
        TargetKind::Executable,
        &["main.c", "prebuilt.data"],
    ))?;
    let props = model
        .dir_mut(DirId::ROOT)
        .source_props
        .entry("/src/prebuilt.data".into())
        .or_default();
    props.set("EXTERNAL_OBJECT".into(), "ON".into());
    props.set("LANGUAGE".into(), "CXX".into());
    let host = PureHost::default();
    host.seed_file(Path::new("/src/main.c"), b"")?;
    host.seed_file(Path::new("/src/prebuilt.data"), b"")?;

    let result = resolve(&model, &host, &[])?;
    assert_eq!(result.targets[0].compile_units.len(), 1);
    assert_object_input(&result.graphs[0], "app", &"/src/prebuilt.data".into())
}

#[test]
fn relative_custom_dependencies_resolve_to_source_files_or_generated_outputs() -> TestResult {
    use rcmake_model::{CustomCommand, CustomCommandKind};

    let mut model = project();
    let command = |output| {
        let mut command = CustomCommand::new(
            DirId::ROOT,
            CustomCommandKind::Output,
            backtrace(7),
            Default::default(),
        );
        command.outputs.push(entry(output));
        command
    };
    let mut consumer = command("result");
    consumer.depends.extend(
        [
            "source.txt",
            "both.txt",
            "generated.txt",
            "byproduct.txt",
            "missing.txt",
        ]
        .map(entry),
    );
    model.add_command(consumer)?;
    model.add_command(command("both.txt"))?;
    let mut producer = command("generated.txt");
    producer.byproducts.push(entry("byproduct.txt"));
    model.add_command(producer)?;
    model
        .dir_mut(DirId::ROOT)
        .source_props
        .entry("/src/generated.txt".into())
        .or_default()
        .set("GENERATED".into(), "1".into());
    let host = PureHost::default();
    for source in ["/src/source.txt", "/src/both.txt"] {
        host.seed_file(Path::new(source), b"")?;
    }

    let result = resolve(&model, &host, &[])?;
    let Some(BuildStep::Custom(recipe)) = result.graphs[0].steps.first() else {
        return Err("Expected a custom command.".into());
    };
    assert_eq!(
        recipe.depends,
        [
            "/src/source.txt",
            "/src/both.txt",
            "generated.txt",
            "byproduct.txt",
            "missing.txt"
        ],
    );
    Ok(())
}

#[test]
fn custom_command_outputs_and_byproducts_are_sources_across_directories() -> TestResult {
    use rcmake_model::{CustomCommand, CustomCommandKind};

    let mut model = project();
    let producer = model.add_directory(
        DirId::ROOT,
        "/src/gen".into(),
        "/out/gen".into(),
        backtrace(3),
    )?;
    let mut command = CustomCommand::new(
        producer,
        CustomCommandKind::Output,
        backtrace(7),
        Default::default(),
    );
    command.outputs.push(entry("generated-$<CONFIG>.cpp"));
    command.byproducts.push(entry("generated-$<CONFIG>.o"));
    command
        .depends
        .push(entry("$<IF:$<CONFIG:Debug>,input1;input2,input3>"));
    command.working_directory = Some("stage-$<CONFIG>".into());
    command.comment = Some("Generate $<CONFIG> sources".into());
    command.depfile = Some("deps-$<CONFIG>.d".into());
    command.command_lines.push(vec![
        "echo".into(),
        "$<CONFIG>".into(),
        "$<0:empty>".into(),
        "a;b".into(),
    ]);
    model.add_command(command)?;
    model.add_target(target(
        "generated",
        TargetKind::ObjectLibrary,
        &["gen/generated-$<CONFIG>.cpp"],
    ))?;
    model.add_target(target(
        "app",
        TargetKind::Executable,
        &["$<TARGET_OBJECTS:generated>", "gen/generated-$<CONFIG>.o"],
    ))?;

    let result = resolve(&model, &PureHost::default(), &[ConfigId("Debug".into())])?;
    let [generated, app] = result.targets.as_slice() else {
        return Err("Expected two resolved targets.".into());
    };
    assert_eq!(
        generated.compile_units[0].source.value,
        "/out/gen/generated-Debug.cpp"
    );
    let graph = &result.graphs[0];
    let Some(BuildStep::Custom(recipe)) = graph.steps.first() else {
        return Err("Expected the source generator.".into());
    };
    assert_eq!(recipe.depends, ["gen/input1", "gen/input2"]);
    assert_eq!(recipe.depfile, Some("gen/deps-Debug.d".into()));
    assert_eq!(recipe.comment, Some("Generate Debug sources".into()));
    assert_eq!(
        recipe.invocations[0].working_dir,
        Some("/out/gen/stage-Debug".into())
    );
    assert_eq!(recipe.invocations[0].argv, ["echo", "Debug", "", "a;b"]);
    for target in &graph.targets {
        assert!(target.steps.iter().any(|&index| matches!(
            &graph.steps[index],
            BuildStep::Custom(step) if step.outputs == ["gen/generated-Debug.cpp"]
        )));
    }
    assert!(app.compile_units.is_empty());
    assert_object_input(graph, "app", &generated.compile_units[0].object)?;
    assert_object_input(graph, "app", &"gen/generated-Debug.o".into())
}

#[test]
fn transitive_compile_features_raise_the_standard_when_needed() -> TestResult {
    use rcmake_diag::{PolicyId, PolicyStatus};

    let mut model = project();
    for (key, value) in [
        ("CMAKE_CXX_STANDARD_DEFAULT", "17"),
        ("CMAKE_CXX_EXTENSIONS_DEFAULT", "ON"),
        ("CMAKE_C_STANDARD_DEFAULT", "90"),
        ("CMAKE_C_EXTENSIONS_DEFAULT", "ON"),
    ] {
        model
            .dir_mut(DirId::ROOT)
            .props
            .set(key.into(), value.into());
    }
    let mut features = target("features", TargetKind::InterfaceLibrary, &[]);
    features.usage.push(
        UsageField::InterfaceCompileFeatures,
        entry("cxx_std_98;cxx_std_14;c_static_assert"),
    );
    model.add_target(features)?;
    let mut bridge = target("bridge", TargetKind::InterfaceLibrary, &[]);
    bridge.usage.interface_link_libraries = Some(vec![link_item("features")]);
    model.add_target(bridge)?;

    for (name, sources, properties) in [
        (
            "explicit_cxx",
            vec!["main.cpp"],
            vec![("CXX_STANDARD", "11"), ("CXX_EXTENSIONS", "OFF")],
        ),
        ("default_cxx", vec!["main.cpp"], vec![]),
        (
            "strict_default_cxx",
            vec!["main.cpp"],
            vec![("CXX_EXTENSIONS", "OFF")],
        ),
        ("explicit_c", vec!["main.c"], vec![("C_STANDARD", "99")]),
    ] {
        let mut app = target(name, TargetKind::Executable, &sources);
        app.policies
            .policies
            .set(PolicyId::new(128), PolicyStatus::New)?;
        for (key, value) in properties {
            app.props.set(key.into(), value.into());
        }
        app.usage.link_libraries = Some(vec![link_item("bridge")]);
        model.add_target(app)?;
    }
    let host = PureHost::default();
    host.seed_file(Path::new("/src/main.cpp"), b"")?;
    host.seed_file(Path::new("/src/main.c"), b"")?;
    let result = resolve(&model, &host, &[])?;
    let graph = &result.graphs[0];
    for (name, expected) in [
        ("explicit_cxx", Some("-std=c++14")),
        ("default_cxx", None),
        ("strict_default_cxx", Some("-std=c++17")),
        ("explicit_c", Some("-std=gnu11")),
    ] {
        let invocation = graph
            .targets
            .iter()
            .find(|target| target.name == name)
            .and_then(|target| {
                target
                    .steps
                    .iter()
                    .find_map(|&index| match &graph.steps[index] {
                        BuildStep::Compile { invocation, .. } => Some(invocation),
                        _ => None,
                    })
            })
            .ok_or("Expected a compiler invocation.")?;
        let flags: Vec<_> = invocation
            .argv
            .iter()
            .filter(|arg| arg.starts_with(b"-std="))
            .cloned()
            .collect();
        assert_eq!(
            flags,
            expected.into_iter().map(BString::from).collect::<Vec<_>>(),
            "{name}"
        );
    }
    Ok(())
}

#[test]
fn install_file_paths_use_target_artifacts_and_the_install_configuration() -> TestResult {
    let mut model = project();
    let mut library = target("png_shared", TargetKind::SharedLibrary, &["png.c"]);
    library
        .props
        .set("LIBRARY_OUTPUT_DIRECTORY_DEBUG".into(), "/out/debug".into());
    library.props.set(
        "LIBRARY_OUTPUT_DIRECTORY_RELEASE".into(),
        "/out/release".into(),
    );
    model.add_target(library)?;
    model.add_install(InstallRule {
        dir: DirId::ROOT,
        backtrace: backtrace(15),
        kind: InstallKind::Files(vec![
            entry("$<TARGET_LINKER_FILE_DIR:png_shared>/libpng.dylib"),
            entry("$<$<CONFIG:Debug>:debug.txt>"),
        ]),
        options: InstallOptions {
            destination: Some("lib".into()),
            ..InstallOptions::default()
        },
    });
    let host = PureHost::default();
    host.seed_file(Path::new("/src/png.c"), b"")?;
    let configs = [ConfigId("Debug".into()), ConfigId("Release".into())];
    let result = resolve(&model, &host, &configs)?;
    let [debug, release] = result.install.actions.as_slice() else {
        return Err("Expected one install action per configuration.".into());
    };
    for (action, config, paths) in [
        (
            debug,
            "Debug",
            vec!["/out/debug/libpng.dylib", "/src/debug.txt"],
        ),
        (release, "Release", vec!["/out/release/libpng.dylib"]),
    ] {
        assert_eq!(action.configurations, [config]);
        let InstallActionKind::Files { files, .. } = &action.kind else {
            return Err("Expected a file install action.".into());
        };
        assert_eq!(files, &paths);
    }
    Ok(())
}

fn clang_scan(model: &mut ProjectModel) {
    for (key, value) in [
        ("CMAKE_CXX_COMPILER_ID", "Clang"),
        ("CMAKE_CXX_MODULE_MAP_FORMAT", "clang"),
        ("CMAKE_CXX_MODULE_MAP_FLAG", "@<MODULE_MAP_FILE>"),
        (
            "CMAKE_CXX_SCANDEP_SOURCE",
            "scan <CMAKE_CXX_COMPILER> <DEFINES> <INCLUDES> <FLAGS> <SOURCE> <DYNDEP_INTERMEDIATE_FILE> <OBJ_FILE> <DEP_FILE> <PREPROCESSED_OUTPUT_FILE>",
        ),
    ] {
        model.global.props.set(key.into(), value.into());
    }
}

fn module_set(model: &mut ProjectModel, target: TargetId, file: &str) -> TestResult {
    model.add_file_set(FileSet {
        target,
        name: "modules".into(),
        kind: FileSetKind::CxxModules,
        scope: UsageScope::Public,
        base_dirs: vec![entry("/src/lib")],
        files: vec![entry(file)],
        props: Default::default(),
    })?;
    Ok(())
}

#[test]
fn lowers_clang_module_scans_and_linked_module_inputs() -> TestResult {
    let mut model = project();
    let host = PureHost::default();
    host.seed_file(
        Path::new("/src/lib/detail/math.cppm"),
        b"export module math;",
    )?;
    host.seed_file(Path::new("/src/main.cpp"), b"import math;")?;
    clang_scan(&mut model);
    let mut library = target("math", TargetKind::StaticLibrary, &[]);
    library
        .usage
        .push(UsageField::CompileFeatures, entry("cxx_std_20"));
    library
        .usage
        .push(UsageField::CompileDefinitions, entry("TEST_MODULE"));
    library
        .usage
        .push(UsageField::IncludeDirectories, entry("/src/include path"));
    library
        .props
        .set("CXX_SCAN_FOR_MODULES".into(), "OFF".into());
    let library = model.add_target(library)?;
    module_set(&mut model, library, "lib/detail/math.cppm")?;

    let mut app = target("app", TargetKind::Executable, &["main.cpp"]);
    app.usage
        .push(UsageField::CompileFeatures, entry("cxx_std_20"));
    app.usage.link_libraries = Some(vec![link_item("math")]);
    app.policies
        .policies
        .set(PolicyId::new(155), PolicyStatus::New)?;
    model.add_target(app)?;

    let result = resolve(&model, &host, &[])?;
    let producer = result.targets[0]
        .cxx_modules
        .as_ref()
        .ok_or("Missing producer module info")?;
    let consumer = result.targets[1]
        .cxx_modules
        .as_ref()
        .ok_or("Missing consumer module info")?;
    assert_eq!(producer.units[0].relative_directory, "detail");
    assert_eq!(producer.units[0].visibility, ModuleVisibility::Public);
    assert_eq!(
        consumer.linked_target_dirs,
        std::slice::from_ref(&producer.module_dir)
    );
    let graph = &result.graphs[0];
    let [scan, collate, compile, ..] = graph.targets[0].steps.as_slice() else {
        return Err("Missing module build steps".into());
    };
    let BuildStep::Scan {
        invocation,
        ddi,
        object,
        depfile,
        ..
    } = &graph.steps[*scan]
    else {
        return Err("Missing scan step".into());
    };
    assert_eq!(
        invocation.argv,
        [
            BString::from("scan"),
            "/toolchain/c++".into(),
            "-DTEST_MODULE".into(),
            "-I/src/include path".into(),
            "-std=gnu++20".into(),
            "/src/lib/detail/math.cppm".into(),
            ddi.clone(),
            object.clone(),
            depfile.clone().ok_or("Missing scan dependency file")?,
            format!("{object}.ddi.i").into(),
        ]
    );
    assert!(
        matches!(&graph.steps[*collate], BuildStep::Collate { ddis, .. } if ddis == std::slice::from_ref(ddi))
    );
    assert!(matches!(&graph.steps[*compile], BuildStep::Compile {
        modmap: Some(map), dyndep: Some(dd), invocation, ..
    } if map == format!("{object}.modmap").as_str() && dd == &producer.dyndep
        && invocation.argv.contains(&BString::from(format!("@{map}")))));
    assert!(graph.steps.iter().any(|step| matches!(step, BuildStep::Collate {
        target, implicit_inputs, depend_info, ..
    } if target == "app"
        && implicit_inputs == &[consumer.tdi.clone(), format!("{}/CXXModules.json", producer.module_dir).into()]
        && depend_info.linked_target_dirs == consumer.linked_target_dirs)));
    Ok(())
}

#[test]
fn cpp20_without_a_scan_rule_only_errors_when_scanning_is_required() -> TestResult {
    let mut model = project();
    let host = PureHost::default();
    host.seed_file(Path::new("/src/main.cpp"), b"int main() {}")?;
    host.seed_file(Path::new("/src/lib/math.cppm"), b"export module math;")?;
    model
        .global
        .props
        .set("CMAKE_CXX_COMPILER_ID".into(), "AppleClang".into());
    let mut app = target("app", TargetKind::Executable, &["main.cpp"]);
    app.props.set("CXX_STANDARD".into(), "20".into());
    app.policies
        .policies
        .set(PolicyId::new(155), PolicyStatus::New)?;
    let app = model.add_target(app)?;

    let result = resolve(&model, &host, &[])?;
    assert!(!result.targets[0].compile_units[0].scan);
    assert!(result.targets[0].cxx_modules.is_none());

    model
        .target_mut(app)
        .ok_or("Missing app")?
        .props
        .set("CXX_SCAN_FOR_MODULES".into(), "ON".into());
    let error = resolve(&model, &host, &[]).expect_err("Explicit scanning must fail");
    assert!(
        error
            .to_string()
            .contains("compiler does not provide a way to discover the import graph")
    );

    model
        .target_mut(app)
        .ok_or("Missing app")?
        .props
        .set("CXX_SCAN_FOR_MODULES".into(), "OFF".into());
    module_set(&mut model, app, "lib/math.cppm")?;
    let error = resolve(&model, &host, &[]).expect_err("Module file sets require scanning");
    assert!(
        error
            .to_string()
            .contains("compiler does not provide a way to discover the import graph")
    );
    Ok(())
}

#[test]
fn module_scan_precedence_uses_file_set_source_target_and_policy() -> TestResult {
    let mut model = project();
    let host = PureHost::default();
    for name in ["file_set.cpp", "source.cpp", "target.cpp"] {
        host.seed_file(&Path::new("/src").join(name), b"int x;")?;
    }
    clang_scan(&mut model);
    let mut library = target(
        "library",
        TargetKind::ObjectLibrary,
        &["file_set.cpp", "source.cpp", "target.cpp"],
    );
    library.props.set("CXX_STANDARD".into(), "20".into());
    library
        .props
        .set("CXX_SCAN_FOR_MODULES".into(), "ON".into());
    let library = model.add_target(library)?;
    for file in ["/src/file_set.cpp", "/src/source.cpp"] {
        model
            .dir_mut(DirId::ROOT)
            .source_props
            .entry(file.into())
            .or_default()
            .set("CXX_SCAN_FOR_MODULES".into(), "OFF".into());
    }
    let mut set = FileSet {
        target: library,
        name: "ordinary".into(),
        kind: FileSetKind::Sources,
        scope: UsageScope::Private,
        base_dirs: vec![entry("/src")],
        files: vec![entry("file_set.cpp")],
        props: Default::default(),
    };
    set.props.set("CXX_SCAN_FOR_MODULES".into(), "ON".into());
    model.add_file_set(set)?;
    let result = resolve(&model, &host, &[])?;
    assert_eq!(
        result.targets[0]
            .compile_units
            .iter()
            .map(|unit| unit.scan)
            .collect::<Vec<_>>(),
        [true, false, true]
    );

    model
        .target_mut(library)
        .ok_or("Missing target")?
        .props
        .set("CXX_STANDARD".into(), "17".into());
    let result = resolve(&model, &host, &[])?;
    assert!(
        result.targets[0]
            .compile_units
            .iter()
            .all(|unit| !unit.scan)
    );
    Ok(())
}

#[test]
fn synthesizes_std_module_target_and_bmi_install_metadata() -> TestResult {
    let mut model = project();
    let host = PureHost::default();
    clang_scan(&mut model);
    for (file, contents) in [
        ("/src/main.cpp", "import std;"),
        ("/src/lib/part.cppm", "export module part;"),
        ("/toolchain/stdlib/std.cppm", "export module std;"),
        (
            "/toolchain/modules.json",
            r#"{"version":1,"modules":[{"logical-name":"std","source-path":"stdlib/std.cppm"}]}"#,
        ),
    ] {
        host.seed_file(Path::new(file), contents.as_bytes())?;
    }
    model.global.props.set(
        "CMAKE_CXX_STDLIB_MODULES_JSON".into(),
        "/toolchain/modules.json".into(),
    );
    let mut lib = target("lib", TargetKind::StaticLibrary, &["main.cpp"]);
    lib.props.set("CXX_STANDARD".into(), "23".into());
    lib.props.set("CXX_SCAN_FOR_MODULES".into(), "ON".into());
    let lib = model.add_target(lib)?;
    module_set(&mut model, lib, "lib/part.cppm")?;
    model.add_install(InstallRule {
        dir: DirId::ROOT,
        backtrace: backtrace(10),
        kind: InstallKind::Targets {
            targets: vec![lib],
            export: None,
        },
        options: InstallOptions::default(),
    });
    for (index, arg) in [
        "TARGETS",
        "lib",
        "FILE_SET",
        "modules",
        "DESTINATION",
        "include/modules",
        "CXX_MODULES_BMI",
        "DESTINATION",
        "bmi",
        "COMPONENT",
        "sdk",
        "OPTIONAL",
    ]
    .into_iter()
    .enumerate()
    {
        model
            .global
            .props
            .set(format!("_RCMAKE_INSTALL_0_ARG{index}").into(), arg.into());
    }

    let result = resolve(&model, &host, &[])?;
    let std = &result.targets[1];
    assert_eq!(std.metadata.name, STD_MODULE_TARGET);
    assert!(std.metadata.generator_provided);
    let std_modules = std
        .cxx_modules
        .as_ref()
        .ok_or("Missing standard library modules")?;
    assert!(std_modules.units[0].bmi_only);
    let modules = result.targets[0]
        .cxx_modules
        .as_ref()
        .ok_or("Missing library modules")?;
    assert_eq!(
        modules.linked_target_dirs,
        ["CMakeFiles/@cmake_cxx_std.dir"]
    );
    let install = modules
        .bmi_install
        .as_ref()
        .ok_or("Missing BMI installation")?;
    assert_eq!(install.destination, "bmi");
    assert_eq!(install.component, "sdk");
    assert!(install.optional);
    assert_eq!(result.graphs[0].targets[0].depends, [1]);
    assert!(!result.graphs[0].targets[1].in_all);
    Ok(())
}

#[test]
fn xcode_ide_facts_keep_raw_settings_and_resolve_apple_artifacts() -> TestResult {
    use ide::{BundleKind, IdeFlavor, IdeLinkItem, IdeSourceKind};
    use rcmake_model::GeneratorKind;

    let mut model = project();
    model.generator = Some(GeneratorKind::Xcode);
    model
        .global
        .props
        .set("CMAKE_XCODE_ARCHS".into(), "arm64".into());
    model
        .global
        .props
        .set("CMAKE_C_FLAGS_DEBUG".into(), "-g".into());
    let obj = model.add_target(target("obj", TargetKind::ObjectLibrary, &["core.c"]))?;
    let mut fw = target("fw", TargetKind::SharedLibrary, &["core.c"]);
    fw.props.set("FRAMEWORK".into(), "ON".into());
    let fw = model.add_target(fw)?;
    let plain = model.add_target(target("tool", TargetKind::Executable, &["main.c"]))?;
    let archive = model.add_target(target("archive", TargetKind::StaticLibrary, &["core.c"]))?;
    let mut app = target(
        "app",
        TargetKind::Executable,
        &["main.c", "$<TARGET_OBJECTS:obj>"],
    );
    for (name, value) in [
        ("MACOSX_BUNDLE", "ON"),
        ("MACOSX_BUNDLE_GUI_IDENTIFIER", "org.example.app"),
        ("MACOSX_BUNDLE_INFO_PLIST", "Custom.plist.in"),
        ("PUBLIC_HEADER", "public.h"),
        ("RESOURCE", "image.png"),
        (
            "XCODE_ATTRIBUTE_TEST[variant=Debug][arch=arm64]",
            "debug $<CONFIG>",
        ),
        ("XCODE_ATTRIBUTE_TEST[arch=arm64]", "generic $<CONFIG>"),
        (
            "XCODE_ATTRIBUTE_FILTER[sdk=macosx*][variant=Debug]",
            "filtered",
        ),
        ("XCODE_GENERATE_SCHEME", "ON"),
        ("XCODE_SCHEME_ARGUMENTS", "an argument;second"),
        ("COMPILE_FLAGS", "-DNAME=\"two words\""),
    ] {
        app.props.set(name.into(), value.into());
    }
    for (field, value) in [
        (UsageField::CompileOptions, "-fno-common"),
        (UsageField::CompileDefinitions, "APP_$<CONFIG>"),
        (UsageField::IncludeDirectories, "/path with spaces"),
        (UsageField::IncludeDirectories, "/sdk/Kit.framework/Headers"),
        (UsageField::LinkDirectories, "/link path/$<CONFIG>"),
        (UsageField::LinkOptions, "SHELL:-exported_symbol _main"),
    ] {
        app.usage.push(field, entry(value));
    }
    app.props.set(
        "SYSTEM_INCLUDE_DIRECTORIES".into(),
        "/path with spaces".into(),
    );
    app.usage.link_libraries = Some(
        ["fw", "z", "/vendor/lib.a", "-framework Cocoa", "-pthread"]
            .into_iter()
            .map(link_item)
            .collect(),
    );
    let app = model.add_target(app)?;
    model.add_file_set(FileSet {
        target: app,
        name: "headers".into(),
        kind: FileSetKind::Headers,
        scope: UsageScope::Private,
        base_dirs: vec![entry("/src")],
        files: vec![entry("/src/file_set.h")],
        props: Default::default(),
    })?;
    let mut event = rcmake_model::CustomCommand::new(
        DirId::ROOT,
        rcmake_model::CustomCommandKind::TargetEvent {
            target: app,
            event: rcmake_model::TargetEvent::PreLink,
        },
        Backtrace::default(),
        Default::default(),
    );
    event
        .command_lines
        .push(vec!["echo".into(), "two words $<CONFIG>".into()]);
    model.add_command(event)?;

    let source = model
        .dir_mut(DirId::ROOT)
        .source_props
        .entry("/src/main.c".into())
        .or_default();
    source.set("COMPILE_FLAGS".into(), "-DSOURCE=\"$<CONFIG> x\"".into());
    source.set("COMPILE_OPTIONS".into(), "-Wextra".into());
    source.set("COMPILE_DEFINITIONS".into(), "SOURCE=$<CONFIG>".into());
    source.set("SOURCE_GROUP".into(), "App\\Code".into());
    let host = PureHost::default();
    for path in [
        "/src/core.c",
        "/src/main.c",
        "/src/public.h",
        "/src/file_set.h",
        "/src/image.png",
    ] {
        host.seed_file(Path::new(path), b"")?;
    }

    let configs = [ConfigId("Debug".into()), ConfigId("Release".into())];
    let result = resolve_with_options(
        &model,
        &host,
        &configs,
        &ResolveOptions {
            ide: Some(IdeFlavor::Xcode),
        },
    )?;
    let resolved = |id, config| {
        result
            .targets
            .iter()
            .find(|target| target.id == id && target.config.0 == config)
            .ok_or("Missing target.")
    };
    let debug = resolved(app, "Debug")?;
    let facts = debug.ide.as_deref().ok_or("Missing Xcode facts.")?;
    assert_eq!(result.graphs.len(), 2);
    assert_eq!(
        debug.artifacts.file,
        Some("/out/Debug/app.app/Contents/MacOS/app".into())
    );
    assert_eq!(
        resolved(plain, "Debug")?.artifacts.file,
        Some("/out/Debug/tool".into())
    );
    assert_eq!(
        resolved(archive, "Debug")?.artifacts.file,
        Some("/out/Debug/libarchive.a".into())
    );
    assert_eq!(
        resolved(fw, "Debug")?.artifacts.file,
        Some("/out/Debug/fw.framework/Versions/A/fw".into())
    );
    assert_eq!(
        resolved(fw, "Debug")?
            .ide
            .as_deref()
            .ok_or("Missing framework facts.")?
            .bundle,
        BundleKind::Framework {
            version: "A".into()
        }
    );
    assert_eq!(facts.bundle, BundleKind::App);
    assert_eq!(
        facts.info_plist,
        Some("/out/CMakeFiles/app.dir/Info.plist".into())
    );
    assert_eq!(facts.naming.dir, "/out/Debug");
    assert_eq!(facts.naming.base_name, "app");
    assert_eq!(
        facts.attributes[b"MACOSX_BUNDLE_GUI_IDENTIFIER".as_slice()],
        "org.example.app"
    );
    assert_eq!(
        facts.attributes[b"MACOSX_BUNDLE_INFO_PLIST".as_slice()],
        "Custom.plist.in"
    );
    assert_eq!(
        facts.attributes[b"XCODE_SCHEME_ARGUMENTS".as_slice()],
        "an argument;second"
    );
    assert_eq!(
        facts.attributes[b"XCODE_ATTRIBUTE_TEST[arch=arm64]".as_slice()],
        "debug Debug"
    );
    assert_eq!(
        facts.attributes[b"XCODE_ATTRIBUTE_FILTER[sdk=macosx*]".as_slice()],
        "filtered"
    );
    let release = resolved(app, "Release")?
        .ide
        .as_deref()
        .ok_or("Missing Release facts.")?;
    assert_eq!(
        release.attributes[b"XCODE_ATTRIBUTE_TEST[arch=arm64]".as_slice()],
        "generic Release"
    );
    assert!(
        !release
            .attributes
            .contains_key(b"XCODE_ATTRIBUTE_FILTER[sdk=macosx*]".as_slice())
    );

    let compile = &facts.compile[0];
    assert_eq!(
        compile
            .flags
            .iter()
            .map(|flag| flag.value.as_slice())
            .collect::<Vec<_>>(),
        [
            b"SHELL:-g".as_slice(),
            b"SHELL:-DNAME=\"two words\"",
            b"-fno-common"
        ]
    );
    assert_eq!(compile.defines[0].value, "APP_Debug");
    assert_eq!(
        compile.includes,
        [
            ide::IdeInclude {
                path: "/sdk".into(),
                system: false,
                framework: true
            },
            ide::IdeInclude {
                path: "/path with spaces".into(),
                system: true,
                framework: false
            },
        ]
    );
    let main = facts
        .sources
        .iter()
        .find(|source| source.path == "/src/main.c")
        .ok_or("Missing main.c.")?;
    assert_eq!(main.group, "App\\Code");
    let overrides = main
        .compile
        .as_ref()
        .ok_or("Missing source compile settings.")?;
    assert_eq!(overrides.flags[0].value, "SHELL:-DSOURCE=\"Debug x\"");
    assert_eq!(overrides.flags[1].value, "-Wextra");
    assert_eq!(overrides.defines[0].value, "SOURCE=Debug");
    assert!(
        facts
            .sources
            .iter()
            .any(|source| source.path == "/src/public.h" && source.kind == IdeSourceKind::Header)
    );
    assert!(facts.sources.iter().any(|source| source.path == "/src/image.png" && source.kind == IdeSourceKind::Resource));
    assert!(facts.sources.iter().any(|source| source.path
        == "/out/build/obj.build/Debug/Objects-normal/arm64/core.o"
        && source.kind == IdeSourceKind::Object));

    assert!(facts.sources.iter().any(|source| {
        source.path == "/src/file_set.h"
            && source
                .file_set
                .as_ref()
                .is_some_and(|name| name == "headers")
            && source.kind == IdeSourceKind::Header
    }));
    assert_eq!(facts.custom.len(), 1);
    assert_eq!(facts.custom[0].phase, ide::IdeCustomPhase::PreLink);
    assert_eq!(facts.custom[0].commands[0], ["echo", "two words Debug"]);
    assert_eq!(facts.custom[0].working_dir, "/out");

    assert_eq!(facts.link_options[0].value, "SHELL:-exported_symbol _main");
    assert_eq!(facts.link_directories, ["/link path/Debug"]);
    assert_eq!(
        facts.link_items,
        [
            IdeLinkItem::Target(fw),
            IdeLinkItem::Library("z".into()),
            IdeLinkItem::File("/vendor/lib.a".into()),
            IdeLinkItem::Framework("Cocoa".into()),
            IdeLinkItem::Flag("-pthread".into()),
        ]
    );
    let usage = UsageResolver::new(&model);
    assert_eq!(
        usage.target_objects(obj, &configs[0], None),
        Some("/out/build/obj.build/Debug/Objects-normal/arm64/core.o".into())
    );
    model
        .target_mut(obj)
        .ok_or("Missing object library.")?
        .props
        .set("OSX_ARCHITECTURES".into(), "arm64;x86_64".into());
    assert_eq!(
        UsageResolver::new(&model).target_objects(obj, &configs[0], None),
        Some("/out/build/obj.build/Debug/Objects-normal/$(CURRENT_ARCH)/core.o".into())
    );
    Ok(())
}

#[test]
fn visual_studio_layout_and_per_configuration_source_union() -> TestResult {
    use ide::{IdeFlavor, IdeLinkItem};
    let mut model = project();
    let obj = model.add_target(target("obj", TargetKind::ObjectLibrary, &["obj.c"]))?;
    let lib = model.add_target(target("support", TargetKind::StaticLibrary, &["lib.c"]))?;
    let dll = model.add_target(target("shared", TargetKind::SharedLibrary, &["lib.c"]))?;
    let mut app = target(
        "app",
        TargetKind::Executable,
        &[
            "main.c",
            "$<$<CONFIG:Debug>:debug.c>",
            "$<$<CONFIG:Release>:release.c>",
        ],
    );
    for (name, value) in [
        ("FOLDER", "Apps/$<CONFIG>"),
        ("VS_GLOBAL_RootNamespace", "tools"),
        ("VS_DEBUGGER_COMMAND_ARGUMENTS", "--config $<CONFIG>"),
        ("VS_DPI_AWARE", "PerMonitor"),
    ] {
        app.props.set(name.into(), value.into());
    }
    app.usage.link_libraries = Some(
        ["support", "shared", "kernel32.lib"]
            .into_iter()
            .map(link_item)
            .collect(),
    );
    let app = model.add_target(app)?;
    let host = PureHost::default();
    for path in [
        "/src/main.c",
        "/src/debug.c",
        "/src/release.c",
        "/src/lib.c",
        "/src/obj.c",
    ] {
        host.seed_file(Path::new(path), b"")?;
    }
    let result = resolve_with_options(
        &model,
        &host,
        &[ConfigId("Debug".into()), ConfigId("Release".into())],
        &ResolveOptions {
            ide: Some(IdeFlavor::VisualStudio),
        },
    )?;
    let debug = |id| {
        result
            .targets
            .iter()
            .find(|target| target.id == id && target.config.0 == "Debug")
            .ok_or("Missing Debug target.")
    };
    let facts = debug(app)?
        .ide
        .as_deref()
        .ok_or("Missing Visual Studio facts.")?;
    assert_eq!(
        debug(app)?.artifacts.file,
        Some("/out/Debug/app.exe".into())
    );
    assert_eq!(
        debug(lib)?.artifacts.file,
        Some("/out/Debug/support.lib".into())
    );
    assert_eq!(
        debug(dll)?.artifacts.file,
        Some("/out/Debug/shared.dll".into())
    );
    assert_eq!(
        debug(dll)?.artifacts.linker_file,
        Some("/out/Debug/shared.lib".into())
    );
    assert_eq!(debug(obj)?.compile_units[0].object, "obj.dir/Debug/obj.obj");
    assert_eq!(
        facts.attributes[b"VS_GLOBAL_RootNamespace".as_slice()],
        "tools"
    );
    assert_eq!(
        facts.attributes[b"VS_DEBUGGER_COMMAND_ARGUMENTS".as_slice()],
        "--config Debug"
    );
    assert_eq!(facts.attributes[b"VS_DPI_AWARE".as_slice()], "PerMonitor");
    assert_eq!(facts.attributes[b"FOLDER".as_slice()], "Apps/Debug");
    assert_eq!(
        facts.link_items,
        [
            IdeLinkItem::Target(lib),
            IdeLinkItem::Target(dll),
            IdeLinkItem::Library("kernel32.lib".into()),
        ]
    );
    for resolved in result.targets.iter().filter(|target| target.id == app) {
        let facts = resolved.ide.as_deref().ok_or("Missing source union.")?;
        assert_eq!(facts.sources.len(), 3);
        assert_eq!(facts.sources[0].path, "/src/main.c");
        assert!(facts.sources[0].excluded_configs.is_empty());
        assert_eq!(facts.sources[1].path, "/src/debug.c");
        assert_eq!(facts.sources[1].excluded_configs, ["Release"]);
        assert_eq!(facts.sources[2].path, "/src/release.c");
        assert_eq!(facts.sources[2].excluded_configs, ["Debug"]);
    }
    Ok(())
}

#[test]
fn xcode_rejects_sources_that_vary_by_configuration() -> TestResult {
    let mut model = project();
    let mut pc = target(
        "pc",
        TargetKind::Executable,
        &["main.c", "$<$<CONFIG:Debug,DebugExtra>:core.c>"],
    );
    pc.props.set("RESOURCE".into(), "image.png".into());
    model.add_target(pc)?;
    let host = PureHost::default();
    for path in ["/src/main.c", "/src/core.c"] {
        host.seed_file(Path::new(path), b"")?;
    }
    let result = resolve_with_options(
        &model,
        &host,
        &[
            ConfigId("Debug".into()),
            ConfigId("DebugExtra".into()),
            ConfigId("Release".into()),
            ConfigId("MinSizeRel".into()),
            ConfigId("RelWithDebInfo".into()),
        ],
        &ResolveOptions {
            ide: Some(ide::IdeFlavor::Xcode),
        },
    );
    assert_eq!(result.err(), Some(ResolveError::Message(concat!(
        "Target \"pc\" has source files which vary by configuration. This is not supported by the \"Xcode\" generator.",
        "\n\nConfig \"Debug\":\n\n  /src/main.c\n  /src/core.c",
        "\n\nConfig \"Release\":\n\n  /src/main.c",
    ).into())));
    Ok(())
}

#[test]
fn default_resolve_keeps_ninja_and_make_layouts() -> TestResult {
    use rcmake_model::GeneratorKind;
    let mut model = project();
    let obj = model.add_target(target("obj", TargetKind::ObjectLibrary, &["core.c"]))?;
    let mut app = target(
        "app",
        TargetKind::Executable,
        &["main.c", "$<TARGET_OBJECTS:obj>"],
    );
    app.props.set("MACOSX_BUNDLE".into(), "ON".into());
    let app = model.add_target(app)?;
    let host = PureHost::default();
    for path in ["/src/main.c", "/src/core.c"] {
        host.seed_file(Path::new(path), b"")?;
    }
    for (generator, object, artifact) in [
        (
            GeneratorKind::Ninja,
            "CMakeFiles/obj.dir/core.c.o",
            "/out/app",
        ),
        (
            GeneratorKind::NinjaMultiConfig,
            "CMakeFiles/obj.dir/Debug/core.c.o",
            "/out/Debug/app",
        ),
        (
            GeneratorKind::UnixMakefiles,
            "CMakeFiles/obj.dir/core.c.o",
            "/out/app",
        ),
    ] {
        model.generator = Some(generator);
        let configs = [ConfigId("Debug".into())];
        let legacy = resolve(&model, &host, &configs)?;
        let explicit =
            resolve_with_options(&model, &host, &configs, &ResolveOptions { ide: None })?;
        assert_eq!(legacy.graphs, explicit.graphs);
        assert!(
            legacy
                .targets
                .iter()
                .chain(&explicit.targets)
                .all(|target| target.ide.is_none())
        );
        let resolved = |id| {
            legacy
                .targets
                .iter()
                .find(|target| target.id == id)
                .ok_or("Missing target.")
        };
        assert_eq!(resolved(obj)?.compile_units[0].object, object);
        assert_eq!(resolved(app)?.artifacts.file, Some(artifact.into()));
        assert!(legacy.graphs[0].steps.iter().any(|step| matches!(step,
            BuildStep::Link { inputs, .. } if inputs.iter().any(|input| input == object))));
    }
    Ok(())
}

#[test]
fn xcode_reports_configuration_with_no_sources_before_link_language_error() -> TestResult {
    let mut model = project();
    model.add_target(target(
        "pc",
        TargetKind::Executable,
        &["$<$<CONFIG:Debug>:main.c>"],
    ))?;
    let host = PureHost::default();
    host.seed_file(Path::new("/src/main.c"), b"")?;
    let result = resolve_with_options(
        &model,
        &host,
        &[ConfigId("Debug".into()), ConfigId("Release".into())],
        &ResolveOptions {
            ide: Some(ide::IdeFlavor::Xcode),
        },
    );
    assert_eq!(result.err(), Some(ResolveError::Message(concat!(
        "Target \"pc\" has source files which vary by configuration. This is not supported by the \"Xcode\" generator.",
        "\n\nConfig \"Debug\":\n\n  /src/main.c",
        "\n\nConfig \"Release\":",
    ).into())));
    Ok(())
}

#[test]
fn xcode_uses_root_object_directory_for_subdirectory_targets() -> TestResult {
    use ide::{IdeFlavor, IdeSourceKind};

    let mut model = project();
    let sub = model.add_directory(
        DirId::ROOT,
        "/src/sub".into(),
        "/out/sub".into(),
        Default::default(),
    )?;
    let mut obj = target("obj", TargetKind::ObjectLibrary, &["core.c"]);
    obj.dir = sub;
    obj.props
        .set("OSX_ARCHITECTURES_DEBUG".into(), "ignored".into());
    model.add_target(obj)?;
    let app = model.add_target(target(
        "app",
        TargetKind::Executable,
        &["main.c", "$<TARGET_OBJECTS:obj>"],
    ))?;
    let host = PureHost::default();
    for path in ["/src/main.c", "/src/sub/core.c"] {
        host.seed_file(Path::new(path), b"")?;
    }
    let debug = ConfigId("Debug".into());
    for (key, value, arch) in [
        ("CMAKE_XCODE_ARCHS", "x86_64;arm64", "x86_64"),
        ("_CMAKE_APPLE_ARCHS_DEFAULT", "arm64", "arm64"),
        ("CMAKE_OSX_ARCHITECTURES", "x86_64", "x86_64"),
        ("CMAKE_OSX_SYSROOT", "/SDKs/iPhoneOS.sdk", "$(CURRENT_ARCH)"),
    ] {
        model
            .dir_mut(DirId::ROOT)
            .vars
            .insert(key.into(), value.into());
        let result = resolve_with_options(
            &model,
            &host,
            std::slice::from_ref(&debug),
            &ResolveOptions {
                ide: Some(IdeFlavor::Xcode),
            },
        )?;
        let object = format!("/out/build/obj.build/Debug/Objects-normal/{arch}/core.o");
        let facts = result
            .targets
            .iter()
            .find(|target| target.id == app)
            .and_then(|target| target.ide.as_deref())
            .ok_or("Missing IDE facts.")?;
        assert!(
            facts
                .sources
                .iter()
                .any(|source| source.kind == IdeSourceKind::Object && source.path == object)
        );
    }
    Ok(())
}

#[test]
fn xcode_evaluates_embed_paths_and_rejects_per_config_embed_paths() -> TestResult {
    use ide::IdeFlavor;

    let mut model = project();
    let mut app = target("app", TargetKind::Executable, &["main.c"]);
    app.props.set(
        "XCODE_EMBED_FRAMEWORKS".into(),
        "$<1:Vendor.framework>".into(),
    );
    let app = model.add_target(app)?;
    let host = PureHost::default();
    host.seed_file(Path::new("/src/main.c"), b"")?;
    let configs = [ConfigId("Debug".into())];
    let options = ResolveOptions {
        ide: Some(IdeFlavor::Xcode),
    };
    let result = resolve_with_options(&model, &host, &configs, &options)?;
    let facts = result
        .targets
        .iter()
        .find(|target| target.id == app)
        .and_then(|target| target.ide.as_deref())
        .ok_or("Missing IDE facts.")?;
    assert_eq!(
        facts.attributes[b"XCODE_EMBED_FRAMEWORKS".as_slice()],
        "Vendor.framework"
    );
    model.target_mut(app).ok_or("Missing target.")?.props.set(
        "XCODE_EMBED_FRAMEWORKS".into(),
        "$<$<CONFIG:Debug>:Vendor.framework>".into(),
    );
    let error = resolve_with_options(&model, &host, &configs, &options).err();
    assert_eq!(
        error,
        Some(ResolveError::Message(
            concat!(
                "Xcode does not support per-config XCODE_EMBED_FRAMEWORKS:",
                "\n\n  $<$<CONFIG:Debug>:Vendor.framework>",
                "\n\nspecified for target:\n\n  app",
            )
            .into()
        ))
    );
    Ok(())
}
