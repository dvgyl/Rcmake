use std::path::Path;
use std::sync::{Arc, Mutex};

use rcmake_host::{FileKind, Follow, PureHost, RunOutput};
use rcmake_resolve::{CustomStep, GraphTarget, LinkKind, LinkStep};

use super::*;

fn invocation(argv: &[&str]) -> Invocation {
    Invocation {
        argv: argv.iter().copied().map(BString::from).collect(),
        ..Invocation::default()
    }
}

fn custom(program: &str) -> BuildStep {
    BuildStep::Custom(CustomStep {
        outputs: vec!["custom/out".into()],
        byproducts: vec!["custom/byproduct/out".into()],
        invocations: vec![invocation(&[program])],
        ..CustomStep::default()
    })
}

fn link(file: &str, invocations: Vec<Invocation>) -> BuildStep {
    BuildStep::Link {
        implicit_inputs: Vec::new(),
        inputs: vec!["obj/main.o".into()],
        step: LinkStep {
            kind: LinkKind::Executable,
            language: "C".into(),
            fragments: Vec::new(),
            output: file.into(),
        },
        invocations,
    }
}

fn target(name: &str, steps: Vec<usize>, depends: Vec<usize>, in_all: bool) -> GraphTarget {
    GraphTarget {
        name: name.into(),
        steps,
        depends,
        in_all,
    }
}

#[test]
fn builds_selected_dependencies_first_and_runs_shared_steps_once() {
    let host = PureHost::default();
    let calls = Arc::new(Mutex::new(Vec::new()));
    let order = calls.clone();
    host.set_command_handler(move |host, request, _| {
        assert_eq!(request.stderr, Output::SharedWithStdout);
        assert!(request.timeout.is_some());
        assert_eq!(request.cwd, Path::new("/work/build"));
        for dir in ["/work/build/custom", "/work/build/custom/byproduct"] {
            assert_eq!(
                host.metadata(Path::new(dir), Follow::No)?.kind,
                FileKind::Dir
            );
        }
        order
            .lock()
            .unwrap()
            .push(request.pipeline[0].program.clone());
        Ok(RunOutput {
            stdout: b"stdout\n".to_vec(),
            stderr: b"stderr\n".to_vec(),
            results: vec![ProcessResult::Exited(0)],
        })
    })
    .unwrap();
    let graph = BuildGraph {
        steps: vec![
            custom("main"),
            custom("dep"),
            custom("common"),
            custom("extra"),
        ],
        targets: vec![
            target("main", vec![0], vec![1, 2], true),
            target("dep", vec![1, 2], vec![2], false),
            target("common", vec![2], vec![], false),
            target("extra", vec![3], vec![], false),
        ],
        ..BuildGraph::default()
    };
    let opts = BuildOptions {
        build_dir: "/work/build".into(),
        ..BuildOptions::default()
    };
    let result = execute(&graph, &host, &opts).unwrap();
    assert_eq!(*calls.lock().unwrap(), ["common", "dep", "main"]);
    assert_eq!(
        result,
        BuildOutcome {
            success: true,
            exit_code: 0,
            steps_run: 3,
            output: b"stdout\nstderr\n".repeat(3).into()
        }
    );

    calls.lock().unwrap().clear();
    let opts = BuildOptions {
        targets: vec!["extra".into(), "dep".into()],
        ..opts
    };
    execute(&graph, &host, &opts).unwrap();
    assert_eq!(*calls.lock().unwrap(), ["extra", "common", "dep"]);
}

#[test]
fn rejects_bad_indices_and_cycles_before_running_steps() {
    let host = PureHost::default();
    let options = BuildOptions::default();
    let mut graph = BuildGraph {
        steps: vec![custom("must-not-run")],
        targets: vec![target("all", vec![0], vec![1], true)],
        ..BuildGraph::default()
    };
    assert_eq!(
        execute(&graph, &host, &options),
        Err(BuildError::InvalidTarget(1))
    );
    graph.targets[0].depends = vec![0];
    assert_eq!(execute(&graph, &host, &options), Err(BuildError::Cycle));
    graph.targets[0].depends.clear();
    graph.targets[0].steps.push(1);
    assert_eq!(
        execute(&graph, &host, &options),
        Err(BuildError::InvalidStep(1))
    );
    let options = BuildOptions {
        targets: vec!["unknown".into()],
        ..options
    };
    assert_eq!(
        execute(&graph, &host, &options),
        Err(BuildError::UnknownTarget("unknown".into()))
    );
}

