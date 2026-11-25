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
use rcmake_model::{BT, DirId, GeneratorKind, ProjectModel, Target, TargetKind, TargetOrigin};
use rcmake_resolve::{
    BuildGraph, BuildStep, CustomStep, GenerationResult, GraphTarget, Invocation, LinkKind,
    LinkStep, resolve,
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
        byproducts: Vec::new(),
        compile_input: None,
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
        implicit_inputs: Vec::new(),
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
    assert!(first.contains("Linking C executable bin/app"));
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

    // Some Make versions compare timestamps in whole seconds.
    host.sleep(Duration::from_millis(1100));
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

#[test]
fn native_make_builds_install_and_test_inputs_on_demand() {
    use rcmake_base::hash::HashAlgo;
    use rcmake_model::{
        BT, DirId, InstallKind, InstallOptions, InstallRule, ProjectModel, TestEntry, TestKind,
    };
    use rcmake_resolve::lower::lower_install_manifest;

    let host = NativeHost::new().expect("native host");
    let temp = TempDir {
        host: &host,
        path: run(
            &host,
            Path::new("/tmp"),
            "/usr/bin/mktemp",
            &["-d", "/tmp/rcmake-prep.XXXXXX"],
        )
        .trim_end()
        .into(),
    };
    let root = &temp.path;
    let mut model = ProjectModel::default();
    let binary = path::from_native(root);
    model.set_root_dirs(binary.clone(), binary.clone());
    let child_binary = path::from_native(&root.join("child"));
    let child = model
        .add_directory(
            DirId::ROOT,
            binary.clone(),
            child_binary.clone(),
            Default::default(),
        )
        .expect("add directory");
    model
        .dir_mut(child)
        .props
        .set("CMAKE_TEST_BUILD_DEPENDS".into(), "static.txt".into());
    host.write(
        &root.join("static.txt"),
        b"source input\n",
        WriteMode::Truncate,
    )
    .expect("write static input");
    for (component, file, exclude_from_all) in [
        ("runtime", "runtime.txt", false),
        ("extra", "extra.txt", true),
    ] {
        model.add_install(InstallRule {
            dir: DirId::ROOT,
            backtrace: Default::default(),
            kind: InstallKind::Files(vec![BT::new(
                path::from_native(&root.join(file)),
                Default::default(),
            )]),
            options: InstallOptions {
                destination: Some("share".into()),
                component: Some(component.into()),
                exclude_from_all,
                ..InstallOptions::default()
            },
        });
    }
    for (name, test_kind, input) in [
        ("smoke", TestKind::Single, "smoke.txt"),
        (
            "discovery",
            TestKind::Discover(Box::default()),
            "$<1:discovery.txt>",
        ),
    ] {
        model
            .add_test(TestEntry {
                name: name.into(),
                kind: test_kind,
                dir: DirId::ROOT,
                backtrace: Default::default(),
                command: vec!["true".into()],
                configurations: Vec::new(),
                working_directory: None,
                command_expand_lists: false,
                old_style: false,
                policies: Default::default(),
                props: Default::default(),
                build_depends: vec![BT::new(input.into(), Default::default())],
            })
            .expect("add test");
    }

    let files = [
        "runtime.txt",
        "extra.txt",
        "smoke.txt",
        "discovery.txt",
        "user.txt",
    ];
    let result = GenerationResult {
        graphs: vec![BuildGraph {
            steps: files
                .iter()
                .map(|file| {
                    BuildStep::Custom(CustomStep {
                        outputs: vec![(*file).into()],
                        invocations: vec![invocation(&format!("touch {file}"))],
                        ..CustomStep::default()
                    })
                })
                .collect(),
            targets: ["runtime", "extra", "smoke", "discovery", "test_prep"]
                .iter()
                .enumerate()
                .map(|(index, name)| graph_target(name, vec![index], vec![], false))
                .collect(),
            ..BuildGraph::default()
        }],
        install: lower_install_manifest(&model, &[]),
        ..GenerationResult::default()
    };
    rcmake_gen_make::generate_with_model(&result, &host, root, &model).expect("generate Makefiles");

    let exists = |file: &str| host.metadata(&root.join(file), Follow::Yes).is_ok();
    run(&host, root, "/usr/bin/make", &["install_prep"]);
    assert!(exists("runtime.txt"));
    assert!(!exists("extra.txt"));
    assert!(!exists("smoke.txt"));

    run(
        &host,
        root,
        "/usr/bin/make",
        &["install_prep/extra", "test_prep/smoke"],
    );
    assert!(exists("extra.txt"));
    assert!(exists("smoke.txt"));
    assert!(!exists("discovery.txt"));

    let hash = HashAlgo::Md5.hex_digest(&binary);
    let goal = format!(
        "test_prep/directory_{}",
        hash.get(..8).expect("directory id")
    );
    let child_hash = HashAlgo::Md5.hex_digest(&child_binary);
    let child_goal = format!(
        "test_prep/directory_{}",
        child_hash.get(..8).expect("child directory id")
    );
    run(
        &host,
        root,
        "/usr/bin/make",
        &[&goal, &child_goal, "test_prep/all"],
    );
    assert!(exists("discovery.txt"));
    assert!(!exists("user.txt"));

    run(&host, root, "/usr/bin/make", &["test_prep"]);
    assert!(exists("user.txt"));
    assert!(read(&host, &root.join("Makefile")).contains("install_prep/runtime:"));
}

#[test]
fn long_posix_link_uses_ordered_response_arguments() {
    long_posix_link_or_archive(false);
}

#[test]
fn batched_archive_keeps_each_shell_command_bounded() {
    long_posix_link_or_archive(true);
}

fn long_posix_link_or_archive(archive: bool) {
    let host = NativeHost::new().expect("native host");
    let temp = TempDir {
        host: &host,
        path: run(
            &host,
            Path::new("/tmp"),
            "/usr/bin/mktemp",
            &["-d", "/tmp/rcmake-make-link.XXXXXX"],
        )
        .trim_end()
        .into(),
    };
    let root = &temp.path;
    let build = root.join("build space");
    host.create_dir_all(&build).expect("create build directory");
    let write = |name: &str, content: &[u8]| {
        host.write(&root.join(name), content, WriteMode::Truncate)
            .expect("write source");
    };
    let build_run = |program, args: &[&str]| run(&host, &build, program, args);
    write("empty.c", b"/* Empty translation unit. */\n");
    write("main.c", b"int main(void) { return 0; }\n");
    build_run("cc", &["-c", "../empty.c", "-o", "empty.o"]);
    let bytes = host
        .read(&build.join("empty.o"), ReadRange::All { limit: 64 * 1024 })
        .expect("read object");
    let mut model = ProjectModel::default();
    model.generator = Some(GeneratorKind::UnixMakefiles);
    model.set_root_dirs(path::from_native(root), path::from_native(&build));
    for (key, value) in [
        ("CMAKE_COMMAND", "cmake"),
        ("CMAKE_C_COMPILER", "cc"),
        ("CMAKE_AR", "ar"),
        ("CMAKE_RANLIB", "ranlib"),
    ] {
        model
            .dir_mut(DirId::ROOT)
            .vars
            .insert(key.into(), value.into());
    }
    if !archive {
        model
            .dir_mut(DirId::ROOT)
            .vars
            .insert("CMAKE_C_USE_RESPONSE_FILE_FOR_OBJECTS".into(), "ON".into());
    }
    let kind = if archive {
        TargetKind::StaticLibrary
    } else {
        TargetKind::Executable
    };
    let mut target = Target::new(
        "long_input".into(),
        kind,
        TargetOrigin::Normal,
        DirId::ROOT,
        Default::default(),
        Default::default(),
    );
    target
        .sources
        .push(BT::new("main.c".into(), Default::default()));
    const COUNT: usize = 680;
    let mut last = String::new();
    let mut total = 0;
    for index in 0..COUNT {
        last = format!("object {index:04} $ '.{}.o", "p".repeat(186));
        let path = build.join(&last);
        host.write(&path, &bytes, WriteMode::Truncate)
            .expect("write object");
        total += last.len();
        target
            .sources
            .push(BT::new(path::from_native(&path), Default::default()));
    }
    assert!(total > 128 * 1024);
    model.add_target(target).expect("add target");
    let result = resolve(&model, &host, &[]).expect("resolve model");
    rcmake_gen_make::generate_with_model(&result, &host, &build, &model)
        .expect("generate build files");
    if !archive {
        host.metadata(
            &build.join("CMakeFiles/long_input.dir/objects1.rsp"),
            Follow::Yes,
        )
        .expect("read the object response file");
    }
    let stdout = build_run("make", &["-j2"]);
    assert!(!stdout.is_empty());
    if archive {
        let members = build_run("ar", &["t", "liblong_input.a"]);
        assert_eq!(
            members
                .lines()
                .filter(|member| !member.starts_with("__.SYMDEF"))
                .count(),
            COUNT + 1
        );
        assert!(members.lines().any(|member| member == last));
    } else {
        build_run("./long_input", &[]);
    }
    let output = if archive {
        "liblong_input.a"
    } else {
        "long_input"
    };
    let before = host
        .metadata(&build.join(output), Follow::Yes)
        .expect("read output")
        .mtime;
    build_run("make", &["-j2"]);
    let after = host
        .metadata(&build.join(output), Follow::Yes)
        .expect("read output again")
        .mtime;
    assert_eq!(before, after);
}
