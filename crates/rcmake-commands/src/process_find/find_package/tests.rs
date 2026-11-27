use std::path::Path;
use std::sync::Arc;

use rcmake_base::BString;
use rcmake_eval::find_trace::{FindMode, FindOutcome, FindTraceStore, Rejection};
use rcmake_eval::{FindTraceLevel, FindTraceLimits, Interp, InterpConfig, RunMode, Services};
use rcmake_host::{Host, PureHost, Stream};

const LOG: &str = "/work/build/CMakeFiles/CMakeConfigureLog.yaml";

struct Run {
    log: BString,
    stderr: BString,
    traces: FindTraceStore,
}

fn configure(script: &str, files: &[(&str, &str)], level: FindTraceLevel) -> Run {
    let host = Arc::new(PureHost::default());
    for (path, text) in files {
        host.seed_file(Path::new(path), text.as_bytes())
            .expect("seed file");
    }
    let lists = format!("cmake_minimum_required(VERSION 3.20)\n{script}\n");
    host.seed_file(Path::new("/work/src/CMakeLists.txt"), lists.as_bytes())
        .expect("seed lists");
    let config = InterpConfig {
        services: Arc::new(Services {
            find_trace: level,
            ..Services::default()
        }),
        ..InterpConfig::new(
            host.clone(),
            RunMode::Configure,
            "/share/cmake",
            "/bin/cmake",
            "/work",
        )
    };
    let mut interp = Interp::new(config, crate::register_all);
    interp.run_configure(Path::new("/work/src"), Path::new("/work/build"), &[]);
    let log = host
        .read(
            Path::new(LOG),
            rcmake_host::ReadRange::All { limit: 1 << 20 },
        )
        .map(BString::from)
        .unwrap_or_default();
    Run {
        log,
        stderr: host.stream_bytes(Stream::Stderr),
        traces: interp.take_find_traces(),
    }
}

fn collect(max_candidates: u32) -> FindTraceLevel {
    FindTraceLevel::Collect(FindTraceLimits {
        max_candidates,
        ..FindTraceLimits::default()
    })
}

fn widget(dir: &str, version: &str, extra: &str) -> [(String, String); 2] {
    [
        (
            format!("{dir}/WidgetConfig.cmake"),
            "set(Widget_LOADED 1)".to_owned(),
        ),
        (
            format!("{dir}/WidgetConfigVersion.cmake"),
            format!(
                "set(PACKAGE_VERSION {version})\nif(PACKAGE_FIND_VERSION VERSION_LESS_EQUAL PACKAGE_VERSION)\n  set(PACKAGE_VERSION_COMPATIBLE TRUE)\nendif()\n{extra}\n"
            ),
        ),
    ]
}

fn files(owned: &[(String, String)]) -> Vec<(&str, &str)> {
    owned
        .iter()
        .map(|(path, text)| (path.as_str(), text.as_str()))
        .collect()
}

#[test]
fn native_collection_keeps_configure_log_and_debug_text_identical() {
    let mut owned = Vec::new();
    for index in 0..4 {
        owned.extend(widget(&format!("/p{index}"), "1.0", ""));
    }
    let owned = files(&owned);
    for prelude in [
        "",
        "set(CMAKE_FIND_DEBUG_MODE_NO_IMPLICIT_CONFIGURE_LOG ON)",
        "set(CMAKE_FIND_DEBUG_MODE ON)",
    ] {
        let script = format!(
            "{prelude}\nfind_package(Widget 2.0 CONFIG NO_DEFAULT_PATH PATHS /p0 /p1 /p2 /p3 /missing)"
        );
        let off = configure(&script, &owned, FindTraceLevel::Off);
        let on = configure(&script, &owned, collect(2));
        assert_eq!(off.log, on.log, "configure log changed for {prelude:?}");
        assert_eq!(off.stderr, on.stderr, "stderr changed for {prelude:?}");
        assert!(off.traces.calls.is_empty());
        let [call] = on.traces.calls.as_slice() else {
            panic!("one call expected: {:?}", on.traces.calls);
        };
        assert_eq!(call.candidates.len(), 2);
        assert!(call.candidates_omitted > 0);
        assert_eq!(call.outcome, FindOutcome::NotFound);
    }
    let script = "find_package(Widget 2.0 CONFIG NO_DEFAULT_PATH PATHS /p0 /p1 /p2 /p3)";
    let off = configure(script, &owned, FindTraceLevel::Off);
    assert!(
        off.log.windows(13).any(|part| part == b"find_package-"),
        "implicit log event expected"
    );
}

