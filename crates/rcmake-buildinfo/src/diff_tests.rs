use std::path::Path;
use std::sync::Arc;

use rcmake_base::BString;
use rcmake_host::{Host as _, PureHost};

use super::*;
use crate::{BuildSnapshot, load};

const CACHE: &str = "CMAKE_HOME_DIRECTORY:INTERNAL=/src\nCMAKE_GENERATOR:INTERNAL=Ninja\nCMAKE_BUILD_TYPE:STRING=Debug\nCMAKE_COMMAND:INTERNAL=/usr/bin/cmake\n";

struct Target<'a> {
    name: &'a str,
    kind: &'a str,
    groups: &'a [(&'a str, &'a [&'a str])],
    sources: &'a [(&'a str, usize)],
    link: &'a [(&'a str, &'a str)],
}

fn target_json(target: &Target<'_>) -> String {
    let groups: Vec<String> = target
        .groups
        .iter()
        .map(|(language, defines)| {
            let defines: Vec<String> = defines.iter().map(|d| format!(r#"{{"define":"{d}"}}"#)).collect();
            format!(r#"{{"language":"{language}","defines":[{}],"compileCommandFragments":[{{"fragment":"-O2"}}]}}"#, defines.join(","))
        })
        .collect();
    let sources: Vec<String> = target
        .sources
        .iter()
        .map(|(path, group)| format!(r#"{{"path":"{path}","compileGroupIndex":{group}}}"#))
        .collect();
    let link: Vec<String> = target
        .link
        .iter()
        .map(|(fragment, role)| format!(r#"{{"fragment":"{fragment}","role":"{role}"}}"#))
        .collect();
    format!(
        r#"{{"name":"{}","type":"{}","compileGroups":[{}],"sources":[{}],"link":{{"commandFragments":[{}]}}}}"#,
        target.name,
        target.kind,
        groups.join(","),
        sources.join(","),
        link.join(",")
    )
}

fn seed(
    host: &PureHost,
    root: &str,
    configs: &[&str],
    targets: &[Target<'_>],
    extra_index: Option<&str>,
) {
    // CMake saves the cache before generation writes the reply, so the reply is newer.
    host.seed_file(
        Path::new(&format!("{root}/CMakeCache.txt")),
        CACHE.as_bytes(),
    )
    .expect("cache");
    let reply = format!("{root}/.cmake/api/v1/reply");
    let file = |name: &str, text: &str| {
        host.seed_file(Path::new(&format!("{reply}/{name}")), text.as_bytes())
            .expect("seed");
    };
    let mut configurations = Vec::new();
    for config in configs {
        let mut listed = Vec::new();
        for target in targets {
            let json = format!("target-{}-{config}.json", target.name);
            file(&json, &target_json(target));
            listed.push(format!(
                r#"{{"name":"{}","id":"{}::@1","jsonFile":"{json}"}}"#,
                target.name, target.name
            ));
        }
        configurations.push(format!(
            r#"{{"name":"{config}","targets":[{}]}}"#,
            listed.join(",")
        ));
    }
    file(
        "codemodel-v2-1.json",
        &format!(
            r#"{{"paths":{{"source":"/src","build":"{root}"}},"configurations":[{}]}}"#,
            configurations.join(",")
        ),
    );
    file(
        "index-2026-01-01T00-00-00-0000.json",
        r#"{"cmake":{"generator":{"name":"Ninja"},"version":{"string":"4.5.1"}},
            "objects":[{"kind":"codemodel","version":{"major":2,"minor":9},"jsonFile":"codemodel-v2-1.json"}],
            "reply":{}}"#,
    );
    if let Some(error) = extra_index {
        file("error-2026-02-01T00-00-00-0000.json", error);
    }
}

fn snapshot(host: &PureHost, root: &str) -> BuildSnapshot {
    load(host, Path::new(root), crate::TraceInput::Missing).expect("load")
}

fn core<'a>(groups: &'a [(&'a str, &'a [&'a str])], link: &'a [(&'a str, &'a str)]) -> Target<'a> {
    Target {
        name: "core",
        kind: "STATIC_LIBRARY",
        groups,
        sources: &[("a.cpp", 0), ("b.cpp", 1)],
        link,
    }
}

fn host() -> Arc<PureHost> {
    Arc::new(PureHost::default())
}

#[test]
fn identical_trees_in_other_roots_have_no_differences() {
    let host = host();
    let groups: &[(&str, &[&str])] = &[("CXX", &["A"]), ("CXX", &["B"])];
    seed(&host, "/ba", &["Debug"], &[core(groups, &[])], None);
    seed(&host, "/bb", &["Debug"], &[core(groups, &[])], None);
    let diff = compare(
        &snapshot(&host, "/ba"),
        &snapshot(&host, "/bb"),
        &DiffOptions::default(),
    );
    assert!(!diff.has_changes(), "{diff:?}");
    let packages_partial = diff
        .partial
        .iter()
        .any(|(section, _)| *section == Section::Packages);
    assert!(packages_partial);
}

#[test]
fn same_language_group_swap_is_detected_per_source() {
    let host = host();
    seed(
        &host,
        "/ba",
        &["Debug"],
        &[core(&[("CXX", &["A"]), ("CXX", &["B"])], &[])],
        None,
    );
    seed(
        &host,
        "/bb",
        &["Debug"],
        &[core(&[("CXX", &["B"]), ("CXX", &["A"])], &[])],
        None,
    );
    let diff = compare(
        &snapshot(&host, "/ba"),
        &snapshot(&host, "/bb"),
        &DiffOptions::default(),
    );
    let [change] = diff.targets_changed.as_slice() else {
        panic!("one change expected: {diff:?}");
    };
    let swaps: Vec<_> = change
        .compile
        .iter()
        .map(|compile| (compile.sources.clone(), compile.defines.added.clone()))
        .collect();
    assert_eq!(
        swaps,
        [
            (
                vec![BString::from("${source}/a.cpp")],
                vec![BString::from("B")]
            ),
            (
                vec![BString::from("${source}/b.cpp")],
                vec![BString::from("A")]
            ),
        ]
    );
}

#[test]
fn static_library_reorder_and_duplicates_are_kept() {
    let host = host();
    let groups: &[(&str, &[&str])] = &[("CXX", &[]), ("CXX", &[])];
    seed(
        &host,
        "/ba",
        &["Debug"],
        &[core(
            groups,
            &[("liba.a", "libraries"), ("libb.a", "libraries")],
        )],
        None,
    );
    seed(
        &host,
        "/bb",
        &["Debug"],
        &[core(
            groups,
            &[("libb.a", "libraries"), ("liba.a", "libraries")],
        )],
        None,
    );
    seed(
        &host,
        "/bc",
        &["Debug"],
        &[core(
            groups,
            &[
                ("liba.a", "libraries"),
                ("libb.a", "libraries"),
                ("liba.a", "libraries"),
            ],
        )],
        None,
    );
    let a = snapshot(&host, "/ba");
    let reorder = compare(&a, &snapshot(&host, "/bb"), &DiffOptions::default());
    let link = reorder.targets_changed[0]
        .link
        .clone()
        .expect("link change");
    assert!(link.reordered);
    assert!(link.added.is_empty() && link.removed.is_empty());
    let duplicate = compare(&a, &snapshot(&host, "/bc"), &DiffOptions::default());
    let link = duplicate.targets_changed[0]
        .link
        .clone()
        .expect("link change");
    assert_eq!(link.added.len(), 1);
    assert!(!link.reordered);
}

#[test]
fn removed_configuration_is_reported() {
    let host = host();
    let groups: &[(&str, &[&str])] = &[("CXX", &[]), ("CXX", &[])];
    seed(
        &host,
        "/ba",
        &["Debug", "Release"],
        &[core(groups, &[])],
        None,
    );
    seed(&host, "/bb", &["Debug"], &[core(groups, &[])], None);
    let mut a = snapshot(&host, "/ba");
    a.generator = Some("Ninja Multi-Config".into());
    let mut b = snapshot(&host, "/bb");
    b.configs.push("Other".into());
    let diff = compare(&a, &b, &DiffOptions::default());
    assert_eq!(
        diff.unmatched_configs,
        [
            ('a', BString::from("Release")),
            ('b', BString::from("Other"))
        ]
    );
    assert_eq!(diff.exit_code(), 1);
}

#[test]
fn error_index_after_success_makes_graph_sections_incomplete() {
    let host = host();
    let groups: &[(&str, &[&str])] = &[("CXX", &[]), ("CXX", &[])];
    seed(&host, "/ba", &["Debug"], &[core(groups, &[])], None);
    seed(
        &host,
        "/bb",
        &["Debug"],
        &[core(groups, &[])],
        Some(r#"{"objects":[],"reply":{"codemodel-v2":{"error":"failed"}}}"#),
    );
    let b = snapshot(&host, "/bb");
    assert!(!b.evidence(Section::Compile).comparable());
    let diff = compare(&snapshot(&host, "/ba"), &b, &DiffOptions::default());
    assert!(!diff.has_changes());
    assert_eq!(diff.exit_code(), 2);
    assert!(
        diff.incomplete
            .iter()
            .any(|(section, _)| *section == Section::Compile)
    );
}

#[test]
fn cache_only_directory_cannot_prove_equal_compile_settings() {
    let host = host();
    for root in ["/ca", "/cb"] {
        host.seed_file(
            Path::new(&format!("{root}/CMakeCache.txt")),
            CACHE.as_bytes(),
        )
        .expect("cache");
    }
    let options = DiffOptions {
        sections: [Section::Compile, Section::Cache].into_iter().collect(),
        ..DiffOptions::default()
    };
    let diff = compare(&snapshot(&host, "/ca"), &snapshot(&host, "/cb"), &options);
    assert_eq!(diff.exit_code(), 2);
    let cache_only = DiffOptions {
        sections: [Section::Cache].into_iter().collect(),
        ..DiffOptions::default()
    };
    assert_eq!(
        compare(
            &snapshot(&host, "/ca"),
            &snapshot(&host, "/cb"),
            &cache_only
        )
        .exit_code(),
        0
    );
}

#[test]
fn client_query_only_replies_are_read() {
    let host = host();
    let groups: &[(&str, &[&str])] = &[("CXX", &[]), ("CXX", &[])];
    seed(&host, "/ba", &["Debug"], &[core(groups, &[])], None);
    host.seed_file(
        Path::new("/ba/.cmake/api/v1/reply/index-2026-03-01T00-00-00-0000.json"),
        br#"{"reply":{"client-ide":{"codemodel-v2":{"kind":"codemodel","version":{"major":2,"minor":9},"jsonFile":"codemodel-v2-1.json"}}}}"#,
    )
    .expect("index");
    let snapshot = snapshot(&host, "/ba");
    assert!(snapshot.evidence(Section::Compile).complete());
    assert_eq!(snapshot.targets.len(), 1);
}

#[test]
fn unsafe_reply_paths_are_ignored_with_a_warning() {
    let host = host();
    host.seed_file(Path::new("/ba/CMakeCache.txt"), CACHE.as_bytes())
        .expect("cache");
    host.seed_file(Path::new("/outside.json"), b"{}")
        .expect("outside");
    host.seed_file(
        Path::new("/ba/.cmake/api/v1/reply/index-2026-01-01T00-00-00-0000.json"),
        br#"{"objects":[{"kind":"codemodel","version":{"major":2,"minor":0},"jsonFile":"../../../../../outside.json"}]}"#,
    )
    .expect("index");
    let snapshot = snapshot(&host, "/ba");
    assert!(
        snapshot
            .warnings
            .iter()
            .any(|warning| warning.starts_with(b"Ignored File API reply file"))
    );
    assert!(!snapshot.evidence(Section::Compile).comparable());
}

#[test]
fn packages_keep_dir_hint_and_outcome_separate() {
    let host = host();
    let cache = |dir: &str| {
        format!(
            "{CACHE}//The directory containing a CMake configuration file for Widget.\nWidget_DIR:PATH={dir}\n"
        )
    };
    host.seed_file(Path::new("/pa/CMakeCache.txt"), cache("/opt/w1").as_bytes())
        .expect("seed");
    host.seed_file(Path::new("/pb/CMakeCache.txt"), cache("/opt/w2").as_bytes())
        .expect("seed");
    host.seed_file(
        Path::new("/opt/w1/WidgetConfigVersion.cmake"),
        b"set(PACKAGE_VERSION \"1.0\")\n",
    )
    .expect("seed");
    host.seed_file(
        Path::new("/opt/w2/WidgetConfigVersion.cmake"),
        b"if(X)\nset(PACKAGE_VERSION 2.0)\nendif()\n",
    )
    .expect("seed");
    let options = DiffOptions {
        sections: [Section::Packages].into_iter().collect(),
        ..DiffOptions::default()
    };
    let a = snapshot(&host, "/pa");
    let package = &a.packages[b"Widget".as_slice()];
    assert_eq!(package.version_hint, Some("1.0".into()));
    assert!(package.observed.is_empty());
    let diff = compare(&a, &snapshot(&host, "/pb"), &options);
    assert_eq!(diff.packages[0].fields, ["dir", "version_hint"]);
}

#[test]
fn json_is_deterministic_and_typed() {
    let host = host();
    seed(
        &host,
        "/ba",
        &["Debug"],
        &[core(&[("CXX", &["A"]), ("CXX", &["B"])], &[])],
        None,
    );
    seed(
        &host,
        "/bb",
        &["Debug"],
        &[core(&[("CXX", &["B"]), ("CXX", &["A"])], &[])],
        None,
    );
    let (a, b) = (snapshot(&host, "/ba"), snapshot(&host, "/bb"));
    let diff = compare(&a, &b, &DiffOptions::default());
    let first = crate::render_json(&a, &b, &diff);
    assert_eq!(first, crate::render_json(&a, &b, &diff));
    let value = rcmake_json::parse(&first, &rcmake_json::ReadOptions::default()).expect("json");
    assert_eq!(
        value.get("schema").and_then(rcmake_json::Value::as_str),
        Some(crate::DIFF_CONFIG_SCHEMA)
    );
    assert!(crate::render_text(&a, &b, &diff, false).contains("defines"));
}

#[test]
fn compile_database_fallback_infers_targets_with_a_launcher() {
    let host = host();
    let cache = "CMAKE_HOME_DIRECTORY:INTERNAL=/src\nCMAKE_GENERATOR:INTERNAL=Unix Makefiles\nCMAKE_CXX_COMPILER:FILEPATH=/usr/bin/c++\nCMAKE_CACHE_MAJOR_VERSION:INTERNAL=4\nCMAKE_CACHE_MINOR_VERSION:INTERNAL=5\nCMAKE_CACHE_PATCH_VERSION:INTERNAL=1\n";
    for (root, define) in [("/da", "A"), ("/db", "B")] {
        host.seed_file(
            Path::new(&format!("{root}/CMakeCache.txt")),
            cache.as_bytes(),
        )
        .expect("seed");
        host.seed_file(
            Path::new(&format!("{root}/CMakeFiles/4.5.1/CMakeCXXCompiler.cmake")),
            b"set(CMAKE_CXX_COMPILER \"/usr/bin/c++\")\nset(CMAKE_CXX_COMPILER_ID \"GNU\")\n",
        )
        .expect("seed");
        host.seed_file(
            Path::new(&format!("{root}/compile_commands.json")),
            format!(r#"[{{"directory":"{root}","file":"/src/main.cpp","command":"/usr/bin/ccache /usr/bin/c++ -D{define} -o CMakeFiles/app.dir/main.cpp.o -c /src/main.cpp"}}]"#).as_bytes(),
        )
        .expect("seed");
    }
    let a = snapshot(&host, "/da");
    assert!(matches!(
        a.evidence(Section::Compile),
        Evidence::Partial(..)
    ));
    assert!(!a.evidence(Section::Link).comparable());
    let options = DiffOptions {
        sections: [Section::Compile].into_iter().collect(),
        ..DiffOptions::default()
    };
    let diff = compare(&a, &snapshot(&host, "/db"), &options);
    let change = &diff.targets_changed[0];
    assert_eq!(change.name, "app");
    assert_eq!(change.compile[0].defines.added, [BString::from("B")]);
}

fn only(sections: &[Section]) -> DiffOptions {
    DiffOptions {
        sections: sections.iter().copied().collect(),
        ..DiffOptions::default()
    }
}

#[test]
fn equal_partial_evidence_is_not_parity() {
    let host = host();
    let groups: &[(&str, &[&str])] = &[("CXX", &[]), ("CXX", &[])];
    seed(&host, "/ba", &["Debug"], &[core(groups, &[])], None);
    seed(&host, "/bb", &["Debug"], &[core(groups, &[])], None);
    let (a, b) = (snapshot(&host, "/ba"), snapshot(&host, "/bb"));
    let complete = compare(&a, &b, &only(&[Section::Targets, Section::Compile]));
    assert_eq!(complete.exit_code(), 0, "{complete:?}");
    let packages = compare(&a, &b, &only(&[Section::Packages]));
    assert!(!packages.has_changes());
    assert_eq!(
        packages.exit_code(),
        2,
        "package outcomes are unknown without a trace"
    );
}

#[test]
fn stale_reply_never_supplies_cache_or_complete_graph_data() {
    let host = host();
    let groups: &[(&str, &[&str])] = &[("CXX", &[]), ("CXX", &[])];
    seed(&host, "/ba", &["Debug"], &[core(groups, &[])], None);
    host.seed_file(
        Path::new("/ba/.cmake/api/v1/reply/cache-v2-1.json"),
        br#"{"kind":"cache","entries":[{"name":"CMAKE_BUILD_TYPE","type":"STRING","value":"Stale","properties":[]}]}"#,
    )
    .expect("seed");
    host.seed_file(
        Path::new("/ba/.cmake/api/v1/reply/index-2026-01-01T00-00-00-0000.json"),
        br#"{"objects":[{"kind":"codemodel","version":{"major":2,"minor":9},"jsonFile":"codemodel-v2-1.json"},
                       {"kind":"cache","version":{"major":2,"minor":0},"jsonFile":"cache-v2-1.json"}]}"#,
    )
    .expect("seed");
    let fresh = snapshot(&host, "/ba");
    assert_eq!(fresh.cache[b"CMAKE_BUILD_TYPE".as_slice()].value, "Stale");
    host.set_times(
        Path::new("/ba/CMakeCache.txt"),
        None,
        Some(rcmake_host::FileTime {
            secs: 9_999_999_999,
            nanos: 1,
        }),
    )
    .expect("touch");
    let stale = snapshot(&host, "/ba");
    assert_eq!(stale.cache[b"CMAKE_BUILD_TYPE".as_slice()].value, "Debug");
    assert_eq!(
        stale.evidence(Section::Cache),
        Evidence::Complete(crate::DataSource::Cache)
    );
    assert!(matches!(
        stale.evidence(Section::Compile),
        Evidence::Partial(..)
    ));
}

#[test]
fn malformed_codemodel_is_not_an_empty_complete_graph() {
    let host = host();
    seed(&host, "/ba", &["Debug"], &[], None);
    host.seed_file(
        Path::new("/ba/.cmake/api/v1/reply/codemodel-v2-1.json"),
        b"{}",
    )
    .expect("seed");
    let snapshot = snapshot(&host, "/ba");
    assert!(!snapshot.evidence(Section::Targets).comparable());
    assert!(
        snapshot
            .warnings
            .iter()
            .any(|warning| warning.ends_with(b"is not a valid codemodel object."))
    );
}

#[test]
fn unknown_configurations_and_targets_are_rejected() {
    let host = host();
    let groups: &[(&str, &[&str])] = &[("CXX", &[]), ("CXX", &[])];
    seed(&host, "/ba", &["Debug"], &[core(groups, &[])], None);
    seed(&host, "/bb", &["Debug"], &[core(groups, &[])], None);
    let (a, b) = (snapshot(&host, "/ba"), snapshot(&host, "/bb"));
    let pair = DiffOptions {
        config_pair: Some(("Debug".into(), "Release".into())),
        ..DiffOptions::default()
    };
    assert!(validate(&a, &b, &pair).is_err());
    let lower = DiffOptions {
        config_pair: Some(("debug".into(), "DEBUG".into())),
        ..DiffOptions::default()
    };
    assert!(validate(&a, &b, &lower).is_ok());
    let target = DiffOptions {
        targets: vec!["nope".into()],
        ..DiffOptions::default()
    };
    assert!(validate(&a, &b, &target).is_err());
}

#[test]
fn absence_needs_complete_evidence_on_the_side_without_the_target() {
    let host = host();
    let groups: &[(&str, &[&str])] = &[("CXX", &[]), ("CXX", &[])];
    seed(&host, "/ba", &["Debug"], &[core(groups, &[])], None);
    let extra = Target {
        name: "extra",
        ..core(groups, &[])
    };
    seed(&host, "/bb", &["Debug"], &[core(groups, &[]), extra], None);
    let mut a = snapshot(&host, "/ba");
    let b = snapshot(&host, "/bb");
    let options = only(&[Section::Targets]);
    assert_eq!(compare(&a, &b, &options).targets_added.len(), 1);
    a.evidence.insert(
        Section::Targets,
        Evidence::Partial(crate::DataSource::FileApi, "cut".into()),
    );
    let diff = compare(&a, &b, &options);
    assert!(diff.targets_added.is_empty());
    assert_eq!(diff.exit_code(), 2);
}

fn observation(call: u32, outcome: &str) -> crate::PackageObservation {
    crate::PackageObservation {
        package_call: Some(call),
        outcome: outcome.into(),
        version: None,
        selected: None,
    }
}

#[test]
fn package_outcomes_compare_known_calls_only() {
    let complete = [observation(1, "not_found"), observation(2, "found")];
    let prefix = [observation(1, "not_found")];
    assert!(!outcomes_differ(&prefix, &complete, false, true));
    assert!(outcomes_differ(&prefix, &complete, true, true));
    let changed = [observation(1, "found")];
    assert!(outcomes_differ(&changed, &complete, false, true));
    assert!(!outcomes_differ(&[], &complete, false, true));
    assert!(outcomes_differ(&[], &complete, true, true));
}

#[test]
fn normalize_replaces_home_toolchain_packages_backtrace_and_is_idempotent() {
    let host = host();
    host.env_set(b"HOME", Some(b"/Users/alice"));
    let cache = "\
CMAKE_HOME_DIRECTORY:INTERNAL=/Users/alice/proj\n\
CMAKE_GENERATOR:INTERNAL=Ninja\n\
CMAKE_BUILD_TYPE:STRING=Debug\n\
CMAKE_COMMAND:INTERNAL=/usr/bin/cmake\n\
API_KEY:STRING=secret-123\n\
DB_PASSWORD:STRING=hunter2\n\
ENABLE_FEATURE:BOOL=ON\n\
Widget_DIR:PATH=/Users/alice/vcpkg/widget\n";
    let build = "/Users/alice/proj/build";
    host.seed_file(
        Path::new(&format!("{build}/CMakeCache.txt")),
        cache.as_bytes(),
    )
    .expect("cache");
    host.seed_file(
        Path::new(&format!("{build}/CMakeFiles/4.5.1/CMakeCXXCompiler.cmake")),
        b"set(CMAKE_CXX_COMPILER \"/Users/alice/llvm/bin/clang++\")\nset(CMAKE_CXX_COMPILER_ID \"Clang\")\n",
    )
    .expect("compiler");

    let trace = crate::TraceInput::Current {
        calls: vec![crate::TraceCall {
            name: "Widget".into(),
            observation: crate::PackageObservation {
                package_call: Some(1),
                outcome: "found".into(),
                version: Some("2.0".into()),
                selected: Some("/Users/alice/vcpkg/widget/WidgetConfig.cmake".into()),
            },
        }],
        complete: true,
    };
    let mut snap = load(host.as_ref(), Path::new(build), trace).expect("load");
    assert_eq!(snap.home_root, Some("/Users/alice".into()));
    assert!(!snap.is_normalized);
    snap.targets.insert(
        (BString::from("Debug"), BString::from("app")),
        TargetSnapshot {
            kind: Some("EXECUTABLE".into()),
            attribution: Attribution::Exact,
            sources: BTreeMap::new(),
            link: None,
            dependencies: BTreeSet::new(),
            artifacts: vec!["/Users/alice/proj/build/app".into()],
            backtrace: Some("/Users/alice/proj/CMakeLists.txt:9".into()),
        },
    );

    normalize(&mut snap);
    assert!(snap.is_normalized);
    assert_eq!(snap.build_root, "${build}");
    assert_eq!(snap.source_root, Some("${source}".into()));
    assert_eq!(snap.home_root, None);
    assert_eq!(
        snap.toolchains[0].path,
        Some("${home}/llvm/bin/clang++".into())
    );
    assert_eq!(
        snap.packages[b"Widget".as_slice()].observed[0].selected,
        Some("${home}/vcpkg/widget/WidgetConfig.cmake".into())
    );
    assert_eq!(
        snap.targets[&(BString::from("Debug"), BString::from("app"))].backtrace,
        Some("${source}/CMakeLists.txt:9".into())
    );

    // Second normalize call is a complete no-op.
    let once = snap.clone();
    normalize(&mut snap);
    assert_eq!(snap, once);

    // Secret cache filtering in CacheSet::Key vs CacheSet::All.
    assert!(!cache_set_includes(
        CacheSet::Key,
        b"API_KEY",
        &snap.cache[b"API_KEY".as_slice()]
    ));
    assert!(!cache_set_includes(
        CacheSet::Key,
        b"DB_PASSWORD",
        &snap.cache[b"DB_PASSWORD".as_slice()]
    ));
    assert!(cache_set_includes(
        CacheSet::Key,
        b"ENABLE_FEATURE",
        &snap.cache[b"ENABLE_FEATURE".as_slice()]
    ));
    assert!(cache_set_includes(
        CacheSet::All,
        b"API_KEY",
        &snap.cache[b"API_KEY".as_slice()]
    ));
}

#[test]
fn secret_cache_filter_splits_camel_and_pascal_case() {
    let string_record = CacheRecord {
        ty: "STRING".into(),
        value: "secret".into(),
        help: BString::default(),
    };
    for secret in [
        b"API_KEY".as_slice(),
        b"clientSecret",
        b"accessToken",
        b"apiKey",
        b"authToken",
        b"dbPassword",
        b"myAPIKey",
        b"AWSClientSecret",
    ] {
        assert!(
            !cache_set_includes(CacheSet::Key, secret, &string_record),
            "expected {} to be excluded from CacheSet::Key",
            String::from_utf8_lossy(secret)
        );
        assert!(cache_set_includes(CacheSet::All, secret, &string_record));
    }
    for non_secret in [
        b"CMAKE_BUILD_TYPE".as_slice(),
        b"enableKeyboard",
        b"useSsl",
        b"buildSharedLibs",
    ] {
        assert!(
            cache_set_includes(CacheSet::Key, non_secret, &string_record),
            "expected {} to stay in CacheSet::Key",
            String::from_utf8_lossy(non_secret)
        );
    }
}

#[test]
fn posix_backslashes_do_not_collide_and_windows_quoted_spaces_normalize() {
    // On a POSIX snapshot, literal `\` in relative filenames stays distinct from `/`.
    let host = host();
    host.seed_file(Path::new("/ba/CMakeCache.txt"), CACHE.as_bytes())
        .expect("cache");
    let mut posix = snapshot(&host, "/ba");
    let settings = Arc::new(CompileSettings::default());
    let mut sources = BTreeMap::new();
    sources.insert(BString::from(r"a\b.c"), vec![Arc::clone(&settings)]);
    sources.insert(BString::from("a/b.c"), vec![Arc::clone(&settings)]);
    posix.targets.insert(
        (BString::from("Debug"), BString::from("app")),
        TargetSnapshot {
            kind: Some("EXECUTABLE".into()),
            attribution: Attribution::Exact,
            sources,
            link: None,
            dependencies: BTreeSet::new(),
            artifacts: Vec::new(),
            backtrace: None,
        },
    );
    normalize(&mut posix);
    let target = &posix.targets[&(BString::from("Debug"), BString::from("app"))];
    assert_eq!(target.sources.len(), 2);
    assert!(target.sources.contains_key(b"a\\b.c".as_slice()));
    assert!(target.sources.contains_key(b"a/b.c".as_slice()));

    // Quoted Windows paths containing spaces normalize every backslash across spaces.
    let mut win = snapshot(&host, "/ba");
    win.host = Some(crate::SnapshotHost {
        os: "Windows".into(),
        arch: "x86_64".into(),
    });
    win.build_root = r"C:\Users\alice\My Build".into();
    win.source_root = Some(r"C:\Users\alice\My Project".into());
    win.home_root = Some(r"C:\Users\alice".into());
    let win_settings = Arc::new(CompileSettings {
        defines: vec![
            r#"HDR="C:\Program Files\Foo\bar.h""#.into(),
            r#"SRC="C:\Users\alice\My Project\Sub Dir\baz.h""#.into(),
        ],
        fragments: vec![r#"-I"C:\Program Files\SDK\Include""#.into()],
        ..CompileSettings::default()
    });
    let mut win_sources = BTreeMap::new();
    win_sources.insert(
        BString::from(r"C:\Users\alice\My Project\Sub Dir\main.cpp"),
        vec![win_settings],
    );
    win.targets.insert(
        (BString::from("Debug"), BString::from("winapp")),
        TargetSnapshot {
            kind: Some("EXECUTABLE".into()),
            attribution: Attribution::Exact,
            sources: win_sources,
            link: None,
            dependencies: BTreeSet::new(),
            artifacts: Vec::new(),
            backtrace: None,
        },
    );
    normalize(&mut win);
    let win_target = &win.targets[&(BString::from("Debug"), BString::from("winapp"))];
    let inv = &win_target.sources[b"${source}/Sub Dir/main.cpp".as_slice()][0];
    assert_eq!(inv.defines[0], r#"HDR="C:/Program Files/Foo/bar.h""#);
    assert_eq!(inv.defines[1], r#"SRC="${source}/Sub Dir/baz.h""#);
    assert_eq!(inv.fragments[0], r#"-I"C:/Program Files/SDK/Include""#);
}

#[test]
fn cache_all_against_key_snapshot_compares_shared_subset_and_returns_partial() {
    let host = host();
    let cache = format!("{CACHE}ENABLE_OPT:BOOL=ON\nAPI_KEY:STRING=secret-val\n");
    host.seed_file(Path::new("/ca/CMakeCache.txt"), cache.as_bytes())
        .expect("seed a");
    let live = snapshot(&host, "/ca");
    let saved_bytes =
        crate::encode_snapshot(&live, &crate::SaveOptions::default()).expect("encode");
    let saved = crate::decode_snapshot(&saved_bytes, "saved.json".into()).expect("decode");

    let all_opts = DiffOptions {
        sections: [Section::Cache].into_iter().collect(),
        cache: CacheSet::All,
        ..DiffOptions::default()
    };
    let diff = compare(&saved, &live, &all_opts);
    assert!(
        !diff.has_changes(),
        "must not report false diff on omitted non-key entries: {diff:?}"
    );
    assert_eq!(diff.exit_code(), 2);
    assert!(
        diff.partial
            .iter()
            .any(|(section, _)| *section == Section::Cache)
    );
}
