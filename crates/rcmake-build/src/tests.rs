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
    let host = PureHost::default();
    let calls = Arc::new(Mutex::new(Vec::new()));
    let observed = calls.clone();
    host.set_command_handler(move |host, request, _| {
        let command = request.pipeline[0].clone();
        let shell = command.program == "/bin/sh";
        if shell {
            assert_eq!(command.args, ["-c", "first link"]);
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
    assert_eq!(*calls.lock().unwrap(), ["compiler", "/bin/sh"]);
    assert!(!result.success);
    assert_eq!(result.exit_code, 7);
    assert_eq!(result.steps_run, 2);
    assert_eq!(result.output, "compile output\nlink failed\n");
}

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
