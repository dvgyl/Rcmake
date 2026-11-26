use std::collections::BTreeMap;

use rcmake_json::{JsonError, ReadOptions, ToJson, Value, parse, write_compact};

use super::dispatch;

type TestResult = Result<(), Box<dyn std::error::Error>>;

const STRICT_JSON: ReadOptions = ReadOptions {
    allow_comments: false,
    dup_keys_last_wins: true,
};

fn call(request: Value) -> Result<Value, JsonError> {
    parse(&dispatch(write_compact(&request).as_slice()), &STRICT_JSON)
}

fn call_raw(input: &[u8]) -> Result<Value, JsonError> {
    parse(&dispatch(input), &STRICT_JSON)
}

fn example(id: &str) -> Result<BTreeMap<String, String>, Box<dyn std::error::Error>> {
    let adapter = include_str!("../../../site/wasm-adapter.js");
    let (_, definition) = adapter
        .split_once(&format!("  \"{id}\": {{"))
        .ok_or("Example is missing.")?;
    let (_, definition) = definition
        .split_once("files: {")
        .ok_or("Example files are missing.")?;
    let mut files = BTreeMap::new();
    let mut remaining = definition.trim_start();
    while let Some(file) = remaining.strip_prefix('"') {
        let (name, content) = file
            .split_once("\": `")
            .ok_or("Example filename is missing.")?;
        let (content, rest) = content
            .split_once('`')
            .ok_or("Example contents are missing.")?;
        files.insert(name.to_owned(), content.replace("\\${", "${"));
        remaining = rest.trim_start().trim_start_matches(',').trim_start();
    }
    Ok(files)
}

fn diag_text(response: &Value) -> String {
    response
        .get("diagnostics")
        .map(|value| String::from_utf8_lossy(write_compact(value).as_slice()).into_owned())
        .unwrap_or_default()
}

#[test]
fn configure_playground_examples() -> TestResult {
    for (id, target, expected) in [
        ("hello-cpp20", "hello", "HELLO_TRACE=1"),
        ("library-export", "demo_cli", "GEOM_API_VERSION=2"),
        ("genex", "codec_app", "CODEC_ASSERTIONS=1"),
        ("presets-v13", "bench", "runner"),
        ("policy-block", "scope_check", "propagated-from-block"),
    ] {
        let response = call(Value::object_from([
            ("op", Value::string("configure")),
            ("files", example(id)?.to_json()),
        ]))?;
        assert_eq!(
            response.get("ok").and_then(Value::as_bool),
            Some(true),
            "{id}: {}",
            diag_text(&response)
        );
        let targets = response
            .get("targets")
            .and_then(Value::as_array)
            .ok_or("Targets are missing.")?;
        assert!(
            targets
                .iter()
                .any(|item| item.get("name").and_then(Value::as_str) == Some(target)),
            "{id}: {targets:?}"
        );
        let files = response
            .get("files")
            .and_then(Value::as_object)
            .ok_or("Build files are missing.")?;
        assert!(files.contains_key(b"build.ninja".as_slice()), "{id}");
        assert!(
            files
                .values()
                .filter_map(Value::as_str)
                .any(|file| file.contains(expected)),
            "{id}: {files:?}"
        );
    }
    Ok(())
}

#[test]
fn apply_preset_and_explicit_overrides() -> TestResult {
    let response = call(Value::object_from([
        ("op", Value::string("configure")),
        ("files", example("presets-v13")?.to_json()),
        ("preset", Value::string("ci-release")),
        ("generator", Value::string("Ninja")),
        ("buildType", Value::string("Release")),
        (
            "defines",
            Value::object_from([("DEMO_ENABLE_BENCH", Value::string("OFF"))]),
        ),
    ]))?;
    assert_eq!(
        response.get("ok").and_then(Value::as_bool),
        Some(true),
        "{}",
        diag_text(&response)
    );
    let files = response.get("files").ok_or("Build files are missing.")?;
    let ninja = files
        .get("build.ninja")
        .and_then(Value::as_str)
        .ok_or("Ninja file is missing.")?;
    assert!(ninja.contains("runner") && ninja.contains("-O3") && !ninja.contains("bench"));
    assert!(files.get("build-Release.ninja").is_none());
    assert!(
        files
            .get("compile_commands.json")
            .and_then(Value::as_str)
            .is_some()
    );
    Ok(())
}

