//! End-to-end checks with the pinned Ninja binary on PATH.

#![allow(clippy::panic)]

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use rcmake_base::BString;
use rcmake_gen_ninja::{generate_multi_config, generate_single_config};
use rcmake_host::{
    CommandSpec, Follow, Host, Input, NativeHost, Output, ProcessResult, ReadRange, RunOutput,
    RunRequest, WriteMode, path,
};
use rcmake_resolve::{
    BuildGraph, BuildStep, GenerationResult, GraphTarget, Invocation, LinkKind, LinkStep,
};

const LIB_SOURCE: &[u8] = b"#include \"value.h\"\nint answer(void) { return VALUE; }\n";
const APP_SOURCE: &[u8] = b"#include <stdio.h>\nint answer(void);\nint main(void) { printf(\"answer=%d\\n\", answer()); return 0; }\n";

struct Project {
    host: NativeHost,
    dir: PathBuf,
}

impl Project {
    fn new() -> Self {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let host = NativeHost::new().unwrap_or_else(|error| panic!("{error}"));
        let parent = host
            .env_get(b"TMPDIR")
            .map_or_else(|| PathBuf::from("/tmp"), |path| path::to_native(&path));
        let now = host.now();
        let index = NEXT.fetch_add(1, Ordering::Relaxed);
        let dir = parent.join(format!("rcmake-ninja-{}-{}-{index}", now.secs, now.nanos));
        host.create_dir_all(&dir)
            .unwrap_or_else(|error| panic!("{error}"));
        let project = Self { host, dir };
        project.write("lib source$.c", LIB_SOURCE);
        project.write("main.c", APP_SOURCE);
        project.write("value.h", b"#define VALUE 42\n");
        project.write("CMakeLists.txt", b"project(NinjaTest C)\n");
        project
    }

    fn write(&self, file: &str, bytes: &[u8]) {
        self.host
            .write(&self.dir.join(file), bytes, WriteMode::Truncate)
            .unwrap_or_else(|error| panic!("{error}"));
    }

