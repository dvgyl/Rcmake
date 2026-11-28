//! Tests for the generator-independent output files.
//!
//! Set `RCMAKE_TEST_REFERENCE_CMAKE` to a reference CMake binary for the
//! differential tests. Set `RCMAKE_TEST_EXECUTABLE` to check script mode with
//! another CMake-compatible binary.

use std::path::Path;

use crate::{
    GenError, compile_commands,
    configure_log::*,
    ctest_file,
    install_script::{self, *},
};
use rcmake_base::BString;
use rcmake_diag::{Backtrace, Frame, FrameKind};
use rcmake_host::{Host, PureHost, ReadRange};
use rcmake_json::{ReadOptions, Value};
use rcmake_model::{DirId, ProjectModel, PropertyMap, TestEntry};
use rcmake_resolve::{
    BuildGraph, BuildStep, CustomStep, InstallAction, InstallActionKind, InstallManifest,
    Invocation,
};

fn read(host: &dyn Host, path: &Path) -> Vec<u8> {
    host.read(path, ReadRange::All { limit: 4_000_000 })
        .unwrap()
}

fn compile_step(
    directory: Option<BString>,
    command: BString,
    source: BString,
    object: BString,
) -> BuildStep {
    BuildStep::Compile {
        byproducts: Vec::new(),
        compile_input: None,
        object,
        source,
        depfile: None,
        invocation: Invocation {
            shell: command,
            working_dir: directory,
            ..Invocation::default()
        },
        modmap: None,
        dyndep: None,
    }
}

#[test]
fn compile_database_preserves_order_and_round_trips_paths_and_commands() {
    let host = PureHost::default();
    let graph = BuildGraph {
        steps: vec![
            compile_step(
                None,
                r#"cc -DNAME="\"a b\"" -c "a é.c""#.into(),
                "/src/a é.c".into(),
                "a é.o".into(),
            ),
            BuildStep::Custom(CustomStep::default()),
            compile_step(
                Some("/work/sub".into()),
                "cc -c b.c".into(),
                "/src/b.c".into(),
                "b.o".into(),
            ),
        ],
        ..BuildGraph::default()
    };
    let work = Path::new("/work");
    compile_commands::write_compile_commands(&graph, work, &host).unwrap();
    let content = read(&host, &work.join("compile_commands.json"));
    let expected = r#"[
{
  "directory": "/work",
  "command": "cc -DNAME=\"\\\"a b\\\"\" -c \"a é.c\"",
  "file": "/src/a é.c",
  "output": "a é.o"
},
{
  "directory": "/work/sub",
  "command": "cc -c b.c",
  "file": "/src/b.c",
  "output": "b.o"
}
]"#;
    assert_eq!(content, expected.as_bytes());
    let Value::Array(items) = rcmake_json::parse(&content, &ReadOptions::default()).unwrap() else {
        panic!("expected an array");
    };
    assert_eq!(items.len(), 2);
    assert_eq!(
        items[1],
        Value::Object(
            [
                ("directory".into(), Value::string("/work/sub")),
                ("command".into(), Value::string("cc -c b.c")),
                ("file".into(), Value::string("/src/b.c")),
                ("output".into(), Value::string("b.o")),
            ]
            .into()
        )
    );
    assert_eq!(
        compile_commands::render(&BuildGraph::default(), b"/work").unwrap(),
        b"[]"
    );
}

fn action(destination: BString, kind: InstallActionKind) -> InstallAction {
    InstallAction {
        all_components: false,
        exclude_from_all: false,
        component: "Data".into(),
        destination,
        configurations: vec!["Debug".into(), "Rel.With+Info".into()],
        kind,
        optional: false,
        message: BString::default(),
    }
}

fn files(
    paths: Vec<BString>,
    rename: Option<BString>,
    permissions: Vec<BString>,
) -> InstallActionKind {
    InstallActionKind::Files {
        file_type: "FILE".into(),
        files: paths,
        rename,
        permissions,
    }
}

