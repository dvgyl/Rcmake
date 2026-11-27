use std::path::Path;
use std::sync::Arc;

use rcmake_base::BString;
use rcmake_host::PureHost;

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
    host.seed_file(
        Path::new(&format!("{root}/CMakeCache.txt")),
        CACHE.as_bytes(),
    )
    .expect("cache");
}

fn snapshot(host: &PureHost, root: &str) -> BuildSnapshot {
    load(host, Path::new(root)).expect("load")
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
    assert_eq!(diff.exit_code(), 3);
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
    assert_eq!(diff.exit_code(), 3);
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
    assert!(crate::render_text(&a, &b, &diff).contains("defines"));
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
