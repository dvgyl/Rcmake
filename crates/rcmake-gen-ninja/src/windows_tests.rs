use std::path::Path;

use rcmake_base::BString;
use rcmake_genex::ConfigId;
use rcmake_host::{Host, PureHost};
use rcmake_model::{DirId, ProjectModel};
use rcmake_resolve::{
    BuildGraph, BuildStep, CustomStep, GenerationResult, Invocation, LinkKind, LinkStep,
};

use crate::{commands::Commands, generate_single, multi::generate_multi, tests::read};

fn windows() -> Commands {
    Commands::for_windows(br"C:\Windows\System32\cmd.exe")
}

fn argv(args: &[&str]) -> Invocation {
    Invocation {
        argv: args.iter().copied().map(BString::from).collect(),
        ..Invocation::default()
    }
}

fn msvc_compile() -> BuildStep {
    let object = "obj dir/test.obj";
    let source = "D:/O'Neil $ source/test.c";
    let mut invocation = argv(&[
        "C:/Program Files/VC/cl.exe",
        "/nologo",
        r#"/DNAME="value $ with spaces""#,
        "/Foobj dir/test.obj",
        "/Fdobj dir\\",
        "/FS",
        "-c",
        source,
        "/showIncludes",
    ]);
    invocation.shell = "cl.exe -c 'POSIX shell text'".into();
    BuildStep::Compile {
        byproducts: Vec::new(),
        compile_input: None,
        object: object.into(),
        source: source.into(),
        invocation,
        depfile: None,
        modmap: None,
        dyndep: None,
    }
}

