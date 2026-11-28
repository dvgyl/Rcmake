use std::path::{Path, PathBuf};
use std::time::Duration;

use rcmake_base::BString;
use rcmake_host::{CommandSpec, Host, Input, NativeHost, Output, ProcessResult, RunRequest, path};
use rcmake_json::Value;

use crate::files::{read_json, timestamp};
use crate::{
    CustomContent, CustomContentType, Hook, hooks, launcher, query, snippet,
    tests::{array, field, object, only_file, text},
};

struct Scratch<'a> {
    host: &'a dyn Host,
    dir: PathBuf,
}

#[test]
fn index_lock_is_released_when_process_exits() {
    let host = NativeHost::new().expect("native host");
    let child_build = b"RCMAKE_TEST_INDEX_LOCK_BUILD";
    if let Some(build) = host.env_get(child_build) {
        let path = crate::files::api_dir(&path::to_native(&build)).join(".index.lock");
        let guard = host
            .lock_file(&path, Some(Duration::ZERO))
            .expect("child acquires the index lock");
        // Exit the process while the lock is still held.
        std::mem::forget(guard);
        return;
    }
    let temp = host
        .env_get(b"TMPDIR")
        .map_or_else(|| PathBuf::from("/tmp"), |dir| path::to_native(&dir));
    let scratch = Scratch {
        host: &host,
        dir: temp.join(format!(
            "rcmake-index-lock-{}-{}",
            host.system().pid,
            timestamp(&host)
        )),
    };
    let api = crate::files::api_dir(&scratch.dir);
    host.create_dir_all(&api)
        .expect("create instrumentation directory");
    host.env_set(child_build, Some(&path::from_native(&scratch.dir)));
    let exe = host.current_exe().expect("test binary");
    run(
        &host,
        &scratch.dir,
        &[
            path::from_native(&exe),
            "--exact".into(),
            "tests::oracle::index_lock_is_released_when_process_exits".into(),
        ],
        0,
    );
    let lock = api.join(".index.lock");
    assert_eq!(crate::files::read_bytes(&host, &lock), Ok(Vec::new()));
    let guard = host
        .lock_file(&lock, Some(Duration::ZERO))
        .expect("parent acquires the index lock");
    drop(guard);
    hooks::run_hook(&host, &scratch.dir, Hook::Manual).expect("run hook");
    assert_eq!(crate::files::read_bytes(&host, &lock), Ok(Vec::new()));
}

impl Drop for Scratch<'_> {
    fn drop(&mut self) {
        let _ = self.host.remove_dir_all(&self.dir);
    }
}

