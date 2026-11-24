//! Structural and native Xcode generator tests.

#![allow(clippy::expect_used)]

use std::path::Path;

use rcmake_gen_common::ide::{BuildRequest, Jobs};
use rcmake_gen_xcode::{build_commands, generate, populate_guid_cache};
use rcmake_host::{Host, PureHost, ReadRange, path::from_native};
use rcmake_model::{
    BT, CustomCommand, CustomCommandKind, DirId, GeneratorKind, InstallKind, InstallOptions,
    InstallRule, ProjectModel, Target, TargetEvent, TargetId, TargetKind, TargetOrigin,
};
use rcmake_resolve::{
    BuildGraph, GenerationResult, ResolveOptions, ide::IdeFlavor, resolve, resolve_with_options,
};

fn add(model: &mut ProjectModel, name: &str, kind: TargetKind, source: Option<&str>) -> TargetId {
    let mut target = Target::new(
        name.into(),
        kind,
        TargetOrigin::Normal,
        DirId::ROOT,
        Default::default(),
        Default::default(),
    );
    target.sources.extend(
        source
            .into_iter()
            .map(|source| BT::new(source.into(), Default::default())),
    );
    model.add_target(target).expect("add target")
}

fn define(model: &mut ProjectModel, name: &str, value: &str) {
    model
        .dir_mut(DirId::ROOT)
        .vars
        .insert(name.into(), value.into());
}

fn prop(model: &mut ProjectModel, id: TargetId, key: &str, value: &str) {
    model
        .target_mut(id)
        .expect("target")
        .props
        .set(key.into(), value.into());
}

fn model(source: &Path, out: &Path) -> ProjectModel {
    let mut model = ProjectModel::default();
    model.set_root_dirs(from_native(source), from_native(out));
    model.generator = Some(GeneratorKind::Xcode);
    define(&mut model, "PROJECT_NAME", "Demo");
    define(&mut model, "XCODE_VERSION", "27.2");
    define(&mut model, "CMAKE_SYSTEM_NAME", "Darwin");
    define(&mut model, "CMAKE_OSX_DEPLOYMENT_TARGET", "13.0");
    define(&mut model, "CMAKE_C_COMPILER", "/usr/bin/cc");
    define(&mut model, "CMAKE_C_SOURCE_FILE_EXTENSIONS", "c");
    define(&mut model, "CMAKE_OSX_ARCHITECTURES", "arm64");
    model
}

fn read(host: &dyn Host, path: &Path) -> String {
    let data = host
        .read(
            path,
            ReadRange::All {
                limit: 2 * 1024 * 1024,
            },
        )
        .expect("read");
    String::from_utf8(data).expect("UTF-8")
}

fn resolved(model: &mut ProjectModel, host: &dyn Host, ide: bool) -> GenerationResult {
    let configs = ["Debug", "Release", "MinSizeRel", "RelWithDebInfo"].map(|name| {
        let mut graph = BuildGraph::default();
        graph.config.0 = name.into();
        graph.config
    });
    if ide {
        resolve_with_options(
            model,
            host,
            &configs,
            &ResolveOptions {
                ide: Some(IdeFlavor::Xcode),
            },
        )
        .expect("resolve IDE project")
    } else {
        resolve(model, host, &configs).expect("resolve build graph")
    }
}

#[test]
fn builds_argument_vectors_in_oracle_order() {
    let targets = ["core".into(), "app".into()];
    let mut request = BuildRequest {
        generator: GeneratorKind::Xcode,
        build_dir: b"/out",
        project_name: b"Demo",
        make_program: b"/usr/bin/xcodebuild",
        config: Some(b"Release"),
        targets: &targets,
        jobs: Jobs::Count(4),
        clean_first: true,
        verbose: true,
        native_args: &["-quiet".into()],
        platform: b"",
    };
    let commands = build_commands(&request).expect("plan");
    let [clean, build] = commands.as_slice() else {
        unreachable!()
    };
    assert_eq!(
        clean.argv,
        [
            "/usr/bin/xcodebuild",
            "-project",
            "Demo.xcodeproj",
            "clean",
            "-configuration",
            "Release",
            "-parallelizeTargets",
            "-jobs",
            "4",
            "-hideShellScriptEnvironment",
            "-target",
            "ALL_BUILD"
        ]
    );
    assert_eq!(
        build.argv,
        [
            "/usr/bin/xcodebuild",
            "-project",
            "Demo.xcodeproj",
            "build",
            "-configuration",
            "Release",
            "-parallelizeTargets",
            "-jobs",
            "4",
            "-hideShellScriptEnvironment",
            "-target",
            "core",
            "-target",
            "app",
            "-quiet"
        ]
    );
    assert_eq!(build.working_dir, "/out");
    request.targets = &[];
    request.config = None;
    request.clean_first = false;
    request.jobs = Jobs::Default;
    request.native_args = &[];
    let commands = build_commands(&request).expect("plan");
    let [build] = commands.as_slice() else {
        unreachable!()
    };
    assert_eq!(
        build.argv,
        [
            "/usr/bin/xcodebuild",
            "-project",
            "Demo.xcodeproj",
            "build",
            "-configuration",
            "Debug",
            "-parallelizeTargets",
            "-hideShellScriptEnvironment",
            "-target",
            "ALL_BUILD"
        ]
    );
}

