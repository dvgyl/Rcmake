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
