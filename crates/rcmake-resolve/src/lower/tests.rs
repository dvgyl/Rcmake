use rcmake_base::BString;
use rcmake_genex::ConfigId;
use rcmake_model::{
    BT, CacheEntry, CacheType, CustomCommand, CustomCommandKind, DirId, ImportedVisibility,
    InstallKind, InstallOptions, InstallRule, ProjectModel, Target, TargetEvent, TargetKind,
    TargetOrigin,
};

use super::*;
use crate::{
    BuildStep, CompileGroup, CompileUnit, InstallActionKind, InterfaceInclude, LinkFragment,
    LinkFragmentRole, LinkKind, LinkStep, ResolvedTarget, TargetMetadata,
};

type TestResult = Result<(), Box<dyn std::error::Error>>;

fn fixture(system: &str) -> ProjectModel {
    let mut model = ProjectModel::default();
    model.set_root_dirs("/project".into(), "/build".into());
    var(&mut model, "CMAKE_SYSTEM_NAME", system);
    model
}

fn var(model: &mut ProjectModel, name: &str, value: &str) {
    model.global.props.set(name.into(), value.into());
}

fn bt(value: &str) -> BT<BString> {
    BT::new(value.into(), Default::default())
}

fn raw_target(name: &str, kind: TargetKind) -> Target {
    Target::new(
        name.into(),
        kind,
        TargetOrigin::Normal,
        DirId::ROOT,
        Default::default(),
        Default::default(),
    )
}

fn resolve_target(
    model: &mut ProjectModel,
    raw: Target,
    config: &ConfigId,
    sources: &[&str],
) -> Result<ResolvedTarget, rcmake_model::ModelError> {
    let dir = model.dir(raw.dir);
    let mut compile_groups: Vec<CompileGroup> = Vec::new();
    let mut compile_units = Vec::new();
    for source in sources {
        let Some(language) = source_language(model, &raw, source.as_bytes()) else {
            continue;
        };
        let group = compile_groups
            .iter()
            .position(|group| group.language == language)
            .unwrap_or_else(|| {
                let index = compile_groups.len();
                compile_groups.push(CompileGroup {
                    language,
                    ..CompileGroup::default()
                });
                index
            });
        compile_units.push(CompileUnit {
            source: bt(source),
            object: object_path(model, &raw, config, source.as_bytes()),
            group,
            scan: false,
        });
    }
    let metadata = TargetMetadata {
        name: raw.name.clone(),
        kind: raw.kind,
        source_dir: dir.source_dir.clone(),
        binary_dir: dir.binary_dir.clone(),
        backtrace: raw.backtrace.clone(),
        folder: None,
        exclude_from_all: false,
        generator_provided: false,
    };
    let artifacts = compute_artifacts(model, &raw, config);
    Ok(ResolvedTarget {
        id: model.add_target(raw)?,
        config: config.clone(),
        metadata,
        artifacts,
        compile_groups,
        compile_units,
        link: None,
        dependencies: Vec::new(),
        cxx_modules: None,
        ide: None,
    })
}

#[test]
fn expands_known_placeholders_only() {
    let out = expand_rule(
        b"<CMAKE_C_COMPILER> <FLAGS> -o <OBJECT> -c <SOURCE> <x> <",
        |n| match n {
            b"FLAGS" => Some("-O2".into()),
            b"OBJECT" => Some("a.o".into()),
            b"SOURCE" => Some("a.c".into()),
            _ => None,
        },
    );
    assert_eq!(out, "<CMAKE_C_COMPILER> -O2 -o a.o -c a.c <x> <");
}

