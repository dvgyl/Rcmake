//! End-to-end checks with the pinned Ninja binary on PATH.

#![cfg(unix)]
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
use rcmake_model::{
    BT, CustomCommand, CustomCommandKind, DirId, GeneratorKind, LinkItem, ProjectModel, Target,
    TargetKind, TargetOrigin,
};
use rcmake_resolve::{
    BuildGraph, BuildStep, GenerationResult, GraphTarget, Invocation, LinkKind, LinkStep, resolve,
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
        let parent = std::env::temp_dir();
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
        modmap: None,
        dyndep: None,
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

fn c_target(name: &str, kind: TargetKind, source: &str) -> Target {
    let mut target = Target::new(
        name.into(),
        kind,
        TargetOrigin::Normal,
        DirId::ROOT,
        Default::default(),
        Default::default(),
    );
    target
        .sources
        .push(BT::new(source.into(), Default::default()));
    target
}

#[test]
fn ninja_builds_an_executable_whose_output_name_matches_its_library()
-> Result<(), Box<dyn std::error::Error>> {
    let project = Project::new();
    let build = project.dir.join("build");
    let mut model = ProjectModel::default();
    model.set_root_dirs(path::from_native(&project.dir), path::from_native(&build));
    model.generator = Some(GeneratorKind::Ninja);
    model
        .dir_mut(DirId::ROOT)
        .vars
        .insert("CMAKE_C_COMPILER".into(), "cc".into());
    model.add_target(c_target(
        "sqlite3",
        TargetKind::StaticLibrary,
        "lib source$.c",
    ))?;
    let mut shell = c_target("sqlite_shell", TargetKind::Executable, "main.c");
    shell.props.set("OUTPUT_NAME".into(), "sqlite3".into());
    shell.usage.link_libraries = Some(vec![LinkItem {
        text: BT::new("sqlite3".into(), Default::default()),
        lookup_dir: DirId::ROOT,
    }]);
    model.add_target(shell)?;
    let mut check = CustomCommand::new(
        DirId::ROOT,
        CustomCommandKind::Output,
        Default::default(),
        Default::default(),
    );
    check
        .outputs
        .push(BT::new("check".into(), Default::default()));
    check
        .depends
        .push(BT::new("sqlite_shell".into(), Default::default()));
    check
        .command_lines
        .push(vec!["sh".into(), "-c".into(), "./sqlite3 > check".into()]);
    model.add_command(check)?;

    let result = resolve(&model, &project.host, &[])?;
    generate_single_config(&result, &project.host, &build)?;
    project.run(&build, "ninja", &["-j", "2", "sqlite3"]);
    project
        .host
        .metadata(&build.join("libsqlite3.a"), Follow::Yes)?;
    assert_eq!(project.run(&build, "./sqlite3", &[]).stdout, b"answer=42\n");
    assert_eq!(
        project.run(&build, "ninja", &["-j", "2"]).stdout,
        b"ninja: no work to do.\n"
    );

    project.host.remove_file(&build.join("sqlite3"))?;
    project.run(&build, "ninja", &["-j", "2", "check"]);
    assert_eq!(
        project
            .host
            .read(&build.join("check"), ReadRange::All { limit: 64 })?,
        b"answer=42\n"
    );
    Ok(())
}

fn versioned_library_model(
    project: &Project,
    build: &Path,
    generator: GeneratorKind,
) -> Result<ProjectModel, Box<dyn std::error::Error>> {
    let symlink = project.dir.join("symlink-library");
    project.write(
        "symlink-library",
        b"#!/bin/sh\n\
          set -eu\n\
          test \"$1\" = -E\n\
          test \"$2\" = cmake_symlink_library\n\
          ln -sfn \"${3##*/}\" \"$4\"\n\
          ln -sfn \"${4##*/}\" \"$5\"\n",
    );
    project.host.set_permissions(&symlink, 0o700)?;

    let mut model = ProjectModel::default();
    model.set_root_dirs(path::from_native(&project.dir), path::from_native(build));
    model.generator = Some(generator);
    let vars = &mut model.dir_mut(DirId::ROOT).vars;
    vars.insert("CMAKE_C_COMPILER".into(), "cc".into());
    vars.insert("CMAKE_COMMAND".into(), path::from_native(&symlink));
    let mut library = c_target("demo", TargetKind::SharedLibrary, "lib source$.c");
    library.props.set("VERSION".into(), "1.2.3".into());
    library.props.set("SOVERSION".into(), "1".into());
    model.add_target(library)?;
    let mut app = c_target("app", TargetKind::Executable, "main.c");
    app.usage.link_libraries = Some(vec![LinkItem {
        text: BT::new("demo".into(), Default::default()),
        lookup_dir: DirId::ROOT,
    }]);
    model.add_target(app)?;
    Ok(model)
}

#[test]
fn ninja_links_and_rebuilds_a_versioned_shared_library() -> Result<(), Box<dyn std::error::Error>> {
    let project = Project::new();
    let [file, soname, linker] = if cfg!(target_os = "macos") {
        ["libdemo.1.2.3.dylib", "libdemo.1.dylib", "libdemo.dylib"]
    } else {
        ["libdemo.so.1.2.3", "libdemo.so.1", "libdemo.so"]
    };
    let mut debug = BuildGraph::default().config;
    debug.0 = "Debug".into();
    let mut release = debug.clone();
    release.0 = "Release".into();
    let configs = [debug, release];

    for generator in [GeneratorKind::Ninja, GeneratorKind::NinjaMultiConfig] {
        let build = project.dir.join(generator.name());
        let model = versioned_library_model(&project, &build, generator)?;
        let (entries, prefix, all, clean): (&[&str], &str, &str, &str) = if generator.multi_config()
        {
            let result = resolve(&model, &project.host, &configs)?;
            generate_multi_config(&result, &project.host, &build)?;
            project.run(&build, "ninja", &["-j", "2"]);
            assert_eq!(
                project.run(&build, "./Debug/app", &[]).stdout,
                b"answer=42\n"
            );
            (
                &["-f", "build-Release.ninja"],
                "Release/",
                "all:Release",
                "clean:Release",
            )
        } else {
            let result = resolve(&model, &project.host, &[])?;
            generate_single_config(&result, &project.host, &build)?;
            (&[], "", "all", "clean")
        };

        let args = [entries, &["-j", "2", all]].concat();
        let app = format!("./{prefix}app");
        let output_dir = build.join(prefix);
        project.run(&build, "ninja", &args);
        assert_eq!(project.run(&build, &app, &[]).stdout, b"answer=42\n");
        assert_eq!(
            project.host.read_link(&output_dir.join(soname))?,
            Path::new(file)
        );
        assert_eq!(
            project.host.read_link(&output_dir.join(linker))?,
            Path::new(soname)
        );
        assert_eq!(
            project.run(&build, "ninja", &args).stdout,
            b"ninja: no work to do.\n"
        );

        project.host.remove_file(&output_dir.join(linker))?;
        project.run(&build, "ninja", &args);
        assert_eq!(
            project.host.read_link(&output_dir.join(linker))?,
            Path::new(soname)
        );
        assert_eq!(project.run(&build, &app, &[]).stdout, b"answer=42\n");

        project.run(&build, "ninja", &[entries, &[clean]].concat());
        for output in [file, soname, linker, "app"] {
            assert!(
                project
                    .host
                    .metadata(&output_dir.join(output), Follow::No)
                    .is_err()
            );
        }
        if generator.multi_config() {
            assert!(
                project
                    .host
                    .metadata(&build.join("Debug").join(linker), Follow::Yes)
                    .is_ok()
            );
        }
    }
    Ok(())
}

#[test]
fn ninja_reads_cxx_dyndep_and_builds_a_scanned_unit() -> Result<(), Box<dyn std::error::Error>> {
    use rcmake_resolve::Toolchain;

    let project = Project::new();
    let build = &project.dir;
    let object = BString::from("module state/unit.o");
    let ddi = BString::from("module state/unit.o.ddi");
    let dd = BString::from("module state/CXX.dd");
    let map = BString::from("module state/unit.o.modmap");
    let depfile = BString::from("module state/unit.o.ddi.d");
    let cmake = project.host.env_get(b"RCMAKE_NINJA_TEST_CMAKE");
    project.write(
        "scan.sh",
        br#"set -eu
printf '%s\n' '{"version":1,"revision":0,"rules":[{"primary-output":"module state/unit.o"}]}' > "$1"
printf '%s\n' 'module\ state/unit.o.ddi: main.c' > "$2"
"#,
    );
    let mut result = GenerationResult {
        graphs: vec![BuildGraph {
            steps: vec![
                BuildStep::Scan {
                    source: "main.c".into(),
                    object: object.clone(),
                    ddi: ddi.clone(),
                    invocation: Invocation {
                        argv: vec!["sh".into(), "scan.sh".into(), ddi.clone(), depfile.clone()],
                        ..Invocation::default()
                    },
                    depfile: Some(depfile),
                },
                BuildStep::Collate {
                    target: "demo".into(),
                    ddis: vec![ddi],
                    dd: dd.clone(),
                    modmaps: vec![map.clone()],
                    tdi: "module state/CXXDependInfo.json".into(),
                    depend_info: Box::default(),
                    modmap_format: rcmake_cxxmodules::ModmapFormat::Clang,
                    implicit_inputs: vec![],
                },
                BuildStep::Compile {
                    object: object.clone(),
                    source: "main.c".into(),
                    invocation: Invocation {
                        argv: vec![
                            "sh".into(),
                            "-c".into(),
                            "test -f \"$1\" && printf built > \"$2\"".into(),
                            "compile".into(),
                            map,
                            object,
                        ],
                        ..Invocation::default()
                    },
                    depfile: None,
                    modmap: Some("module state/unit.o.modmap".into()),
                    dyndep: Some(dd),
                },
            ],
            targets: vec![GraphTarget {
                name: "demo".into(),
                steps: vec![0, 1, 2],
                in_all: true,
                ..GraphTarget::default()
            }],
            ..BuildGraph::default()
        }],
        ..GenerationResult::default()
    };
    result.metadata.cmake_command.clone_from(&cmake);
    result.metadata.toolchains.insert(
        "CXX".into(),
        Toolchain {
            module_map_format: Some("clang".into()),
            ..Toolchain::default()
        },
    );
    generate_single_config(&result, &project.host, build)?;
    if cmake.is_some() {
        project.run(build, "ninja", &["demo"]);
        assert_eq!(
            project.host.read(
                &build.join("module state/unit.o"),
                ReadRange::All { limit: 1024 }
            )?,
            b"built"
        );
        assert!(
            project
                .host
                .metadata(&build.join("module state/CXXModules.json"), Follow::Yes)
                .is_ok()
        );
        project
            .host
            .remove_file(&build.join("module state/unit.o.modmap"))?;
        project.run(build, "ninja", &["demo"]);
    } else {
        project.write(
            "module state/CXX.dd",
            b"ninja_dyndep_version = 1.0\nbuild module$ state/unit.o: dyndep\n",
        );
        project.run(build, "ninja", &["-n", "demo"]);
    }
    Ok(())
}
