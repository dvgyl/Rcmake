//! Verify generated build files with the system make program.

#![cfg(unix)]
#![allow(clippy::expect_used)]

use std::path::{Path, PathBuf};
use std::time::Duration;

use rcmake_base::BString;
use rcmake_gen_make::generate_unix_makefiles;
use rcmake_host::{
    CommandSpec, Follow, Host, Input, NativeHost, Output, ProcessResult, ReadRange, RunRequest,
    WriteMode, path,
};
use rcmake_resolve::{
    BuildGraph, BuildStep, CustomStep, GenerationResult, GraphTarget, Invocation, LinkKind,
    LinkStep,
};

fn invocation(shell: &str) -> Invocation {
    Invocation {
        shell: shell.into(),
        ..Invocation::default()
    }
}

fn graph_target(name: &str, steps: Vec<usize>, depends: Vec<usize>, in_all: bool) -> GraphTarget {
    GraphTarget {
        name: name.into(),
        steps,
        depends,
        in_all,
    }
}

fn read(host: &dyn Host, path: &Path) -> String {
    let contents = host
        .read(path, ReadRange::All { limit: 128 * 1024 })
        .expect("read file");
    String::from_utf8(contents).expect("read UTF-8")
}

struct TempDir<'a> {
    host: &'a NativeHost,
    path: PathBuf,
}

impl Drop for TempDir<'_> {
    fn drop(&mut self) {
        let _ = self.host.remove_dir_all(&self.path);
    }
}