#[test]
fn executable_rules_preserve_paths_and_library_inputs() -> TestResult {
    let mut model = fixture("Linux");
    let config = ConfigId("Debug".into());
    var(&mut model, "CMAKE_C_COMPILER", "/tool dir/cc");
    var(
        &mut model,
        "CMAKE_C_COMPILE_OBJECT",
        "<CMAKE_C_COMPILER> <FLAGS> <DEFINES> <INCLUDES> -MD -MF <DEP_FILE> -c <SOURCE> -o <OBJECT>",
    );
    var(
        &mut model,
        "CMAKE_C_LINK_EXECUTABLE",
        "<CMAKE_C_COMPILER> <FLAGS> <OBJECTS> <LINK_FLAGS> <LINK_LIBRARIES> -o <TARGET>",
    );
    var(&mut model, "CMAKE_EXE_LINKER_FLAGS_DEBUG", "-g");
    let mut main = raw_target("main", TargetKind::Executable);
    main.props.set("OUTPUT_NAME_DEBUG".into(), "my app".into());
    main.props
        .set("RUNTIME_OUTPUT_DIRECTORY_DEBUG".into(), "my bin".into());
    let mut target = resolve_target(&mut model, main, &config, &["main file.c"])?;
    assert_eq!(target.artifacts.file, Some("/build/my bin/my app".into()));
    target.link = Some(LinkStep {
        kind: LinkKind::Executable,
        language: "C".into(),
        fragments: vec![
            LinkFragment {
                role: LinkFragmentRole::LibraryPath,
                text: bt("-L'/other libs'"),
            },
            LinkFragment {
                role: LinkFragmentRole::Libraries,
                text: bt("'/other libs/libx.a' -lm"),
            },
        ],
        output: BString::default(),
    });
    let graph = lower(&model, &[target], &config)?;
    let [
        BuildStep::Compile {
            invocation,
            depfile,
            ..
        },
        BuildStep::Link {
            inputs,
            step,
            invocations,
        },
    ] = graph.steps.as_slice()
    else {
        return Err("Expected a compile step and a link step.".into());
    };
    assert_eq!(
        invocation.argv,
        [
            "/tool dir/cc",
            "-MD",
            "-MF",
            "CMakeFiles/main.dir/main_file.c.o.d",
            "-c",
            "/project/main file.c",
            "-o",
            "CMakeFiles/main.dir/main_file.c.o"
        ]
    );
    assert_eq!(*depfile, Some("CMakeFiles/main.dir/main_file.c.o.d".into()));
    assert_eq!(step.output, "my bin/my app");
    assert_eq!(
        *inputs,
        ["CMakeFiles/main.dir/main_file.c.o", "/other libs/libx.a"]
    );
    let [linker] = invocations.as_slice() else {
        return Err("Expected one linker invocation.".into());
    };
    assert_eq!(
        linker.argv,
        [
            "/tool dir/cc",
            "CMakeFiles/main.dir/main_file.c.o",
            "-g",
            "-L/other libs",
            "/other libs/libx.a",
            "-lm",
            "-o",
            "my bin/my app"
        ]
    );
    Ok(())
}

#[test]
fn archive_create_finish_and_cleanup_use_configured_tools() -> TestResult {
    let mut model = fixture("Linux");
    let config = ConfigId::default();
    var(&mut model, "CMAKE_AR", "/tools dir/ar");
    var(&mut model, "CMAKE_RANLIB", "custom-ranlib");
    var(
        &mut model,
        "CMAKE_C_ARCHIVE_CREATE",
        "<CMAKE_AR> qc <TARGET> <OBJECTS>",
    );
    var(
        &mut model,
        "CMAKE_C_ARCHIVE_FINISH",
        "<CMAKE_RANLIB> -D <TARGET>",
    );
    let mut raw = raw_target("stuff", TargetKind::StaticLibrary);
    raw.props
        .set("ARCHIVE_OUTPUT_DIRECTORY".into(), "lib".into());
    raw.props.set("SUFFIX".into(), ".archive".into());
    let target = resolve_target(&mut model, raw, &config, &["a.c", "b.c"])?;
    let graph = lower(&model, &[target], &config)?;
    let Some(BuildStep::Link { invocations, .. }) = graph.steps.last() else {
        return Err("Expected a link step.".into());
    };
    let [cleanup, create, finish] = invocations.as_slice() else {
        return Err("Expected three archive invocations.".into());
    };
    assert_eq!(
        cleanup.argv,
        ["cmake", "-E", "rm", "-f", "lib/libstuff.archive"]
    );
    assert_eq!(
        create.argv,
        [
            "/tools dir/ar",
            "qc",
            "lib/libstuff.archive",
            "CMakeFiles/stuff.dir/a.c.o",
            "CMakeFiles/stuff.dir/b.c.o"
        ]
    );
    assert_eq!(finish.argv, ["custom-ranlib", "-D", "lib/libstuff.archive"]);
    Ok(())
}

#[test]
fn darwin_shared_library_uses_install_name_versions_and_symlinks() -> TestResult {
    let mut model = fixture("Darwin");
    let config = ConfigId("Debug".into());
    var(
        &mut model,
        "CMAKE_C_CREATE_SHARED_LIBRARY",
        "<CMAKE_C_COMPILER> <LANGUAGE_COMPILE_FLAGS> <LINK_FLAGS> -o <TARGET> <SONAME_FLAG> <TARGET_INSTALLNAME_DIR><TARGET_SONAME> <OBJECTS> <LINK_LIBRARIES>",
    );
    let mut raw = raw_target("foo", TargetKind::SharedLibrary);
    for (key, value) in [
        ("VERSION", "1.2.3"),
        ("SOVERSION", "1"),
        ("OUTPUT_NAME_DEBUG", "thing"),
        ("DEBUG_POSTFIX", "_dbg"),
        ("BUILD_RPATH", "/sample path;/another"),
    ] {
        raw.props.set(key.into(), value.into());
    }
    let darwin = compute_artifacts(&model, &raw, &config);
    assert_eq!(darwin.file, Some("/build/libthing_dbg.1.2.3.dylib".into()));
    assert_eq!(
        darwin.soname_file,
        Some("/build/libthing_dbg.1.dylib".into())
    );
    assert_eq!(darwin.linker_file, Some("/build/libthing_dbg.dylib".into()));
    let linux = compute_artifacts(&fixture("Linux"), &raw, &config);
    assert_eq!(linux.file, Some("/build/libthing_dbg.so.1.2.3".into()));
    assert_eq!(linux.soname_file, Some("/build/libthing_dbg.so.1".into()));
    assert_eq!(linux.linker_file, Some("/build/libthing_dbg.so".into()));
    let target = resolve_target(&mut model, raw, &config, &["a.c"])?;
    let graph = lower(&model, &[target], &config)?;
    let Some(BuildStep::Link { invocations, .. }) = graph.steps.last() else {
        return Err("Expected a shared library link step.".into());
    };
    let [linker, symlinks] = invocations.as_slice() else {
        return Err("Expected the linker and the symlink command.".into());
    };
    assert_eq!(
        linker.argv,
        [
            "cc",
            "-dynamiclib",
            "-compatibility_version",
            "1.0.0",
            "-current_version",
            "1.2.3",
            "-o",
            "libthing_dbg.1.2.3.dylib",
            "-install_name",
            "@rpath/libthing_dbg.1.dylib",
            "CMakeFiles/foo.dir/a.c.o",
            "-Wl,-rpath,/sample path",
            "-Wl,-rpath,/another",
        ]
    );
    assert_eq!(
        symlinks.argv,
        [
            "cmake",
            "-E",
            "cmake_symlink_library",
            "libthing_dbg.1.2.3.dylib",
            "libthing_dbg.1.dylib",
            "libthing_dbg.dylib"
        ]
    );
    Ok(())
}