#[test]
fn native_argv_and_ninja_paths_use_distinct_escaping() -> Result<(), Box<dyn std::error::Error>> {
    let dir = Path::new("/work/build");
    let host = PureHost::default();
    let mut model = ProjectModel::default();
    model.set_root_dirs("/work/source".into(), "/work/build".into());
    let vars = &mut model.dir_mut(DirId::ROOT).vars;
    vars.insert("CMAKE_SYSTEM_NAME".into(), "Windows".into());
    vars.insert(
        "CMAKE_CL_SHOWINCLUDES_PREFIX".into(),
        "Hinweis: Datei: ".into(),
    );
    let result = GenerationResult {
        graphs: vec![BuildGraph {
            steps: vec![msvc_compile()],
            ..BuildGraph::default()
        }],
        ..GenerationResult::default()
    };

    generate_single(&result, &host, dir, Some(&model), windows())?;
    let win = read(&host, "build.ninja");
    assert!(win.contains("build obj$ dir/test.obj: COMPILE_MSVC D$:/O'Neil$ $$$ source/test.c"));
    assert!(win.contains(
        r#"  COMMAND = "C:/Program Files/VC/cl.exe" /nologo "/DNAME=\"value $$ with spaces\"" "/Foobj dir/test.obj" "/Fdobj dir\\" /FS -c "D:/O'Neil $$ source/test.c" /showIncludes"#
    ));
    let rules = read(&host, "CMakeFiles/rules.ninja");
    assert!(rules.contains("msvc_deps_prefix = Hinweis: Datei: \n"));
    assert!(rules.contains(
        "rule COMPILE_MSVC\n  command = $COMMAND\n  description = $DESC\n  deps = msvc\n"
    ));

    generate_single(&result, &host, dir, Some(&model), Commands::default())?;
    let posix_cross = read(&host, "build.ninja");
    assert!(posix_cross.contains("  COMMAND = cl.exe -c 'POSIX shell text'"));

    model
        .dir_mut(DirId::ROOT)
        .vars
        .insert("CMAKE_SYSTEM_NAME".into(), "Linux".into());
    let unix_target = GenerationResult {
        graphs: vec![BuildGraph {
            steps: vec![BuildStep::Custom(CustomStep {
                outputs: vec!["cross.out".into()],
                invocations: vec![argv(&[
                    "C:/Compiler Tools/clang.exe",
                    "--target=x86_64-linux-gnu",
                ])],
                ..CustomStep::default()
            })],
            ..BuildGraph::default()
        }],
        ..GenerationResult::default()
    };
    generate_single(&unix_target, &host, dir, Some(&model), windows())?;
    assert!(
        read(&host, "build.ninja")
            .contains(r#"  COMMAND = "C:/Compiler Tools/clang.exe" --target=x86_64-linux-gnu"#)
    );
    Ok(())
}

#[test]
fn windows_uses_configured_response_files_and_moves_long_archive_inputs()
-> Result<(), Box<dyn std::error::Error>> {
    let dir = Path::new("/work/build");
    let host = PureHost::default();
    host.env_set(b"CMAKE_NINJA_FORCE_RESPONSE_FILE", Some(b""));
    let mut compile_graph = BuildGraph::default();
    compile_graph.steps.push(msvc_compile());
    let mut result = GenerationResult {
        graphs: vec![compile_graph],
        ..GenerationResult::default()
    };
    generate_single(&result, &host, dir, None, windows())?;
    let forced = read(&host, "build.ninja");
    assert!(forced.contains("build obj$ dir/test.obj: COMPILE_MSVC_RSP"));
    assert!(forced.contains(
        r#"  COMMAND = "C:/Program Files/VC/cl.exe" "@obj dir/test.obj.rsp" "/Foobj dir/test.obj""#
    ));
    assert!(forced.contains(r#"  RSP_ARGS = /nologo "/DNAME=\"value $$ with spaces\"""#));

    let graph = result.graphs.first_mut().ok_or("Missing build graph.")?;
    graph.steps.clear();
    graph.steps.push(BuildStep::Link {
        implicit_inputs: Vec::new(),
        inputs: vec!["obj dir/main.obj".into()],
        step: LinkStep {
            kind: LinkKind::Executable,
            language: "CXX".into(),
            fragments: Vec::new(),
            output: "program dir/app.exe".into(),
        },
        invocations: vec![argv(&[
            "rcmake.exe",
            "-E",
            "vs_link_exe",
            "--intdir=CMakeFiles/app.dir",
            "--",
            "C:/VC/link.exe",
            "obj dir/main.obj",
            "/out:program dir/app.exe",
            "/INCREMENTAL:YES",
            "kernel32.lib",
        ])],
    });
    generate_single(&result, &host, dir, None, windows())?;
    let wrapped = read(&host, "build.ninja");
    assert!(wrapped.contains(
        r#"C:/VC/link.exe "/out:program dir/app.exe" /INCREMENTAL:YES @CMakeFiles/link-"#
    ));
    assert!(wrapped.contains(r#"  RSP_ARGS = "obj dir/main.obj" kernel32.lib"#));

    host.env_set(b"CMAKE_NINJA_FORCE_RESPONSE_FILE", None);
    let objects: Vec<BString> = (0..340)
        .map(|i| format!("many archive objects/compiled source with spaces {i}.obj").into())
        .collect();
    let mut libr = argv(&["C:/VC/lib.exe", "/nologo", "/out:archive.lib"]);
    libr.argv.extend(objects.iter().cloned());
    let archive = BuildStep::Link {
        implicit_inputs: Vec::new(),
        inputs: objects,
        step: LinkStep {
            kind: LinkKind::StaticArchive,
            language: "CXX".into(),
            fragments: Vec::new(),
            output: "archive.lib".into(),
        },
        invocations: vec![argv(&["rcmake.exe", "-E", "rm", "-f", "archive.lib"]), libr],
    };
    let graph = result.graphs.first_mut().ok_or("Missing build graph.")?;
    graph.steps.clear();
    graph.steps.push(archive);
    generate_single(&result, &host, dir, None, windows())?;
    let automatic = read(&host, "build.ninja");
    assert!(automatic.contains("build archive.lib: STATIC_ARCHIVE_RSP"));
    assert!(automatic.contains(r#" /C "rcmake.exe -E rm -f archive.lib && C:/VC/lib.exe /nologo /out:archive.lib @archive.lib.rsp""#));
    assert!(
        automatic
            .contains(r#"  RSP_ARGS = "many archive objects/compiled source with spaces 0.obj""#)
    );
    assert!(automatic.contains(r#""many archive objects/compiled source with spaces 339.obj""#));
    Ok(())
}

#[test]
fn windows_compound_commands_change_drives_and_group_alternatives() {
    let commands = windows();
    let mut first = argv(&["tool.cmd", "O'Neil!", "||", "fallback.exe"]);
    first.working_dir = Some("D:/a directory".into());
    let mut second = argv(&["tool.exe", "argument"]);
    second.working_dir = Some("//server/share/directory".into());
    let third = argv(&["tool.exe", "base"]);
    let rendered = commands.line(&[first, second, third], true, b"C:/build dir", false);
    assert_eq!(
        rendered,
        br#"C:\Windows\System32\cmd.exe /D /S /V:OFF /C "cd /D "D:\a directory" && ( call tool.cmd O'Neil! || fallback.exe ) && pushd "\\server\share\directory" && tool.exe argument && cd /D "C:\build dir" && tool.exe base""#
    );
    assert_eq!(
        commands.path(br"\\server\share\dir\..\file"),
        "//server/share/file"
    );
    assert_eq!(commands.path(br"C:\root\..\..\file"), "C:/file");
}

#[test]
fn windows_runs_each_simple_command_in_a_lowered_chain() {
    let commands = windows();
    let shell = "first.exe 'a && b' && second.exe 'C:/a path'".to_owned();
    let invocation = Invocation {
        argv: vec!["sh".into(), "-c".into(), shell.clone().into()],
        shell: shell.into(),
        ..Default::default()
    };
    let rendered = commands.line(&[invocation], false, b"C:/build dir", false);
    assert_eq!(rendered, br#"C:\Windows\System32\cmd.exe /D /S /V:OFF /C "first.exe "a && b" && second.exe "C:/a path"""#);
}

#[test]
fn windows_multi_config_clean_uses_make_program_and_selected_paths()
-> Result<(), Box<dyn std::error::Error>> {
    let host = PureHost::default();
    let dir = Path::new("/work/build");
    let mut model = ProjectModel::default();
    model.set_root_dirs("/work/src".into(), "/work/build".into());
    let root = model.dir_mut(DirId::ROOT);
    root.vars.insert(
        "CMAKE_MAKE_PROGRAM".into(),
        "C:/Ninja Tools/ninja.exe".into(),
    );
    root.vars.insert("CMAKE_CROSS_CONFIGS".into(), "all".into());
    root.props.set(
        "ADDITIONAL_CLEAN_FILES".into(),
        "extra $<CONFIG>.txt".into(),
    );
    let configs = vec![ConfigId("Debug".into()), ConfigId("Release".into())];
    let result = GenerationResult {
        graphs: configs
            .iter()
            .cloned()
            .map(|config| BuildGraph {
                config,
                ..BuildGraph::default()
            })
            .collect(),
        configs,
        ..GenerationResult::default()
    };
    generate_multi(&result, &host, dir, Some(&model), windows(), None)?;
    let rules = read(&host, "CMakeFiles/rules.ninja");
    assert!(
        rules.contains(r#"  command = "C:/Ninja Tools/ninja.exe" $FILE_ARG -t clean $TARGETS"#),
        "{rules}"
    );
    assert!(
        rules.contains("rule CLEAN_ADDITIONAL\n  command = cmake -DCONFIG=$CONFIG -P CMakeFiles/clean_additional.cmake\n"),
        "{rules}"
    );
    let script = read(&host, "CMakeFiles/clean_additional.cmake");
    assert!(
        script.contains("if(\"${CONFIG}\" STREQUAL \"\" OR \"${CONFIG}\" STREQUAL \"Debug\")\n  file(REMOVE_RECURSE\n  \"extra Debug.txt\"\n  )\nendif()"),
        "{script}"
    );
    assert!(
        script.contains("if(\"${CONFIG}\" STREQUAL \"\" OR \"${CONFIG}\" STREQUAL \"Release\")\n  file(REMOVE_RECURSE\n  \"extra Release.txt\"\n  )\nendif()"),
        "{script}"
    );
    let manifest = read(&host, "CMakeFiles/impl-Debug.ninja");
    assert!(
        manifest.contains(
            "build CMakeFiles/clean.additional$:Debug: CLEAN_ADDITIONAL\n  CONFIG = Debug\n"
        ),
        "{manifest}"
    );
    assert!(
        manifest.contains(
            "build CMakeFiles/clean.additional$:Release: CLEAN_ADDITIONAL\n  CONFIG = Release\n"
        ),
        "{manifest}"
    );
    assert!(
        manifest.contains(
            "build clean$:Debug: CLEAN CMakeFiles/clean.additional$:Debug\n  FILE_ARG = -f CMakeFiles/impl-Debug.ninja\n  TARGETS = CMakeFiles/rcmake-clean:Debug\n"
        ),
        "{manifest}"
    );
    assert!(
        manifest.contains(
            "build clean$:all: CLEAN CMakeFiles/clean.additional$:Debug CMakeFiles/clean.additional$:Release\n  FILE_ARG = -f CMakeFiles/impl-Debug.ninja\n  TARGETS = CMakeFiles/rcmake-clean:Debug CMakeFiles/rcmake-clean:Release\n"
        ),
        "{manifest}"
    );
    Ok(())
}

#[test]
fn prefixed_windows_graph_keeps_real_import_outputs() -> Result<(), Box<dyn std::error::Error>> {
    use std::collections::BTreeMap;

    use rcmake_resolve::{ArtifactSet, GraphTarget};

    let input = BuildGraph {
        steps: vec![BuildStep::Link {
            implicit_inputs: Vec::new(),
            inputs: vec!["C:/build root/generated.res".into()],
            step: LinkStep {
                kind: LinkKind::SharedLibrary,
                language: "C".into(),
                fragments: Vec::new(),
                output: "runtime/app.dll".into(),
            },
            invocations: vec![argv(&[
                "link.exe",
                "generated.res",
                "/out:runtime/app.dll",
                "/implib:lib/app.lib",
                "/pdb:app.pdb",
            ])],
        }],
        targets: vec![GraphTarget {
            name: "app".into(),
            steps: vec![0],
            ..GraphTarget::default()
        }],
        ..BuildGraph::default()
    };
    let artifacts = ArtifactSet {
        file: Some("runtime/app.dll".into()),
        import_file: Some("lib/app.lib".into()),
        pdb_file: Some("app.pdb".into()),
        ..ArtifactSet::default()
    };
    let map = BTreeMap::from([(b"app".as_slice(), &artifacts)]);
    let graph = crate::Graph::with_prefix(
        &input,
        &map,
        Path::new("C:/build root"),
        None,
        b"build root",
        windows(),
    )?;
    let mut ninja = crate::Ninja::default();
    graph.emit(&mut ninja, b"cmake", b"", &[])?;
    let manifest = String::from_utf8(ninja.0)?;
    assert!(manifest.contains("build build$ root/runtime/app.dll | build$ root/lib/app.lib: LINK_SHARED_LIBRARY build$ root/generated.res\n"));
    assert!(manifest.contains(r#" /C "cd /D "C:\build root" && link.exe generated.res /out:runtime/app.dll /implib:lib/app.lib /pdb:app.pdb""#));
    Ok(())
}

#[cfg(windows)]
#[test]
fn native_ninja_preserves_chained_argv_cwd_and_failure() -> Result<(), Box<dyn std::error::Error>> {
    use std::{path::PathBuf, time::Duration};

    use rcmake_host::{
        CommandSpec, Follow, Input, NativeHost, Output, ProcessResult, RunRequest, WriteMode, path,
    };

    struct Fixture<'a>(&'a NativeHost, PathBuf);
    impl Drop for Fixture<'_> {
        fn drop(&mut self) {
            let _ = self.0.remove_dir_all(&self.1);
        }
    }

    let host = NativeHost::new()?;
    let now = host.now();
    let root = std::env::temp_dir().join(format!("rcmake ninja {}-{}", now.secs, now.nanos));
    host.create_dir_all(&root)?;
    let fixture = Fixture(&host, root);
    let run = |program: &str, args: &[&str]| {
        host.run(
            &RunRequest {
                pipeline: vec![CommandSpec {
                    program: program.into(),
                    args: args.iter().copied().map(BString::from).collect(),
                }],
                cwd: fixture.1.clone(),
                env: host.env_all(),
                stdin: Input::Null,
                stdout: Output::Capture,
                stderr: Output::Capture,
                timeout: Some(Duration::from_secs(30)),
                tee: false,
                default_error_mode: false,
            },
            &mut |_, _| {},
        )
    };

    for tool in ["python.exe", "ninja.exe"] {
        if run(tool, &["--version"])?.results != [ProcessResult::Exited(0)] {
            return Ok(());
        }
    }

    host.write(
        &fixture.1.join("capture argv.py"),
        br#"import pathlib, sys
mode, *args = sys.argv[1:]
cwd = pathlib.Path.cwd()
if mode == "fail":
    sys.exit(23)
if mode == "check":
    assert cwd.name == "first cwd", cwd
    assert args == ["dollar $ and O'Neil", '/DNAME="two words"', "D:\\path with space\\", "100%PATH%!"], args
else:
    assert cwd.name == "second cwd", cwd
    pathlib.Path("captured").touch()
"#,
        WriteMode::Truncate,
    )?;
    let invocation = |directory: &str, mode: &str, args: &[&str]| {
        let mut invocation = argv(&["python.exe"]);
        invocation
            .argv
            .push(path::from_native_cmake(&fixture.1.join("capture argv.py")));
        invocation.argv.push(mode.into());
        invocation
            .argv
            .extend(args.iter().copied().map(BString::from));
        invocation.working_dir = Some(path::from_native_cmake(&fixture.1.join(directory)));
        invocation
    };

    let first = fixture.1.join("first cwd");
    let second = fixture.1.join("second cwd");
    host.create_dir_all(&first)?;
    host.create_dir_all(&second)?;
    let result = GenerationResult {
        graphs: vec![BuildGraph {
            steps: vec![
                BuildStep::Custom(CustomStep {
                    outputs: vec!["second cwd/captured".into()],
                    invocations: vec![
                        invocation(
                            "first cwd",
                            "check",
                            &[
                                "dollar $ and O'Neil",
                                r#"/DNAME="two words""#,
                                "D:\\path with space\\",
                                "100%PATH%!",
                            ],
                        ),
                        invocation("second cwd", "commit", &[]),
                    ],
                    ..CustomStep::default()
                }),
                BuildStep::Custom(CustomStep {
                    outputs: vec!["fail".into()],
                    invocations: vec![
                        invocation("first cwd", "fail", &[]),
                        invocation("second cwd", "commit", &[]),
                    ],
                    ..CustomStep::default()
                }),
            ],
            ..BuildGraph::default()
        }],
        ..GenerationResult::default()
    };
    crate::generate_single_config(&result, &host, &fixture.1)?;
    let ok = run("ninja.exe", &["second cwd/captured"])?;
    assert_eq!(ok.results, [ProcessResult::Exited(0)], "{ok:?}");
    let marker = second.join("captured");
    assert!(host.metadata(&marker, Follow::Yes).is_ok());
    host.remove_file(&marker)?;
    let failed = run("ninja.exe", &["fail"])?;
    assert!(
        matches!(failed.results.as_slice(), [ProcessResult::Exited(code)] if *code != 0),
        "{failed:?}"
    );
    assert!(host.metadata(&marker, Follow::Yes).is_err());
    Ok(())
}

#[test]
fn windows_export_helper_keeps_literal_object_list() -> Result<(), Box<dyn std::error::Error>> {
    use rcmake_model::{BT, GeneratorKind, Target, TargetKind, TargetOrigin};

    let build = Path::new("/work/build");
    for (force, count) in [(true, 3), (false, 100)] {
        let host = PureHost::default();
        host.env_set(b"CMAKE_NINJA_FORCE_RESPONSE_FILE", force.then_some(b""));
        let mut model = ProjectModel::default();
        model.generator = Some(GeneratorKind::Ninja);
        model.set_root_dirs("/work/source".into(), "/work/build".into());
        for (key, value) in [
            ("CMAKE_HOST_SYSTEM_NAME", "Windows"),
            ("CMAKE_SYSTEM_NAME", "Windows"),
            ("CMAKE_COMMAND", "rcmake.exe"),
            ("CMAKE_CXX_COMPILER", "C:/VC/cl.exe"),
            ("CMAKE_CXX_COMPILER_ID", "MSVC"),
            ("CMAKE_CXX_OUTPUT_EXTENSION", ".obj"),
            ("CMAKE_CXX_LINK_DEF_FILE_FLAG", "/DEF:"),
            ("CMAKE_SHARED_LIBRARY_SUFFIX", ".dll"),
            (
                "CMAKE_CXX_CREATE_SHARED_LIBRARY",
                "rcmake.exe -E vs_link_dll --intdir=CMakeFiles/lib.dir -- C:/VC/link.exe /out:<TARGET> <LINK_FLAGS> <OBJECTS> <LINK_LIBRARIES>",
            ),
        ] {
            model
                .dir_mut(DirId::ROOT)
                .vars
                .insert(key.into(), value.into());
        }
        let mut target = Target::new(
            "exported".into(),
            TargetKind::SharedLibrary,
            TargetOrigin::Normal,
            DirId::ROOT,
            Default::default(),
            Default::default(),
        );
        target
            .props
            .set("WINDOWS_EXPORT_ALL_SYMBOLS".into(), "ON".into());
        target
            .sources
            .push(BT::new("main.cpp".into(), Default::default()));
        host.seed_file(
            Path::new("/work/source/main.cpp"),
            b"int exported() { return 42; }",
        )?;
        let mut last = String::new();
        for index in 0..count {
            last = format!("object inputs/obj-{index}-{}.obj", "path".repeat(30));
            host.seed_file(&build.join(&last), b"")?;
            target.sources.push(BT::new(
                format!("/work/build/{last}").into(),
                Default::default(),
            ));
        }
        model.add_target(target)?;
        let result = rcmake_resolve::resolve(&model, &host, &[])?;
        generate_single(&result, &host, build, Some(&model), windows())?;
        let manifest = read(&host, "build.ninja");
        let files = read(&host, "CMakeFiles/exported.dir/exports.def.objs");
        assert_eq!(files.lines().count(), count + 1);
        assert!(files.lines().any(|name| name == last));
        assert!(manifest.contains("__create_def CMakeFiles/exported.dir/exports.def CMakeFiles/exported.dir/exports.def.objs"));
        assert!(manifest.contains("vs_link_dll --intdir=CMakeFiles/lib.dir -- C:/VC/link.exe"));
        assert!(manifest.contains("/out:"));
        assert!(manifest.contains("@CMakeFiles/rcmake-link-"));
        assert!(
            manifest
                .lines()
                .any(|line| line.starts_with("  RSP_ARGS =") && line.contains(&last))
        );
    }
    Ok(())
}

#[test]
fn windows_msvc_compiler_launcher_emits_launched_msvc_rule()
-> Result<(), Box<dyn std::error::Error>> {
    use rcmake_model::{BT, GeneratorKind, Target, TargetKind, TargetOrigin};

    let host = PureHost::default();
    let build = Path::new("/work/build");
    let mut model = ProjectModel::default();
    model.generator = Some(GeneratorKind::Ninja);
    model.set_root_dirs("C:/work/src".into(), "C:/work/build".into());
    for (name, value) in [
        ("CMAKE_HOST_SYSTEM_NAME", "Windows"),
        ("CMAKE_SYSTEM_NAME", "Windows"),
        ("CMAKE_C_COMPILER", "C:/VC/cl.exe"),
        ("CMAKE_C_COMPILER_ID", "MSVC"),
        ("CMAKE_C_DEPFILE_FORMAT", "msvc"),
        ("CMAKE_CL_SHOWINCLUDES_PREFIX", "Note: including file: "),
        (
            "CMAKE_C_COMPILE_OBJECT",
            "<CMAKE_C_COMPILER> /nologo /showIncludes <DEFINES> <INCLUDES> <FLAGS> /Fo<OBJECT> -c <SOURCE>",
        ),
    ] {
        model
            .dir_mut(DirId::ROOT)
            .vars
            .insert(name.into(), value.into());
    }
    model.global.props.set(
        "RULE_LAUNCH_COMPILE".into(),
        "\"C:/Program Files/CMake/bin/ctest.exe\" --launch --target-name <TARGET_NAME> --".into(),
    );
    let mut target = Target::new(
        "app".into(),
        TargetKind::ObjectLibrary,
        TargetOrigin::Normal,
        DirId::ROOT,
        Default::default(),
        Default::default(),
    );
    target.props.set(
        "C_COMPILER_LAUNCHER".into(),
        "C:/Program Files/CMake/bin/cmake.exe;-E;env;USED_LAUNCHER=1".into(),
    );
    target
        .sources
        .push(BT::new("main.c".into(), Default::default()));
    model.add_target(target)?;
    host.seed_file(
        Path::new("C:/work/src/main.c"),
        b"int main(void) { return 0; }\n",
    )?;

    let result = rcmake_resolve::resolve(&model, &host, &[])?;
    generate_single(&result, &host, build, Some(&model), windows())?;
    let rules = read(&host, "CMakeFiles/rules.ninja");
    let manifest = read(&host, "build.ninja");
    assert!(
        rules.contains("rule COMPILE_MSVC_LAUNCHED\n") && rules.contains("  deps = msvc\n"),
        "expected COMPILE_MSVC_LAUNCHED with deps = msvc in rules:\n{rules}"
    );
    assert!(
        rules.contains(
            r#""C:\Program Files\CMake\bin\ctest.exe" --launch $LAUNCH_ARGS -- $LAUNCHER $COMMAND"#
        ) || rules.contains(
            r#""C:/Program Files/CMake/bin/ctest.exe" --launch $LAUNCH_ARGS -- $LAUNCHER $COMMAND"#
        ),
        "expected natively quoted ctest program in rules:\n{rules}"
    );
    assert!(
        manifest.contains(": COMPILE_MSVC_LAUNCHED ")
            && manifest.contains("  LAUNCH_ARGS = --target-name app")
            && manifest.contains(
                "  LAUNCHER = \"C:/Program Files/CMake/bin/cmake.exe\" -E env USED_LAUNCHER=1"
            ),
        "expected COMPILE_MSVC_LAUNCHED edge with LAUNCHER binding:\n{manifest}"
    );
    Ok(())
}

#[test]
fn ninja_prunes_missing_configure_listfiles_when_existing_present()
-> Result<(), Box<dyn std::error::Error>> {
    let host = PureHost::default();
    let build = Path::new("/work/build");
    host.seed_file(Path::new("/work/src/CMakeLists.txt"), b"project(Demo)\n")?;
    let mut result = GenerationResult {
        graphs: vec![BuildGraph::default()],
        ..GenerationResult::default()
    };
    result.metadata.listfiles = vec![
        "/work/src/CMakeLists.txt".into(),
        "/work/build/deleted_temp_probe.cmake".into(),
    ];
    generate_single(&result, &host, build, None, Commands::default())?;
    let manifest = read(&host, "build.ninja");
    assert!(
        manifest.contains("/work/src/CMakeLists.txt"),
        "existing listfile must stay in RERUN_CMAKE:\n{manifest}"
    );
    assert!(
        !manifest.contains("deleted_temp_probe.cmake"),
        "deleted configure input must be pruned to prevent regen loop:\n{manifest}"
    );
    Ok(())
}

#[test]
fn ninja_excluded_dir_explicit_exclude_off_propagates_to_ancestors()
-> Result<(), Box<dyn std::error::Error>> {
    use rcmake_model::{BT, GeneratorKind, Target, TargetKind, TargetOrigin};

    let host = PureHost::default();
    let build = Path::new("/work/build");
    let mut model = ProjectModel::default();
    model.generator = Some(GeneratorKind::Ninja);
    model.set_root_dirs("/work/src".into(), "/work/build".into());
    let sub = model.add_directory(
        DirId::ROOT,
        "/work/src/sub".into(),
        "/work/build/sub".into(),
        Default::default(),
    )?;
    model.dir_mut(sub).exclude_from_all = true;
    let subsub = model.add_directory(
        sub,
        "/work/src/sub/subsub".into(),
        "/work/build/sub/subsub".into(),
        Default::default(),
    )?;
    model.dir_mut(subsub).exclude_from_all = true;
    for dir in [DirId::ROOT, sub, subsub] {
        for (name, value) in [
            ("CMAKE_SYSTEM_NAME", "Linux"),
            ("CMAKE_C_COMPILER", "/usr/bin/cc"),
            ("CMAKE_C_COMPILER_ID", "GNU"),
        ] {
            model.dir_mut(dir).vars.insert(name.into(), value.into());
        }
    }

    let add_lib = |model: &mut ProjectModel,
                   dir: DirId,
                   name: &str,
                   src: &str,
                   exclude: Option<&str>|
     -> Result<(), Box<dyn std::error::Error>> {
        let mut t = Target::new(
            name.into(),
            TargetKind::StaticLibrary,
            TargetOrigin::Normal,
            dir,
            Default::default(),
            Default::default(),
        );
        if let Some(val) = exclude {
            t.props.set("EXCLUDE_FROM_ALL".into(), val.into());
        }
        t.sources.push(BT::new(src.into(), Default::default()));
        model.add_target(t)?;
        Ok(())
    };
    add_lib(&mut model, sub, "zot", "zot.c", None)?;
    add_lib(&mut model, sub, "subinc", "subinc.c", Some("OFF"))?;
    add_lib(&mut model, subsub, "subsub", "subsub.c", None)?;
    add_lib(&mut model, subsub, "subsubinc", "subsubinc.c", Some("OFF"))?;

    host.seed_file(
        Path::new("/work/src/sub/zot.c"),
        b"int z(void){return 0;}\n",
    )?;
    host.seed_file(
        Path::new("/work/src/sub/subinc.c"),
        b"int s(void){return 0;}\n",
    )?;
    host.seed_file(
        Path::new("/work/src/sub/subsub/subsub.c"),
        b"int a(void){return 0;}\n",
    )?;
    host.seed_file(
        Path::new("/work/src/sub/subsub/subsubinc.c"),
        b"int b(void){return 0;}\n",
    )?;

    let result = rcmake_resolve::resolve(&model, &host, &[])?;
    generate_single(&result, &host, build, Some(&model), Commands::default())?;
    let manifest = read(&host, "build.ninja");
    let root_all = manifest
        .lines()
        .find(|line| line.starts_with("build all: phony "))
        .ok_or("missing root all")?;
    assert!(
        root_all.contains("libsubinc.a"),
        "root all missing subinc: {root_all}"
    );
    assert!(
        root_all.contains("libsubsubinc.a"),
        "root all missing subsubinc: {root_all}"
    );
    assert!(
        !root_all.contains("libzot.a"),
        "root all must not contain zot: {root_all}"
    );
    assert!(
        !root_all.contains("libsubsub.a"),
        "root all must not contain subsub: {root_all}"
    );

    let sub_all = manifest
        .lines()
        .find(|line| line.starts_with("build sub/all: phony "))
        .ok_or("missing sub/all")?;
    assert!(
        sub_all.contains("libzot.a"),
        "sub/all missing zot: {sub_all}"
    );
    assert!(
        sub_all.contains("libsubinc.a"),
        "sub/all missing subinc: {sub_all}"
    );
    assert!(
        sub_all.contains("libsubsubinc.a"),
        "sub/all missing subsubinc: {sub_all}"
    );
    assert!(
        !sub_all.contains("libsubsub.a"),
        "sub/all must not contain subsub: {sub_all}"
    );
    Ok(())
}

#[test]
fn windows_msvc_unquoted_backslash_launcher_preserves_backslashes()
-> Result<(), Box<dyn std::error::Error>> {
    use rcmake_model::{BT, GeneratorKind, Target, TargetKind, TargetOrigin};

    let host = PureHost::default();
    let build = Path::new("/work/build");
    let mut model = ProjectModel::default();
    model.generator = Some(GeneratorKind::Ninja);
    model.set_root_dirs("C:/work/src".into(), "C:/work/build".into());
    for (name, value) in [
        ("CMAKE_HOST_SYSTEM_NAME", "Windows"),
        ("CMAKE_SYSTEM_NAME", "Windows"),
        ("CMAKE_C_COMPILER", r"C:\VC\cl.exe"),
        ("CMAKE_C_COMPILER_ID", "MSVC"),
        ("CMAKE_C_DEPFILE_FORMAT", "msvc"),
        ("CMAKE_CL_SHOWINCLUDES_PREFIX", "Note: including file: "),
        (
            "CMAKE_C_COMPILE_OBJECT",
            "<CMAKE_C_COMPILER> /nologo /showIncludes <DEFINES> <INCLUDES> <FLAGS> /Fo<OBJECT> -c <SOURCE>",
        ),
    ] {
        model
            .dir_mut(DirId::ROOT)
            .vars
            .insert(name.into(), value.into());
    }
    model.global.props.set(
        "RULE_LAUNCH_COMPILE".into(),
        r"C:\Tools\ctest.exe --launch --target-name <TARGET_NAME> --".into(),
    );
    let mut target = Target::new(
        "app".into(),
        TargetKind::ObjectLibrary,
        TargetOrigin::Normal,
        DirId::ROOT,
        Default::default(),
        Default::default(),
    );
    target
        .props
        .set("C_COMPILER_LAUNCHER".into(), r"C:\Tools\wrap.exe;-E".into());
    target
        .sources
        .push(BT::new("main.c".into(), Default::default()));
    model.add_target(target)?;
    host.seed_file(
        Path::new("C:/work/src/main.c"),
        b"int main(void) { return 0; }\n",
    )?;

    let result = rcmake_resolve::resolve(&model, &host, &[])?;
    generate_single(&result, &host, build, Some(&model), windows())?;
    let rules = read(&host, "CMakeFiles/rules.ninja");
    let manifest = read(&host, "build.ninja");
    assert!(
        rules.contains(r"C:\Tools\ctest.exe --launch $LAUNCH_ARGS -- $LAUNCHER $COMMAND"),
        "expected unquoted Windows launcher backslashes preserved in rules:\n{rules}"
    );
    assert!(
        !rules.contains("C:Toolsctest.exe"),
        "must not strip backslashes from launcher program:\n{rules}"
    );
    assert!(
        manifest.contains(r"LAUNCHER = C:\Tools\wrap.exe -E"),
        "expected compiler launcher backslashes preserved in manifest:\n{manifest}"
    );
    assert!(
        manifest.contains(r"C:\VC\cl.exe"),
        "expected compiler backslashes preserved in manifest:\n{manifest}"
    );
    Ok(())
}

#[test]
fn ninja_multi_config_excluded_dir_genex_exclude_propagates_per_config()
-> Result<(), Box<dyn std::error::Error>> {
    use rcmake_model::{BT, GeneratorKind, Target, TargetKind, TargetOrigin};

    let host = PureHost::default();
    let build = Path::new("/work/build");
    let mut model = ProjectModel::default();
    model.generator = Some(GeneratorKind::NinjaMultiConfig);
    model.set_root_dirs("/work/src".into(), "/work/build".into());
    let sub = model.add_directory(
        DirId::ROOT,
        "/work/src/sub".into(),
        "/work/build/sub".into(),
        Default::default(),
    )?;
    model.dir_mut(sub).exclude_from_all = true;
    for dir in [DirId::ROOT, sub] {
        for (name, value) in [
            ("CMAKE_SYSTEM_NAME", "Linux"),
            ("CMAKE_C_COMPILER", "/usr/bin/cc"),
            ("CMAKE_C_COMPILER_ID", "GNU"),
        ] {
            model.dir_mut(dir).vars.insert(name.into(), value.into());
        }
    }

    let mut cond = Target::new(
        "cond".into(),
        TargetKind::StaticLibrary,
        TargetOrigin::Normal,
        sub,
        Default::default(),
        Default::default(),
    );
    cond.props
        .set("EXCLUDE_FROM_ALL".into(), "$<CONFIG:Debug>".into());
    cond.sources.push(BT::new("c.c".into(), Default::default()));
    model.add_target(cond)?;
    host.seed_file(Path::new("/work/src/sub/c.c"), b"int c(void){return 0;}\n")?;

    let configs = [ConfigId("Debug".into()), ConfigId("Release".into())];
    let result = rcmake_resolve::resolve(&model, &host, &configs)?;
    generate_multi(
        &result,
        &host,
        build,
        Some(&model),
        Commands::default(),
        None,
    )?;

    let debug_impl = read(&host, "CMakeFiles/impl-Debug.ninja");
    let debug_root_all = debug_impl
        .lines()
        .find(|line| line.starts_with("build all$:Debug: phony"))
        .ok_or("missing Debug root all")?;
    assert!(
        !debug_root_all.contains("libcond.a"),
        "Debug root all must exclude cond: {debug_root_all}"
    );
    let debug_sub_all = debug_impl
        .lines()
        .find(|line| line.starts_with("build sub/all$:Debug: phony"))
        .ok_or("missing Debug sub/all")?;
    assert!(
        !debug_sub_all.contains("libcond.a"),
        "Debug sub/all must exclude cond: {debug_sub_all}"
    );

    let release_impl = read(&host, "CMakeFiles/impl-Release.ninja");
    let release_root_all = release_impl
        .lines()
        .find(|line| line.starts_with("build all$:Release: phony"))
        .ok_or("missing Release root all")?;
    assert!(
        !release_root_all.contains("libcond.a"),
        "Release root all must exclude cond because it is excluded in Debug: {release_root_all}"
    );
    let release_sub_all = release_impl
        .lines()
        .find(|line| line.starts_with("build sub/all$:Release: phony"))
        .ok_or("missing Release sub/all")?;
    assert!(
        release_sub_all.contains("libcond.a"),
        "Release sub/all must include cond: {release_sub_all}"
    );
    Ok(())
}