#[test]
fn version_mismatch_and_unsuitable_are_distinct() {
    let mut owned = Vec::new();
    owned.extend(widget("/old", "1.83.0", ""));
    owned.extend(widget(
        "/arch",
        "9.0",
        "set(PACKAGE_VERSION \"9.0 (32bit)\")\nset(PACKAGE_VERSION_UNSUITABLE TRUE)",
    ));
    let run = configure(
        "find_package(Widget 1.84 CONFIG NO_DEFAULT_PATH PATHS /old /arch)",
        &files(&owned),
        collect(128),
    );
    let call = &run.traces.calls[0];
    let reasons: Vec<_> = call
        .candidates
        .iter()
        .filter(|candidate| candidate.reason != Rejection::NoFile)
        .map(|candidate| (candidate.reason, candidate.found_version.clone()))
        .collect();
    assert_eq!(
        reasons,
        [
            (Rejection::VersionMismatch, Some("1.83.0".into())),
            (Rejection::VersionUnsuitable, Some("9.0 (32bit)".into())),
        ]
    );
    assert_eq!(call.request.version.clone(), Some(BString::from("1.84")));
    assert!(call.selected.is_none());
    assert_eq!(call.outcome, FindOutcome::NotFound);
}

#[test]
fn selection_and_outcome_stay_distinct_when_config_sets_found_false() {
    let files = [
        (
            "/pkg/WidgetConfig.cmake",
            "set(Widget_FOUND FALSE)\nset(Widget_NOT_FOUND_MESSAGE \"no runtime\")",
        ),
        ("/good/GoodConfig.cmake", "set(Good_VERSION 3.1)"),
    ];
    let run = configure(
        "find_package(Widget CONFIG NO_DEFAULT_PATH PATHS /pkg)\nfind_package(Good CONFIG NO_DEFAULT_PATH PATHS /good)",
        &files,
        collect(128),
    );
    let [widget, good] = run.traces.calls.as_slice() else {
        panic!("two calls expected");
    };
    let selected = widget.selected.as_ref().expect("selection");
    assert!(selected.file.ends_with(b"/pkg/WidgetConfig.cmake"));
    assert_eq!(selected.mode, FindMode::Config);
    assert_eq!(widget.outcome, FindOutcome::NotFound);
    let rejected = widget.candidates.last().expect("native rejection");
    assert_eq!(rejected.reason, Rejection::NotFound);
    assert_eq!(rejected.detail.clone(), Some(BString::from("no runtime")));
    assert_eq!(good.outcome, FindOutcome::Found);
    assert_eq!(good.version.clone(), Some(BString::from("3.1")));
}

#[test]
fn early_exits_are_finalized_once() {
    let run = configure(
        "set(CMAKE_DISABLE_FIND_PACKAGE_Off ON)\nfind_package(Off)\nfind_package(Gone MODULE QUIET)\nfind_package(Bad 1.0...2.0 EXACT)",
        &[],
        collect(128),
    );
    let outcomes: Vec<_> = run
        .traces
        .calls
        .iter()
        .map(|call| (call.name.to_string(), call.outcome.clone()))
        .collect();
    assert_eq!(
        outcomes,
        [
            ("Off".to_owned(), FindOutcome::Disabled),
            ("Gone".to_owned(), FindOutcome::NotFound),
            ("Bad".to_owned(), FindOutcome::Error),
        ]
    );
    let gone = &run.traces.calls[1];
    assert_eq!(gone.request.mode, Some(FindMode::Module));
    assert!(gone.candidates.iter().all(
        |candidate| candidate.mode == FindMode::Module && candidate.reason == Rejection::NoFile
    ));
}

#[test]
fn stale_dir_value_is_recorded_before_the_search() {
    let run = configure(
        "set(Widget_DIR /nowhere CACHE PATH \"\")\nfind_package(Widget CONFIG QUIET NO_DEFAULT_PATH)",
        &[],
        collect(128),
    );
    let call = &run.traces.calls[0];
    assert_eq!(
        call.request.dir_value_before,
        Some(BString::from("/nowhere"))
    );
    assert!(call.request.quiet);
    let cmake_path = call.roots.iter().find(|root| root.category == "CMAKE_PATH");
    assert_eq!(cmake_path.map(|root| root.enabled), Some(false));
}

#[test]
fn required_miss_is_not_found_and_stops_later_calls() {
    let run = configure(
        "find_package(Need CONFIG REQUIRED NO_DEFAULT_PATH)\nfind_package(Later CONFIG QUIET)",
        &[],
        collect(128),
    );
    let names: Vec<_> = run
        .traces
        .calls
        .iter()
        .map(|call| {
            (
                call.name.to_string(),
                call.outcome.clone(),
                call.request.required,
            )
        })
        .collect();
    assert_eq!(names, [("Need".to_owned(), FindOutcome::NotFound, true)]);
}

#[test]
fn invalid_requests_with_a_name_record_an_error() {
    let run = configure(
        "find_package(Both REQUIRED OPTIONAL)\nfind_package(Later QUIET MODULE)",
        &[],
        collect(128),
    );
    let calls: Vec<_> = run
        .traces
        .calls
        .iter()
        .map(|call| (call.ordinal, call.name.to_string(), call.outcome.clone()))
        .collect();
    assert_eq!(
        calls,
        [
            (1, "Both".to_owned(), FindOutcome::Error),
            (2, "Later".to_owned(), FindOutcome::NotFound),
        ]
    );
    assert_eq!(run.traces.calls_started, 2);
    assert!(run.traces.all_calls_kept());
}