#[test]
fn executable_uses_runtime_search_paths_for_shared_libraries_in_the_build_tree() -> TestResult {
    let mut model = fixture("Darwin");
    let config = ConfigId::default();
    let mut app = resolve_target(
        &mut model,
        raw_target("app", TargetKind::Executable),
        &config,
        &["app.c"],
    )?;
    app.link = Some(LinkStep {
        kind: LinkKind::Executable,
        language: "C".into(),
        fragments: vec![
            LinkFragment {
                role: LinkFragmentRole::Libraries,
                text: bt("'lib dir/libexample.dylib'"),
            },
            LinkFragment {
                role: LinkFragmentRole::Libraries,
                text: bt("'/usr/lib/libsystem.dylib'"),
            },
        ],
        output: "app".into(),
    });
    let graph = lower(&model, &[app], &config)?;
    let Some(BuildStep::Link { invocations, .. }) = graph.steps.last() else {
        return Err("Expected the executable link step.".into());
    };
    let [linker] = invocations.as_slice() else {
        return Err("Expected one linker invocation.".into());
    };
    assert!(linker.argv.ends_with(&[
        "-Wl,-rpath,/build/lib dir".into(),
        "lib dir/libexample.dylib".into(),
        "/usr/lib/libsystem.dylib".into(),
    ]));
    Ok(())
}

#[test]
fn compile_flag_order_matches_the_lowering_contract() -> TestResult {
    let mut model = fixture("Darwin");
    let config = ConfigId("Debug".into());
    for (key, value) in [
        ("CMAKE_CXX_FLAGS", "-Wall -Wextra"),
        ("CMAKE_CXX_FLAGS_DEBUG", "-g"),
        ("CMAKE_OSX_ARCHITECTURES", "arm64;x86_64"),
        ("CMAKE_OSX_SYSROOT", "/SDK Dir"),
        ("CMAKE_OSX_DEPLOYMENT_TARGET", "12.0"),
    ] {
        var(&mut model, key, value);
    }
    let mut raw = raw_target("foo", TargetKind::SharedLibrary);
    for (key, value) in [
        ("CXX_STANDARD", "17"),
        ("CXX_EXTENSIONS", "OFF"),
        ("CXX_VISIBILITY_PRESET", "hidden"),
        ("VISIBILITY_INLINES_HIDDEN", "ON"),
    ] {
        raw.props.set(key.into(), value.into());
    }
    let mut target = resolve_target(&mut model, raw, &config, &["a.cpp", "b.c"])?;
    let Some(group) = target.compile_groups.first_mut() else {
        return Err("Expected a C++ compile group.".into());
    };
    group.flags.push(bt("-Werror"));
    group.defines.extend([
        bt("lowercase"),
        bt("NAME=Some 'text'"),
        bt("NAME=Some 'text'"),
        bt("FIRST"),
    ]);
    group.includes.extend([
        InterfaceInclude {
            path: bt("/ordinary include///"),
            system: false,
        },
        InterfaceInclude {
            path: bt("/"),
            system: false,
        },
        InterfaceInclude {
            path: bt("/system include/"),
            system: true,
        },
        InterfaceInclude {
            path: bt("/system dot/./"),
            system: true,
        },
        InterfaceInclude {
            path: bt("/Framework Path/Foo.framework/Headers"),
            system: false,
        },
    ]);
    let graph = lower(&model, &[target], &config)?;
    let Some(BuildStep::Compile { invocation, .. }) = graph.steps.first() else {
        return Err("Expected a compile step.".into());
    };
    assert!(invocation.argv.windows(15).any(|flags| flags
        == [
            "-Wall",
            "-Wextra",
            "-g",
            "-std=c++17",
            "-arch",
            "arm64",
            "-arch",
            "x86_64",
            "-isysroot",
            "/SDK Dir",
            "-mmacosx-version-min=12.0",
            "-fPIC",
            "-fvisibility=hidden",
            "-fvisibility-inlines-hidden",
            "-Werror",
        ]));
    assert!(invocation.argv.windows(10).any(|args| args
        == [
            "-DFIRST",
            "-DNAME=Some 'text'",
            "-Dlowercase",
            "-I/ordinary include",
            "-I/",
            "-isystem",
            "/system include",
            "-isystem",
            "/system dot/.",
            "-I/Framework Path/Foo.framework/Headers"
        ]));
    assert!(
        matches!(graph.steps.last(), Some(BuildStep::Link { step, .. }) if step.language == "CXX")
    );
    Ok(())
}