#[test]
fn generates_multiconfig_projects_from_build_graph() {
    let host = PureHost::default();
    let source = Path::new("/src");
    let out = Path::new("/out");
    let mut model = model(source, out);
    for file in ["main.c", "library.c", "api.h", "CMakeLists.txt"] {
        host.seed_file(&source.join(file), b"")
            .expect("seed source");
    }
    host.seed_file(&out.join("CPackConfig.cmake"), b"")
        .expect("seed CPack");
    define(&mut model, "CMAKE_TESTING_ENABLED", "1");
    define(
        &mut model,
        "CMAKE_XCODE_ATTRIBUTE_CLANG_ENABLE_MODULES",
        "YES",
    );
    model.add_install(InstallRule {
        dir: DirId::ROOT,
        backtrace: Default::default(),
        kind: InstallKind::Code("message(STATUS Test)".into()),
        options: InstallOptions::default(),
    });
    add(
        &mut model,
        "static",
        TargetKind::StaticLibrary,
        Some("library.c"),
    );
    add(
        &mut model,
        "shared",
        TargetKind::SharedLibrary,
        Some("library.c"),
    );
    add(
        &mut model,
        "object",
        TargetKind::ObjectLibrary,
        Some("library.c"),
    );
    let frame = add(
        &mut model,
        "framework",
        TargetKind::SharedLibrary,
        Some("library.c;api.h"),
    );
    prop(&mut model, frame, "FRAMEWORK", "ON");
    prop(&mut model, frame, "PUBLIC_HEADER", "api.h");
    let app = add(&mut model, "app", TargetKind::Executable, Some("main.c"));
    prop(&mut model, app, "MACOSX_BUNDLE", "ON");
    prop(&mut model, app, "XCODE_GENERATE_SCHEME", "ON");
    prop(&mut model, app, "XCODE_SCHEME_ARGUMENTS", "hello world;a&b");
    prop(
        &mut model,
        app,
        "XCODE_ATTRIBUTE_PRODUCT_BUNDLE_IDENTIFIER",
        "test.rcmake.app",
    );
    prop(
        &mut model,
        app,
        "XCODE_ATTRIBUTE_GCC_OPTIMIZATION_LEVEL[variant=Debug]",
        "1",
    );
    prop(&mut model, app, "XCODE_EMBED_FRAMEWORKS", "framework");
    let util = add(&mut model, "custom", TargetKind::Utility, None);
    for (id, kind) in [
        (util, CustomCommandKind::Utility(util)),
        (
            app,
            CustomCommandKind::TargetEvent {
                target: app,
                event: TargetEvent::PostBuild,
            },
        ),
    ] {
        let mut command =
            CustomCommand::new(DirId::ROOT, kind, Default::default(), Default::default());
        command
            .command_lines
            .push(vec!["/bin/echo".into(), "word with spaces".into()]);
        let command = model.add_command(command).expect("command");
        model
            .target_mut(id)
            .expect("target")
            .event_commands
            .push(command);
    }
    let sub = model
        .add_directory(
            DirId::ROOT,
            "/src/sub".into(),
            "/out/sub".into(),
            Default::default(),
        )
        .expect("subdir");
    model
        .dir_mut(sub)
        .vars
        .insert("PROJECT_NAME".into(), "Sub".into());
    model
        .dir_mut(sub)
        .vars
        .insert("PROJECT_SOURCE_DIR".into(), "/src/sub".into());
    let mut results = resolved(&mut model, &host, false);
    results.metadata.listfiles = vec!["/src/CMakeLists.txt".into()];
    populate_guid_cache(&results, &mut model, &host);
    model.cache.get_mut(b"app_GUID_CMAKE").expect("GUID").value = "ABCDEF0123456789ABCDEF01".into();
    generate(&results, &model, &host, out).expect("generate");
    let project = out.join("Demo.xcodeproj/project.pbxproj");
    let pbx = read(&host, &project);
    for expected in [
        "ABCDEF0123456789ABCDEF01 /* app */",
        "libstatic.a",
        "libshared.dylib",
        "libobject.a",
        "com.apple.product-type.application",
        "com.apple.product-type.framework",
        "PBXHeadersBuildPhase",
        "PBXCopyFilesBuildPhase",
        "CMake PostBuild Rules",
        "word with spaces",
        "ALL_BUILD",
        "ZERO_CHECK",
        "cmake_install.cmake",
        "RUN_TESTS",
        "CPackConfig.cmake",
        "RelWithDebInfo",
        "CLANG_ENABLE_MODULES = YES;",
        "GCC_OPTIMIZATION_LEVEL = 1;",
        "test.rcmake.app",
    ] {
        assert!(pbx.contains(expected), "{expected} is absent");
    }
    assert!(
        read(
            &host,
            &out.join("Demo.xcodeproj/xcshareddata/xcschemes/app.xcscheme")
        )
        .contains("a&amp;b")
    );
    assert!(
        read(&host, &out.join("sub/Sub.xcodeproj/project.pbxproj"))
            .contains("projectDirPath = /src/sub")
    );
    assert!(read(&host, &out.join("CMakeScripts/ReRunCMake.sh")).contains("/src/CMakeLists.txt"));
    generate(&results, &model, &host, out).expect("regenerate");
    assert_eq!(read(&host, &project), pbx);
    model
        .cache
        .get_mut(b"static_GUID_CMAKE")
        .expect("GUID")
        .value = "ABCDEF0123456789ABCDEF01".into();
    let error = generate(&results, &model, &host, out).expect_err("duplicate GUID");
    assert!(
        error.to_string().contains("target/app") && error.to_string().contains("target/static")
    );
}

