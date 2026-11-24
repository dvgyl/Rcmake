use std::path::Path;

use rcmake_base::BString;
use rcmake_genex::ConfigId;
use rcmake_host::{Host, PureHost};
use rcmake_model::{DirId, ProjectModel};
use rcmake_resolve::{
    BuildGraph, BuildStep, CustomStep, GenerationResult, Invocation, LinkKind, LinkStep,
};

use crate::{commands::Commands, generate_multi, generate_single, tests::read};

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
    generate_multi(&result, &host, dir, Some(&model), windows())?;
    let manifest = read(&host, "CMakeFiles/impl-Debug.ninja");
    assert!(manifest.contains(
        r#" /C "cmake -E rm -rf -- "/work/build/extra Debug.txt" && "C:/Ninja Tools/ninja.exe" -f CMakeFiles/impl-Debug.ninja -t clean CMakeFiles/rcmake-clean:Debug""#
    ));
    assert!(manifest.contains(
        r#" /C "cmake -E rm -rf -- "/work/build/extra Debug.txt" "/work/build/extra Release.txt" && "C:/Ninja Tools/ninja.exe" -f CMakeFiles/impl-Debug.ninja -t clean CMakeFiles/rcmake-clean:Debug CMakeFiles/rcmake-clean:Release""#
    ));
    Ok(())
}

#[test]
fn prefixed_windows_graph_keeps_real_import_outputs() -> Result<(), Box<dyn std::error::Error>> {
    use std::collections::BTreeMap;

    use rcmake_resolve::{ArtifactSet, GraphTarget};

    let input = BuildGraph {
        steps: vec![BuildStep::Link {
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
    graph.emit(&mut ninja, b"cmake", b"", &[]);
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
    assert_eq!(failed.results, [ProcessResult::Exited(1)], "{failed:?}");
    assert!(host.metadata(&marker, Follow::Yes).is_err());
    Ok(())
}