#[test]
fn configure_custom_project_and_explain_provenance() -> TestResult {
    let files = Value::object_from([(
        "CMakeLists.txt",
        Value::string(
            r#"
cmake_minimum_required(VERSION 4.5)
project(Custom LANGUAGES CXX)
set(TOKEN before)
set(TOKEN after)
add_library(settings INTERFACE)
target_compile_definitions(settings INTERFACE FOO=${EXTRA})
add_executable(my_custom_tool foo.cpp)
target_link_libraries(my_custom_tool PRIVATE settings)
"#,
        ),
    )]);
    let defines = Value::object_from([("EXTRA", Value::string("42"))]);
    let response = call(Value::object_from([
        ("op", Value::string("configure")),
        ("files", files.clone()),
        ("defines", defines.clone()),
    ]))?;
    assert_eq!(
        response.get("ok").and_then(Value::as_bool),
        Some(true),
        "{}",
        diag_text(&response)
    );
    let ninja = response
        .get("files")
        .and_then(|files| files.get("build.ninja"))
        .and_then(Value::as_str)
        .ok_or("Ninja file is missing.")?;
    for part in ["my_custom_tool", "FOO=42", "/src/foo.cpp"] {
        assert!(ninja.contains(part), "{ninja}");
    }

    let provenance = call(Value::object_from([
        ("op", Value::string("explain")),
        ("files", files.clone()),
        ("defines", defines.clone()),
        ("symbol", Value::string("my_custom_tool")),
        ("config", Value::string("Debug")),
    ]))?;
    assert_eq!(
        provenance.get("schema").and_then(Value::as_str),
        Some("rcmake-explain-v1")
    );
    assert_eq!(
        provenance.get("kind").and_then(Value::as_str),
        Some("target")
    );
    let proof = String::from_utf8_lossy(write_compact(&provenance).as_slice()).into_owned();
    assert!(
        proof.contains("FOO=42") && proof.contains("\"chain\":[{"),
        "{proof}"
    );

    let history = call(Value::object_from([
        ("op", Value::string("explain")),
        ("files", files),
        ("defines", defines),
        ("symbol", Value::string("TOKEN")),
    ]))?;
    assert_eq!(
        history.get("kind").and_then(Value::as_str),
        Some("variable")
    );
    let proof = String::from_utf8_lossy(write_compact(&history).as_slice()).into_owned();
    assert!(
        proof.contains("before") && proof.contains("after"),
        "{proof}"
    );
    Ok(())
}

#[test]
fn return_syntax_diagnostic_with_location() -> TestResult {
    let response = call(Value::object_from([
        ("op", Value::string("configure")),
        (
            "files",
            Value::object_from([(
                "CMakeLists.txt",
                Value::string(
                    "cmake_minimum_required(VERSION 4.5)\nadd_executable(broken missing.cpp\n",
                ),
            )]),
        ),
    ]))?;
    assert_eq!(response.get("ok").and_then(Value::as_bool), Some(false));
    let diagnostic = response
        .get("diagnostics")
        .and_then(Value::as_array)
        .and_then(|items| items.first())
        .ok_or("Diagnostic is missing.")?;
    assert_eq!(
        diagnostic.get("severity").and_then(Value::as_str),
        Some("error")
    );
    assert_eq!(
        diagnostic.get("file").and_then(Value::as_str),
        Some("CMakeLists.txt")
    );
    assert!(
        diagnostic
            .get("line")
            .and_then(Value::as_u64)
            .is_some_and(|line| line >= 2)
    );
    assert!(
        diagnostic
            .get("message")
            .and_then(Value::as_str)
            .is_some_and(|message| message.contains("Parse error"))
    );
    assert_eq!(response.get("files"), Some(&Value::object()));
    Ok(())
}

#[test]
fn reject_paths_outside_the_project() -> TestResult {
    let response = call(Value::object_from([
        ("op", Value::string("configure")),
        (
            "files",
            Value::object_from([("../CMakeLists.txt", Value::string("project(Test)"))]),
        ),
    ]))?;
    assert_eq!(response.get("ok").and_then(Value::as_bool), Some(false));
    let category = response
        .get("diagnostics")
        .and_then(Value::as_array)
        .and_then(|items| items.first())
        .and_then(|item| item.get("category"))
        .and_then(Value::as_str);
    assert_eq!(category, Some("INPUT"));
    Ok(())
}