#[cfg(target_os = "macos")]
#[test]
fn native_xcodebuild_compiles_and_runs_program() {
    use rcmake_host::{
        CommandSpec, Input, NativeHost, Output, ProcessResult, RunRequest, WriteMode,
    };
    use std::{path::PathBuf, time::Duration};

    let host = NativeHost::new().expect("native host");
    let cwd = host.initial_cwd();
    let run = |cwd: &Path, program: &str, args: &[&str], timeout: u64| {
        host.run(
            &RunRequest {
                pipeline: vec![CommandSpec {
                    program: program.into(),
                    args: args.iter().map(|arg| (*arg).into()).collect(),
                }],
                cwd: cwd.into(),
                env: host.env_all(),
                stdin: Input::Null,
                stdout: Output::Capture,
                stderr: Output::Capture,
                timeout: Some(Duration::from_secs(timeout)),
                tee: false,
            },
            &mut |_, _| {},
        )
        .expect("run native process")
    };
    let check = |output: rcmake_host::RunOutput| {
        assert_eq!(
            output.results,
            [ProcessResult::Exited(0)],
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).into_owned()
    };
    let selected = run(&cwd, "/usr/bin/xcode-select", &["-p"], 10);
    if selected.results != [ProcessResult::Exited(0)]
        || !String::from_utf8_lossy(&selected.stdout)
            .trim_end()
            .ends_with(".app/Contents/Developer")
    {
        return;
    }
    check(run(&cwd, "/usr/bin/xcodebuild", &["-version"], 15));

    struct Temp<'a>(&'a dyn Host, PathBuf);
    impl Drop for Temp<'_> {
        fn drop(&mut self) {
            let _ = self.0.remove_dir_all(&self.1);
        }
    }
    let stamp = host.now();
    let tmp = Temp(
        &host,
        Path::new("/tmp").join(format!(
            "rcmake-live-xcode-{}-{}-{}",
            host.system().pid,
            stamp.secs,
            stamp.nanos
        )),
    );
    let source = tmp.1.join("src");
    let out = tmp.1.join("build dir");
    host.create_dir_all(&source.join("include dir"))
        .expect("source dir");
    host.create_dir_all(&out).expect("build dir");
    for (file, data) in [
        (
            "main.c",
            "#include <stdio.h>\n#include <string.h>\n#include \"test_header.h\"\n#ifndef MAIN_ONLY\n#error MAIN_ONLY is required\n#endif\nint core(void); int shared(void); int obj(void); int generated(void);\nint main(void) { if (core() + shared() + obj() + generated() != EXPECTED_TOTAL || strlen(CMAKE_INTDIR) == 0) return 1; puts(\"Rcmake Xcode smoke test\"); return 0; }\n",
        ),
        ("include dir/test_header.h", "#define EXPECTED_TOTAL 42\n"),
        ("bundle.c", "int main(void) { return 0; }\n"),
        ("bridge.h", "int shared(void);\n"),
        ("resource.txt", "sample resource\n"),
        ("core.c", "int core(void) { return 30; }\n"),
        ("shared.c", "int shared(void) { return 2; }\n"),
        ("object.c", "int obj(void) { return 3; }\n"),
        ("generate.in", "int generated(void) { return 7; }\n"),
        (
            "CMakeLists.txt",
            "cmake_minimum_required(VERSION 4.5)\nproject(Demo C)\nadd_library(core STATIC core.c)\nadd_library(shared SHARED shared.c)\nset_target_properties(shared PROPERTIES VERSION 5.2.0 SOVERSION 5)\nadd_library(object OBJECT object.c)\nadd_custom_command(OUTPUT generated.c COMMAND /bin/cp \"${CMAKE_CURRENT_SOURCE_DIR}/generate.in\" generated.c DEPENDS generate.in VERBATIM)\nadd_executable(app main.c generated.c)\ntarget_link_libraries(app PRIVATE core shared object)\ntarget_include_directories(app PRIVATE \"include dir\")\nset_source_files_properties(main.c PROPERTIES COMPILE_OPTIONS -DMAIN_ONLY)\nadd_library(bridge SHARED shared.c bridge.h)\nset_target_properties(bridge PROPERTIES FRAMEWORK TRUE PUBLIC_HEADER bridge.h)\nadd_executable(bundle MACOSX_BUNDLE bundle.c resource.txt)\nset_source_files_properties(resource.txt PROPERTIES MACOSX_PACKAGE_LOCATION Resources)\n",
        ),
    ] {
        host.write(&source.join(file), data.as_bytes(), WriteMode::Truncate)
            .expect("write source");
    }
    let mut model = model(&source, &out);
    let arch = host.system().machine;
    define(
        &mut model,
        "CMAKE_OSX_ARCHITECTURES",
        &String::from_utf8_lossy(&arch),
    );
    add(
        &mut model,
        "core",
        TargetKind::StaticLibrary,
        Some("core.c"),
    );
    let shared = add(
        &mut model,
        "shared",
        TargetKind::SharedLibrary,
        Some("shared.c"),
    );
    prop(&mut model, shared, "VERSION", "5.2.0");
    prop(&mut model, shared, "SOVERSION", "5");
    add(
        &mut model,
        "object",
        TargetKind::ObjectLibrary,
        Some("object.c"),
    );
    let app = add(
        &mut model,
        "app",
        TargetKind::Executable,
        Some("main.c;generated.c"),
    );
    let bridge = add(
        &mut model,
        "bridge",
        TargetKind::SharedLibrary,
        Some("shared.c;bridge.h"),
    );
    prop(&mut model, bridge, "FRAMEWORK", "ON");
    prop(&mut model, bridge, "PUBLIC_HEADER", "bridge.h");
    let bundle = add(
        &mut model,
        "bundle",
        TargetKind::Executable,
        Some("bundle.c;resource.txt"),
    );
    prop(&mut model, bundle, "MACOSX_BUNDLE", "ON");
    let dir = model.dir_mut(DirId::ROOT);
    for (file, key, value) in [
        ("main.c", "COMPILE_OPTIONS", "-DMAIN_ONLY"),
        ("resource.txt", "MACOSX_PACKAGE_LOCATION", "Resources"),
    ] {
        dir.source_props
            .entry(from_native(&source.join(file)))
            .or_default()
            .set(key.into(), value.into());
    }
    model.target_mut(app).expect("app").usage.push(
        rcmake_model::UsageField::IncludeDirectories,
        BT::new(from_native(&source.join("include dir")), Default::default()),
    );
    model.target_mut(app).expect("app").usage.link_libraries = Some(
        ["core", "shared", "object"]
            .into_iter()
            .map(|name| rcmake_model::LinkItem {
                text: BT::new(name.into(), Default::default()),
                lookup_dir: DirId::ROOT,
            })
            .collect(),
    );
    let mut generate_source = CustomCommand::new(
        DirId::ROOT,
        CustomCommandKind::Output,
        Default::default(),
        Default::default(),
    );
    generate_source
        .outputs
        .push(BT::new("generated.c".into(), Default::default()));
    let data = from_native(&source.join("generate.in"));
    generate_source
        .depends
        .push(BT::new(data.clone(), Default::default()));
    generate_source
        .command_lines
        .push(vec!["/bin/cp".into(), data, "generated.c".into()]);
    model.add_command(generate_source).expect("custom command");
    let result = resolved(&mut model, &host, true);
    generate(&result, &model, &host, &out).expect("generate");
    let list = check(run(
        &out,
        "/usr/bin/xcodebuild",
        &["-project", "Demo.xcodeproj", "-list"],
        60,
    ));
    assert!(
        list.contains("ALL_BUILD") && list.contains("app") && list.contains("Release"),
        "{list}"
    );
    for config in ["Debug", "Release"] {
        let log = check(run(
            &out,
            "/usr/bin/xcodebuild",
            &[
                "-project",
                "Demo.xcodeproj",
                "build",
                "-configuration",
                config,
                "-jobs",
                "2",
                "-target",
                "ALL_BUILD",
            ],
            120,
        ));
        assert!(log.contains("BUILD SUCCEEDED"), "{log}");
        for artifact in [
            "libshared.5.2.0.dylib",
            "libshared.5.dylib",
            "libshared.dylib",
            "bridge.framework/Versions/A/bridge",
            "bridge.framework/Headers/bridge.h",
            "bundle.app/Contents/Resources/resource.txt",
        ] {
            host.metadata(&out.join(config).join(artifact), rcmake_host::Follow::Yes)
                .expect(artifact);
        }
        let binary = out.join(config).join("app");
        assert_eq!(
            check(run(&out, &binary.to_string_lossy(), &[], 10)),
            "Rcmake Xcode smoke test\n"
        );
    }
    let script = cwd.join("../../tools/measure/pbxproj_canon.py");
    let canonical = check(run(
        &out,
        "/usr/bin/python3",
        &[&script.to_string_lossy(), "Demo.xcodeproj"],
        30,
    ));
    assert!(canonical.contains("@project") && canonical.contains("PBXNativeTarget"));
}

