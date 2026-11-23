#![allow(clippy::panic)]

use std::path::Path;

use rcmake_base::BString;
use rcmake_host::{Follow, Host, PureHost, ReadRange};
use rcmake_resolve::{
    BuildGraph, BuildStep, CustomStep, GraphTarget, Invocation, LinkKind, LinkStep,
};

use super::{GenerationResult, Ninja, generate_multi_config, generate_single_config};

fn read(host: &PureHost, file: &str) -> String {
    let bytes = host
        .read(
            &Path::new("/work/build").join(file),
            ReadRange::All { limit: 100_000 },
        )
        .unwrap_or_else(|error| panic!("{file}: {error}"));
    String::from_utf8(bytes).unwrap_or_else(|error| panic!("{error}"))
}

#[test]
fn single_config_writes_rules_edges_and_utilities() {
    let host = PureHost::default();
    host.env_set(b"CMAKE_JOB_POOLS", Some(b"slow=3;unused=4"));
    let mut result = GenerationResult {
        graphs: vec![BuildGraph {
            steps: vec![
                BuildStep::Custom(CustomStep {
                    outputs: vec!["generated $:file".into()],
                    byproducts: vec!["include|generated.h".into()],
                    depends: vec!["/work/config.in".into()],
                    invocations: vec![Invocation {
                        shell: "printf '%s' '$value'".into(),
                        working_dir: Some("/work/a dir".into()),
                        ..Invocation::default()
                    }],
                    comment: Some("Generate $: file".into()),
                    depfile: Some("a $:file.d".into()),
                    uses_terminal: true,
                    job_pool: Some("ignored".into()),
                }),
                BuildStep::Compile {
                    object: "obj/a $:file.o".into(),
                    source: "/work/src/a $:file.c".into(),
                    invocation: Invocation {
                        shell: "cc -c '/work/src/a $:file.c'".into(),
                        ..Invocation::default()
                    },
                    depfile: Some("obj/a $:file.d".into()),
                },
                BuildStep::Link {
                    inputs: vec!["obj/a $:file.o".into()],
                    step: LinkStep {
                        kind: LinkKind::SharedLibrary,
                        language: "C".into(),
                        fragments: vec![],
                        output: "libshared.so".into(),
                    },
                    invocations: vec![Invocation {
                        shell: "cc -shared".into(),
                        ..Invocation::default()
                    }],
                },
                BuildStep::Custom(CustomStep {
                    invocations: vec![Invocation {
                        argv: vec!["true".into()],
                        ..Invocation::default()
                    }],
                    job_pool: Some("slow".into()),
                    ..CustomStep::default()
                }),
            ],
            targets: vec![
                GraphTarget {
                    name: "generate".into(),
                    steps: vec![0],
                    ..GraphTarget::default()
                },
                GraphTarget {
                    name: "shared".into(),
                    steps: vec![1, 2],
                    depends: vec![0],
                    in_all: true,
                },
                GraphTarget {
                    name: "optional".into(),
                    steps: vec![3],
                    ..GraphTarget::default()
                },
            ],
            ..BuildGraph::default()
        }],
        ..GenerationResult::default()
    };
    result.metadata.listfiles = vec![
        "/work/source dir/CMakeLists.txt".into(),
        "/work/source dir/options$:file.cmake".into(),
    ];
    let dir = Path::new("/work/build");
    generate_single_config(&result, &host, dir).unwrap_or_else(|error| panic!("{error}"));
    let rules = read(&host, "CMakeFiles/rules.ninja");
    assert!(rules.contains("rule COMPILE_DEPFILE\n  command = $COMMAND\n  description = Building $out\n  depfile = $DEP_FILE\n  deps = gcc"));
    assert!(rules.contains(
        "rule CUSTOM_COMMAND\n  command = $COMMAND\n  description = $DESC\n  restat = 1"
    ));
    assert!(rules.contains("rule RERUN_CMAKE\n  command = $COMMAND\n  description = Re-running CMake\n  generator = 1\n  pool = console\n  restat = 1"));
    assert!(rules.contains("pool slow\n  depth = 3"));
    assert!(!rules.contains("pool ignored"));
    let build = read(&host, "build.ninja");
    for line in [
        "include CMakeFiles/rules.ninja",
        "build generated$ $$$:file | include${ninja_pipe}generated.h: CUSTOM_COMMAND /work/config.in",
        "  COMMAND = (cd$ --$ '/work/a$ dir'$ &&$ printf$ '%s'$ '$$value')",
        "  DESC = Generate$ $$$:$ file",
        "  depfile = a$ $$$:file.d",
        "  pool = console",
        "  pool = slow",
        "build obj/a$ $$$:file.o: COMPILE_DEPFILE /work/src/a$ $$$:file.c || generate",
        "  DEP_FILE = obj/a$ $$$:file.d",
        "build libshared.so: LINK_SHARED_LIBRARY obj/a$ $$$:file.o",
        "build shared: phony obj/a$ $$$:file.o libshared.so generate",
        "build generate: phony generated$ $$$:file include${ninja_pipe}generated.h",
        "build optional: phony CMakeFiles/rcmake-custom-3.util",
        "build all: phony shared",
        "build build.ninja CMakeFiles/rules.ninja: RERUN_CMAKE | /work/source$ dir/CMakeLists.txt /work/source$ dir/options$$$:file.cmake",
        "  COMMAND = cmake$ --regenerate-during-build$ -S'/work/source$ dir'$ -B/work/build",
        "build CMakeFiles/install.util: CUSTOM_COMMAND all",
        "  COMMAND = cmake$ -P$ cmake_install.cmake",
        "build install: phony CMakeFiles/install.util",
        "  COMMAND = ctest$ --force-new-ctest-process",
        "build test: phony CMakeFiles/test.util",
        "  COMMAND = ninja$ -t$ clean",
        "  COMMAND = ninja$ -t$ targets",
        "build clean: phony CMakeFiles/clean.util",
        "  COMMAND = cmake$ -S'/work/source$ dir'$ -B/work/build",
        "build rebuild_cache: phony CMakeFiles/rebuild_cache.util",
        "default all",
    ] {
        assert!(
            build.lines().any(|candidate| candidate == line),
            "Missing {line:?} in:\n{build}"
        );
    }

    let manifest = dir.join("build.ninja");
    let original = host
        .metadata(&manifest, Follow::Yes)
        .map(|metadata| metadata.mtime);
    host.sleep(std::time::Duration::from_secs(1));
    generate_single_config(&result, &host, dir).unwrap_or_else(|error| panic!("{error}"));
    assert_eq!(
        original,
        host.metadata(&manifest, Follow::Yes)
            .map(|metadata| metadata.mtime)
    );
}