#[test]
fn compile_architecture_defaults_follow_darwin_initialization() -> TestResult {
    let config = ConfigId("Debug".into());
    for (system, default, explicit, configured, compiler_target, expected) in [
        ("Darwin", "", "", None, "", None),
        ("Darwin", "arm64", "", None, "", Some("arm64")),
        ("Darwin", "arm64", "x86_64", None, "", Some("x86_64")),
        ("Darwin", "arm64", "x86_64", Some(""), "", Some("arm64")),
        ("Darwin", "arm64", "", Some("x86_64"), "", Some("x86_64")),
        ("Darwin", "arm64", "", None, "x86_64-apple-darwin", None),
        ("Linux", "arm64", "", None, "", None),
    ] {
        let mut model = fixture(system);
        var(&mut model, "_CMAKE_APPLE_ARCHS_DEFAULT", default);
        var(&mut model, "CMAKE_OSX_ARCHITECTURES", explicit);
        var(&mut model, "CMAKE_C_COMPILER_TARGET", compiler_target);
        let mut raw = raw_target("foo", TargetKind::ObjectLibrary);
        if let Some(arch) = configured {
            raw.props.set("OSX_ARCHITECTURES_DEBUG".into(), arch.into());
        }
        let target = resolve_target(&mut model, raw, &config, &["a.c"])?;
        let graph = lower(&model, &[target], &config)?;
        let Some(BuildStep::Compile { invocation, .. }) = graph.steps.first() else {
            return Err("Expected a compile step.".into());
        };
        let architecture = invocation.argv.windows(2).find_map(|args| match args {
            [flag, value] if flag == "-arch" => Some(value.as_slice()),
            _ => None,
        });
        assert_eq!(architecture, expected.map(str::as_bytes));
    }
    Ok(())
}

#[test]
fn object_paths_use_the_project_source_and_build_trees() -> TestResult {
    let mut model = fixture("Linux");
    let mut raw = raw_target("scan-test", TargetKind::ObjectLibrary);
    raw.dir = model.add_directory(
        DirId::ROOT,
        "/project/test/detail".into(),
        "/build/test/detail".into(),
        Default::default(),
    )?;
    let config = ConfigId::default();
    for (source, object) in [
        ("scan.cc", "scan.cc.o"),
        ("../src/os.cc", "__/src/os.cc.o"),
        ("../../src/os.cc", "__/__/src/os.cc.o"),
        ("/build/test/generated.c", "__/generated.c.o"),
        ("/build/test/detail/generated.c", "generated.c.o"),
        ("/build-other/generated.c", "build-other/generated.c.o"),
        ("../../../external/os.cc", "external/os.cc.o"),
    ] {
        let expected = format!("test/detail/CMakeFiles/scan-test.dir/{object}");
        assert_eq!(
            object_path(&model, &raw, &config, source.as_bytes()),
            expected.as_str()
        );
    }
    Ok(())
}

#[test]
fn classifies_sources_and_keeps_external_object_paths_distinct() {
    let mut model = fixture("Linux");
    let raw = raw_target("main", TargetKind::ObjectLibrary);
    let config = ConfigId::default();
    model
        .dir_mut(DirId::ROOT)
        .source_props
        .entry("/project/a.txt".into())
        .or_default()
        .set("LANGUAGE".into(), "OBJC".into());
    model
        .dir_mut(DirId::ROOT)
        .source_props
        .entry("/project/unused.cpp".into())
        .or_default()
        .set("HEADER_FILE_ONLY".into(), "ON".into());
    for (path, language) in [
        ("x.C", "CXX"),
        ("x.mm", "OBJCXX"),
        ("x.S", "ASM"),
        ("a.txt", "OBJC"),
    ] {
        assert_eq!(
            source_language(&model, &raw, path.as_bytes()),
            Some(language.into())
        );
    }
    for path in [
        b"unused.cpp".as_slice(),
        b"header.h",
        b"header.tcc",
        b"binary.o",
    ] {
        assert_eq!(source_language(&model, &raw, path), None);
    }
    assert_eq!(
        object_path(&model, &raw, &config, b"d/main.c"),
        "CMakeFiles/main.dir/d/main.c.o"
    );
    let out = object_path(&model, &raw, &config, b"../ext/main.c");
    assert_eq!(out, "CMakeFiles/main.dir/ext/main.c.o");
    assert_ne!(out, object_path(&model, &raw, &config, b"/ext2/main.c"));
    assert_ne!(
        out,
        object_path(&model, &raw, &config, b"__/external/root-/ext/main.c")
    );
    var(&mut model, "CMAKE_C_LINKER_PREFERENCE", "40");
    assert_eq!(
        linker_language(&model, &raw, [b"CXX".as_slice(), b"C"]),
        Some("C".into())
    );
}