#[test]
fn applies_ide_facts_and_checks_config_source_lists() {
    use rcmake_resolve::ide::{IdeCompileSettings, IdeFacts, OutputNaming};

    let host = PureHost::default();
    let bin = Path::new("/out");
    let mut model = model(Path::new("/src"), bin);
    host.seed_file(Path::new("/src/main.c"), b"")
        .expect("seed source");
    let app = add(&mut model, "app", TargetKind::Executable, Some("main.c"));
    let mut result = resolved(&mut model, &host, false);
    for target in result.targets.iter_mut().filter(|target| target.id == app) {
        let config = &target.config.0;
        let mut facts = IdeFacts {
            naming: OutputNaming {
                dir: format!("/out/artifacts/{config}").into(),
                base_name: "renamed".into(),
                ..OutputNaming::default()
            },
            compile: vec![IdeCompileSettings {
                language: "C".into(),
                defines: vec![BT::new("IDE_FACT=1".into(), Default::default())],
                ..IdeCompileSettings::default()
            }],
            ..IdeFacts::default()
        };
        facts.attributes.insert(
            "XCODE_ATTRIBUTE_GCC_TREAT_WARNINGS_AS_ERRORS".into(),
            "YES".into(),
        );
        target.ide = Some(Box::new(facts));
    }
    generate(&result, &model, &host, bin).expect("generate IDE facts");
    let pbx = read(&host, &bin.join("Demo.xcodeproj/project.pbxproj"));
    for expected in [
        "PRODUCT_NAME = renamed;",
        "CONFIGURATION_BUILD_DIR = /out/artifacts/Release;",
        "IDE_FACT=1",
        "GCC_TREAT_WARNINGS_AS_ERRORS = YES;",
    ] {
        assert!(pbx.contains(expected), "{expected} is absent");
    }
    for target in &mut result.targets {
        target.ide = None;
    }
    let release = result
        .targets
        .iter_mut()
        .find(|target| target.id == app && target.config.0.as_slice() == b"Release")
        .expect("Release");
    release.compile_units.clear();
    let error = generate(&result, &model, &host, bin).expect_err("config-specific sources");
    assert!(
        error
            .to_string()
            .contains("source files which vary by configuration")
    );
}
