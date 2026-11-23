#![allow(clippy::expect_used)]

use std::path::{Path, PathBuf};
use std::time::Duration;

use rcmake_base::BString;
use rcmake_host::{Host, Output, ProcessResult, PureHost, RunOutput, Stream, path};
use rcmake_json::Value;

use crate::files::{json_files, read_json};
use crate::{
    CustomContent, CustomContentType, DATA_VERSION, Hook, InstrumentError, InstrumentOption,
    Object, hooks, launcher, query, snippet,
};

#[cfg(unix)]
mod oracle;

fn host() -> PureHost {
    PureHost::new(PathBuf::from("/work"), PathBuf::from("/bin/rcmake"))
}

fn object(value: &Value) -> &Object {
    let Value::Object(fields) = value else {
        panic!("Expected a JSON object. Found {value:?}.");
    };
    fields
}

fn field<'a>(value: &'a Value, name: &str) -> &'a Value {
    object(value).get(name.as_bytes()).expect(name)
}

fn array(value: &Value) -> &[Value] {
    let Value::Array(items) = value else {
        panic!("Expected a JSON array. Found {value:?}.");
    };
    items
}

fn text(value: &Value) -> &BString {
    let Value::String(value) = value else {
        panic!("Expected a JSON string. Found {value:?}.");
    };
    value
}

fn only_file(host: &dyn Host, dir: &Path) -> PathBuf {
    let files = json_files(host, dir).expect("list JSON files");
    let [file] = files.as_slice() else {
        panic!("Expected one JSON file. Found {files:?}.");
    };
    dir.join(path::to_native(file))
}

fn text_list(values: &[&str]) -> Value {
    crate::strings(values.iter().copied())
}

