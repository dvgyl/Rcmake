use std::collections::BTreeMap;

use serde_json::{Value, json};

use super::dispatch;

type TestResult = Result<(), Box<dyn std::error::Error>>;

fn call(request: Value) -> Result<Value, serde_json::Error> {
    serde_json::from_slice(&dispatch(request.to_string().as_bytes()))
}

fn example(id: &str) -> Result<BTreeMap<&str, String>, Box<dyn std::error::Error>> {
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
        files.insert(name, content.replace("\\${", "${"));
        remaining = rest.trim_start().trim_start_matches(',').trim_start();
    }
    Ok(files)
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
        let response = call(json!({ "op": "configure", "files": example(id)? }))?;
        assert_eq!(response["ok"], true, "{id}: {}", response["diagnostics"]);
        let targets = response["targets"]
            .as_array()
            .ok_or("Targets are missing.")?;
        assert!(
            targets.iter().any(|item| item["name"] == target),
            "{id}: {targets:?}"
        );
        let files = response["files"]
            .as_object()
            .ok_or("Build files are missing.")?;
        assert!(files.contains_key("build.ninja"), "{id}");
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
    let response = call(json!({
        "op": "configure",
        "files": example("presets-v13")?,
        "preset": "ci-release",
        "generator": "Ninja",
        "buildType": "Release",
        "defines": { "DEMO_ENABLE_BENCH": "OFF" },
    }))?;
    assert_eq!(response["ok"], true, "{}", response["diagnostics"]);
    let ninja = response["files"]["build.ninja"]
        .as_str()
        .ok_or("Ninja file is missing.")?;
    assert!(ninja.contains("runner") && ninja.contains("-O3") && !ninja.contains("bench"));
    assert!(response["files"]["build-Release.ninja"].is_null());
    assert!(response["files"]["compile_commands.json"].is_string());
    Ok(())
}

#[test]
fn configure_custom_project_and_explain_provenance() -> TestResult {
    let files = json!({
        "CMakeLists.txt": r#"
cmake_minimum_required(VERSION 4.5)
project(Custom LANGUAGES CXX)
set(TOKEN before)
set(TOKEN after)
add_library(settings INTERFACE)
target_compile_definitions(settings INTERFACE FOO=${EXTRA})
add_executable(my_custom_tool foo.cpp)
target_link_libraries(my_custom_tool PRIVATE settings)
"#
    });
    let defines = json!({ "EXTRA": "42" });
    let response = call(json!({ "op": "configure", "files": files, "defines": defines }))?;
    assert_eq!(response["ok"], true, "{}", response["diagnostics"]);
    let ninja = response["files"]["build.ninja"]
        .as_str()
        .ok_or("Ninja file is missing.")?;
    for part in ["my_custom_tool", "FOO=42", "/src/foo.cpp"] {
        assert!(ninja.contains(part), "{ninja}");
    }

    let provenance = call(json!({
        "op": "explain", "files": files, "defines": defines, "symbol": "my_custom_tool", "config": "Debug",
    }))?;
    assert_eq!(provenance["schema"], "rcmake-explain-v1");
    assert_eq!(provenance["kind"], "target");
    let proof = provenance.to_string();
    assert!(
        proof.contains("FOO=42") && proof.contains("\"chain\":[{"),
        "{proof}"
    );

    let history =
        call(json!({ "op": "explain", "files": files, "defines": defines, "symbol": "TOKEN" }))?;
    assert_eq!(history["kind"], "variable");
    let proof = history.to_string();
    assert!(
        proof.contains("before") && proof.contains("after"),
        "{proof}"
    );
    Ok(())
}

#[test]
fn return_syntax_diagnostic_with_location() -> TestResult {
    let response = call(json!({
        "op": "configure",
        "files": { "CMakeLists.txt": "cmake_minimum_required(VERSION 4.5)\nadd_executable(broken missing.cpp\n" },
    }))?;
    assert_eq!(response["ok"], false);
    let diagnostic = &response["diagnostics"][0];
    assert_eq!(diagnostic["severity"], "error");
    assert_eq!(diagnostic["file"], "CMakeLists.txt");
    assert!(diagnostic["line"].as_u64().is_some_and(|line| line >= 2));
    assert!(
        diagnostic["message"]
            .as_str()
            .is_some_and(|message| message.contains("Parse error"))
    );
    assert_eq!(response["files"], json!({}));
    Ok(())
}

#[test]
fn reject_paths_outside_the_project() -> TestResult {
    let response =
        call(json!({ "op": "configure", "files": { "../CMakeLists.txt": "project(Test)" } }))?;
    assert_eq!(response["ok"], false);
    assert_eq!(response["diagnostics"][0]["category"], "INPUT");
    Ok(())
}
