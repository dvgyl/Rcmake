use std::collections::HashMap;

use rcmake_base::BString;
use rcmake_eval::find_trace::{
    FindCandidate, FindMode, FindOutcome, FindPackageTrace, FindRequestSummary, Rejection,
    SearchRoot,
};

use super::*;

#[derive(Default)]
struct Probe {
    dirs: HashMap<BString, Vec<BString>>,
    env: HashMap<BString, BString>,
    calls: usize,
}

impl NearMissProbe for Probe {
    fn list_dir(&mut self, path: &[u8]) -> Vec<BString> {
        self.calls += 1;
        self.dirs.get(path).cloned().unwrap_or_default()
    }

    fn env(&mut self, name: &[u8]) -> Option<BString> {
        self.env.get(name).cloned()
    }
}

fn trace(candidates: Vec<FindCandidate>) -> FindPackageTrace {
    FindPackageTrace {
        name: "Boost".into(),
        call: Vec::new(),
        request: FindRequestSummary {
            mode: Some(FindMode::Auto),
            version: Some("1.84".into()),
            required: true,
            config_files: vec!["BoostConfig.cmake".into(), "boost-config.cmake".into()],
            dir_var: "Boost_DIR".into(),
            ..FindRequestSummary::default()
        },
        roots: vec![SearchRoot {
            category: "CMAKE_PACKAGE_REGISTRY".into(),
            enabled: false,
        }],
        root_path_mode: "BOTH".into(),
        candidates,
        candidates_omitted: 0,
        fields_cut: 0,
        selected: None,
        outcome: FindOutcome::NotFound,
        version: None,
    }
}

fn candidate(path: &str, reason: Rejection, version: Option<&str>) -> FindCandidate {
    FindCandidate {
        path: path.into(),
        mode: FindMode::Config,
        reason,
        found_version: version.map(BString::from),
        missing_components: Vec::new(),
        detail: None,
    }
}

#[test]
fn version_mismatch_suggests_the_exact_dir_define() {
    let found = trace(vec![candidate(
        "/opt/homebrew/lib/cmake/Boost-1.83.0/BoostConfig.cmake",
        Rejection::VersionMismatch,
        Some("1.83.0"),
    )]);
    let suggestions = suggest(&found, &mut Probe::default());
    assert_eq!(suggestions[0].kind, SuggestionKind::RelaxVersion);
    assert_eq!(
        suggestions[0].define.as_ref().map(ToString::to_string),
        Some("-DBoost_DIR=/opt/homebrew/lib/cmake/Boost-1.83.0".to_owned())
    );
}

#[test]
fn stale_dir_and_unsuitable_and_components() {
    let mut stale = trace(vec![
        candidate("/old/BoostConfig.cmake", Rejection::NoFile, None),
        candidate(
            "/x/BoostConfig.cmake",
            Rejection::VersionUnsuitable,
            Some("1.84 (32bit)"),
        ),
    ]);
    stale.request.dir_value_before = Some("/old".into());
    let mut missing = candidate("/y/BoostConfig.cmake", Rejection::ComponentsMissing, None);
    missing.missing_components = vec!["filesystem".into()];
    stale.candidates.push(missing);
    let kinds: Vec<_> = suggest(&stale, &mut Probe::default())
        .into_iter()
        .map(|suggestion| (suggestion.kind, suggestion.define.map(|d| d.to_string())))
        .collect();
    assert_eq!(
        kinds,
        [
            (
                SuggestionKind::ClearStaleDir,
                Some("-U Boost_DIR".to_owned())
            ),
            (SuggestionKind::MatchArch, None),
            (SuggestionKind::InstallComponents, None),
        ]
    );
}

#[test]
fn near_miss_probe_gives_exact_prefix_and_dir() {
    let mut probe = Probe::default();
    probe
        .env
        .insert("HOMEBREW_PREFIX".into(), "/opt/homebrew".into());
    probe.dirs.insert(
        "/opt/homebrew/lib/cmake".into(),
        vec!["Boost-1.86.0".into(), "fmt".into()],
    );
    probe.dirs.insert(
        "/opt/homebrew/lib/cmake/Boost-1.86.0".into(),
        vec!["BoostConfig.cmake".into()],
    );
    let missing = trace(vec![candidate(
        "/usr/BoostConfig.cmake",
        Rejection::NoFile,
        None,
    )]);
    let defines: Vec<_> = suggest(&missing, &mut probe)
        .into_iter()
        .filter_map(|suggestion| suggestion.define.map(|d| d.to_string()))
        .collect();
    assert_eq!(
        defines,
        [
            "-DBoost_DIR=/opt/homebrew/lib/cmake/Boost-1.86.0".to_owned(),
            "-DCMAKE_PREFIX_PATH=/opt/homebrew".to_owned(),
        ]
    );

    let nothing = suggest(&missing, &mut Probe::default());
    assert_eq!(nothing[0].kind, SuggestionKind::AddPrefix);
    assert_eq!(
        nothing[0].define.as_ref().map(ToString::to_string),
        Some("-DCMAKE_PREFIX_PATH=<install-prefix-of-Boost>".to_owned())
    );
    assert_eq!(nothing[1].kind, SuggestionKind::ReenableRoot);
}

#[test]
fn report_selects_calls_and_marks_unknown_absence() {
    let calls = vec![trace(Vec::new()), {
        let mut found = trace(Vec::new());
        found.outcome = FindOutcome::Found;
        found
    }];
    let mut probe = Probe::default();
    let report = FindReport::new(b"Boost", &calls, Some(2), &mut probe);
    assert_eq!(report.calls.len(), 1);
    assert!(report.found());
    let mut absent = FindReport::new(b"Qt6", &calls, None, &mut probe);
    assert!(!absent.absence_unknown());
    absent.completed = false;
    assert!(absent.absence_unknown());
    let json = render_find_json(&absent);
    let value = rcmake_json::parse(&json, &rcmake_json::ReadOptions::default()).expect("json");
    assert_eq!(
        value.get("absence_unknown"),
        Some(&rcmake_json::Value::Bool(true))
    );
    assert_eq!(
        value.get("schema").and_then(rcmake_json::Value::as_str),
        Some(EXPLAIN_FIND_SCHEMA)
    );
}
