//! Verify the workflow command through the CLI.

#![cfg(unix)]
#![allow(clippy::expect_used)]

use std::path::{Path, PathBuf};
use std::time::Duration;

use rcmake_base::BString;
use rcmake_host::{
    CommandSpec, Host, Input, NativeHost, Output, ProcessResult, ReadRange, RunOutput, RunRequest,
    WriteMode, path,
};

struct Fixture<'a> {
    host: &'a NativeHost,
    root: PathBuf,
}

impl Fixture<'_> {
    fn write(&self, file: &str, contents: &[u8]) {
        self.host
            .write(&self.root.join(file), contents, WriteMode::Truncate)
            .expect("write fixture file");
    }
}

impl Drop for Fixture<'_> {
    fn drop(&mut self) {
        let _ = self.host.remove_dir_all(&self.root);
    }
}

fn run(host: &NativeHost, cwd: &Path, program: &BString, args: &[&str]) -> RunOutput {
    let env = host
        .env_all()
        .into_iter()
        .filter(|(key, _)| key != "PWD")
        .collect();
    host.run(
        &RunRequest {
            pipeline: vec![CommandSpec {
                program: program.clone(),
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
    .expect("run fixture command")
}

fn assert_exit(output: &RunOutput, code: i32) {
    assert_eq!(
        output.results,
        [ProcessResult::Exited(code)],
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn headers(output: &RunOutput) -> Vec<String> {
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter(|line| line.starts_with("Executing workflow step"))
        .map(str::to_owned)
        .collect()
}

#[test]
fn workflow_configures_builds_tests_and_packages() {
    let host = NativeHost::new().expect("native host");
    let temp = run(
        &host,
        Path::new("/tmp"),
        &"/usr/bin/mktemp".into(),
        &["-d", "/tmp/rcmake-workflow.XXXXXX"],
    );
    assert_exit(&temp, 0);
    let fixture = Fixture {
        host: &host,
        root: path::to_native(String::from_utf8_lossy(&temp.stdout).trim_end().as_bytes()),
    };
    fixture.write("CMakeLists.txt", br#"
cmake_minimum_required(VERSION 4.5)
project(Workflow NONE)
enable_testing()
file(MAKE_DIRECTORY "${CMAKE_BINARY_DIR}/payload")
add_custom_target(payload ALL
  COMMAND "${CMAKE_COMMAND}" -E echo good
  COMMAND "${CMAKE_COMMAND}" -E copy "${CMAKE_SOURCE_DIR}/input.txt" "${CMAKE_BINARY_DIR}/payload/item.txt")
add_test(NAME smoke COMMAND "${CMAKE_COMMAND}" -E compare_files
  "${CMAKE_BINARY_DIR}/payload/item.txt" "${CMAKE_SOURCE_DIR}/expected.txt")
file(WRITE "${CMAKE_BINARY_DIR}/CPackConfig.cmake"
  "set(CPACK_PACKAGE_NAME Workflow)\n"
  "set(CPACK_PACKAGE_DESCRIPTION Workflow)\n"
  "set(CPACK_PACKAGE_VERSION 1.0.0)\n"
  "set(CPACK_PACKAGE_FILE_NAME workflow)\n"
  "set(CPACK_INCLUDE_TOPLEVEL_DIRECTORY OFF)\n"
  "set(CPACK_INSTALLED_DIRECTORIES \"${CMAKE_BINARY_DIR}/payload;/\")\n")
"#);
    fixture.write(
        "custom.json",
        br#"{
      "version": 13,
      "configurePresets": [
        {"name": "cfg", "generator": "Unix Makefiles", "binaryDir": "${sourceDir}/build"},
        {"name": "disabled", "condition": false, "binaryDir": "${sourceDir}/unused"}
      ],
      "buildPresets": [{"name": "make", "configurePreset": "cfg"}],
      "testPresets": [{"name": "check", "configurePreset": "cfg"}],
      "packagePresets": [{"name": "archive", "configurePreset": "cfg", "generators": ["TGZ"]}],
      "workflowPresets": [
        {"name": "all", "displayName": "Build and package", "steps": [
          {"type": "configure", "name": "cfg"}, {"type": "build", "name": "make"},
          {"type": "test", "name": "check"}, {"type": "package", "name": "archive"}
        ]},
        {"name": "unavailable", "steps": [{"type": "configure", "name": "disabled"}]}
      ]
    }"#,
    );
    fixture.write("input.txt", b"workflow payload\n");

    let rcmake = BString::from(env!("CARGO_BIN_EXE_rcmake"));
    let tool = &rcmake;
    let root = &fixture.root;
    for (args, expected) in [
        (
            vec!["--workflow", "--list-presets", "--presets-file=custom.json"],
            "Available workflow presets:\n\n  \"all\" - Build and package\n",
        ),
        (
            vec![
                "--workflow",
                "--list-presets=defined",
                "--presets-file",
                "custom.json",
            ],
            "Defined workflow presets:\n\n  \"all\"         - Build and package\n  \"unavailable\"\n    Unavailable: step 1: configure preset \"disabled\": condition evaluated to false: false\n",
        ),
        (
            vec![
                "--workflow",
                "--preset=unknown",
                "--presets-file",
                "custom.json",
            ],
            "Available workflow presets:\n\n  \"all\" - Build and package\n",
        ),
    ] {
        let output = run(&host, root, tool, &args);
        let unknown = args.contains(&"--preset=unknown");
        assert_exit(&output, i32::from(unknown));
        if unknown {
            assert!(String::from_utf8_lossy(&output.stderr).contains("No such workflow preset"));
        }
        assert_eq!(String::from_utf8_lossy(&output.stdout), expected);
    }

    fixture.write("expected.txt", b"workflow payload\n");
    fixture.write(
        "instrument.sh",
        b"cp \"$1\" \"$(dirname \"$0\")/workflow-index.json\"\n",
    );
    let query_dir = root.join("build/.cmake/instrumentation/v1/query");
    host.create_dir_all(&query_dir)
        .expect("create query directory");
    let query = format!(
        r#"{{"version":1,"hooks":["postCMakeWorkflow"],"callbacks":["/bin/sh {}/instrument.sh"]}}"#,
        root.display(),
    );
    host.write(
        &query_dir.join("workflow.json"),
        query.as_bytes(),
        WriteMode::Truncate,
    )
    .expect("write instrumentation query");
    let success = run(
        &host,
        root,
        tool,
        &["--workflow", "all", "--presets-file", "custom.json"],
    );
    assert_exit(&success, 0);
    let index = host
        .read(
            &root.join("workflow-index.json"),
            ReadRange::All { limit: 16 * 1024 },
        )
        .expect("read workflow instrumentation index");
    assert!(String::from_utf8_lossy(&index).contains("postCMakeWorkflow"));
    let success_headers = headers(&success);
    assert_eq!(
        success_headers,
        [
            "Executing workflow step 1 of 4: configure preset \"cfg\"",
            "Executing workflow step 2 of 4: build preset \"make\"",
            "Executing workflow step 3 of 4: test preset \"check\"",
            "Executing workflow step 4 of 4: package preset \"archive\"",
        ]
    );
    let archive = run(
        &host,
        &root.join("build"),
        &"/usr/bin/tar".into(),
        &["-xOzf", "workflow.tar.gz", "item.txt"],
    );
    assert_exit(&archive, 0);
    assert_eq!(archive.stdout, b"workflow payload\n");
    host.remove_file(&root.join("build/workflow.tar.gz"))
        .expect("remove archive");

    fixture.write("expected.txt", b"force a failed test\n");
    host.write(
        &root.join("build/CMakeCache.txt"),
        b"\nWORKFLOW_STALE:BOOL=ON\n",
        WriteMode::Append,
    )
    .expect("add stale cache entry");
    let fail = run(
        &host,
        root,
        tool,
        &[
            "--workflow",
            "--preset",
            "all",
            "--presets-file",
            "custom.json",
            "--fresh",
        ],
    );
    assert_exit(&fail, 8);
    let fail_headers = headers(&fail);
    assert_eq!(
        fail_headers,
        success_headers.into_iter().take(3).collect::<Vec<_>>()
    );
    assert!(
        String::from_utf8_lossy(&fail.stderr)
            .contains("Workflow step of type \"test\" named \"check\" failed with exit code 8")
    );
    let cache = host
        .read(
            &root.join("build/CMakeCache.txt"),
            ReadRange::All { limit: 128 * 1024 },
        )
        .expect("read cache");
    assert!(!String::from_utf8_lossy(&cache).contains("WORKFLOW_STALE"));
    assert!(
        host.metadata(
            &root.join("build/workflow.tar.gz"),
            rcmake_host::Follow::Yes
        )
        .is_err()
    );
    host.remove_dir_all(&root.join("build"))
        .expect("reset build directory");
}
