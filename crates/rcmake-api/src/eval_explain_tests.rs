use std::path::Path;
use std::sync::Arc;

use rcmake_base::BString;
use rcmake_host::PureHost;

use super::*;
use crate::{ConfigureRequest, run_configure};

fn configure(body: &str) -> ConfigureResult {
    let root = Path::new(if cfg!(windows) { "C:/" } else { "/" });
    let host = Arc::new(PureHost::new(root.to_owned(), root.join("bin/rcmake")));
    let script =
        format!("cmake_minimum_required(VERSION 4.5)\nproject(Eval LANGUAGES NONE)\n{body}\n");
    host.seed_file(&root.join("src/CMakeLists.txt"), script.as_bytes())
        .expect("seed");
    run_configure(ConfigureRequest::new("src", "build", "cmake", host)).expect("configure")
}

fn value(report: &EvalReport) -> Vec<Result<BString, BString>> {
    report
        .results
        .iter()
        .map(|result| result.value.clone())
        .collect()
}

#[test]
fn property_owner_and_consumer_are_separate() {
    let result = configure(
        "add_library(liba INTERFACE)\nadd_library(app INTERFACE)\nset_property(TARGET liba PROPERTY INTERFACE_COMPILE_DEFINITIONS \"$<$<STREQUAL:$<TARGET_PROPERTY:NAME>,app>:FOR_APP>\")",
    );
    let request = EvalRequest {
        owner: Some("liba".into()),
        property: Some("INTERFACE_COMPILE_DEFINITIONS".into()),
        consumer: Some("app".into()),
        step_limit: 64,
        ..EvalRequest::default()
    };
    let report = result.evaluate_genex(&request).expect("report");
    assert_eq!(value(&report), [Ok(BString::from("FOR_APP"))]);
    assert!(report.success(), "{report:?}");
    assert!(report.results[0].head_sensitive);

    let own = EvalRequest {
        consumer: None,
        ..request
    };
    let report = result.evaluate_genex(&own).expect("report");
    assert_eq!(value(&report), [Ok(BString::default())]);
    assert_eq!(report.consumer, Some(BString::from("liba")));
}

#[test]
fn skipped_literal_and_expression_operands_are_recorded() {
    let result = configure("set(CMAKE_BUILD_TYPE Debug)");
    let request = EvalRequest {
        expression: Some("$<IF:$<CONFIG:Debug>,dbg,$<UPPER_CASE:rel>>$<OR:1,lit>".into()),
        configs: vec!["Debug".into()],
        step_limit: 64,
        ..EvalRequest::default()
    };
    let report = result.evaluate_genex(&request).expect("report");
    assert_eq!(value(&report), [Ok(BString::from("dbg1"))]);
    let skipped: Vec<_> = report.results[0]
        .skipped
        .iter()
        .map(|operand| (operand.index, operand.literal))
        .collect();
    assert_eq!(skipped, [(2, false), (1, true)]);
    let json = render_eval_json(&report);
    let parsed = rcmake_json::parse(&json, &rcmake_json::ReadOptions::default()).expect("json");
    assert_eq!(
        parsed.get("schema").and_then(rcmake_json::Value::as_str),
        Some(EXPLAIN_EVAL_SCHEMA)
    );
    assert!(render_eval_terminal(&report, true, false).contains("not evaluated"));
}

#[test]
fn cross_config_operators_without_evidence_are_unavailable() {
    let result = configure("");
    let request = EvalRequest {
        expression: Some("$<COMMAND_CONFIG:x>".into()),
        step_limit: 64,
        ..EvalRequest::default()
    };
    let report = result.evaluate_genex(&request).expect("report");
    assert_eq!(
        report.results[0].unavailable,
        [BString::from("COMMAND_CONFIG")]
    );
    assert!(!report.success());
    let hypothetical = EvalRequest {
        command_config: Some("Release".into()),
        ..request
    };
    assert!(
        result.evaluate_genex(&hypothetical).is_err(),
        "a command configuration without a cross-configuration graph has no context"
    );
}

#[test]
fn unknown_inputs_are_errors() {
    let result = configure("set(CMAKE_BUILD_TYPE Debug)");
    let unknown = EvalRequest {
        expression: Some("x".into()),
        consumer: Some("missing".into()),
        ..EvalRequest::default()
    };
    assert!(result.evaluate_genex(&unknown).is_err());
    let config = EvalRequest {
        expression: Some("$<CONFIG>".into()),
        configs: vec!["Release".into()],
        ..EvalRequest::default()
    };
    assert!(
        result.evaluate_genex(&config).is_err(),
        "a Debug-only tree has no Release configuration"
    );
    let debug = EvalRequest {
        configs: vec!["debug".into()],
        ..config
    };
    let report = result.evaluate_genex(&debug).expect("report");
    assert_eq!(value(&report), [Ok(BString::from("Debug"))]);
}

#[test]
fn same_named_imported_targets_resolve_in_one_directory() {
    let result = {
        let host_root = Path::new(if cfg!(windows) { "C:/" } else { "/" });
        let host = Arc::new(PureHost::new(
            host_root.to_owned(),
            host_root.join("bin/rcmake"),
        ));
        host.seed_file(
            &host_root.join("src/CMakeLists.txt"),
            b"cmake_minimum_required(VERSION 4.5)\nproject(Eval LANGUAGES NONE)\nadd_subdirectory(a)\nadd_subdirectory(b)\n",
        )
        .expect("seed");
        for (dir, value) in [("a", "FROM_A"), ("b", "FROM_B")] {
            host.seed_file(
                &host_root.join(format!("src/{dir}/CMakeLists.txt")),
                format!(
                    "add_library(dep INTERFACE IMPORTED)\nset_property(TARGET dep PROPERTY INTERFACE_COMPILE_DEFINITIONS {value})\nset_property(TARGET dep PROPERTY TAG {dir})\n"
                )
                .as_bytes(),
            )
            .expect("seed");
        }
        run_configure(ConfigureRequest::new("src", "build", "cmake", host)).expect("configure")
    };
    let ambiguous = EvalRequest {
        owner: Some("dep".into()),
        property: Some("INTERFACE_COMPILE_DEFINITIONS".into()),
        step_limit: 64,
        ..EvalRequest::default()
    };
    assert!(result.evaluate_genex(&ambiguous).is_err());
    let in_b = EvalRequest {
        expression: None,
        directory: Some("b".into()),
        ..ambiguous
    };
    let report = result.evaluate_genex(&in_b).expect("report");
    assert_eq!(value(&report), [Ok(BString::from("FROM_B"))]);
    let context = EvalRequest {
        expression: Some("$<TARGET_PROPERTY:TAG>".into()),
        property: None,
        owner: None,
        consumer: Some("dep".into()),
        directory: Some("a".into()),
        step_limit: 64,
        ..EvalRequest::default()
    };
    let report = result.evaluate_genex(&context).expect("report");
    assert_eq!(value(&report), [Ok(BString::from("a"))]);
    assert_eq!(report.directory, BString::from("a"));
}