#[test]
fn reject_comments_and_non_utf8_and_keep_last_duplicate_key() -> TestResult {
    let commented = call_raw(b"/* comment */ {\"op\":\"configure\",\"files\":{}}")?;
    assert_eq!(commented.get("ok").and_then(Value::as_bool), Some(false));
    assert_eq!(
        commented
            .get("diagnostics")
            .and_then(Value::as_array)
            .and_then(|items| items.first())
            .and_then(|item| item.get("category"))
            .and_then(Value::as_str),
        Some("INPUT")
    );

    let non_utf8 = call_raw(b"{\"op\":\"configure\",\"files\":{\"CMakeLists.txt\":\"\xff\"}}")?;
    assert_eq!(non_utf8.get("ok").and_then(Value::as_bool), Some(false));
    assert_eq!(
        non_utf8
            .get("diagnostics")
            .and_then(Value::as_array)
            .and_then(|items| items.first())
            .and_then(|item| item.get("category"))
            .and_then(Value::as_str),
        Some("INPUT")
    );

    let dup_key = call_raw(
        br#"{"op":"unknown","op":"configure","files":{"CMakeLists.txt":"cmake_minimum_required(VERSION 4.5)\nproject(Dup NONE)\nadd_custom_target(ok)\n"}}"#,
    )?;
    assert_eq!(
        dup_key.get("ok").and_then(Value::as_bool),
        Some(true),
        "{}",
        diag_text(&dup_key)
    );
    Ok(())
}

#[test]
fn reject_non_utf8_in_every_string_field() -> TestResult {
    for (op, field) in [
        ("configure", "buildType"),
        ("configure", "generator"),
        ("configure", "preset"),
        ("explain", "symbol"),
        ("explain", "config"),
    ] {
        let mut input = format!(r#"{{"op":"{op}","files":{{}},"{field}":"bad-"#).into_bytes();
        let position = input.len();
        input.extend_from_slice(b"\xff\"}");
        let response = call_raw(&input)?;
        assert_eq!(
            response.get("ok").and_then(Value::as_bool),
            Some(false),
            "{field}"
        );
        let diagnostic = response
            .get("diagnostics")
            .and_then(Value::as_array)
            .and_then(|items| items.first())
            .ok_or("Diagnostic is missing.")?;
        assert_eq!(
            diagnostic.get("category").and_then(Value::as_str),
            Some("INPUT"),
            "{field}"
        );
        assert_eq!(
            diagnostic.get("message").and_then(Value::as_str),
            Some(format!("The request is not valid UTF-8 at byte {position}.").as_str()),
            "{field}"
        );
    }
    Ok(())
}

#[test]
fn configure_nested_file_with_posix_preset_paths() -> TestResult {
    let response = call(Value::object_from([
        ("op", Value::string("configure")),
        ("preset", Value::string("virtual")),
        (
            "files",
            Value::object_from([
                (
                    "CMakeLists.txt",
                    Value::string(
                        "cmake_minimum_required(VERSION 4.5)\nproject(Virtual NONE)\nadd_subdirectory(nested)",
                    ),
                ),
                (
                    "nested/CMakeLists.txt",
                    Value::string("add_custom_target(virtual)"),
                ),
                (
                    "CMakePresets.json",
                    Value::string(
                        r#"{"version": 3, "configurePresets": [
                {"name": "virtual", "generator": "Ninja", "binaryDir": "/output/build"}
            ]}"#,
                    ),
                ),
            ]),
        ),
    ]))?;
    assert_eq!(
        response.get("ok").and_then(Value::as_bool),
        Some(true),
        "{}",
        diag_text(&response)
    );
    assert!(
        response
            .get("files")
            .and_then(|files| files.get("build.ninja"))
            .and_then(Value::as_str)
            .is_some()
    );
    assert_eq!(
        response
            .get("targets")
            .and_then(Value::as_array)
            .and_then(|items| items.first())
            .and_then(|item| item.get("name"))
            .and_then(Value::as_str),
        Some("virtual")
    );
    Ok(())
}