fn run(host: &dyn Host, cwd: &Path, args: &[BString], code: i32) -> Vec<u8> {
    let (program, args) = args.split_first().expect("program");
    let output = host
        .run(
            &RunRequest {
                pipeline: vec![CommandSpec {
                    program: program.clone(),
                    args: args.to_vec(),
                }],
                cwd: cwd.to_path_buf(),
                env: host.env_all(),
                stdin: Input::Null,
                stdout: Output::Capture,
                stderr: Output::Capture,
                timeout: Some(Duration::from_secs(30)),
                tee: false,
                default_error_mode: false,
            },
            &mut |_, _| {},
        )
        .expect("run oracle");
    assert_eq!(
        output.results,
        [ProcessResult::Exited(code)],
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    output.stdout
}

fn write(host: &dyn Host, path: &Path, value: &[u8]) {
    host.write(path, value, rcmake_host::WriteMode::Truncate)
        .expect("write fixture");
}

#[test]
fn compares_live_cmake_4_5_1_instrumentation() {
    let host = NativeHost::new().expect("native host");
    let Some(cmake) = host.env_get(b"RCMAKE_TEST_REFERENCE_CMAKE") else {
        return;
    };
    let ctest = path::from_native(&path::to_native(&cmake).with_file_name("ctest"));
    let temp = host
        .env_get(b"TMPDIR")
        .map_or_else(|| PathBuf::from("/tmp"), |dir| path::to_native(&dir));
    let dir = temp.join(format!(
        "rcmake-instrument-oracle-{}-{}",
        host.system().pid,
        timestamp(&host)
    ));
    host.create_dir_all(&dir).expect("test directory");
    let scratch = Scratch { host: &host, dir };
    let cwd = host
        .canonicalize(&scratch.dir)
        .expect("canonical working directory");
    host.env_set(
        b"CMAKE_CONFIG_DIR",
        Some(&path::from_native(&cwd.join("isolated-config"))),
    );
    let version = run(&host, &cwd, &[cmake.clone(), "--version".into()], 0);
    assert!(version.starts_with(b"cmake version 4.5.1\n"));
    write(&host, &cwd.join("callback.sh"), b"cp \"$2\" \"$1\"\n");
    write(
        &host,
        &cwd.join("CMakeLists.txt"),
        br#"
cmake_minimum_required(VERSION 4.5)
project(example NONE)
cmake_instrumentation(API_VERSION 1 DATA_VERSION 1.2
  OPTIONS trace captureOutput compileTrace processMetrics staticSystemInformation
  HOOKS postCMakeInstall postCTest postGenerate
  CALLBACK /bin/sh callback.sh observed.json
  CUSTOM_CONTENT greeting STRING "hello"
  CUSTOM_CONTENT enabled BOOL OFF
  CUSTOM_CONTENT members LIST "a;;b"
  CUSTOM_CONTENT object JSON "{\"x\":1}")
"#,
    );
    let oracle_build = cwd.join("oracle");
    let our_build = cwd.join("ours");
    for build in [&oracle_build, &our_build] {
        host.create_dir_all(&build.join(".cmake/instrumentation/v1/query"))
            .expect("query dir");
        write(
            &host,
            &build.join(".cmake/instrumentation/v1/query/ignored.json"),
            br#"{"version":{"major":1,"minor":3},"hooks":["unknown"]}"#,
        );
    }
    run(
        &host,
        &cwd,
        &[
            cmake,
            "-S".into(),
            ".".into(),
            "-B".into(),
            path::from_native(&oracle_build),
            "-G".into(),
            "Ninja".into(),
        ],
        0,
    );
    let observed = cwd.join("observed.json");
    let oracle_index = read_json(&host, &observed).expect("callback received the index");
    let oracle_data = oracle_build.join(".cmake/instrumentation/v1/data");

    let oracle_query = read_json(
        &host,
        &oracle_build.join(".cmake/instrumentation/v1/query/generated/query-0.json"),
    )
    .expect("generated query");
    let mut queries = query::read_queries(&host, &oracle_build).expect("queries");
    let generated = queries.pop().expect("oracle query");
    assert!(queries.is_empty());
    query::write_generated_query(&host, &our_build, &generated).expect("write generated query");
    assert_eq!(
        read_json(
            &host,
            &our_build.join(".cmake/instrumentation/v1/query/generated/query-0.json")
        )
        .expect("ours"),
        oracle_query
    );

    let entries = [
        ("greeting", CustomContentType::String, "hello"),
        ("enabled", CustomContentType::Bool, "OFF"),
        ("members", CustomContentType::List, "a;;b"),
        ("object", CustomContentType::Json, r#"{"x":1}"#),
    ]
    .map(|(name, ty, content)| CustomContent {
        name: name.into(),
        ty,
        content: content.into(),
    });
    let oracle_content =
        read_json(&host, &only_file(&host, &oracle_data.join("content"))).expect("oracle content");
    let our_content = snippet::write_content(&host, &our_build, b"example", &entries, &[], false)
        .expect("write content");
    assert_eq!(
        read_json(&host, &our_content).expect("our content"),
        oracle_content
    );

    let launch = |build: &Path| {
        let mut argv = [
            "--instrument",
            "--command-type",
            "compile",
            "--target-name",
            "example",
            "--config",
            "Debug",
            "--output",
            "main.o",
            "--source",
            "main.c",
            "--language",
            "C",
            "--build-dir",
        ]
        .map(BString::from)
        .to_vec();
        argv.push(path::from_native(build));
        argv.extend(
            [
                "--",
                "/bin/sh",
                "-c",
                "echo stdout; echo stderr >&2; exit 7",
            ]
            .map(BString::from),
        );
        argv
    };
    let mut args = vec![ctest];
    args.extend(launch(&oracle_build));
    run(&host, &cwd, &args, 7);
    // NativeHost captures its current working directory at construction. Use a
    // relative-free command with the same arguments from the test process.
    assert_eq!(
        launcher::run_launcher(&host, &launch(&our_build)).expect("run launcher"),
        7
    );
    let oracle_snippet = read_json(&host, &only_file(&host, &oracle_data)).expect("oracle snippet");
    let our_data = our_build.join(".cmake/instrumentation/v1/data");
    let our_snippet = read_json(&host, &only_file(&host, &our_data)).expect("our snippet");
    assert!(
        object(&our_snippet)
            .keys()
            .eq(object(&oracle_snippet).keys())
    );
    for key in [
        "version",
        "role",
        "command",
        "result",
        "target",
        "config",
        "source",
        "language",
        "outputs",
        "outputSizes",
        "stdout",
        "stderr",
        "targetLabels",
    ] {
        assert_eq!(
            field(&our_snippet, key),
            field(&oracle_snippet, key),
            "field {key}"
        );
    }
    assert_eq!(*field(&our_snippet, "processMetrics"), Value::Null);

    // Use an absolute callback path because the native caller's cwd is the workspace.
    let mut generated = generated;
    generated.callbacks = vec![launcher::command_line(&[
        "/bin/sh".into(),
        path::from_native(&cwd.join("callback.sh")),
        path::from_native(&observed),
    ])];
    host.remove_file(&our_build.join(".cmake/instrumentation/v1/query/generated/query-0.json"))
        .expect("remove generated test fixture");
    query::write_generated_query(&host, &our_build, &generated).expect("write callback query");
    hooks::run_hook(&host, &our_build, Hook::PostGenerate).expect("run hook");

    let our_index = read_json(&host, &observed).expect("our callback read index");
    assert!(object(&our_index).keys().eq(object(&oracle_index).keys()));
    for key in ["hook", "version"] {
        assert_eq!(field(&our_index, key), field(&oracle_index, key));
    }
    let load_trace = |data: &Path, index: &Value| {
        read_json(
            &host,
            &data.join(path::to_native(text(field(index, "trace")))),
        )
        .expect("load trace")
    };
    let oracle_trace = load_trace(&oracle_data, &oracle_index);
    let our_trace = load_trace(&our_data, &our_index);
    let oracle_event = array(&oracle_trace).first().expect("one oracle event");
    let events = array(&our_trace);
    assert_eq!(events.len(), 1);
    let event = events.first().expect("one event");
    assert!(object(event).keys().eq(object(oracle_event).keys()));
    assert_eq!(*field(event, "name"), Value::string("compile: main.c"));
    assert_eq!(*field(event, "args"), our_snippet);
}