#[test]
fn install_script_covers_each_action_and_manifest_scope() {
    let absolute = if cfg!(windows) {
        "C:/absolute"
    } else {
        "/absolute"
    };
    let host = PureHost::default();
    let build = Path::new("/work");
    let mut file = action(
        "share/data".into(),
        files(
            vec!["/src/a ${literal}.txt".into()],
            Some("renamed.txt".into()),
            vec!["OWNER_READ".into()],
        ),
    );
    file.optional = true;
    let manifest = InstallManifest {
        directories: Default::default(),
        target_installers: Vec::new(),
        actions: vec![
            file,
            action(
                absolute.into(),
                InstallActionKind::Directory {
                    dirs: vec!["/src/assets/".into()],
                    options: vec![
                        "FILES_MATCHING".into(),
                        "PATTERN".into(),
                        "*.txt".into(),
                        "EXCLUDE".into(),
                    ],
                },
            ),
            action(
                BString::default(),
                InstallActionKind::Script("/src/setup.cmake".into()),
            ),
            action(
                BString::default(),
                InstallActionKind::Code(
                    br#"message(STATUS [[one
two]])"#
                        .into(),
                ),
            ),
            action("lib/cmake".into(), InstallActionKind::Export { plan: 1 }),
        ],
    };
    let options = InstallScriptOptions {
        prefix: b"/opt/rcmake///",
        configuration: b"Debug",
        cross_compiling: true,
        shared_libraries_without_execute: true,
        export_files: &[
            "/work/UnusedTargets.cmake".into(),
            "/work/ExampleTargets.cmake".into(),
        ],
        ..InstallScriptOptions::default()
    };
    assert_eq!(
        install_script::render(&manifest, b"/work"),
        Err(GenError::MissingExport(1))
    );
    assert!(
        host.metadata(&build.join("cmake_install.cmake"), rcmake_host::Follow::Yes)
            .is_err()
    );
    write_install_script(&manifest, build, &options, &host).unwrap();
    let content = read(&host, &build.join("cmake_install.cmake"));
    let content = std::str::from_utf8(&content).unwrap();
    for snippet in [
        r#"set(CMAKE_INSTALL_PREFIX "/opt/rcmake///")"#,
        r#"set(CMAKE_INSTALL_SO_NO_EXE "TRUE")"#,
        r#"set(CMAKE_CROSSCOMPILING "TRUE")"#,
        r#"if(CMAKE_INSTALL_COMPONENT STREQUAL "Data" OR NOT CMAKE_INSTALL_COMPONENT)"#,
        r#"if(CMAKE_INSTALL_CONFIG_NAME MATCHES "^([Dd][Ee][Bb][Uu][Gg]|[Rr][Ee][Ll]\\.[Ww][Ii][Tt][Hh]\\+[Ii][Nn][Ff][Oo])$")"#,
        r#"file(INSTALL DESTINATION "${CMAKE_INSTALL_PREFIX}/share/data" TYPE "FILE" OPTIONAL RENAME "renamed.txt" PERMISSIONS "OWNER_READ" FILES "/src/a \${literal}.txt")"#,
        &format!(
            r#"file(INSTALL DESTINATION "{absolute}" TYPE "DIRECTORY" FILES "/src/assets/" "FILES_MATCHING" REGEX "/[^/]*\\.txt\$" "EXCLUDE")"#
        ),
        r#"include("/src/setup.cmake")"#,
        "message(STATUS [[one\ntwo]])",
        r#"file(INSTALL DESTINATION "${CMAKE_INSTALL_PREFIX}/lib/cmake" TYPE "FILE" FILES "/work/ExampleTargets.cmake")"#,
        r#"file(WRITE "/work/${CMAKE_INSTALL_MANIFEST}" "${CMAKE_INSTALL_MANIFEST_CONTENT}")"#,
    ] {
        assert!(
            content.contains(snippet),
            "missing {snippet} in:\n{content}"
        );
    }
    let child = render_with_options(
        &manifest,
        b"/work",
        &InstallScriptOptions {
            top_level: false,
            ..options
        },
    )
    .unwrap();
    assert!(
        !std::str::from_utf8(&child)
            .unwrap()
            .contains("CMAKE_INSTALL_MANIFEST")
    );
}