    fn run(&self, cwd: &Path, program: &str, args: &[&str]) -> RunOutput {
        let command = CommandSpec {
            program: program.into(),
            args: args.iter().copied().map(BString::from).collect(),
        };
        let output = self
            .host
            .run(
                &RunRequest {
                    pipeline: vec![command],
                    cwd: cwd.to_path_buf(),
                    env: self.host.env_all(),
                    stdin: Input::Null,
                    stdout: Output::Capture,
                    stderr: Output::Capture,
                    timeout: Some(Duration::from_secs(45)),
                    tee: false,
                },
                &mut |_, _| {},
            )
            .unwrap_or_else(|error| panic!("{error}"));
        assert_eq!(
            output.results,
            [ProcessResult::Exited(0)],
            "{program} {args:?}\n{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        output
    }
}

impl Drop for Project {
    fn drop(&mut self) {
        let _ = self.host.remove_dir_all(&self.dir);
    }
}

fn compile(source: BString, object: BString) -> BuildStep {
    let mut depfile = object.clone();
    depfile.extend_from_slice(b".d");
    let invocation = Invocation {
        argv: vec![
            "cc".into(),
            "-MMD".into(),
            "-MF".into(),
            depfile.clone(),
            "-c".into(),
            source.clone(),
            "-o".into(),
            object.clone(),
        ],
        ..Invocation::default()
    };
    BuildStep::Compile {
        object,
        source,
        invocation,
        depfile: Some(depfile),
    }
}

fn link(kind: LinkKind, output: BString, inputs: Vec<BString>, args: Vec<BString>) -> BuildStep {
    BuildStep::Link {
        inputs,
        step: LinkStep {
            kind,
            language: "C".into(),
            fragments: Vec::new(),
            output,
        },
        invocations: vec![Invocation {
            argv: args,
            ..Invocation::default()
        }],
    }
}

fn graph(project: &Project, config: &str, prefix: &str) -> BuildGraph {
    let lib_object = BString::from(format!("{prefix}objects/lib.o"));
    let app_object = BString::from(format!("{prefix}objects/app.o"));
    let archive = BString::from(format!("{prefix}libanswer.a"));
    let app = BString::from(format!("{prefix}app"));
    let mut graph = BuildGraph {
        steps: vec![
            compile(
                path::from_native(&project.dir.join("lib source$.c")),
                lib_object.clone(),
            ),
            link(
                LinkKind::StaticArchive,
                archive.clone(),
                vec![lib_object.clone()],
                vec!["ar".into(), "rcs".into(), archive.clone(), lib_object],
            ),
            compile(
                path::from_native(&project.dir.join("main.c")),
                app_object.clone(),
            ),
            link(
                LinkKind::Executable,
                app.clone(),
                vec![app_object.clone(), archive.clone()],
                vec!["cc".into(), app_object, archive, "-o".into(), app],
            ),
        ],
        targets: vec![
            GraphTarget {
                name: "answer".into(),
                steps: vec![0, 1],
                in_all: true,
                ..GraphTarget::default()
            },
            GraphTarget {
                name: "app".into(),
                steps: vec![2, 3],
                depends: vec![0],
                in_all: true,
            },
        ],
        ..BuildGraph::default()
    };
    graph.config.0 = config.into();
    graph
}

#[test]
fn ninja_builds_and_rebuilds_a_static_library_and_executable() {
    let project = Project::new();
    let build = project.dir.join("build $ space");
    let cmake = project.dir.join("fake cmake's path");
    project.write(
        "fake cmake's path",
        b"#!/bin/sh\nprintf '%s\\n' \"$@\" > rerun.args\ntouch build.ninja CMakeFiles/rules.ninja\n",
    );
    project
        .host
        .set_permissions(&cmake, 0o700)
        .unwrap_or_else(|error| panic!("{error}"));
    project
        .host
        .env_set(b"CMAKE_COMMAND", Some(&path::from_native(&cmake)));
    let graph = graph(&project, "Debug", "");
    let mut result = GenerationResult {
        configs: vec![graph.config.clone()],
        graphs: vec![graph],
        ..GenerationResult::default()
    };
    result.metadata.listfiles = vec![path::from_native(&project.dir.join("CMakeLists.txt"))];
    generate_single_config(&result, &project.host, &build)
        .unwrap_or_else(|error| panic!("{error}"));
    project.run(&build, "ninja", &["-j", "2"]);
    assert_eq!(project.run(&build, "./app", &[]).stdout, b"answer=42\n");
    assert_eq!(
        project.run(&build, "ninja", &["-j", "2"]).stdout,
        b"ninja: no work to do.\n"
    );

    project.write("value.h", b"#define VALUE 43\n");
    project.run(&build, "ninja", &["-j", "2"]);
    assert_eq!(project.run(&build, "./app", &[]).stdout, b"answer=43\n");

    project.write("CMakeLists.txt", b"project(NinjaTest C)\n# Changed\n");
    project.run(&build, "ninja", &["-j", "2"]);
    let rerun = project
        .host
        .read(&build.join("rerun.args"), ReadRange::All { limit: 4096 })
        .unwrap_or_else(|error| panic!("{error}"));
    assert_eq!(
        rerun,
        [
            b"--regenerate-during-build\n-S".as_slice(),
            &path::from_native(&project.dir),
            b"\n-B",
            &path::from_native(&build),
            b"\n",
        ]
        .concat()
    );

    project.run(&build, "ninja", &["clean"]);
    assert!(
        project
            .host
            .metadata(&build.join("app"), Follow::Yes)
            .is_err()
    );
    assert!(
        project
            .host
            .metadata(&build.join("build.ninja"), Follow::Yes)
            .is_ok()
    );
}

#[test]
fn ninja_builds_the_requested_configuration_and_default() {
    let project = Project::new();
    let build = project.dir.join("build");
    let graphs = vec![
        graph(&project, "Debug", "Debug/"),
        graph(&project, "Release", "Release/"),
    ];
    let mut result = GenerationResult {
        configs: graphs.iter().map(|graph| graph.config.clone()).collect(),
        graphs,
        ..GenerationResult::default()
    };
    result.metadata.listfiles = vec![path::from_native(&project.dir.join("CMakeLists.txt"))];
    generate_multi_config(&result, &project.host, &build).unwrap_or_else(|error| panic!("{error}"));
    project.run(
        &build,
        "ninja",
        &["-f", "build-Release.ninja", "app:Release", "-j", "2"],
    );
    assert_eq!(
        project.run(&build, "./Release/app", &[]).stdout,
        b"answer=42\n"
    );
    assert!(
        project
            .host
            .metadata(&build.join("Debug/app"), Follow::Yes)
            .is_err()
    );

    project.run(&build, "ninja", &["-j", "2"]);
    assert_eq!(
        project.run(&build, "./Debug/app", &[]).stdout,
        b"answer=42\n"
    );
    project.run(
        &build,
        "ninja",
        &["-f", "build-Release.ninja", "clean:Release"],
    );
    assert!(
        project
            .host
            .metadata(&build.join("Release/app"), Follow::Yes)
            .is_err()
    );
    assert!(
        project
            .host
            .metadata(&build.join("Debug/app"), Follow::Yes)
            .is_ok()
    );
}