#[test]
fn imported_artifacts_follow_configuration_mapping() {
    let mut raw = raw_target("imported", TargetKind::SharedLibrary);
    raw.origin = TargetOrigin::Imported(ImportedVisibility::Global);
    raw.props.set(
        "MAP_IMPORTED_CONFIG_DEBUG".into(),
        "RelWithDebInfo;Release".into(),
    );
    raw.props.set(
        "IMPORTED_LOCATION_RELEASE".into(),
        "/opt/lib/libfoo.so".into(),
    );
    raw.props
        .set("IMPORTED_LOCATION".into(), "/default/other.so".into());
    let artifacts = compute_artifacts(&fixture("Linux"), &raw, &ConfigId("debug".into()));
    assert_eq!(artifacts.file, Some("/opt/lib/libfoo.so".into()));
    assert_eq!(artifacts.object_dir, None);
}

#[test]
fn custom_commands_attach_to_consumers_in_dependency_order() -> TestResult {
    let mut model = fixture("Linux");
    let config = ConfigId::default();
    let program = resolve_target(
        &mut model,
        raw_target("generator", TargetKind::Executable),
        &config,
        &["generator.c"],
    )?;
    let app = resolve_target(
        &mut model,
        raw_target("app", TargetKind::Executable),
        &config,
        &["/build/generated.c"],
    )?;
    let util = resolve_target(
        &mut model,
        raw_target("assets", TargetKind::Utility),
        &config,
        &[],
    )?;
    let mut generated = CustomCommand::new(
        DirId::ROOT,
        CustomCommandKind::Output,
        Default::default(),
        Default::default(),
    );
    generated.outputs.push(bt("generated.c"));
    generated.depends.push(bt("input.txt"));
    generated.command_lines.push(vec![
        "generator".into(),
        "it's literal $HOME".into(),
        "".into(),
    ]);
    generated.working_directory = Some("stage".into());
    generated.byproducts.push(bt("extra.h"));
    generated.depfile = Some("gen.d".into());
    generated.uses_terminal = true;
    model.add_command(generated)?;
    let mut input = CustomCommand::new(
        DirId::ROOT,
        CustomCommandKind::Output,
        Default::default(),
        Default::default(),
    );
    input.outputs.push(bt("input.txt"));
    model.add_command(input)?;
    for (event, label) in [
        (TargetEvent::PreBuild, "pre-build"),
        (TargetEvent::PreLink, "pre-link"),
        (TargetEvent::PostBuild, "post-build"),
    ] {
        let mut command = CustomCommand::new(
            DirId::ROOT,
            CustomCommandKind::TargetEvent {
                target: app.id,
                event,
            },
            Default::default(),
            Default::default(),
        );
        command
            .command_lines
            .push(vec!["echo".into(), label.into()]);
        model.add_command(command)?;
    }
    let mut command = CustomCommand::new(
        DirId::ROOT,
        CustomCommandKind::Utility(util.id),
        Default::default(),
        Default::default(),
    );
    command.depends.push(bt("extra.h"));
    model.add_command(command)?;
    let graph = lower(&model, &[app, util, program], &config)?;
    let [_, app, utility] = graph.targets.as_slice() else {
        return Err("Expected three targets in model order.".into());
    };
    assert_eq!(app.name, "app");
    assert_eq!(app.depends, [0]);
    assert_eq!(utility.depends, [0]);
    let steps: Vec<_> = app
        .steps
        .iter()
        .filter_map(|&index| graph.steps.get(index))
        .collect();
    let [
        BuildStep::Custom(input),
        BuildStep::Custom(generated),
        BuildStep::Compile { .. },
        BuildStep::Link { invocations, .. },
    ] = steps.as_slice()
    else {
        return Err("Expected custom commands, compile, and link in dependency order.".into());
    };
    assert_eq!(input.outputs, ["input.txt"]);
    assert_eq!(generated.byproducts, ["extra.h"]);
    assert_eq!(generated.depfile, Some("gen.d".into()));
    assert!(generated.uses_terminal);
    let Some(invocation) = generated.invocations.first() else {
        return Err("Expected the generator invocation.".into());
    };
    assert_eq!(
        invocation.argv,
        ["/build/generator", "it's literal $HOME", ""]
    );
    assert_eq!(invocation.working_dir, Some("/build/stage".into()));
    assert_eq!(
        invocation.shell,
        r"/build/generator 'it'\''s literal $HOME' ''"
    );
    assert_eq!(
        super::shell::simple_args(&invocation.shell),
        Some(invocation.argv.clone())
    );
    let [pre_build, pre_link, linker, post_build] = invocations.as_slice() else {
        return Err("Expected build events around the linker invocation.".into());
    };
    assert_eq!(pre_build.argv, ["echo", "pre-build"]);
    assert_eq!(pre_link.argv, ["echo", "pre-link"]);
    assert!(linker.argv.iter().any(|arg| arg == "-o"));
    assert_eq!(post_build.argv, ["echo", "post-build"]);
    Ok(())
}