#[test]
fn query_versions_and_keywords() {
    for hook in Hook::ALL {
        assert_eq!(Hook::from_name(hook.as_str().as_bytes()), Some(hook));
    }
    for option in InstrumentOption::ALL {
        assert_eq!(
            InstrumentOption::from_name(option.as_str().as_bytes()),
            Some(option)
        );
    }
    assert_eq!(
        query::parse_query(br#"{"version":1}"#)
            .expect("legacy version")
            .version
            .minor,
        0
    );
    assert_eq!(
        query::parse_query(br#"{"version":{"major":1}}"#)
            .expect("default minor")
            .version
            .minor,
        0
    );
    assert_eq!(
        query::parse_query(br#"{"version":{"major":1,"minor":2}}"#)
            .expect("version 1.2")
            .version,
        DATA_VERSION
    );
    for query in [
        br#"{"version":3,"hooks":false}"#.as_slice(),
        br#"{"version":{"major":1,"minor":3}}"#,
    ] {
        assert_eq!(
            query::parse_query(query),
            Err(InstrumentError::UnsupportedVersion)
        );
    }
    for query in [
        br#"{"version":1.2}"#.as_slice(),
        br#"{"version":1,"hooks":["unknown"]}"#,
        br#"{"version":1,"options":false}"#,
        br#"{"version":1,"callbacks":[1]}"#,
    ] {
        assert!(matches!(
            query::parse_query(query),
            Err(InstrumentError::Invalid(_))
        ));
    }
}

#[test]
fn reads_user_project_and_generated_queries() {
    let host = host();
    let build = Path::new("/build");
    host.env_set(b"CMAKE_CONFIG_DIR", Some(b"/config"));
    for (file, json) in [
        (
            "/config/instrumentation/v1/query/user.json",
            r#"{"version":1,"hooks":["manual"]}"#,
        ),
        (
            "/config/cmake/instrumentation/v1/query/user.json",
            r#"{"version":1,"hooks":["prepareForCDash"]}"#,
        ),
        (
            "/build/.cmake/instrumentation/v1/query/00-ignore.json",
            r#"{"version":99,"options":["unknown"]}"#,
        ),
        (
            "/build/.cmake/instrumentation/v1/query/project.json",
            r#"{"version":1,"hooks":["postCTest"]}"#,
        ),
        (
            "/build/.cmake/instrumentation/v1/query/query.txt",
            "invalid",
        ),
    ] {
        host.seed_file(Path::new(file), json.as_bytes())
            .expect("seed query");
    }
    let generated = query::parse_query(br#"{"version":{"major":1,"minor":2},"hooks":["preBuild"],"options":["trace","captureOutput","trace"]}"#).expect("generated query");
    for _ in 0..2 {
        query::write_generated_query(&host, build, &generated).expect("write query");
    }
    let generated_dir = build.join(".cmake/instrumentation/v1/query/generated");
    assert_eq!(
        json_files(&host, &generated_dir).expect("generated files"),
        [BString::from("query-0.json"), "query-1.json".into()]
    );
    let queries = query::read_queries(&host, build).expect("read queries");
    assert_eq!(
        queries
            .iter()
            .flat_map(|query| query.hooks.iter().copied())
            .collect::<Vec<_>>(),
        [
            Hook::Manual,
            Hook::PostCTest,
            Hook::PreBuild,
            Hook::PreBuild
        ]
    );
    assert_eq!(
        queries.last().expect("last query").options,
        [InstrumentOption::CaptureOutput, InstrumentOption::Trace]
    );
    query::clear_generated_queries(&host, build).expect("clear previous configure queries");
    query::clear_generated_queries(&host, build).expect("clear absent queries");
    query::write_generated_query(&host, build, &generated).expect("write query after reset");
    assert_eq!(
        json_files(&host, &generated_dir).expect("reset query numbers"),
        [BString::from("query-0.json")]
    );
}

#[test]
fn query_errors_show_the_source_value_and_file() {
    let host = host();
    let build = Path::new("/build");
    let query = build.join(".cmake/instrumentation/v1/query");
    host.seed_file(
        &query.join("invalid.json"),
        b"{\n  \"version\": 1,\n  \"options\": [\"trace\", \"bad option\"]\n}\n",
    )
    .expect("write invalid query");
    let error = query::read_queries(&host, build).expect_err("reject invalid query");
    assert_eq!(
        error.to_string(),
        concat!(
            "Could not load instrumentation queries from /build/.cmake/instrumentation/v1/query:\n",
            "invalid.json:3: Not a valid option: \"bad option\"\n",
            "  \"options\": [\"trace\", \"bad option\"]\n",
            "                       ^",
        ),
    );
}

#[test]
fn manual_index_preserves_data_covered_by_an_existing_index() {
    let host = host();
    let build = Path::new("/build");
    let api = build.join(".cmake/instrumentation/v1");
    let data = api.join("data");
    host.seed_file(
        &api.join("query/trace.json"),
        br#"{"version":1,"options":["trace"]}"#,
    )
    .expect("write trace query");
    for _ in 0..2 {
        snippet::write_content(&host, build, b"example", &[], &[], false).expect("write content");
    }
    let run = || {
        launcher::run_instrumented(&host, build, crate::SnippetRole::Custom, &["work".into()])
            .expect("record command");
    };
    host.set_command_handler(|_, _, _| {
        Ok(RunOutput {
            results: vec![ProcessResult::Exited(0)],
            ..RunOutput::default()
        })
    })
    .expect("set command handler");
    run();
    host.sleep(Duration::from_secs(1));
    let marker = data.join("index/index-existing.json");
    host.seed_file(&marker, b"").expect("write existing index");
    host.sleep(Duration::from_secs(1));
    run();
    hooks::run_hook(&host, build, Hook::Manual).expect("index the newer snippet");
    assert_eq!(json_files(&host, &data).expect("list snippets").len(), 1);
    assert_eq!(
        json_files(&host, &data.join("content"))
            .expect("list content")
            .len(),
        2
    );
    host.remove_file(&marker).expect("remove existing index");
    hooks::run_hook(&host, build, Hook::Manual).expect("index the older snippet");
    assert!(json_files(&host, &data).expect("list snippets").is_empty());
    only_file(&host, &data.join("content"));
    only_file(&host, &data.join("trace"));
}

#[test]
fn writes_custom_content_and_system_information() {
    let host = host();
    let entries = [
        ("text", CustomContentType::String, "hello"),
        ("flag", CustomContentType::Bool, "OFF"),
        ("list", CustomContentType::List, "a;;b"),
        ("object", CustomContentType::Json, r#"{"ok":true}"#),
    ]
    .map(|(name, ty, content)| CustomContent {
        name: name.into(),
        ty,
        content: content.into(),
    });
    let file = snippet::write_content(&host, Path::new("/build"), b"example", &entries, &[], true)
        .expect("write content");
    let content = read_json(&host, &file).expect("read content");
    let custom = field(&content, "custom");
    assert_eq!(*field(custom, "text"), Value::string("hello"));
    assert_eq!(*field(custom, "flag"), Value::Bool(false));
    assert_eq!(*field(custom, "list"), text_list(&["a", "b"]));
    assert_eq!(*field(field(custom, "object"), "ok"), Value::Bool(true));
    assert_eq!(
        *field(field(&content, "staticSystemInformation"), "OSName"),
        Value::string("PureOS")
    );
    assert!(
        snippet::write_content(
            &host,
            Path::new("/build"),
            b"example",
            &[CustomContent {
                name: "invalid".into(),
                ty: CustomContentType::Json,
                content: "{".into(),
            }],
            &[],
            false
        )
        .is_err()
    );
}

#[test]
fn launcher_records_metadata_and_hook_dispatches_callbacks_once_per_batch() {
    let host = host();
    let build = Path::new("/build");
    let api = build.join(".cmake/instrumentation/v1");
    let data = api.join("data");
    host.seed_file(&api.join("query/one.json"), br#"{"version":1,"hooks":["postBuild"],"options":["processMetrics","dynamicSystemInformation","captureOutput","compileTrace","staticSystemInformation","trace"]}"#).expect("query");
    host.seed_file(
        &api.join("query/two.json"),
        br#"{"version":1,"callbacks":["observe \"two words\" ''","observe next"]}"#,
    )
    .expect("query");
    host.seed_file(
        Path::new("/proc/meminfo"),
        b"MemTotal: 1000 kB\nMemAvailable: 600 kB\n",
    )
    .expect("meminfo");
    host.seed_file(Path::new("/proc/loadavg"), b"0.25 0.42 0.88 1/10 1\n")
        .expect("loadavg");
    host.seed_file(Path::new("/work/outputs.txt"), b"main.o,missing.o")
        .expect("write output list");
    snippet::write_content(&host, build, b"example", &[], &[], false).expect("content");
    host.set_command_handler(|host, request, output| {
        let stage = request.pipeline.first().expect("one stage");
        assert_eq!(request.cwd, Path::new("/work"));
        if stage.program.as_slice() == b"cc" {
            assert_eq!(request.stdout, Output::Capture);
            host.sleep(Duration::from_millis(25));
            host.seed_file(Path::new("/build/main.o"), b"object")?;
            host.seed_file(Path::new("/work/main.json"), br#"{"traceEvents":[]}"#)?;
            output(Stream::Stdout, b"out\n");
            return Ok(RunOutput {
                results: vec![ProcessResult::Exited(37)],
                stdout: b"out\n".to_vec(),
                stderr: b"err\n".to_vec(),
            });
        }
        assert_eq!(stage.program, "observe");
        let (index, args) = stage.args.split_last().expect("callback arguments");
        assert!(
            args == [BString::from("next")]
                || args == [BString::from("two words"), BString::default()]
        );
        let index = read_json(host, &path::to_native(index)).expect("callback can read index");
        assert_eq!(*field(&index, "hook"), Value::string("postBuild"));
        let data = path::to_native(text(field(&index, "dataDir")));
        let items = array(field(&index, "snippets"));
        let trace = read_json(
            host,
            &data.join(path::to_native(text(field(&index, "trace")))),
        )
        .expect("trace");
        let events = array(&trace);
        assert_eq!(events.len(), items.len());
        for (item, event) in items.iter().zip(events) {
            let value =
                read_json(host, &data.join(path::to_native(text(item)))).expect("indexed snippet");
            assert_eq!(*field(event, "args"), value);
        }
        host.write_out(
            Stream::Stdout,
            format!("callback {}\n", items.len()).as_bytes(),
        );
        Ok(RunOutput {
            results: vec![ProcessResult::Exited(3)],
            ..RunOutput::default()
        })
    })
    .expect("install handler");

    let argv = [
        "--instrument",
        "--command-type",
        "compile",
        "--target-name",
        "example",
        "--config",
        "Debug",
        "--build-dir",
        "/build",
        "--output-as-file-name",
        "outputs.txt",
        "--source",
        "main.c",
        "--language",
        "C",
        "--",
        "cc",
        "-ftime-trace=main.json",
        "main.c",
    ]
    .map(BString::from);
    assert_eq!(
        launcher::run_launcher(&host, &argv).expect("run launcher"),
        37
    );
    let value = read_json(&host, &only_file(&host, &data)).expect("read snippet");
    assert_eq!(*field(&value, "stdout"), Value::string("out\n"));
    assert_eq!(*field(&value, "stderr"), Value::string("err\n"));
    assert_eq!(*field(&value, "duration"), Value::Int(25));
    assert_eq!(
        *field(&value, "outputSizes"),
        Value::Array(vec![Value::Int(6), Value::Int(0)])
    );
    assert_eq!(*field(&value, "config"), Value::string("Debug"));
    assert_eq!(*field(&value, "processMetrics"), Value::Null);
    assert_eq!(
        *field(
            field(&value, "dynamicSystemInformation"),
            "beforeHostMemoryUsed"
        ),
        Value::Int(400)
    );
    assert!(text(field(&value, "cmakeContent")).starts_with(b"content/cmake-"));
    assert!(text(field(&value, "traceFile")).starts_with(b"compile-trace/"));

    hooks::run_hook(&host, build, Hook::PreBuild).expect("unsubscribed hook");
    assert_eq!(host.stream_bytes(Stream::Stdout), "out\n");
    for _ in 0..2 {
        hooks::run_hook(&host, build, Hook::PostBuild).expect("subscribed hook");
    }
    assert_eq!(
        host.stream_bytes(Stream::Stdout),
        "out\ncallback 1\ncallback 1\ncallback 0\ncallback 0\n"
    );
    assert!(json_files(&host, &data).expect("list snippets").is_empty());
    assert!(
        json_files(&host, &data.join("index"))
            .expect("list indexes")
            .is_empty()
    );
}

#[test]
fn interrupted_command_records_signal() {
    let host = host();
    host.set_command_handler(|_, _, _| {
        Ok(RunOutput {
            results: vec![ProcessResult::Signaled(2)],
            ..RunOutput::default()
        })
    })
    .expect("install handler");
    let build = Path::new("/build");
    assert_eq!(
        launcher::run_instrumented(
            &host,
            build,
            crate::SnippetRole::CMakeBuild,
            &["build".into()]
        )
        .expect("run launcher"),
        130
    );
    let data = build.join(".cmake/instrumentation/v1/data");
    let value = read_json(&host, &only_file(&host, &data)).expect("read snippet");
    assert_eq!(*field(&value, "result"), Value::Null);
    assert_eq!(*field(&value, "interruptSignal"), Value::Int(2));
    assert!(!object(&value).contains_key(b"stdout".as_slice()));
}

#[test]
fn simultaneous_hooks_do_not_index_the_same_snippet() {
    use std::sync::{Mutex, mpsc};
    use std::thread;

    let host = host();
    let build = Path::new("/build");
    let api = build.join(".cmake/instrumentation/v1");
    host.seed_file(
        &api.join("query/request.json"),
        br#"{"version":1,"hooks":["preBuild","postBuild"],"callbacks":["observe"]}"#,
    )
    .expect("seed query");
    let (first_started, wait_for_first) = mpsc::channel();
    let (second_finished, wait_for_second) = mpsc::channel();
    let wait_for_second = Mutex::new(wait_for_second);
    host.set_command_handler(move |host, request, _| {
        let stage = request.pipeline.first().expect("one stage");
        if stage.program.as_slice() == b"observe" {
            let file = path::to_native(stage.args.last().expect("index argument"));
            let index = read_json(host, &file).expect("read index");
            let count = array(field(&index, "snippets")).len();
            host.write_out(Stream::Stdout, count.to_string().as_bytes());
            if text(field(&index, "hook")).as_slice() == b"preBuild" {
                first_started.send(()).expect("signal first callback");
                wait_for_second
                    .lock()
                    .expect("wait lock")
                    .recv_timeout(Duration::from_secs(5))
                    .expect("second callback");
            }
        }
        Ok(RunOutput {
            results: vec![ProcessResult::Exited(0)],
            ..RunOutput::default()
        })
    })
    .expect("install handler");

    launcher::run_instrumented(&host, build, crate::SnippetRole::Custom, &["work".into()])
        .expect("write snippet");
    thread::scope(|scope| {
        let pre = scope.spawn(|| hooks::run_hook(&host, build, Hook::PreBuild));
        wait_for_first
            .recv_timeout(Duration::from_secs(5))
            .expect("first callback");
        hooks::run_hook(&host, build, Hook::PostBuild).expect("post-build hook");
        second_finished
            .send(())
            .expect("signal second hook completion");
        pre.join()
            .expect("join pre-build hook")
            .expect("pre-build hook");
    });
    assert_eq!(host.stream_bytes(Stream::Stdout), "10");
}