#[test]
fn uses_argv_and_shell_fallback_and_stops_at_first_failed_invocation() -> Result<(), BuildError> {
    #[cfg(windows)]
    let build_dir = Path::new("C:/work/build");
    #[cfg(not(windows))]
    let build_dir = Path::new("/work/build");
    let host = PureHost::default();
    let calls = Arc::new(Mutex::new(Vec::new()));
    let observed = calls.clone();
    host.set_command_handler(move |host, request, _| {
        let [command] = request.pipeline.as_slice() else {
            return Err(HostError::unsupported("Expected one command."));
        };
        let shell = command.program == "/bin/sh";
        if shell {
            assert_eq!(command.args, ["-c", "first link"]);
            assert_eq!(request.cwd, build_dir.join("obj"));
            assert_eq!(
                host.metadata(&build_dir.join("bin"), Follow::No)?.kind,
                FileKind::Dir
            );
        } else {
            assert_eq!(command.args, ["arg with spaces"]);
            assert_eq!(
                host.metadata(&build_dir.join("obj"), Follow::No)?.kind,
                FileKind::Dir
            );
        }
        observed
            .lock()
            .map_err(|_| HostError::unsupported("The command log lock is poisoned."))?
            .push(command.program.clone());
        Ok(RunOutput {
            stdout: if shell {
                b"link failed\n".to_vec()
            } else {
                b"compile output\n".to_vec()
            },
            results: vec![ProcessResult::Exited(if shell { 7 } else { 0 })],
            ..RunOutput::default()
        })
    })?;
    let graph = BuildGraph {
        steps: vec![
            BuildStep::Compile {
                byproducts: Vec::new(),
                compile_input: None,
                object: "obj/main.o".into(),
                source: "main.c".into(),
                invocation: invocation(&["compiler", "arg with spaces"]),
                depfile: None,
                modmap: None,
                dyndep: None,
            },
            link(
                "bin/main",
                vec![
                    Invocation {
                        shell: "first link".into(),
                        working_dir: Some("obj".into()),
                        ..Invocation::default()
                    },
                    invocation(&["must-not-run"]),
                ],
            ),
            custom("must-not-run-either"),
        ],
        targets: vec![target("main", vec![0, 1, 2], vec![], true)],
        ..BuildGraph::default()
    };
    let result = execute(
        &graph,
        &host,
        &BuildOptions {
            build_dir: build_dir.into(),
            ..BuildOptions::default()
        },
    )?;
    assert_eq!(
        *calls
            .lock()
            .map_err(|_| HostError::unsupported("The command log lock is poisoned."))?,
        ["compiler", "/bin/sh"]
    );
    assert!(!result.success);
    assert_eq!(result.exit_code, 7);
    assert_eq!(result.steps_run, 2);
    assert_eq!(result.output, "compile output\nlink failed\n");
    Ok(())
}

#[cfg(unix)]
#[test]
fn builds_and_links_c_with_native_host() -> Result<(), Box<dyn std::error::Error>> {
    use rcmake_host::{NativeHost, WriteMode};

    let host = NativeHost::new()?;
    let temp = host.run(
        &RunRequest {
            pipeline: vec![CommandSpec {
                program: "mktemp".into(),
                args: vec!["-d".into(), "/tmp/rcmake-build.XXXXXXXX".into()],
            }],
            cwd: "/tmp".into(),
            env: host.env_all(),
            stdin: Input::Null,
            stdout: Output::Capture,
            stderr: Output::Capture,
            timeout: Some(Duration::from_secs(15)),
            tee: false,
        },
        &mut |_, _| {},
    )?;
    assert_eq!(temp.results, [ProcessResult::Exited(0)]);
    let work = PathBuf::from(std::str::from_utf8(&temp.stdout)?.trim());
    struct Temp<'a>(&'a dyn Host, &'a Path);
    impl Drop for Temp<'_> {
        fn drop(&mut self) {
            let _ = self.0.remove_dir_all(self.1);
        }
    }
    let _temp = Temp(&host, &work);
    host.write(
        &work.join("main.c"),
        b"int main(void) { return 0; }",
        WriteMode::CreateNew,
    )?;
    let graph = BuildGraph {
        steps: vec![
            BuildStep::Compile {
                byproducts: Vec::new(),
                compile_input: None,
                object: "obj/main.o".into(),
                source: "main.c".into(),
                depfile: None,
                invocation: invocation(&["/usr/bin/cc", "-v", "-c", "main.c", "-o", "obj/main.o"]),
                modmap: None,
                dyndep: None,
            },
            link(
                "bin/main",
                vec![invocation(&["/usr/bin/cc", "obj/main.o", "-o", "bin/main"])],
            ),
        ],
        targets: vec![target("main", vec![0, 1], vec![], true)],
        ..BuildGraph::default()
    };
    let options = BuildOptions {
        build_dir: work.clone(),
        ..BuildOptions::default()
    };
    let outcome = execute(&graph, &host, &options)?;
    assert!(outcome.success, "{}", outcome.output);
    assert!(!outcome.output.is_empty());
    assert_ne!(
        host.metadata(&work.join("bin/main"), Follow::Yes)?.mode & 0o111,
        0
    );
    Ok(())
}