#[test]
fn lowers_install_rules_and_collects_metadata() -> TestResult {
    let mut model = fixture("Linux");
    let config = ConfigId::default();
    var(&mut model, "CMAKE_INSTALL_LIBDIR", "lib64");
    var(&mut model, "CMAKE_INSTALL_DEFAULT_COMPONENT_NAME", "Core");
    var(&mut model, "CMAKE_C_COMPILER_ID", "GNU");
    var(
        &mut model,
        "CMAKE_C_IMPLICIT_INCLUDE_DIRECTORIES",
        "/one;/two",
    );
    model.cache.insert(
        "CMAKE_C_COMPILER".into(),
        CacheEntry {
            value: "/tools/cc".into(),
            ty: CacheType::Filepath,
            help: BString::default(),
            advanced: false,
            props: Default::default(),
        },
    );
    model
        .dir_mut(DirId::ROOT)
        .listfiles
        .push("/project/CMakeLists.txt".into());
    let child = model.add_directory(
        DirId::ROOT,
        "/project/sub".into(),
        "/build/sub".into(),
        Default::default(),
    )?;
    model.dir_mut(child).listfiles.extend([
        BString::from("/project/CMakeLists.txt"),
        BString::from("/project/sub/CMakeLists.txt"),
    ]);
    let mut raw = raw_target("foo", TargetKind::SharedLibrary);
    raw.props.set("VERSION".into(), "2.3".into());
    raw.props.set("SOVERSION".into(), "2".into());
    let target = resolve_target(&mut model, raw, &config, &["a.c"])?;
    let rules = [
        (
            InstallKind::Targets {
                targets: vec![target.id],
                export: Some("pkg".into()),
            },
            InstallOptions::default(),
        ),
        (
            InstallKind::Files(vec![bt("README")]),
            InstallOptions {
                destination: Some("share".into()),
                rename: Some("MANUAL".into()),
                optional: true,
                ..InstallOptions::default()
            },
        ),
        (
            InstallKind::Programs(vec![bt("bootstrap")]),
            InstallOptions::default(),
        ),
        (
            InstallKind::Directory(vec![bt("docs/")]),
            InstallOptions::default(),
        ),
        (
            InstallKind::Script("after.cmake".into()),
            InstallOptions::default(),
        ),
        (
            InstallKind::Code("message(STATUS ready)".into()),
            InstallOptions::default(),
        ),
        (
            InstallKind::Export {
                set: "pkg".into(),
                namespace: "pkg::".into(),
                file: None,
                cxx_modules_directory: None,
            },
            InstallOptions {
                destination: Some("lib64/cmake/pkg".into()),
                ..InstallOptions::default()
            },
        ),
    ];
    for (kind, options) in rules {
        model.installs.push(InstallRule {
            dir: DirId::ROOT,
            backtrace: Default::default(),
            kind,
            options,
        });
    }
    for (index, word) in [
        "DIRECTORY",
        "docs/",
        "DESTINATION",
        "share",
        "FILES_MATCHING",
        "PATTERN",
        "*.md",
    ]
    .into_iter()
    .enumerate()
    {
        var(&mut model, &format!("_RCMAKE_INSTALL_3_ARG{index}"), word);
    }
    let install = lower_install_manifest(&model, &[target]);
    let [lib, file, program, directory, script, code, export] = install.actions.as_slice() else {
        return Err("Expected seven install actions.".into());
    };
    assert_eq!(lib.destination, "lib64");
    assert_eq!(lib.component, "Core");
    assert!(
        matches!(&lib.kind, InstallActionKind::Files { files, file_type, .. } if file_type == "SHARED_LIBRARY" && files == &["/build/libfoo.so.2.3", "/build/libfoo.so.2", "/build/libfoo.so"])
    );
    assert!(
        matches!(&file.kind, InstallActionKind::Files { files, rename: Some(rename), .. } if files == &["/project/README"] && rename == "MANUAL")
    );
    assert!(file.optional);
    assert!(
        matches!(&program.kind, InstallActionKind::Files { file_type, .. } if file_type == "PROGRAM")
    );
    assert!(
        matches!(&directory.kind, InstallActionKind::Directory { dirs, options } if dirs == &["/project/docs/"] && options == &["FILES_MATCHING", "PATTERN", "*.md"])
    );
    assert_eq!(
        script.kind,
        InstallActionKind::Script("/project/after.cmake".into())
    );
    assert_eq!(
        code.kind,
        InstallActionKind::Code("message(STATUS ready)".into())
    );
    assert_eq!(export.kind, InstallActionKind::Export { plan: 0 });
    let metadata = collect_metadata(&model);
    assert_eq!(
        metadata.listfiles,
        ["/project/CMakeLists.txt", "/project/sub/CMakeLists.txt"]
    );
    let Some(toolchain) = metadata.toolchains.get(b"C".as_slice()) else {
        return Err("Expected toolchain data for C.".into());
    };
    assert_eq!(toolchain.compiler_path, "/tools/cc");
    assert_eq!(toolchain.compiler_id, "GNU");
    assert_eq!(toolchain.implicit_includes, ["/one", "/two"]);
    Ok(())
}

