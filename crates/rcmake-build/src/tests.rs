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
fn uses_argv_and_shell_fallback_and_stops_at_first_failed_invocation() {
    let (shell_program, shell_flag) = if cfg!(windows) {
        ("cmd.exe", "/C")
    } else {
        ("/bin/sh", "-c")
    };
    let host = PureHost::default();
    let calls = Arc::new(Mutex::new(Vec::new()));
    let observed = calls.clone();
    host.set_command_handler(move |host, request, _| {
        let command = request.pipeline[0].clone();
        let shell = command.program == shell_program;
        if shell {
            assert_eq!(command.args, [shell_flag, "first link"]);
            assert_eq!(request.cwd, Path::new("/work/build/obj"));
            assert_eq!(
                host.metadata(Path::new("/work/build/bin"), Follow::No)?
                    .kind,
                FileKind::Dir
            );
        } else {
            assert_eq!(command.args, ["arg with spaces"]);
            assert_eq!(
                host.metadata(Path::new("/work/build/obj"), Follow::No)?
                    .kind,
                FileKind::Dir
            );
        }
        observed.lock().unwrap().push(command.program.clone());
        Ok(RunOutput {
            stdout: if shell {
                b"link failed\n".to_vec()
            } else {
                b"compile output\n".to_vec()
            },
            results: vec![ProcessResult::Exited(if shell { 7 } else { 0 })],
            ..RunOutput::default()
        })
    })
    .unwrap();
    let graph = BuildGraph {
        steps: vec![
            BuildStep::Compile {
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
            build_dir: "/work/build".into(),
            ..BuildOptions::default()
        },
    )
    .unwrap();
    assert_eq!(*calls.lock().unwrap(), ["compiler", shell_program]);
    assert!(!result.success);
    assert_eq!(result.exit_code, 7);
    assert_eq!(result.steps_run, 2);
    assert_eq!(result.output, "compile output\nlink failed\n");
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
        source: source.into(),
        object: object.into(),
        ddi: ddi.into(),
        invocation: invocation(&["scan", ddi]),
        depfile: None,
    };
    let compile = |source: &str, object: &str, map: &str| BuildStep::Compile {
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
                target: "main".into(),
                ddis: vec!["obj/use.o.ddi".into(), "obj/math.o.ddi".into()],
                dd: "obj/CXX.dd".into(),
                modmaps: vec!["obj/use.o.modmap".into(), "obj/math.o.modmap".into()],
                tdi: "obj/CXXDependInfo.json".into(),
                depend_info: Box::new(TargetDependInfo {
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
                }),
                modmap_format: ModmapFormat::Clang,
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
