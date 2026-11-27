use std::path::Path;
use std::sync::Arc;

use rcmake_base::BString;
use rcmake_host::PureHost;
use rcmake_model::DirId;

use crate::{ConfigureRequest, ConfigureResult, DependencyEdge, run_configure};

fn configure(body: &str) -> ConfigureResult {
    let root = Path::new(if cfg!(windows) { "C:/" } else { "/" });
    let host = Arc::new(PureHost::new(root.to_owned(), root.join("bin/rcmake")));
    let script =
        format!("cmake_minimum_required(VERSION 4.5)\nproject(Deps LANGUAGES NONE)\n{body}\n");
    host.seed_file(&root.join("src/CMakeLists.txt"), script.as_bytes())
        .expect("seed");
    run_configure(ConfigureRequest::new("src", "build", "cmake", host)).expect("configure")
}

fn paths(result: &ConfigureResult, head: &str, goal: &str) -> Vec<Vec<(String, String)>> {
    let head = result
        .model
        .find_target(DirId::ROOT, head.as_bytes())
        .expect("head");
    let report = result.dependency_paths(DirId::ROOT, head, goal.as_bytes(), &[BString::default()]);
    assert!(report.complete(), "{report:?}");
    report
        .paths
        .iter()
        .map(|path| {
            path.hops
                .iter()
                .map(|hop| {
                    let edge = match hop.edge {
                        DependencyEdge::Link { link_only, .. } => {
                            if link_only {
                                "link-only"
                            } else {
                                "link"
                            }
                        }
                        DependencyEdge::OrderOnly => "order",
                    };
                    (hop.to.to_string(), edge.to_owned())
                })
                .collect()
        })
        .collect()
}

fn hop(to: &str, edge: &str) -> (String, String) {
    (to.to_owned(), edge.to_owned())
}

#[test]
fn diamond_keeps_both_paths_shortest_first() {
    let result = configure(
        "add_library(z INTERFACE)\nadd_library(a INTERFACE)\nadd_library(b INTERFACE)\nadd_library(c INTERFACE)\ntarget_link_libraries(a INTERFACE z)\ntarget_link_libraries(c INTERFACE z)\ntarget_link_libraries(b INTERFACE c)\nadd_library(top INTERFACE)\ntarget_link_libraries(top INTERFACE a b)",
    );
    assert_eq!(
        paths(&result, "top", "z"),
        [
            vec![hop("a", "link"), hop("z", "link")],
            vec![hop("b", "link"), hop("c", "link"), hop("z", "link")],
        ]
    );
}

#[test]
fn parallel_link_and_order_only_edges_stay_distinct() {
    let result = configure(
        "add_library(z INTERFACE)\nadd_library(top INTERFACE)\ntarget_link_libraries(top INTERFACE z)\nadd_dependencies(top z)",
    );
    assert_eq!(
        paths(&result, "top", "z"),
        [vec![hop("z", "link")], vec![hop("z", "order")]]
    );
}

#[test]
fn consecutive_order_only_edges_form_one_path() {
    let result = configure(
        "add_custom_target(gen2)\nadd_custom_target(gen1)\nadd_custom_target(app)\nadd_dependencies(gen1 gen2)\nadd_dependencies(app gen1)",
    );
    assert_eq!(
        paths(&result, "app", "gen2"),
        [vec![hop("gen1", "order"), hop("gen2", "order")]]
    );
}

#[test]
fn link_only_edges_are_marked_and_unknown_targets_get_suggestions() {
    let result = configure(
        "add_library(z INTERFACE)\nadd_library(top INTERFACE)\ntarget_link_libraries(top INTERFACE $<LINK_ONLY:z>)",
    );
    assert_eq!(paths(&result, "top", "z"), [vec![hop("z", "link-only")]]);
    let head = result.model.find_target(DirId::ROOT, b"top").expect("head");
    let report = result.dependency_paths(DirId::ROOT, head, b"topp", &[]);
    assert!(report.resolved.is_none());
    assert_eq!(report.suggestions, [BString::from("top")]);
}

#[test]
fn consumer_sensitive_interface_edges_use_the_consuming_target() {
    let result = configure(
        "add_library(z INTERFACE)\nadd_library(w INTERFACE)\nadd_library(iface INTERFACE)\ntarget_link_libraries(iface INTERFACE $<$<STREQUAL:$<TARGET_PROPERTY:NAME>,top>:z> $<$<STREQUAL:$<TARGET_PROPERTY:NAME>,iface>:w>)\nadd_library(left INTERFACE)\ntarget_link_libraries(left INTERFACE iface)\nadd_library(top INTERFACE)\ntarget_link_libraries(top INTERFACE left iface)",
    );
    assert_eq!(
        paths(&result, "top", "z"),
        [
            vec![hop("iface", "link"), hop("z", "link")],
            vec![hop("left", "link"), hop("iface", "link"), hop("z", "link")],
        ]
    );
    assert!(
        paths(&result, "top", "w").is_empty(),
        "an edge for another consumer is not a path of top"
    );
}