#[test]
fn installs_public_headers_with_their_own_options() -> TestResult {
    let mut model = fixture("Linux");
    let generated = if cfg!(windows) {
        "C:/generated/config.h"
    } else {
        "/generated/config.h"
    };
    let child = model.add_directory(
        DirId::ROOT,
        "/project/sub".into(),
        "/build/sub".into(),
        Default::default(),
    )?;
    let mut raw = raw_target("api", TargetKind::StaticLibrary);
    raw.dir = child;
    raw.props
        .set("PUBLIC_HEADER".into(), format!("api.h;;{generated}").into());
    let target = resolve_target(&mut model, raw, &ConfigId("Debug".into()), &["a.c"])?;
    for _ in 0..2 {
        model.add_install(InstallRule {
            dir: DirId::ROOT,
            backtrace: Default::default(),
            kind: InstallKind::Targets {
                targets: vec![target.id],
                export: None,
            },
            options: InstallOptions::default(),
        });
    }
    for (index, word) in [
        "TARGETS",
        "api",
        "ARCHIVE",
        "DESTINATION",
        "lib64",
        "CONFIGURATIONS",
        "Release",
        "PUBLIC_HEADER",
        "DESTINATION",
        "headers",
        "COMPONENT",
        "Development",
        "OPTIONAL",
        "CONFIGURATIONS",
        "Debug",
        "PERMISSIONS",
        "OWNER_READ",
    ]
    .into_iter()
    .enumerate()
    {
        var(&mut model, &format!("_RCMAKE_INSTALL_1_ARG{index}"), word);
    }
    let manifest = lower_install_manifest(&model, &[target]);
    let [_, defaults, explicit] = manifest.actions.as_slice() else {
        return Err("Expected one archive and two header actions.".into());
    };
    assert_eq!(defaults.destination, "include");
    assert_eq!(explicit.destination, "headers");
    assert_eq!(explicit.component, "Development");
    assert_eq!(explicit.configurations, ["Debug"]);
    assert!(explicit.optional);
    for action in [defaults, explicit] {
        assert!(
            matches!(&action.kind, InstallActionKind::Files { file_type, files, .. }
            if file_type == "FILE" && files == &["/project/api.h", generated])
        );
    }
    assert!(
        matches!(&explicit.kind, InstallActionKind::Files { permissions, .. }
        if permissions == &["OWNER_READ"])
    );
    Ok(())
}

#[test]
fn target_events_evaluate_artifacts_and_run_after_linker_symlinks() -> TestResult {
    let mut model = fixture("Darwin");
    let config = ConfigId("Release".into());
    let mut library = raw_target("png_shared", TargetKind::SharedLibrary);
    library.props.set("OUTPUT_NAME".into(), "png16".into());
    library.props.set("VERSION".into(), "16.50.0".into());
    library.props.set("SOVERSION".into(), "16".into());
    library.props.set("MARKER".into(), "library context".into());
    let library = resolve_target(&mut model, library, &config, &["png.c"])?;
    let mut pre = CustomCommand::new(
        DirId::ROOT,
        CustomCommandKind::TargetEvent {
            target: library.id,
            event: TargetEvent::PreLink,
        },
        Default::default(),
        Default::default(),
    );
    pre.depends.push(bt("input-$<CONFIG>.txt"));
    pre.command_lines
        .push(vec!["echo".into(), "$<TARGET_PROPERTY:MARKER>".into()]);
    model.add_command(pre)?;
    let mut post = CustomCommand::new(
        DirId::ROOT,
        CustomCommandKind::TargetEvent {
            target: library.id,
            event: TargetEvent::PostBuild,
        },
        Default::default(),
        Default::default(),
    );
    post.command_lines.push(
        [
            "rcmake",
            "-E",
            "create_symlink",
            "$<TARGET_LINKER_FILE_NAME:png_shared>",
            "$<TARGET_LINKER_FILE_DIR:png_shared>/libpng.dylib",
        ]
        .map(BString::from)
        .to_vec(),
    );
    post.byproducts
        .push(bt("$<$<CONFIG:Release>:libpng.dylib>"));
    model.add_command(post)?;

    let graph = lower(&model, &[library], &config)?;
    let [BuildStep::Compile { .. }, BuildStep::Custom(recipe)] = graph.steps.as_slice() else {
        return Err("Expected one compile step and one link recipe with byproducts.".into());
    };
    assert_eq!(recipe.outputs, ["libpng16.16.50.0.dylib"]);
    assert!(recipe.byproducts.contains(&"libpng.dylib".into()));
    assert!(recipe.byproducts.contains(&"libpng16.dylib".into()));
    assert!(recipe.depends.contains(&"input-Release.txt".into()));
    let [pre, linker, symlinks, post] = recipe.invocations.as_slice() else {
        return Err(
            "Expected the target events before linking and after the linker symlinks.".into(),
        );
    };
    assert_eq!(pre.argv, ["echo", "library context"]);
    assert!(linker.argv.iter().any(|arg| arg == "-o"));
    assert_eq!(symlinks.argv.get(2), Some(&"cmake_symlink_library".into()));
    assert_eq!(
        post.argv,
        [
            "rcmake",
            "-E",
            "create_symlink",
            "libpng16.dylib",
            "/build/libpng.dylib"
        ]
    );
    Ok(())
}