fn run(host: &NativeHost, cwd: &Path, program: &str, args: &[&str]) -> String {
    let mut env = host.env_all();
    env.retain(|(key, _)| {
        !matches!(
            key.as_slice(),
            b"MAKEFLAGS" | b"MFLAGS" | b"GNUMAKEFLAGS" | b"MAKELEVEL"
        )
    });
    let output = host
        .run(
            &RunRequest {
                pipeline: vec![CommandSpec {
                    program: program.into(),
                    args: args.iter().copied().map(BString::from).collect(),
                }],
                cwd: cwd.into(),
                env,
                stdin: Input::Null,
                stdout: Output::Capture,
                stderr: Output::Capture,
                timeout: Some(Duration::from_secs(30)),
                tee: false,
            },
            &mut |_, _| {},
        )
        .expect("run command");
    assert_eq!(
        output.results,
        [ProcessResult::Exited(0)],
        "{program} {args:?}\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).expect("read stdout")
}

fn compile(source: &str, object: &str) -> BuildStep {
    let depfile = format!("{object}.d");
    BuildStep::Compile {
        object: object.into(),
        source: source.into(),
        invocation: invocation(&format!(
            "cc -MMD -MP -MF '{depfile}' -MQ '{object}' -I'gen $' -c '{source}' -o '{object}'",
        )),
        depfile: Some(depfile.into()),
        modmap: None,
        dyndep: None,
    }
}

fn link(kind: LinkKind, inputs: &[&str], output: &str, commands: &[&str]) -> BuildStep {
    BuildStep::Link {
        inputs: inputs.iter().copied().map(BString::from).collect(),
        step: LinkStep {
            kind,
            language: "C".into(),
            fragments: Vec::new(),
            output: output.into(),
        },
        invocations: commands.iter().copied().map(invocation).collect(),
    }
}

#[test]
fn native_make_builds_c_archive_and_executable_incrementally() {
    let host = NativeHost::new().expect("native host");
    let temp = TempDir {
        host: &host,
        path: run(
            &host,
            Path::new("/tmp"),
            "/usr/bin/mktemp",
            &["-d", "/tmp/rcmake-make.XXXXXX"],
        )
        .trim_end()
        .into(),
    };
    let root = &temp.path;
    let write = |name: &str, value: &[u8]| {
        host.write(&root.join(name), value, WriteMode::Truncate)
            .expect("write fixture");
    };
    write("CMakeLists.txt", b"# Configure input\n");
    write(
        "main file.c",
        b"#include \"value.h\"\nint value(void);\nint main(void) { return value() != VALUE; }\n",
    );
    write(
        "library input.c",
        b"#include \"value.h\"\nint value(void) { return VALUE; }\n",
    );
    write("header input", b"#define VALUE 42\n");
    write("record.sh", b"printf '%s\\n' \"$*\" >> calls\n");

    let result = GenerationResult {
        graphs: vec![BuildGraph {
            steps: vec![
                BuildStep::Custom(CustomStep {
                    outputs: vec!["gen $/library file.c".into(), "gen $/value.h".into()],
                    byproducts: vec!["gen $/record".into()],
                    depends: vec!["library input.c".into(), "header input".into()],
                    comment: Some("Generate C source and header".into()),
                    invocations: vec![
                        invocation("cp 'library input.c' 'gen $/library file.c'"),
                        Invocation {
                            working_dir: Some("gen $".into()),
                            argv: vec!["cp".into(), "../header input".into(), "value.h".into()],
                            ..Invocation::default()
                        },
                        invocation("printf '%s\\n' generated >> 'gen $/record'"),
                    ],
                    ..CustomStep::default()
                }),
                compile("gen $/library file.c", "obj $/library.o"),
                link(
                    LinkKind::StaticArchive,
                    &["obj $/library.o"],
                    "lib/libsample.a",
                    &[
                        "ar rcs lib/libsample.a 'obj $/library.o'",
                        "ranlib lib/libsample.a",
                    ],
                ),
                compile("main file.c", "obj $/main.o"),
                link(
                    LinkKind::Executable,
                    &["obj $/main.o", "lib/libsample.a"],
                    "bin/app",
                    &["cc 'obj $/main.o' lib/libsample.a -o bin/app"],
                ),
                BuildStep::Custom(CustomStep {
                    invocations: vec![invocation("touch manual-ran")],
                    byproducts: vec!["manual-ran".into()],
                    ..CustomStep::default()
                }),
            ],
            targets: vec![
                graph_target("sample app", vec![0, 3, 4], vec![1], true),
                graph_target("sample", vec![0, 1, 2], Vec::new(), false),
                graph_target("manual", vec![5], Vec::new(), false),
            ],
            ..BuildGraph::default()
        }],
        metadata: rcmake_resolve::ConfigureMetadata {
            listfiles: vec![path::from_native(&root.join("CMakeLists.txt"))],
            ..Default::default()
        },
        ..GenerationResult::default()
    };
    generate_unix_makefiles(&result, &host, root).expect("generate");
    let make = |args: &[&str]| run(&host, root, "/usr/bin/make", args);
    let first = make(&["-j2", "CMAKE_COMMAND=/bin/sh record.sh"]);
    assert!(first.contains("Generate C source and header"));
    assert!(first.contains("Building obj $/main.o"));
    assert!(first.contains("Linking bin/app"));
    assert!(!first.contains("cc -MMD"));
    assert!(
        host.metadata(&root.join("manual-ran"), Follow::Yes)
            .is_err()
    );
    assert!(host.metadata(&root.join("calls"), Follow::Yes).is_err());
    run(&host, root, "./bin/app", &[]);

    let products = [
        "obj $/library.o",
        "obj $/main.o",
        "lib/libsample.a",
        "bin/app",
    ];
    let times = || {
        products.map(|name| {
            host.metadata(&root.join(name), Follow::Yes)
                .expect("product")
                .mtime
        })
    };
    let original = times();
    let next = make(&["-j2", "sample app", "CMAKE_COMMAND=/bin/sh record.sh"]);
    assert!(!next.contains("Building ") && !next.contains("Linking "));
    assert_eq!(times(), original);

    host.sleep(Duration::from_millis(1100));
    write("gen $/value.h", b"#define VALUE 73\n");
    let rebuild = make(&["-j2", "VERBOSE=1", "CMAKE_COMMAND=/bin/sh record.sh"]);
    assert!(rebuild.contains("cc -MMD"), "{rebuild}");
    for (old, new) in original.into_iter().zip(times()) {
        assert!(old < new);
    }
    run(&host, root, "./bin/app", &[]);

    host.remove_file(&root.join("gen $/value.h"))
        .expect("remove secondary output");
    make(&["-j2", "CMAKE_COMMAND=/bin/sh record.sh"]);
    assert_eq!(
        read(&host, &root.join("gen $/record")),
        "generated\ngenerated\n"
    );
    run(&host, root, "./bin/app", &[]);

    host.sleep(Duration::from_millis(1100));
    write("CMakeLists.txt", b"# Revised configure input\n");
    make(&["-j2", "CMAKE_COMMAND=/bin/sh record.sh"]);
    let calls = read(&host, &root.join("calls"));
    assert!(calls.contains("--regenerate-during-build"), "{calls}");
    make(&["-j2", "CMAKE_COMMAND=/bin/sh record.sh"]);
    assert_eq!(read(&host, &root.join("calls")), calls);

    make(&["install", "CMAKE_COMMAND=/bin/sh record.sh"]);
    make(&["test", "CTEST_COMMAND=/bin/sh record.sh"]);
    make(&["rebuild_cache", "CMAKE_COMMAND=/bin/sh record.sh"]);
    let calls = read(&host, &root.join("calls"));
    assert!(calls.contains("-P cmake_install.cmake"));
    assert!(calls.contains("--force-new-ctest-process"));
    assert!(
        calls
            .lines()
            .last()
            .expect("configure call")
            .starts_with("-B . -S ")
    );
    assert!(make(&["help"]).contains("... sample app"));
    make(&["manual"]);
    assert!(host.metadata(&root.join("manual-ran"), Follow::Yes).is_ok());

    make(&["-j2", "clean"]);
    for name in products.into_iter().chain([
        "obj $/library.o.d",
        "obj $/main.o.d",
        "gen $/library file.c",
        "gen $/value.h",
        "gen $/record",
        "manual-ran",
    ]) {
        assert!(
            host.metadata(&root.join(name), Follow::Yes).is_err(),
            "{name}"
        );
    }
    assert!(
        host.metadata(&root.join("main file.c"), Follow::Yes)
            .is_ok()
    );
}