#[test]
fn nested_calls_keep_entry_order_and_package_ordinals() {
    let files = [
        (
            "/outer/FindWidget.cmake",
            "find_package(Widget CONFIG QUIET NO_DEFAULT_PATH PATHS /inner)",
        ),
        ("/inner/WidgetConfig.cmake", "set(Widget_VERSION 1.0)"),
    ];
    let run = configure(
        "list(APPEND CMAKE_MODULE_PATH /outer)\nfind_package(Widget MODULE)",
        &files,
        collect(128),
    );
    let calls: Vec<_> = run
        .traces
        .calls
        .iter()
        .map(|call| {
            (
                call.ordinal,
                call.package_call,
                call.selected.as_ref().map(|selected| selected.file.clone()),
            )
        })
        .collect();
    assert_eq!(calls.len(), 2, "{calls:?}");
    assert_eq!((calls[0].0, calls[0].1), (1, Some(1)));
    assert_eq!((calls[1].0, calls[1].1), (2, Some(2)));
    assert!(
        calls[0]
            .2
            .as_ref()
            .is_some_and(|file| file.ends_with(b"/outer/FindWidget.cmake"))
    );
    assert!(
        calls[1]
            .2
            .as_ref()
            .is_some_and(|file| file.ends_with(b"/inner/WidgetConfig.cmake"))
    );
}

#[test]
fn module_that_reports_not_found_is_still_the_selection() {
    let files = [(
        "/mods/FindGizmo.cmake",
        "set(Gizmo_FOUND FALSE)\nset(Gizmo_NOT_FOUND_MESSAGE \"no headers\")",
    )];
    let run = configure(
        "list(APPEND CMAKE_MODULE_PATH /mods)\nfind_package(Gizmo MODULE QUIET)",
        &files,
        collect(128),
    );
    let call = &run.traces.calls[0];
    let selected = call.selected.as_ref().expect("selection");
    assert!(selected.file.ends_with(b"/mods/FindGizmo.cmake"));
    assert_eq!(selected.mode, FindMode::Module);
    assert_eq!(call.outcome, FindOutcome::NotFound);
    let rejected = call
        .candidates
        .iter()
        .find(|candidate| candidate.reason == Rejection::NotFound)
        .expect("rejection");
    assert_eq!(rejected.detail.clone(), Some(BString::from("no headers")));
}

#[test]
fn config_rejection_survives_a_module_fallback() {
    let files = [
        (
            "/pkg/GizmoConfig.cmake",
            "set(Gizmo_FOUND FALSE)\nset(Gizmo_NOT_FOUND_MESSAGE \"bad build\")",
        ),
        ("/mods/FindGizmo.cmake", "set(Gizmo_FOUND TRUE)"),
    ];
    let run = configure(
        "set(CMAKE_FIND_PACKAGE_PREFER_CONFIG ON)\nlist(APPEND CMAKE_MODULE_PATH /mods)\nset(Gizmo_DIR /pkg)\nfind_package(Gizmo)",
        &files,
        collect(128),
    );
    let call = &run.traces.calls[0];
    assert_eq!(call.outcome, FindOutcome::Found);
    let rejected = call
        .candidates
        .iter()
        .find(|candidate| {
            candidate.mode == FindMode::Config && candidate.reason == Rejection::NotFound
        })
        .expect("config rejection");
    assert_eq!(rejected.detail.clone(), Some(BString::from("bad build")));
    let selected = call.selected.as_ref().expect("selection");
    assert!(selected.file.ends_with(b"/mods/FindGizmo.cmake"));
}

#[test]
fn request_lists_are_capped_before_they_are_copied() {
    let components: Vec<String> = (0..600).map(|index| format!("c{index}")).collect();
    let script = format!(
        "find_package(Many CONFIG QUIET NO_DEFAULT_PATH COMPONENTS {})",
        components.join(" ")
    );
    let level = FindTraceLevel::Collect(FindTraceLimits {
        max_list_items: 8,
        ..FindTraceLimits::default()
    });
    let run = configure(&script, &[], level);
    let call = &run.traces.calls[0];
    assert_eq!(call.request.components.len(), 8);
    assert!(call.items_omitted >= 592);
    assert!(run.traces.bytes >= call.estimated_bytes());
}

#[test]
fn byte_budget_omits_calls_that_do_not_fit() {
    let level = FindTraceLevel::Collect(FindTraceLimits {
        max_total_bytes: 4096,
        ..FindTraceLimits::default()
    });
    let calls = (0..8)
        .map(|index| format!("find_package(P{index} CONFIG QUIET NO_DEFAULT_PATH)"))
        .collect::<Vec<_>>()
        .join("\n");
    let run = configure(&calls, &[], level);
    assert_eq!(run.traces.calls_started, 8);
    assert!(run.traces.omitted_calls > 0);
    assert!(run.traces.bytes <= 4096);
    let ordinals: Vec<_> = run.traces.calls.iter().map(|call| call.ordinal).collect();
    assert!(ordinals.windows(2).all(|pair| pair[0] < pair[1]));
    assert_eq!(
        u32::try_from(run.traces.calls.len()).unwrap_or(0) + run.traces.omitted_calls,
        8
    );
}