#[test]
fn utility_events_share_one_recipe_in_event_order() -> TestResult {
    let mut model = fixture("Linux");
    let config = ConfigId("Release".into());
    let utility = resolve_target(
        &mut model,
        raw_target("assets", TargetKind::Utility),
        &config,
        &[],
    )?;
    let command =
        |kind| CustomCommand::new(DirId::ROOT, kind, Default::default(), Default::default());
    let event = |event| CustomCommandKind::TargetEvent {
        target: utility.id,
        event,
    };
    let mut post = command(event(TargetEvent::PostBuild));
    post.byproducts.push(bt("post-$<CONFIG>.txt"));
    post.command_lines.push(vec!["echo".into(), "post".into()]);
    model.add_command(post)?;
    let mut main = command(CustomCommandKind::Utility(utility.id));
    main.command_expand_lists = true;
    main.command_lines.push(
        ["$<0:program>", "echo", "a;;b;", "$<0:arg>", "", "last"]
            .map(BString::from)
            .to_vec(),
    );
    model.add_command(main)?;
    let mut pre = command(event(TargetEvent::PreBuild));
    pre.command_lines
        .push(vec!["$<0:program>".into(), "skip".into()]);
    pre.command_lines.push(vec!["echo".into(), "pre".into()]);
    model.add_command(pre)?;
    let mut pre_link = command(event(TargetEvent::PreLink));
    pre_link
        .command_lines
        .push(vec!["echo".into(), "pre-link".into()]);
    model.add_command(pre_link)?;
    let graph = lower(&model, &[utility], &config)?;
    let [BuildStep::Custom(recipe)] = graph.steps.as_slice() else {
        return Err("Expected one recipe for the utility target.".into());
    };
    let [main, pre, post] = recipe.invocations.as_slice() else {
        return Err("Expected three commands after generator expression evaluation.".into());
    };
    assert_eq!(pre.argv, ["echo", "pre"]);
    assert_eq!(main.argv, ["echo", "a", "b", "last"]);
    assert_eq!(post.argv, ["echo", "post"]);
    assert_eq!(recipe.byproducts, ["post-Release.txt"]);
    Ok(())
}

#[test]
fn compile_excludes_normalized_implicit_include_paths() -> TestResult {
    let mut model = fixture("Darwin");
    let config = ConfigId::default();
    model.dir_mut(DirId::ROOT).props.set(
        "CMAKE_C_IMPLICIT_INCLUDE_DIRECTORIES".into(),
        "/sdk/usr/include;/toolchain/./include/".into(),
    );
    var(
        &mut model,
        "CMAKE_CXX_IMPLICIT_INCLUDE_DIRECTORIES",
        "/keep",
    );
    let mut app = resolve_target(
        &mut model,
        raw_target("app", TargetKind::Executable),
        &config,
        &["main.c"],
    )?;
    app.compile_groups[0].includes = [
        ("/sdk/usr/lib/../include/", true),
        ("/toolchain/include", false),
        ("/sdk/usr/include/addon", true),
        ("/keep/", false),
    ]
    .into_iter()
    .map(|(path, system)| InterfaceInclude {
        path: bt(path),
        system,
    })
    .collect();
    let graph = lower(&model, &[app], &config)?;
    let Some(BuildStep::Compile { invocation, .. }) = graph.steps.first() else {
        return Err("Expected a C compiler invocation.".into());
    };
    assert!(
        invocation
            .argv
            .windows(2)
            .any(|pair| pair == ["-isystem", "/sdk/usr/include/addon"])
    );
    assert!(invocation.argv.contains(&"-I/keep".into()));
    assert!(!invocation.argv.contains(&"/sdk/usr/lib/../include".into()));
    assert!(!invocation.argv.contains(&"-I/toolchain/include".into()));
    Ok(())
}