#[test]
fn scans_and_collates_before_compiling_module_providers() -> Result<(), Box<dyn std::error::Error>>
{
    use rcmake_cxxmodules::tdi::read_tdi;
    use rcmake_cxxmodules::{ModmapFormat, ModuleSourceInfo, ModuleVisibility, TargetDependInfo};

    let host = PureHost::default();
    let calls = Arc::new(Mutex::new(Vec::new()));
    let observed = calls.clone();
    host.set_command_handler(move |host, request, _| {
        let command = &request.pipeline[0];
        let file = &command.args[0];
        observed.lock().unwrap().push(file.clone());
        if command.program == "scan" {
            let data = if file == "obj/use.o.ddi" {
                br#"{"version":1,"rules":[{"primary-output":"obj/use.o","requires":[{"logical-name":"math"}]}]}"#.as_slice()
            } else {
                br#"{"version":1,"rules":[{"primary-output":"obj/math.o","provides":[{"logical-name":"math"}]}]}"#
            };
            host.write(&request.cwd.join(path::to_native(file)), data, WriteMode::Truncate)?;
        } else {
            let contents = host.read(&request.cwd.join("obj/CXX.dd"), ReadRange::All { limit: 1024 })?;
            assert!(contents.starts_with(b"ninja_dyndep_version = 1.0"));
            let map = &command.args[1];
            let map = map.strip_prefix(b"@").unwrap();
            let contents = host.read(&request.cwd.join(path::to_native(map)), ReadRange::All { limit: 1024 })?;
            if file == "obj/use.o" {
                assert!(contents.starts_with(br#"-fmodule-file="math=obj/math.pcm""#));
                assert_eq!(host.metadata(&request.cwd.join("obj/math.o"), Follow::Yes)?.kind, FileKind::File);
            }
            host.write(&request.cwd.join(path::to_native(file)), b"object", WriteMode::Truncate)?;
        }
        Ok(RunOutput { results: vec![ProcessResult::Exited(0)], ..RunOutput::default() })
    })?;
    let scan = |source: &str, object: &str, ddi: &str| BuildStep::Scan {
        lang: "CXX".into(),
        preprocessed: None,
        source: source.into(),
        object: object.into(),
        ddi: ddi.into(),
        invocation: invocation(&["scan", ddi]),
        depfile: None,
    };
    let compile = |source: &str, object: &str, map: &str| BuildStep::Compile {
        byproducts: Vec::new(),
        compile_input: None,
        source: source.into(),
        object: object.into(),
        modmap: Some(map.into()),
        invocation: invocation(&["compile", object, &format!("@{map}")]),
        depfile: None,
        dyndep: Some("obj/CXX.dd".into()),
    };
    let graph = BuildGraph {
        steps: vec![
            scan("use.cpp", "obj/use.o", "obj/use.o.ddi"),
            scan("math.cppm", "obj/math.o", "obj/math.o.ddi"),
            BuildStep::Collate {
                lang: "CXX".into(),
                target: "main".into(),
                ddis: vec!["obj/use.o.ddi".into(), "obj/math.o.ddi".into()],
                dd: "obj/CXX.dd".into(),
                modmaps: vec!["obj/use.o.modmap".into(), "obj/math.o.modmap".into()],
                tdi: "obj/CXXDependInfo.json".into(),
                depend_info: Box::new(rcmake_resolve::DependInfo::Cxx(TargetDependInfo {
                    language: "CXX".into(),
                    cxx_modules: BTreeMap::from([(
                        "obj/math.o".into(),
                        ModuleSourceInfo {
                            source: "/work/math.cppm".into(),
                            file_set: "modules".into(),
                            visibility: ModuleVisibility::Public,
                            ..ModuleSourceInfo::default()
                        },
                    )]),
                    ..TargetDependInfo::default()
                })),
                modmap_format: Some(ModmapFormat::Clang),
                implicit_inputs: vec!["obj/CXXDependInfo.json".into()],
            },
            compile("use.cpp", "obj/use.o", "obj/use.o.modmap"),
            compile("math.cppm", "obj/math.o", "obj/math.o.modmap"),
        ],
        targets: vec![target("main", vec![0, 1, 2, 3, 4], vec![], true)],
        ..BuildGraph::default()
    };
    let outcome = execute(
        &graph,
        &host,
        &BuildOptions {
            build_dir: "/work/build".into(),
            ..BuildOptions::default()
        },
    )?;
    assert!(outcome.success);
    assert_eq!(outcome.steps_run, 5);
    assert_eq!(
        *calls.lock().unwrap(),
        ["obj/use.o.ddi", "obj/math.o.ddi", "obj/math.o", "obj/use.o"]
    );
    let tdi = host.read(
        Path::new("/work/build/obj/CXXDependInfo.json"),
        ReadRange::All { limit: 8192 },
    )?;
    assert_eq!(read_tdi(&tdi)?.module_dir, "/work/build/obj");
    let metadata = host.read(
        Path::new("/work/build/obj/CXXModules.json"),
        ReadRange::All { limit: 8192 },
    )?;
    assert_eq!(
        read_modules_json(&metadata)?.modules["math".as_bytes()].bmi,
        "/work/build/obj/math.pcm"
    );
    Ok(())
}

#[test]
fn windows_build_processes_use_argv_and_prepare_side_output_paths() {
    let cwd = Path::new("C:/Probe Build/CMakeFiles/CMakeScratch");
    let host = PureHost::new(cwd.into(), "C:/Program Files/Rcmake/cmake.exe".into());
    let calls = Arc::new(Mutex::new(Vec::new()));
    let recorded = Arc::clone(&calls);
    host.set_command_handler(move |host, request, _| {
        let command = &request.pipeline[0];
        recorded.lock().unwrap().push(command.program.clone());
        for dir in match command.program.as_slice() {
            b"C:/Program Files (x86)/VC Tools/cl.exe" => {
                assert_eq!(
                    command.args,
                    [
                        "/Foobj dir/main.obj",
                        "/Fdobj dir/pdb/",
                        "C:/Source Tree/a'b.cpp"
                    ]
                );
                &["C:/Probe Build/CMakeFiles/CMakeScratch/obj dir/pdb"][..]
            }
            b"C:/Program Files (x86)/VC Tools/link.exe" => {
                assert_eq!(
                    command.args,
                    [
                        "/PDB:C:/Debug Files/bin/main.pdb",
                        "/implib:import lib/main.lib",
                        "obj dir/main.obj"
                    ]
                );
                &[
                    "C:/Debug Files/bin",
                    "C:/Probe Build/CMakeFiles/CMakeScratch/import lib",
                ][..]
            }
            _ => panic!("Unexpected build command."),
        } {
            assert_eq!(
                host.metadata(Path::new(dir), Follow::Yes)?.kind,
                FileKind::Dir
            );
        }
        Ok(RunOutput {
            results: vec![ProcessResult::Exited(0)],
            ..RunOutput::default()
        })
    })
    .unwrap();
    let mut output = Vec::new();
    for args in [
        &[
            "C:/Program Files (x86)/VC Tools/cl.exe",
            "/Foobj dir/main.obj",
            "/Fdobj dir/pdb/",
            "C:/Source Tree/a'b.cpp",
        ][..],
        &[
            "C:/Program Files (x86)/VC Tools/link.exe",
            "/PDB:C:/Debug Files/bin/main.pdb",
            "/implib:import lib/main.lib",
            "obj dir/main.obj",
        ][..],
    ] {
        assert_eq!(
            run(&host, cwd, &invocation(args), true, false, &mut output).unwrap(),
            0
        );
    }
    assert_eq!(calls.lock().unwrap().len(), 2);
}

#[test]
fn windows_shell_fragment_runs_from_a_temporary_command_file() -> Result<(), HostError> {
    let host = PureHost::default();
    host.env_set(b"ComSpec", Some(b"C:/Windows/System32/cmd.exe"));
    let script = Arc::new(Mutex::new(PathBuf::new()));
    let observed = Arc::clone(&script);
    let cmd = concat!(
        r#"echo one^&two 100%% | find "a file's contents" > "spaced file""#,
        r#" && link.exe /out:demo.exe "C:\trailing space\" 2>&1"#,
    );
    host.set_command_handler(move |host, request, _| {
        let [tool] = request.pipeline.as_slice() else {
            return Err(HostError::unsupported("Expected one command."));
        };
        assert_eq!(tool.program, "C:/Windows/System32/cmd.exe");
        let name = batch_test_name(&tool.args)?;
        let path = request.cwd.join(path::to_native(name));
        assert_eq!(
            host.read(&path, ReadRange::All { limit: 1024 })?,
            format!("@echo off\r\n{cmd}\r\n").as_bytes()
        );
        *observed
            .lock()
            .map_err(|error| HostError::unsupported(error.to_string()))? = path;
        Ok(RunOutput {
            results: vec![ProcessResult::Exited(9)],
            ..RunOutput::default()
        })
    })?;
    let invocation = Invocation {
        shell: cmd.into(),
        ..Invocation::default()
    };
    assert_eq!(
        run(
            &host,
            Path::new("/work"),
            &invocation,
            true,
            false,
            &mut Vec::new()
        )?,
        9
    );
    let script = script
        .lock()
        .map_err(|error| HostError::unsupported(error.to_string()))?;
    assert_eq!(
        host.metadata(&script, Follow::No).unwrap_err().kind,
        std::io::ErrorKind::NotFound
    );
    Ok(())
}

#[test]
fn windows_compound_rule_uses_comspec_and_preserves_literal_arguments() -> Result<(), HostError> {
    use rcmake_base::shell::quote;

    let host = PureHost::default();
    host.env_set(b"COMSPEC", Some(b"C:/Windows/System32/cmd.exe"));
    host.set_command_handler(|host, request, _| {
        let [tool] = request.pipeline.as_slice() else {
            return Err(HostError::unsupported("Expected one command."));
        };
        assert_eq!(tool.program, "C:/Windows/System32/cmd.exe");
        let path = request.cwd.join(path::to_native(batch_test_name(&tool.args)?));
        assert_eq!(
            host.read(&path, ReadRange::All { limit: 1024 })?,
            concat!(
                "@echo off\r\n",
                r#""C:\MSVC Tools\cl.exe" "C:\100%% complete\\" && link.exe "/PDB:symbol dir\build%%PATH%%.pdb" && echo one^&two"#,
                "\r\n"
            ).as_bytes(),
        );
        assert!(host.metadata(&request.cwd.join("symbol dir"), Follow::Yes)?.kind == rcmake_host::FileKind::Dir);
        Ok(RunOutput {
            results: vec![ProcessResult::Exited(0)],
            ..RunOutput::default()
        })
    })?;
    let shell = BString::from(format!(
        "{} {} && link.exe {} && echo one^&two",
        quote(br"C:\MSVC Tools\cl.exe"),
        quote(br"C:\100% complete\"),
        quote(br"/PDB:symbol dir\build%PATH%.pdb"),
    ));
    let invocation = Invocation {
        argv: vec!["sh".into(), "-c".into(), shell.clone()],
        shell,
        working_dir: None,
        link_response: None,
    };
    assert_eq!(
        run(
            &host,
            Path::new("/work"),
            &invocation,
            true,
            false,
            &mut Vec::new()
        )?,
        0
    );
    Ok(())
}

fn batch_test_name(args: &[BString]) -> Result<&[u8], HostError> {
    let [defaults, expansion, execute, script] = args else {
        return Err(HostError::unsupported(
            "Expected the batch command arguments.",
        ));
    };
    assert_eq!(
        [defaults.as_slice(), expansion, execute],
        [b"/D".as_slice(), b"/V:OFF", b"/C"]
    );
    script
        .strip_prefix(b".\\")
        .ok_or_else(|| HostError::unsupported("Expected a relative batch file path."))
}