#[test]
fn continues_ninja_lines_and_keeps_escaped_values() {
    let mut ninja = Ninja::default();
    ninja.0.extend_from_slice(b"ninja_pipe = |\n");
    ninja.phony(&BString::from(b"a b:c$\nrest|here".as_slice()), &[]);
    ninja.binding(b"VAR", b"a b:c$\nrest|here");
    assert_eq!(
        ninja.0,
        b"ninja_pipe = |\nbuild a$ b$:c$$$\nrest${ninja_pipe}here: phony\n  VAR = a$ b$:c$$$\nrest|here\n"
    );
}

#[test]
fn multi_config_uses_config_order_or_host_default() {
    let host = PureHost::default();
    let mut debug = BuildGraph::default();
    debug.config.0 = "Debug".into();
    debug.targets.push(GraphTarget {
        name: "app".into(),
        in_all: true,
        ..GraphTarget::default()
    });
    let mut release = debug.clone();
    release.config.0 = "Release".into();
    let result = GenerationResult {
        configs: vec![debug.config.clone(), release.config.clone()],
        graphs: vec![release, debug],
        ..GenerationResult::default()
    };
    let dir = Path::new("/work/build");
    generate_multi_config(&result, &host, dir).unwrap_or_else(|error| panic!("{error}"));
    assert!(read(&host, "build.ninja").contains("include CMakeFiles/impl-Debug.ninja"));
    host.seed_file(
        &dir.join("CMakeCache.txt"),
        b"CMAKE_DEFAULT_BUILD_TYPE_OTHER:STRING=Debug\nCMAKE_DEFAULT_BUILD_TYPE:STRING=Release\r\n",
    )
    .unwrap_or_else(|error| panic!("{error}"));
    generate_multi_config(&result, &host, dir).unwrap_or_else(|error| panic!("{error}"));
    assert!(read(&host, "build.ninja").contains("include CMakeFiles/impl-Release.ninja"));
    for config in ["Debug", "Release"] {
        let entry = read(&host, &format!("build-{config}.ninja"));
        assert!(entry.contains(&format!("include CMakeFiles/impl-{config}.ninja")));
        assert!(entry.contains(&format!("build all: phony all$:{config}")));
        assert!(entry.contains(&format!("build app: phony app$:{config}")));
        assert!(entry.contains(&format!("ninja$ -f$ build-{config}.ninja$ -t$ clean")));
        assert!(entry.ends_with("default all\n"));
        let implementation = read(&host, &format!("CMakeFiles/impl-{config}.ninja"));
        assert!(implementation.starts_with("include CMakeFiles/rules.ninja\n"));
        assert!(implementation.contains(&format!("build app$:{config}: phony\n")));
        assert!(implementation.contains(&format!("build all$:{config}: phony app$:{config}")));
    }
}