fn backtrace(file: BString, line: u32) -> Backtrace {
    Backtrace::default()
        .push(Frame {
            file: file.as_slice().into(),
            line: 0,
            name: None,
            kind: FrameKind::ModuleInclude,
        })
        .push(Frame {
            file: file.as_slice().into(),
            line,
            name: Some(b"add_test".as_slice().into()),
            kind: FrameKind::Command,
        })
}

#[test]
fn ctest_file_keeps_test_arguments_properties_backtraces_and_children() {
    let host = PureHost::default();
    let mut model = ProjectModel::default();
    model.set_root_dirs("/src".into(), "/work".into());
    let child = model
        .add_directory(
            DirId::ROOT,
            "/src/child".into(),
            "/work/inner/child".into(),
            Backtrace::default(),
        )
        .unwrap();
    model
        .add_directory(
            DirId::ROOT,
            "/other".into(),
            "/external".into(),
            Backtrace::default(),
        )
        .unwrap();
    let mut props = PropertyMap::default();
    props.set("TIMEOUT".into(), "10".into());
    props.set("LABELS".into(), "one;two".into());
    model
        .add_test(TestEntry {
            name: "a test;name".into(),
            dir: DirId::ROOT,
            backtrace: backtrace("/src/CMakeLists.txt".into(), 9),
            command: vec![
                "/work/test_tool".into(),
                "".into(),
                "a;b".into(),
                "\n${literal}]=]".into(),
            ],
            configurations: vec!["Release".into()],
            working_directory: Some("/work/test dir".into()),
            command_expand_lists: false,
            old_style: false,
            policies: Default::default(),
            props,
            build_depends: Vec::new(),
            kind: rcmake_model::TestKind::Single,
        })
        .unwrap();
    model
        .add_test(TestEntry {
            name: "child-only".into(),
            dir: child,
            backtrace: Backtrace::default(),
            command: vec!["echo".into()],
            configurations: Vec::new(),
            working_directory: None,
            command_expand_lists: false,
            old_style: true,
            policies: Default::default(),
            props: PropertyMap::default(),
            build_depends: Vec::new(),
            kind: rcmake_model::TestKind::Single,
        })
        .unwrap();
    host.create_dir_all(Path::new("/work/inner/child")).unwrap();
    ctest_file::write_ctest_file(&model, DirId::ROOT, &host).unwrap();
    ctest_file::write_ctest_file(&model, child, &host).unwrap();
    let root = read(&host, Path::new("/work/CTestTestfile.cmake"));
    let root = std::str::from_utf8(&root).unwrap();
    assert!(root.starts_with(
        "# CMake generated Testfile for \n# Source directory: /src\n# Build directory: /work\n"
    ));
    assert!(
        root.contains(
            r#"if(CTEST_CONFIGURATION_TYPE MATCHES "^([Rr][Ee][Ll][Ee][Aa][Ss][Ee])\$")"#
        )
    );
    assert!(root.contains(
        "  add_test(\"a test;name\" \"/work/test_tool\" \"\" \"a;b\" [==[\n\n${literal}]=]]==])"
    ));
    assert!(root.contains(r#"  set_tests_properties("a test;name" PROPERTIES  LABELS "one;two" TIMEOUT "10" WORKING_DIRECTORY "/work/test dir" _BACKTRACE_TRIPLES "/src/CMakeLists.txt;9;add_test;/src/CMakeLists.txt;0;")"#));
    assert!(root.ends_with("endif()\nsubdirs(\"inner/child\")\nsubdirs(\"/external\")\n"));
    assert!(!root.contains("child-only"));
    let child_output = read(&host, Path::new("/work/inner/child/CTestTestfile.cmake"));
    assert!(
        std::str::from_utf8(&child_output)
            .unwrap()
            .contains(r#"add_test("child-only" "echo")"#)
    );
}

#[test]
fn configure_log_groups_events_by_run_and_keeps_yaml_structure() {
    let host = PureHost::default();
    let build = Path::new("/work");
    let log = build.join("CMakeFiles/CMakeConfigureLog.yaml");
    write_configure_log(&[], build, &host).unwrap();
    assert!(host.metadata(&log, rcmake_host::Follow::Yes).is_err());
    let compile = TryCompile {
        description: Some(b"test compiler"),
        source_dir: b"/work/probe/src",
        binary_dir: b"/work/probe/build",
        cmake_variables: &[("CMAKE_FLAGS".into(), "-DTEST=\"value\"".into())],
        build_result: BuildResult {
            variable: b"compiled",
            cached: true,
            stdout: b"line 1\n\nline 3\n",
            exit_code: 0,
        },
    };
    write_configure_log(
        &[
            ConfigureEvent {
                backtrace: &["CMakeLists.txt:10 (message)".into()],
                checks: &["Check: 😀".into()],
                kind: EventKind::Message(b"  leading\n--- \\ \t\r\xFF"),
            },
            ConfigureEvent {
                backtrace: &[],
                checks: &[],
                kind: EventKind::TryCompile(compile),
            },
            ConfigureEvent {
                backtrace: &[],
                checks: &[],
                kind: EventKind::TryRun {
                    compile,
                    run: RunResult {
                        variable: b"executed",
                        cached: false,
                        stdout: Some(b""),
                        stderr: Some(b"denied\n"),
                        exit_code: Some(RunExitCode::Status(b"FAILED_TO_RUN")),
                    },
                },
            },
        ],
        build,
        &host,
    )
    .unwrap();
    append_event(
        &ConfigureEvent {
            backtrace: &[],
            checks: &[],
            kind: EventKind::Message(b"latest"),
        },
        build,
        &host,
    )
    .unwrap();
    let content = read(&host, &log);
    let content = std::str::from_utf8(&content).unwrap();
    assert_eq!(
        content,
        r#"
---
events:
  -
    kind: "message-v1"
    backtrace:
      - "CMakeLists.txt:10 (message)"
    checks:
      - "Check: 😀"
    message: |2
        leading
      --- \\ 	\x0d\xff
  -
    kind: "try_compile-v1"
    backtrace: []
    description: "test compiler"
    directories:
      source: "/work/probe/src"
      binary: "/work/probe/build"
    cmakeVariables:
      CMAKE_FLAGS: "-DTEST=\"value\""
    buildResult:
      variable: "compiled"
      cached: true
      stdout: |
        line 1
        [blank]
        line 3
      exitCode: 0
  -
    kind: "try_run-v1"
    backtrace: []
    description: "test compiler"
    directories:
      source: "/work/probe/src"
      binary: "/work/probe/build"
    cmakeVariables:
      CMAKE_FLAGS: "-DTEST=\"value\""
    buildResult:
      variable: "compiled"
      cached: true
      stdout: |
        line 1
        [blank]
        line 3
      exitCode: 0
    runResult:
      variable: "executed"
      cached: false
      stdout: |
      stderr: |
        denied
      exitCode: "FAILED_TO_RUN"
...

---
events:
  -
    kind: "message-v1"
    backtrace: []
    message: |
      latest
...
"#
        .replace("[blank]", "")
    );
}

#[cfg(unix)]
mod differential {
    use std::{path::PathBuf, time::Duration};

    use rcmake_host::{
        CommandSpec, Input, NativeHost, Output, ProcessResult, RunOutput, RunRequest, WriteMode,
        path::{from_native, to_native},
    };

    use super::*;

    struct Fixture<'a> {
        host: &'a NativeHost,
        root: PathBuf,
    }

    impl<'a> Fixture<'a> {
        fn new(host: &'a NativeHost) -> Self {
            let time = host.now();
            let name = format!(
                "rcmake-common-{}-{}-{}",
                std::process::id(),
                time.secs,
                time.nanos
            );
            let root = Path::new("/tmp").join(name);
            host.create_dir_all(&root).unwrap();
            Self { host, root }
        }

        fn file(&self, path: &str, content: &[u8]) -> BString {
            let path = self.root.join(path);
            self.host.create_dir_all(path.parent().unwrap()).unwrap();
            self.host
                .write(&path, content, WriteMode::Truncate)
                .unwrap();
            from_native(&path)
        }

        fn run(&self, program: BString, args: Vec<BString>) -> RunOutput {
            let result = self
                .host
                .run(
                    &RunRequest {
                        pipeline: vec![CommandSpec { program, args }],
                        cwd: self.root.clone(),
                        env: self.host.env_all(),
                        stdin: Input::Null,
                        stdout: Output::Capture,
                        stderr: Output::Capture,
                        timeout: Some(Duration::from_secs(45)),
                        tee: false,
                        default_error_mode: false,
                    },
                    &mut |_, _| {},
                )
                .unwrap();
            assert_eq!(
                result.results,
                [ProcessResult::Exited(0)],
                "stdout: {}\nstderr: {}",
                String::from_utf8_lossy(&result.stdout),
                String::from_utf8_lossy(&result.stderr)
            );
            result
        }
    }

    impl Drop for Fixture<'_> {
        fn drop(&mut self) {
            let _ = self.host.remove_dir_all(&self.root);
        }
    }

    #[test]
    fn cmake_oracle_matches_compile_database_and_configure_log() {
        let host = NativeHost::new().unwrap();
        let Some(oracle) = host.env_get(b"RCMAKE_TEST_REFERENCE_CMAKE") else {
            return;
        };
        assert!(
            host.metadata(&to_native(&oracle), rcmake_host::Follow::Yes)
                .is_ok()
        );
        let fixture = Fixture::new(&host);
        fixture.file("CMakeLists.txt", "cmake_minimum_required(VERSION 4.5)\nproject(sample C)\nmessage(CONFIGURE_LOG \"line 1\\nline 2\")\nadd_executable(sample \"é.c\" test.c)\n".as_bytes());
        fixture.file("é.c", b"int main(void) { return 0; }\n");
        fixture.file("test.c", b"void test(void) {}\n");
        fixture.run(
            oracle,
            [
                "-S",
                ".",
                "-B",
                "build",
                "-G",
                "Unix Makefiles",
                "-DCMAKE_EXPORT_COMPILE_COMMANDS=ON",
            ]
            .into_iter()
            .map(BString::from)
            .collect(),
        );
        let build = fixture.root.join("build");
        let content = read(&host, &build.join("compile_commands.json"));
        let Value::Array(items) = rcmake_json::parse(&content, &ReadOptions::default()).unwrap()
        else {
            panic!("expected a compile database");
        };
        assert_eq!(items.len(), 2);
        let steps = items
            .into_iter()
            .map(|item| {
                let Value::Object(mut object) = item else {
                    panic!("expected a compile record");
                };
                let mut field = |key: &[u8]| {
                    let Some(Value::String(value)) = object.remove(key) else {
                        panic!("missing key {key:?}");
                    };
                    value
                };
                compile_step(
                    Some(field(b"directory")),
                    field(b"command"),
                    field(b"file"),
                    field(b"output"),
                )
            })
            .collect();
        let graph = BuildGraph {
            steps,
            ..BuildGraph::default()
        };
        assert_eq!(
            compile_commands::render(&graph, &from_native(&build)).unwrap(),
            content
        );
        let oracle_log = read(&host, &build.join("CMakeFiles/CMakeConfigureLog.yaml"));
        let event = ConfigureEvent {
            backtrace: &["CMakeLists.txt:3 (message)".into()],
            checks: &[],
            kind: EventKind::Message(b"line 1\nline 2"),
        };
        let rendered = render_events(&[event]);
        let expected_event = rendered
            .strip_prefix(b"\n---\nevents:\n")
            .unwrap()
            .strip_suffix(b"...\n")
            .unwrap();
        assert!(
            oracle_log
                .windows(expected_event.len())
                .any(|window| window == expected_event)
        );
    }

    #[test]
    fn ctest_reads_generated_tests_and_configuration_guards() {
        let host = NativeHost::new().unwrap();
        let Some(oracle) = host.env_get(b"RCMAKE_TEST_REFERENCE_CMAKE") else {
            return;
        };
        let ctest = from_native(&to_native(&oracle).with_file_name("ctest"));
        let fixture = Fixture::new(&host);
        let child = fixture.root.join("child");
        host.create_dir_all(&child).unwrap();
        let mut model = ProjectModel::default();
        model.set_root_dirs(from_native(&fixture.root), from_native(&fixture.root));
        let dir = model
            .add_directory(
                DirId::ROOT,
                from_native(&child),
                from_native(&child),
                Backtrace::default(),
            )
            .unwrap();
        model
            .add_test(TestEntry {
                name: "child test".into(),
                dir,
                backtrace: backtrace(from_native(&child.join("CMakeLists.txt")), 1),
                command: vec![oracle, "-E".into(), "echo".into(), "${literal}]=]".into()],
                configurations: vec!["Release".into()],
                working_directory: None,
                command_expand_lists: false,
                old_style: false,
                policies: Default::default(),
                props: PropertyMap::default(),
                build_depends: Vec::new(),
                kind: rcmake_model::TestKind::Single,
            })
            .unwrap();
        ctest_file::write_ctest_file(&model, DirId::ROOT, &host).unwrap();
        ctest_file::write_ctest_file(&model, dir, &host).unwrap();
        let run = fixture.run(
            ctest.clone(),
            vec!["-C".into(), "release".into(), "-V".into()],
        );
        let stdout = String::from_utf8_lossy(&run.stdout);
        assert!(stdout.contains("100% tests passed"), "{stdout}");
        assert!(stdout.contains("1: ${literal}]=]"), "{stdout}");
        let debug = fixture.run(ctest, vec!["-C".into(), "Debug".into(), "-N".into()]);
        let stdout = String::from_utf8_lossy(&debug.stdout);
        assert!(stdout.contains("Total Tests: 0"), "{stdout}");
    }

    #[test]
    fn install_script_selects_the_ide_configuration() {
        let host = NativeHost::new().unwrap();
        for executable in [
            b"RCMAKE_TEST_REFERENCE_CMAKE".as_slice(),
            b"RCMAKE_TEST_EXECUTABLE",
        ]
        .into_iter()
        .filter_map(|key| host.env_get(key))
        {
            let fixture = Fixture::new(&host);
            let manifest = InstallManifest {
                directories: Default::default(),
                target_installers: Vec::new(),
                actions: ["Debug", "Release"]
                    .into_iter()
                    .map(|config| {
                        let source = fixture.file(&format!("{config}/app"), config.as_bytes());
                        let mut install = action("bin".into(), files(vec![source], None, vec![]));
                        install.configurations = vec![config.into()];
                        install
                    })
                    .collect(),
            };
            let script = from_native(&fixture.root.join("cmake_install.cmake"));
            write_install_script(
                &manifest,
                &fixture.root,
                &InstallScriptOptions {
                    configuration: b"Release",
                    ..Default::default()
                },
                &host,
            )
            .unwrap();
            for (folder, flags, expected) in [
                ("default", &[][..], b"Release".as_slice()),
                ("ide", &["-DBUILD_TYPE=Debug"][..], b"Debug"),
                (
                    "explicit",
                    &["-DBUILD_TYPE=Debug", "-DCMAKE_INSTALL_CONFIG_NAME=Release"][..],
                    b"Release",
                ),
            ] {
                let prefix = fixture.root.join(folder);
                let args = flags
                    .iter()
                    .copied()
                    .map(BString::from)
                    .chain([
                        format!("-DCMAKE_INSTALL_PREFIX={}", prefix.display()).into(),
                        "-P".into(),
                        script.clone(),
                    ])
                    .collect();
                fixture.run(executable.clone(), args);
                assert_eq!(read(&host, &prefix.join("bin/app")), expected);
            }
        }
    }

    #[test]
    fn install_script_runs_with_cmake_and_rcmake() {
        let host = NativeHost::new().unwrap();
        let tools = [
            b"RCMAKE_TEST_REFERENCE_CMAKE".as_slice(),
            b"RCMAKE_TEST_EXECUTABLE",
        ];
        for tool in tools.into_iter().filter_map(|key| host.env_get(key)) {
            let fixture = Fixture::new(&host);
            let source = fixture.file("src/input ${literal}.txt", b"data");
            let missing = from_native(&fixture.root.join("src/missing.txt"));
            fixture.file("src/assets/match.txt", b"match");
            fixture.file("src/assets/skip.log", b"skip");
            let include = fixture.file(
                "src/setup.cmake",
                br#"file(WRITE "${CMAKE_INSTALL_PREFIX}/from-include" "yes")"#,
            );
            let export = fixture.file("src/ExampleTargets.cmake", b"# Generated targets\n");
            let build = fixture.root.as_path();
            let prefix = fixture.root.join("prefix");
            let prefix_bytes = [from_native(&prefix).as_slice(), b"///"].concat();
            let absolute = fixture.root.join("absolute");
            let mut optional = action("share".into(), files(vec![missing], None, vec![]));
            optional.optional = true;
            let mut skipped = action("skip".into(), files(vec![source.clone()], None, vec![]));
            skipped.component = "Other".into();
            let mut wrong_config = action(
                "skip-config".into(),
                files(vec![source.clone()], None, vec![]),
            );
            wrong_config.configurations = vec!["Release".into()];
            let mut dirs = from_native(&fixture.root.join("src/assets"));
            dirs.push(b'/');
            let manifest = InstallManifest {
                directories: Default::default(),
        target_installers: Vec::new(),
                actions: vec![
                    skipped,
                    wrong_config,
                    optional,
                    action("share".into(), files(vec![source], Some("renamed.txt".into()), vec!["OWNER_READ".into(), "OWNER_WRITE".into()])),
                    action(from_native(&absolute), InstallActionKind::Directory {
                        dirs: vec![dirs],
                        options: vec!["FILES_MATCHING".into(), "PATTERN".into(), "*.txt".into()],
                    }),
                    action(BString::default(), InstallActionKind::Script(include)),
                    action(BString::default(), InstallActionKind::Code(
                        br#"file(WRITE "${CMAKE_INSTALL_PREFIX}/from-code" "${CMAKE_INSTALL_SO_NO_EXE}/${CMAKE_CROSSCOMPILING}")"#.into(),
                    )),
                    action("lib/cmake".into(), InstallActionKind::Export { plan: 0 }),
                ],
            };
            let options = InstallScriptOptions {
                prefix: &prefix_bytes,
                configuration: b"dEbUg",
                cross_compiling: true,
                shared_libraries_without_execute: true,
                export_files: &[export],
                ..InstallScriptOptions::default()
            };
            write_install_script(&manifest, build, &options, &host).unwrap();
            let script = from_native(&build.join("cmake_install.cmake"));
            fixture.run(
                tool.clone(),
                vec!["-DCOMPONENT=Data".into(), "-P".into(), script.clone()],
            );
            assert_eq!(read(&host, &prefix.join("share/renamed.txt")), b"data");
            assert_eq!(read(&host, &absolute.join("match.txt")), b"match");
            assert_eq!(read(&host, &prefix.join("from-include")), b"yes");
            assert_eq!(read(&host, &prefix.join("from-code")), b"TRUE/TRUE");
            assert_eq!(
                read(&host, &prefix.join("lib/cmake/ExampleTargets.cmake")),
                b"# Generated targets\n"
            );
            for absent in [
                absolute.join("skip.log"),
                prefix.join("skip"),
                prefix.join("skip-config"),
                prefix.join("share/missing.txt"),
            ] {
                assert!(
                    host.metadata(&absent, rcmake_host::Follow::Yes).is_err(),
                    "{absent:?}"
                );
            }
            let paths = [
                from_native(&prefix.join("share/renamed.txt")),
                from_native(&absolute.join("match.txt")),
                from_native(&prefix.join("lib/cmake/ExampleTargets.cmake")),
            ];
            let expected: Vec<u8> = paths
                .iter()
                .map(|path| path.as_slice())
                .collect::<Vec<_>>()
                .join(&b'\n');
            assert_eq!(
                read(&host, &build.join("install_manifest_Data.txt")),
                expected
            );
            fixture.run(tool, vec!["-P".into(), script]);
            assert!(
                host.metadata(
                    &build.join("install_manifest.txt"),
                    rcmake_host::Follow::Yes
                )
                .is_ok()
            );
            assert!(
                host.metadata(
                    &prefix.join("skip/input ${literal}.txt"),
                    rcmake_host::Follow::Yes
                )
                .is_ok()
            );
        }
    }
}

#[test]
fn fileapi_directory_dot_path_avoids_double_dot() {
    use rcmake_host::WriteMode;
    use rcmake_model::{DirId, InstallKind, InstallOptions, InstallRule, ProjectModel};

    let host = PureHost::default();
    let build = Path::new("/work/build");
    host.create_dir_all(&build.join(".cmake/api/v1/query"))
        .expect("query dir");
    host.write(
        &build.join(".cmake/api/v1/query/codemodel-v2"),
        b"",
        WriteMode::Truncate,
    )
    .expect("query");

    let mut model = ProjectModel::default();
    model.set_root_dirs("/work/MySrc".into(), "/work/build".into());
    model.add_install(InstallRule {
        dir: DirId::ROOT,
        backtrace: Default::default(),
        kind: InstallKind::Directory(vec![
            rcmake_model::BT::new("/work/MySrc/.".into(), Default::default()),
            rcmake_model::BT::new("/work/MySrc/sub/.".into(), Default::default()),
            rcmake_model::BT::new("/work/MySrc/cxx/".into(), Default::default()),
            rcmake_model::BT::new("/work/MySrc".into(), Default::default()),
        ]),
        options: InstallOptions {
            destination: Some("dir1".into()),
            ..Default::default()
        },
    });
    let result = rcmake_resolve::resolve(&model, &host, &[]).expect("resolve");
    crate::fileapi::write_replies(&model, &result, &host, build).expect("replies");
    let reply_dir = build.join(".cmake/api/v1/reply");
    let dir_json = host
        .read_dir(&reply_dir)
        .expect("read reply dir")
        .into_iter()
        .find(|entry| entry.name.starts_with(b"directory-"))
        .expect("directory reply");
    let text = String::from_utf8(
        host.read(
            &reply_dir.join(rcmake_host::path::to_native(&dir_json.name)),
            rcmake_host::ReadRange::All { limit: 1 << 20 },
        )
        .expect("read dir json"),
    )
    .expect("utf8");
    assert!(!text.contains("./."), "must not emit './.': {text}");
    assert!(text.contains(r#"".""#), "missing '.': {text}");
    assert!(text.contains(r#""sub/.""#), "missing 'sub/.': {text}");
    assert!(text.contains(r#""from" : "cxx""#) || text.contains(r#""from":"cxx""#));
    assert!(text.contains(r#""to" : "MySrc""#) || text.contains(r#""to":"MySrc""#));
}

#[test]
fn export_main_file_starts_with_generated_by_cmake_header() {
    let plan = rcmake_resolve::ExportPlan {
        file: "FooTargets.cmake".into(),
        ..rcmake_resolve::ExportPlan::default()
    };
    let rendered = crate::export::render(&plan).expect("render");
    let text = String::from_utf8_lossy(&rendered);
    assert!(
        text.starts_with("# Generated by CMake\n"),
        "expected CMake header, got:\n{text}"
    );
}

#[test]
fn xcode_ios_install_script_emits_platform_vars() {
    let manifest = InstallManifest::default();
    let generator = BString::from("Xcode");
    let system = BString::from("iOS");
    let top_bin = BString::from("/work/build");
    let script = install_script::render_with_runtime_definitions(
        &manifest,
        b"/work/build",
        &InstallScriptOptions::default(),
        |name| match name {
            b"CMAKE_GENERATOR" => Some(&generator),
            b"CMAKE_SYSTEM_NAME" => Some(&system),
            b"CMAKE_BINARY_DIR" => Some(&top_bin),
            _ => None,
        },
    )
    .expect("render");
    let text = String::from_utf8_lossy(&script);
    assert!(text.contains("set(CMAKE_BINARY_DIR \"/work/build\")"));
    assert!(text.contains("set(PLATFORM_NAME iphoneos)"));
    assert!(text.contains("set(EFFECTIVE_PLATFORM_NAME -iphoneos)"));
}
