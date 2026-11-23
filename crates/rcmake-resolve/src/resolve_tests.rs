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
            (b"/out/libmath.a", LinkFragmentRole::Libraries),
            (b"-lm", LinkFragmentRole::Libraries),
            (b"-F/SDK/Frameworks", LinkFragmentRole::FrameworkPath),
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