#[test]
fn parallel_matches_stop_at_the_path_limit() {
    let items = vec!["z"; 200].join(" ");
    let result = configure(&format!(
        "add_library(z INTERFACE)\nadd_library(top INTERFACE)\ntarget_link_libraries(top INTERFACE {items})"
    ));
    let head = result.model.find_target(DirId::ROOT, b"top").expect("head");
    let report = result.dependency_paths(DirId::ROOT, head, b"z", &[BString::default()]);
    assert!(report.truncated, "{report:?}");
    assert!(report.paths.len() <= 16);
    assert!(!report.complete());
}

#[test]
fn link_language_edges_use_the_consumer_link_language() {
    let result = configure(
        "add_library(backend INTERFACE)\nadd_library(cbackend INTERFACE)\nadd_library(top INTERFACE)\nset_property(TARGET top PROPERTY LINKER_LANGUAGE CXX)\ntarget_link_libraries(top INTERFACE $<$<LINK_LANGUAGE:CXX>:backend> $<$<LINK_LANGUAGE:C>:cbackend>)",
    );
    assert_eq!(
        paths(&result, "top", "backend"),
        [vec![hop("backend", "link")]]
    );
    assert!(
        paths(&result, "top", "cbackend").is_empty(),
        "a C-only edge is not a path of a CXX consumer"
    );
}

fn configure_multi(body: &str) -> ConfigureResult {
    let root = Path::new(if cfg!(windows) { "C:/" } else { "/" });
    let host = Arc::new(PureHost::new(root.to_owned(), root.join("bin/rcmake")));
    let script =
        format!("cmake_minimum_required(VERSION 4.5)\nproject(Deps LANGUAGES NONE)\n{body}\n");
    host.seed_file(&root.join("src/CMakeLists.txt"), script.as_bytes())
        .expect("seed");
    let mut request = ConfigureRequest::new("src", "build", "cmake", host);
    request.generator = Some(rcmake_model::GeneratorKind::NinjaMultiConfig);
    run_configure(request).expect("configure")
}

#[test]
fn dependency_output_and_status_follow_the_requested_configuration() {
    let result = configure_multi(
        "set(CMAKE_CONFIGURATION_TYPES Debug Release)\nadd_library(dbg INTERFACE)\nadd_library(top INTERFACE)\ntarget_link_libraries(top INTERFACE $<$<CONFIG:Debug>:dbg>)",
    );
    let report = result.explain_with(
        &crate::ExplainQuery::Target("top".into()),
        Some(crate::ExplainFilter::Dependency("dbg".into())),
    );
    let dependency = match &report {
        crate::ExplainReport::Target(target) => target.dependency.as_deref(),
        _ => None,
    }
    .expect("dependency report");
    assert_eq!(dependency.paths_for(Some(b"debug")).count(), 1);
    assert_eq!(dependency.paths_for(Some(b"Release")).count(), 0);

    let render = |config: &'static [u8]| {
        crate::render_explain_terminal(
            &report,
            crate::ExplainRenderOptions {
                config: Some(config),
                ..crate::ExplainRenderOptions::default()
            },
        )
    };
    let release = render(b"Release");
    assert!(
        release.contains("dbg is not a dependency of top in Release."),
        "{release}"
    );
    assert!(!release.contains("top -> dbg"), "{release}");
    assert!(
        !release.contains("No entry matches the filter."),
        "{release}"
    );
    let debug = render(b"Debug");
    assert!(debug.contains("top -> dbg (Debug)"), "{debug}");
    assert!(!debug.contains("not a dependency"), "{debug}");

    let paths = |config: &'static [u8]| {
        crate::explain_json(&report, Some(config))
            .get("details")
            .and_then(|details| details.get("dependency"))
            .and_then(|dependency| dependency.get("paths"))
            .and_then(rcmake_json::Value::as_array)
            .map(<[rcmake_json::Value]>::len)
    };
    assert_eq!(paths(b"Release"), Some(0));
    assert_eq!(paths(b"Debug"), Some(1));
}

#[test]
fn order_only_dependency_has_no_contradicting_filter_message() {
    let result =
        configure("add_custom_target(gen)\nadd_custom_target(app)\nadd_dependencies(app gen)");
    let report = result.explain_with(
        &crate::ExplainQuery::Target("app".into()),
        Some(crate::ExplainFilter::Dependency("gen".into())),
    );
    let text = crate::render_explain_terminal(&report, crate::ExplainRenderOptions::default());
    assert!(text.contains("order-only"), "{text}");
    assert!(!text.contains("No entry matches the filter."), "{text}");
}
