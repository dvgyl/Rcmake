#![allow(clippy::panic)]

use std::path::{Path, PathBuf};

use rcmake_base::BString;
use rcmake_host::{Follow, Host, PureHost, ReadRange};
use rcmake_resolve::{
    ArtifactSet, BuildGraph, BuildStep, CustomStep, GraphTarget, Invocation, LinkKind, LinkStep,
};

use super::{
    GenerationResult, Graph, Ninja, generate_multi_config, generate_single_config, qualified,
};

pub(super) fn read(host: &PureHost, file: &str) -> String {
    read_in(host, Path::new("/work/build"), file)
}

/// Read a generated file below `dir`, which can be a Windows-style root.
pub(super) fn read_in(host: &PureHost, dir: &Path, file: &str) -> String {
    let bytes = host
        .read(&dir.join(file), ReadRange::All { limit: 100_000 })
        .unwrap_or_else(|error| panic!("{file}: {error}"));
    String::from_utf8(bytes).unwrap_or_else(|error| panic!("{error}"))
}

#[test]
fn subdirectory_edit_cache_uses_top_level_paths() -> Result<(), Box<dyn std::error::Error>> {
    use rcmake_model::{DirId, ProjectModel};

    let host = PureHost::default();
    let mut model = ProjectModel::default();
    model.set_root_dirs("/work/src".into(), "/work/build".into());
    model
        .dir_mut(DirId::ROOT)
        .vars
        .insert("CMAKE_EDIT_COMMAND".into(), "/tools/ccmake".into());
    model.add_directory(
        DirId::ROOT,
        "/work/src/sub".into(),
        "/work/build/sub".into(),
        Default::default(),
    )?;
    let result = GenerationResult {
        graphs: vec![BuildGraph::default()],
        ..Default::default()
    };
    super::generate_single_config_with_model(&result, &host, Path::new("/work/build"), &model)?;
    let manifest = read(&host, "build.ninja");
    assert!(manifest.contains("build sub/edit_cache: phony"));
    assert!(manifest.contains("cd -- sub && /tools/ccmake -S/work/src -B/work/build"));
    Ok(())
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
                    ..CustomStep::default()
                }),
                BuildStep::Compile {
                    byproducts: Vec::new(),
                    compile_input: None,
                    object: "obj/a $:file.o".into(),
                    source: "/work/src/a $:file.c".into(),
                    invocation: Invocation {
                        shell: "cc -c '/work/src/a $:file.c'".into(),
                        ..Invocation::default()
                    },
                    depfile: Some("obj/a $:file.d".into()),
                    modmap: None,
                    dyndep: None,
                },
                BuildStep::Link {
                    implicit_inputs: Vec::new(),
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
    assert!(rules.contains("rule COMPILE_DEPFILE\n  command = $COMMAND\n  description = $DESC\n  depfile = $DEP_FILE\n  deps = gcc"));
    assert!(
        rules.contains("rule CUSTOM_COMMAND\n  command = $COMMAND\n  description = $DESC\n\nrule")
    );
    assert!(rules.contains("rule RERUN_CMAKE\n  command = $COMMAND\n  description = Re-running CMake\n  generator = 1\n  pool = console\n  restat = 1"));
    assert!(rules.contains("pool slow\n  depth = 3"));
    assert!(!rules.contains("pool ignored"));
    let build = read(&host, "build.ninja");
    for line in [
        "include CMakeFiles/rules.ninja",
        "build generated$ $$$:file | include${ninja_pipe}generated.h /work/build/generated$ $$$:file /work/build/include${ninja_pipe}generated.h: CUSTOM_COMMAND /work/config.in",
        r#"  COMMAND = cd "/work/a dir" && printf '%s' '$$value'"#,
        "  DESC = Generate $$: file",
        "  depfile = a $$:file.d",
        "  pool = console",
        "  pool = slow",
        "build obj/a$ $$$:file.o: COMPILE_DEPFILE /work/src/a$ $$$:file.c || generated$ $$$:file",
        "  DEP_FILE = obj/a $$:file.d",
        "build libshared.so: LINK_SHARED_LIBRARY obj/a$ $$$:file.o || generate",
        "  DESC = Linking C shared library libshared.so",
        "build shared: phony obj/a$ $$$:file.o libshared.so generate",
        "build generate: phony generated$ $$$:file include${ninja_pipe}generated.h",
        "build optional: phony CMakeFiles/rcmake-custom-3.util",
        "build all: phony shared",
        "build build.ninja CMakeFiles/rules.ninja: RERUN_CMAKE | /work/source$ dir/CMakeLists.txt /work/source$ dir/options$$$:file.cmake",
        "  COMMAND = cmake --regenerate-during-build -S'/work/source dir' -B/work/build",
        "build CMakeFiles/install.util: CUSTOM_COMMAND all",
        "  COMMAND = cmake -P cmake_install.cmake",
        "build install: phony CMakeFiles/install.util",
        "  COMMAND = ctest --force-new-ctest-process",
        "build test: phony CMakeFiles/test.util",
        "  COMMAND = ninja -t clean",
        "  COMMAND = ninja -t targets",
        "build clean: phony CMakeFiles/clean.util",
        "  COMMAND = cmake -S'/work/source dir' -B/work/build",
        "build rebuild_cache: phony CMakeFiles/rebuild_cache.util",
        "default all",
    ] {
        assert!(
            build.lines().any(|candidate| candidate == line),
            "Missing {line:?} in:\n{build}"
        );
    }

    assert!(build.contains("  depfile = a $$:file.d\n  deps = gcc\n"));
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
fn custom_command_orders_after_a_target_without_a_file() {
    let host = PureHost::default();
    let result = GenerationResult {
        graphs: vec![BuildGraph {
            steps: vec![
                BuildStep::Custom(CustomStep {
                    invocations: vec![Invocation {
                        argv: vec!["true".into()],
                        ..Invocation::default()
                    }],
                    ..CustomStep::default()
                }),
                BuildStep::Custom(CustomStep {
                    outputs: vec!["stamp".into()],
                    depends: vec!["order_deps".into(), "/work/tool".into()],
                    invocations: vec![Invocation {
                        argv: vec!["touch".into(), "stamp".into()],
                        ..Invocation::default()
                    }],
                    ..CustomStep::default()
                }),
                BuildStep::Custom(CustomStep {
                    depends: vec!["stamp".into(), "order_deps".into()],
                    ..CustomStep::default()
                }),
            ],
            targets: vec![
                GraphTarget {
                    name: "order_deps".into(),
                    steps: vec![0],
                    ..GraphTarget::default()
                },
                GraphTarget {
                    name: "generate".into(),
                    steps: vec![1, 2],
                    depends: vec![0],
                    in_all: true,
                },
            ],
            ..BuildGraph::default()
        }],
        ..GenerationResult::default()
    };
    generate_single_config(&result, &host, Path::new("/work/build"))
        .unwrap_or_else(|error| panic!("{error}"));
    let build = read(&host, "build.ninja");
    let stamp = build
        .lines()
        .find(|line| line.starts_with("build stamp "))
        .unwrap_or_else(|| panic!("Missing the stamp edge in:\n{build}"));
    // A phony target with no inputs is always dirty, so it must not be an explicit input.
    assert_eq!(
        stamp,
        "build stamp | /work/build/stamp: CUSTOM_COMMAND /work/tool || order_deps"
    );
    assert!(
        build
            .lines()
            .any(|line| line.starts_with("build generate: phony") && line.contains("order_deps")),
        "A phony edge keeps the target as an input:\n{build}"
    );
}

#[test]
fn unity_source_command_omits_the_target_that_needs_its_output()
-> Result<(), Box<dyn std::error::Error>> {
    let targets = vec![
        GraphTarget {
            name: "app".into(),
            depends: vec![1],
            ..GraphTarget::default()
        },
        GraphTarget {
            name: "unity_prerequisites".into(),
            steps: vec![0],
            ..GraphTarget::default()
        },
    ];
    assert_eq!(
        custom_command_build_edge(targets, 1, &[0])?,
        "build generated | /work/build/generated: CUSTOM_COMMAND"
    );
    Ok(())
}

#[test]
fn shared_custom_command_keeps_prerequisites_of_an_owner() -> Result<(), Box<dyn std::error::Error>>
{
    let targets = vec![
        GraphTarget {
            name: "prepare".into(),
            ..GraphTarget::default()
        },
        GraphTarget {
            name: "producer".into(),
            steps: vec![0],
            depends: vec![0],
            ..GraphTarget::default()
        },
        GraphTarget {
            name: "consumer".into(),
            steps: vec![0],
            depends: vec![1],
            ..GraphTarget::default()
        },
    ];
    assert_eq!(
        custom_command_build_edge(targets, 3, &[1, 2])?,
        "build generated | /work/build/generated: CUSTOM_COMMAND || prepare"
    );
    Ok(())
}

fn custom_command_build_edge(
    targets: Vec<GraphTarget>,
    project_targets: usize,
    owners: &[usize],
) -> Result<String, Box<dyn std::error::Error>> {
    use rcmake_model::{
        CustomCommand, CustomCommandKind, DirId, ProjectModel, Target, TargetKind, TargetOrigin,
    };

    let mut model = ProjectModel::default();
    let command = model.add_command(CustomCommand::new(
        DirId::ROOT,
        CustomCommandKind::Output,
        Default::default(),
        Default::default(),
    ))?;
    let target_ids = targets
        .iter()
        .take(project_targets)
        .map(|target| {
            model.add_target(Target::new(
                target.name.clone(),
                TargetKind::Utility,
                TargetOrigin::Normal,
                DirId::ROOT,
                Default::default(),
                Default::default(),
            ))
        })
        .collect::<Result<Vec<_>, _>>()?;
    let owners = rcmake_resolve::CustomOutputOwners {
        command,
        targets: owners
            .iter()
            .map(|&owner| target_ids.get(owner).copied())
            .collect::<Option<_>>()
            .ok_or("Missing the command owner")?,
        explicit_only: false,
        shared_output: false,
    };
    let result = GenerationResult {
        graphs: vec![BuildGraph {
            steps: vec![BuildStep::Custom(CustomStep {
                outputs: vec!["generated".into()],
                invocations: vec![Invocation {
                    argv: vec!["touch".into(), "generated".into()],
                    ..Invocation::default()
                }],
                ..CustomStep::default()
            })],
            custom_output_owners: [(0, owners)].into_iter().collect(),
            target_ids,
            targets,
            ..BuildGraph::default()
        }],
        ..GenerationResult::default()
    };
    let host = PureHost::default();
    generate_single_config(&result, &host, Path::new("/work/build"))?;
    read(&host, "build.ninja")
        .lines()
        .find(|line| line.starts_with("build generated "))
        .map(str::to_owned)
        .ok_or_else(|| "Missing the generated file build edge".into())
}

#[test]
fn continues_ninja_lines_and_keeps_escaped_values() {
    let mut ninja = Ninja::default();
    ninja.0.extend_from_slice(b"ninja_pipe = |\n");
    ninja.phony(&BString::from(b"a b:c$\nrest|here".as_slice()), &[]);
    ninja.binding(b"VAR", b"a b:c$\nrest|here");
    assert_eq!(
        ninja.0,
        b"ninja_pipe = |\nbuild a$ b$:c$$$\nrest${ninja_pipe}here: phony\n  VAR = a b:c$$$\nrest|here\n"
    );
}

#[test]
fn link_byproducts_use_unique_paths_on_the_link_edge() -> Result<(), Box<dyn std::error::Error>> {
    let input = BuildGraph {
        steps: vec![
            BuildStep::Link {
                implicit_inputs: Vec::new(),
                inputs: Vec::new(),
                step: LinkStep {
                    kind: LinkKind::SharedLibrary,
                    language: "C".into(),
                    fragments: Vec::new(),
                    output: "libdemo.so.1".into(),
                },
                invocations: Vec::new(),
            },
            BuildStep::Custom(CustomStep {
                outputs: vec!["check".into()],
                depends: vec!["/work/build/libdemo.so".into()],
                ..CustomStep::default()
            }),
        ],
        targets: vec![GraphTarget {
            name: "libdemo.so".into(),
            steps: vec![0],
            in_all: true,
            ..GraphTarget::default()
        }],
        ..BuildGraph::default()
    };

    for soname in ["/work/build/libdemo.so.1", "/work/build/libdemo.so"] {
        let artifacts = ArtifactSet {
            file: Some("libdemo.so.1".into()),
            soname_file: Some(soname.into()),
            linker_file: Some("./libdemo.so".into()),
            import_file: Some("/work/build/libdemo.lib".into()),
            pdb_file: Some("debug/../libdemo.pdb".into()),
            ..ArtifactSet::default()
        };
        let graph = Graph::new(
            &input,
            &[(b"libdemo.so".as_slice(), &artifacts)].into(),
            Path::new("/work/build"),
            Some(b"Debug"),
        )?;
        let mut ninja = Ninja::default();
        graph.emit(&mut ninja, b"cmake", b"", &[])?;
        graph.emit_entry_aliases(&mut ninja);
        let manifest = String::from_utf8(ninja.0)?;
        assert_eq!(
            manifest,
            concat!(
                "build libdemo.so.1 | libdemo.so libdemo.lib: LINK_SHARED_LIBRARY\n",
                "  COMMAND = : && :\n",
                "  DESC = Linking C shared library libdemo.so.1\n",
                "build check | /work/build/check: phony libdemo.so\n",
                "build libdemo.so$:Debug: phony libdemo.so.1 libdemo.so libdemo.lib\n",
                "build all$:Debug: phony libdemo.so$:Debug\n",
                "build all: phony all$:Debug\n"
            )
        );
    }
    Ok(())
}

#[test]
fn target_dependencies_use_the_library_when_its_name_matches_an_executable_output()
-> Result<(), Box<dyn std::error::Error>> {
    for (config, executable, library_alias) in [
        (None, "sqlite3", "CMakeFiles/rcmake-target-0-2.util"),
        (Some("Debug"), "sqlite3", "sqlite3$:Debug"),
        (
            Some("Debug"),
            "sqlite3:Debug",
            "CMakeFiles/rcmake-target-0-2.util$:Debug",
        ),
    ] {
        let link = |kind, output: &str, inputs| BuildStep::Link {
            implicit_inputs: Vec::new(),
            inputs,
            step: LinkStep {
                kind,
                language: "C".into(),
                fragments: Vec::new(),
                output: output.into(),
            },
            invocations: Vec::new(),
        };
        let occupied_output = qualified(
            &"CMakeFiles/rcmake-target-0-0.util".into(),
            config.map(str::as_bytes),
        );
        let input = BuildGraph {
            steps: vec![
                link(LinkKind::StaticArchive, "libsqlite3.a", Vec::new()),
                BuildStep::Compile {
                    byproducts: Vec::new(),
                    compile_input: None,
                    object: "shell.o".into(),
                    source: "shell.c".into(),
                    invocation: Invocation::default(),
                    depfile: None,
                    modmap: None,
                    dyndep: None,
                },
                link(
                    LinkKind::Executable,
                    executable,
                    vec!["shell.o".into(), "libsqlite3.a".into()],
                ),
                BuildStep::Custom(CustomStep {
                    outputs: vec!["check".into()],
                    byproducts: vec![occupied_output],
                    depends: vec!["sqlite3".into(), format!("./{executable}").into()],
                    ..CustomStep::default()
                }),
            ],
            targets: vec![
                GraphTarget {
                    name: "sqlite3".into(),
                    steps: vec![0],
                    in_all: true,
                    ..GraphTarget::default()
                },
                GraphTarget {
                    name: "sqlite_shell".into(),
                    steps: vec![1, 2],
                    depends: vec![0],
                    in_all: true,
                },
                GraphTarget {
                    name: "CMakeFiles/rcmake-target-0-1.util".into(),
                    ..GraphTarget::default()
                },
            ],
            ..BuildGraph::default()
        };
        let graph = Graph::new(
            &input,
            &Default::default(),
            Path::new("/work/build"),
            config.map(str::as_bytes),
        )?;
        let mut ninja = Ninja::default();
        graph.emit(&mut ninja, b"cmake", b"", &[])?;
        if config.is_some() {
            graph.emit_entry_aliases(&mut ninja);
        }
        let manifest = String::from_utf8(ninja.0)?;
        let executable_node = executable.replace(':', "$:");
        let suffix = config.map_or("", |_| "$:Debug");
        for line in [
            format!("build {library_alias}: phony libsqlite3.a"),
            "build shell.o: COMPILE shell.c".to_owned(),
            format!(
                "build {executable_node}: LINK_EXECUTABLE shell.o libsqlite3.a || {library_alias}"
            ),
            format!("build all{suffix}: phony {library_alias} sqlite_shell{suffix}"),
        ] {
            assert!(
                manifest.lines().any(|candidate| candidate == line),
                "{manifest}"
            );
        }
        assert!(
            manifest.lines().any(|line| {
                line.starts_with("build check | ")
                    && line.ends_with(&format!(": phony {library_alias} {executable_node}"))
            }),
            "{manifest}"
        );
        if config.is_some() && executable != "sqlite3" {
            assert!(manifest.contains(&format!("build sqlite3: phony {library_alias}\n")));
        } else {
            assert!(!manifest.contains("build sqlite3: phony"));
        }
    }
    Ok(())
}

#[test]
fn generator_commands_use_metadata_and_the_requested_build_cache() {
    let host = PureHost::default();
    host.env_set(b"CMAKE_COMMAND", Some(b"/unrelated/cmake"));
    host.env_set(b"CMAKE_CTEST_COMMAND", Some(b"/unrelated/ctest"));
    let dir = Path::new("/work/build");
    host.seed_file(
        &dir.join("CMakeCache.txt"),
        b"CMAKE_COMMAND_OTHER:INTERNAL=ignored\nCMAKE_COMMAND:INTERNAL=/cache tools/cmake\r\nCMAKE_CTEST_COMMAND:INTERNAL=/cache tools/ctest\n",
    ).unwrap_or_else(|error| panic!("{error}"));
    let mut result = GenerationResult::default();
    generate_single_config(&result, &host, dir).unwrap_or_else(|error| panic!("{error}"));
    let build = read(&host, "build.ninja");
    for line in [
        "  COMMAND = '/cache tools/cmake' --regenerate-during-build ",
        "  COMMAND = '/cache tools/cmake' -P cmake_install.cmake",
        "  COMMAND = '/cache tools/cmake' -S",
        "  COMMAND = '/cache tools/ctest' --force-new-ctest-process",
    ] {
        assert!(build.contains(line), "Missing {line:?} in:\n{build}");
    }

    let mut graph = BuildGraph::default();
    graph.config.0 = "Debug".into();
    result.configs.push(graph.config.clone());
    result.graphs.push(graph);
    result.metadata.cmake_command = Some("/current tools/rcmake".into());
    result.metadata.ctest_command = Some("/current tools/ctest".into());
    generate_multi_config(&result, &host, dir).unwrap_or_else(|error| panic!("{error}"));
    for file in ["build.ninja", "build-Debug.ninja"] {
        let build = read(&host, file);
        let line = "  COMMAND = '/current tools/rcmake' -S";
        assert!(build.contains(line), "Missing {line:?} in:\n{build}");
    }
    // The configuration utilities are in the implementation file.
    let implementation = read(&host, "CMakeFiles/impl-Debug.ninja");
    for line in [
        "  COMMAND = '/current tools/rcmake' --regenerate-during-build ",
        "  COMMAND = '/current tools/rcmake' -DCMAKE_INSTALL_CONFIG_NAME=Debug",
        "  COMMAND = '/current tools/ctest' --force-new-ctest-process -C Debug",
    ] {
        assert!(
            implementation.contains(line),
            "Missing {line:?} in:\n{implementation}"
        );
    }
}

#[test]
fn package_targets_use_only_their_own_config_files() -> Result<(), Box<dyn std::error::Error>> {
    let host = PureHost::default();
    let dir = Path::new("/work/build");
    let mut result = GenerationResult::default();
    result.metadata.cmake_command = Some("/tool bin/cmake".into());
    host.seed_file(&dir.join("CPackSourceConfig.cmake"), b"")?;
    generate_single_config(&result, &host, dir)?;
    let build = read(&host, "build.ninja");
    assert!(!build.contains("build package:"));
    assert!(build.contains("build CMakeFiles/package_source.util: CUSTOM_COMMAND\n"));
    assert!(build.contains("'/tool bin/cpack' --config ./CPackSourceConfig.cmake"));

    host.seed_file(
        &dir.join("CMakeCache.txt"),
        b"CMAKE_CPACK_COMMAND:FILEPATH=/tools/packer\nCMAKE_SKIP_PACKAGE_ALL_DEPENDENCY:BOOL=ON\n",
    )?;
    host.seed_file(&dir.join("CPackConfig.cmake"), b"")?;
    generate_single_config(&result, &host, dir)?;
    let build = read(&host, "build.ninja");
    assert!(build.contains("build package: phony CMakeFiles/package.util"));
    assert!(build.contains("build CMakeFiles/package.util: CUSTOM_COMMAND\n"));
    assert!(build.contains("/tools/packer --config ./CPackConfig.cmake"));
    Ok(())
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
        assert!(entry.contains(&format!("build clean: phony clean$:{config}")));
        assert!(entry.ends_with("default all\n"));
        let implementation = read(&host, &format!("CMakeFiles/impl-{config}.ninja"));
        assert!(implementation.starts_with("include CMakeFiles/rules.ninja\n"));
        assert!(implementation.contains(&format!("build app$:{config}: phony\n")));
        assert!(implementation.contains(&format!("build clean$:{config}: CLEAN\n")));
        assert!(implementation.contains(&format!("build all$:{config}: phony app$:{config}")));
    }
}

#[test]
fn module_pipeline_writes_edges_and_target_metadata() -> Result<(), Box<dyn std::error::Error>> {
    use rcmake_cxxmodules::{ModuleVisibility, tdi::read_tdi};
    use rcmake_genex::ConfigId;
    use rcmake_model::{DirId, ProjectModel, Target, TargetKind, TargetOrigin};
    use rcmake_resolve::{
        BmiInstall, CompileUnit, CxxModuleInfo, ModuleUnit, ResolvedTarget, TargetMetadata,
        Toolchain,
    };

    let host = PureHost::default();
    let dir = Path::new("/work/build");
    let mut model = ProjectModel::default();
    model.set_root_dirs("/work/source".into(), "/work/build".into());
    let id = model.add_target(Target::new(
        "producer".into(),
        TargetKind::ObjectLibrary,
        TargetOrigin::Normal,
        DirId::ROOT,
        Default::default(),
        Default::default(),
    ))?;
    let configs = [ConfigId("Debug".into()), ConfigId("Release".into())];
    let mut result = GenerationResult {
        configs: configs.to_vec(),
        ..GenerationResult::default()
    };
    result.metadata.toolchains.insert(
        "CXX".into(),
        Toolchain {
            compiler_id: "Clang".into(),
            module_map_format: Some("clang".into()),
            ..Toolchain::default()
        },
    );
    for config in configs {
        let producer = format!("producer/{}", config.0);
        let consumer = format!("consumer/{}", config.0);
        let stages = |name: &str, base: &str, implicit_inputs| {
            let object: BString = format!("{base}/unit.o").into();
            let ddi: BString = format!("{base}/unit.o.ddi").into();
            let dd: BString = format!("{base}/CXX.dd").into();
            let map: BString = format!("{base}/unit.o.modmap").into();
            let source: BString = format!("/work/source/{name}.cxx").into();
            [
                BuildStep::Scan {
                    lang: "CXX".into(),
                    preprocessed: None,
                    source: source.clone(),
                    object: object.clone(),
                    ddi: ddi.clone(),
                    invocation: Invocation::default(),
                    depfile: Some(format!("{base}/unit.o.ddi.d").into()),
                },
                BuildStep::Collate {
                    lang: "CXX".into(),
                    target: name.into(),
                    ddis: vec![ddi],
                    dd: dd.clone(),
                    modmaps: vec![map.clone()],
                    tdi: format!("{base}/CXXDependInfo.json").into(),
                    depend_info: Box::new(rcmake_resolve::DependInfo::Cxx(Default::default())),
                    modmap_format: Some(rcmake_cxxmodules::ModmapFormat::Clang),
                    implicit_inputs,
                },
                BuildStep::Compile {
                    byproducts: Vec::new(),
                    compile_input: None,
                    object,
                    source,
                    invocation: Invocation::default(),
                    depfile: None,
                    modmap: Some(map),
                    dyndep: Some(dd),
                },
            ]
        };
        let steps = stages("producer", &producer, vec![])
            .into_iter()
            .chain(stages(
                "consumer",
                &consumer,
                vec![format!("{producer}/CXXModules.json").into()],
            ))
            .collect();
        result.graphs.push(BuildGraph {
            config: config.clone(),
            steps,
            targets: vec![
                GraphTarget {
                    name: "producer".into(),
                    steps: vec![0, 1, 2],
                    in_all: true,
                    ..GraphTarget::default()
                },
                GraphTarget {
                    name: "consumer".into(),
                    steps: vec![3, 4, 5],
                    in_all: true,
                    ..GraphTarget::default()
                },
            ],
            ..BuildGraph::default()
        });
        result.targets.push(ResolvedTarget {
            fortran: None,
            id,
            config,
            metadata: TargetMetadata {
                name: "producer".into(),
                kind: TargetKind::ObjectLibrary,
                source_dir: "/work/source/lib".into(),
                binary_dir: "/work/build/lib".into(),
                backtrace: Default::default(),
                folder: None,
                exclude_from_all: false,
                generator_provided: false,
            },
            artifacts: ArtifactSet::default(),
            compile_units: vec![CompileUnit {
                source: rcmake_model::BT::new(
                    "/work/source/producer.cxx".into(),
                    Default::default(),
                ),
                object: format!("{producer}/unit.o").into(),
                scan: true,
                ..CompileUnit::default()
            }],
            compile_groups: vec![],
            link: None,
            dependencies: vec![],
            cxx_modules: Some(CxxModuleInfo {
                units: vec![ModuleUnit {
                    unit: 0,
                    file_set: "modules".into(),
                    public: true,
                    bmi_only: false,
                    ..ModuleUnit::default()
                }],
                module_dir: producer.into(),
                bmi_install: Some(BmiInstall {
                    destination: "lib/modules".into(),
                    component: "Development".into(),
                    ..BmiInstall::default()
                }),
                ..CxxModuleInfo::default()
            }),
            ide: None,
        });
    }
    super::generate_multi_config_with_model(&result, &host, dir, &model)?;
    let rules = read(&host, "CMakeFiles/rules.ninja");
    assert!(rules.contains("rule CXX_SCAN\n"));
    assert!(rules.contains("rule CXX_DYNDEP\n"));
    assert!(
        rules.contains("  rspfile = $out.rsp\n  rspfile_content = $in_newline\n  restat = 1\n")
    );
    for config in ["Debug", "Release"] {
        let manifest = read(&host, &format!("CMakeFiles/impl-{config}.ninja"));
        assert!(manifest.contains(&format!("build consumer/{config}/CXX.dd | consumer/{config}/CXXModules.json consumer/{config}/unit.o.modmap: CXX_DYNDEP consumer/{config}/unit.o.ddi | consumer/{config}/CXXDependInfo.json producer/{config}/CXXModules.json")));
        assert!(manifest.contains(&format!("build producer/{config}/unit.o: CXX_COMPILE_SCANNED /work/source/producer.cxx | producer/{config}/unit.o.modmap || producer/{config}/CXX.dd")));
        assert!(manifest.contains(&format!("  dyndep = producer/{config}/CXX.dd")));
        let producer =
            read_tdi(read(&host, &format!("producer/{config}/CXXDependInfo.json")).as_bytes())?;
        let consumer =
            read_tdi(read(&host, &format!("consumer/{config}/CXXDependInfo.json")).as_bytes())?;
        assert_eq!(producer.config, config);
        assert_eq!(producer.dir_top_src, "/work/source");
        assert_eq!(producer.dir_cur_src, "/work/source/lib");
        assert_eq!(producer.dir_top_bld, "/work/build");
        assert_eq!(producer.dir_cur_bld, "/work/build/lib");
        assert_eq!(
            producer.module_dir,
            format!("/work/build/producer/{config}")
        );
        let module = producer
            .cxx_modules
            .get(format!("producer/{config}/unit.o").as_bytes());
        assert_eq!(
            module.map(|module| module.visibility),
            Some(ModuleVisibility::Public)
        );
        assert_eq!(
            producer.bmi_installation.map(|install| install.component),
            Some("Development".into())
        );
        assert_eq!(
            consumer.linked_target_dirs,
            [BString::from(format!("/work/build/producer/{config}"))]
        );
    }
    Ok(())
}

#[test]
fn prep_targets_preserve_user_outputs_and_select_dependencies()
-> Result<(), Box<dyn std::error::Error>> {
    use rcmake_model::{
        DirId, InstallKind, InstallOptions, InstallRule, ProjectModel, TestEntry, TestKind,
    };
    use rcmake_resolve::{InstallAction, InstallActionKind};

    let host = PureHost::default();
    let dir = Path::new("/work/build");
    let mut model = ProjectModel::default();
    model.set_root_dirs("/work/src".into(), "/work/build".into());
    let test = model.add_test(TestEntry {
        name: "sample".into(),
        dir: DirId::ROOT,
        backtrace: Default::default(),
        command: vec!["echo".into()],
        configurations: vec!["Debug".into()],
        working_directory: None,
        command_expand_lists: false,
        old_style: false,
        policies: Default::default(),
        props: Default::default(),
        build_depends: vec![],
        kind: TestKind::Single,
    })?;
    let test = model.test_mut(test).ok_or("Missing test.")?;
    test.build_depends.push(rcmake_model::BT::new(
        "generated".into(),
        Default::default(),
    ));
    let mut discovery = test.clone();
    discovery.name = "discovery".into();
    discovery.kind = TestKind::Discover(Box::default());
    model.add_test(discovery)?;
    model.add_install(InstallRule {
        dir: DirId::ROOT,
        backtrace: Default::default(),
        kind: InstallKind::Files(vec![rcmake_model::BT::new(
            "/work/build/hidden".into(),
            Default::default(),
        )]),
        options: InstallOptions {
            component: Some("Hidden".into()),
            exclude_from_all: true,
            ..InstallOptions::default()
        },
    });
    let mut result = GenerationResult {
        graphs: vec![BuildGraph {
            steps: vec![BuildStep::Custom(CustomStep {
                outputs: vec![
                    "generated".into(),
                    "hidden".into(),
                    "install_prep/Runtime".into(),
                ],
                ..CustomStep::default()
            })],
            ..BuildGraph::default()
        }],
        ..GenerationResult::default()
    };
    for (component, file) in [("Runtime", "generated"), ("Hidden", "hidden")] {
        let file = BString::from(format!("/work/build/{file}"));
        if component == "Runtime" {
            model.add_install(InstallRule {
                dir: DirId::ROOT,
                backtrace: Default::default(),
                kind: InstallKind::Files(vec![rcmake_model::BT::new(
                    file.clone(),
                    Default::default(),
                )]),
                options: InstallOptions {
                    component: Some(component.into()),
                    ..InstallOptions::default()
                },
            });
        }
        result.install.actions.push(InstallAction {
            all_components: false,
            exclude_from_all: false,
            component: component.into(),
            destination: "share".into(),
            configurations: vec![],
            kind: InstallActionKind::Files {
                file_type: "FILE".into(),
                files: vec![file],
                rename: None,
                permissions: vec![],
            },
            optional: false,
            message: BString::default(),
        });
    }
    super::generate_single_config_with_model(&result, &host, dir, &model)?;
    let build = read(&host, "build.ninja");
    assert!(!build.contains("build install_prep/Runtime:"));
    assert!(build.contains("build install_prep/Hidden: phony hidden\n"));
    assert!(build.contains("build install_prep/all: phony generated\n"));
    assert!(build.contains("build test_prep/sample: phony generated\n"));
    assert!(build.contains("build test_prep/directory_"));
    assert!(build.contains("build test_prep/all: phony test_prep/directory_"));
    assert!(!build.contains("build test_prep/discovery:"));
    assert!(!read(&host, "CMakeFiles/rules.ninja").contains("CXX_SCAN"));

    let release = rcmake_genex::ConfigId("Release".into());
    result.configs = vec![release.clone()];
    if let Some(graph) = result.graphs.first_mut() {
        graph.config = release;
    }
    super::generate_multi_config_with_model(&result, &host, dir, &model)?;
    let release = read(&host, "CMakeFiles/impl-Release.ninja");
    assert!(release.contains("build test_prep/sample$:Release: phony generated\n"));
    let entry = read(&host, "build-Release.ninja");
    assert!(entry.contains("build test_prep/sample: phony test_prep/sample$:Release\n"));
    Ok(())
}

#[test]
fn multi_config_utilities_alias_default_and_cross_configurations()
-> Result<(), Box<dyn std::error::Error>> {
    let host = PureHost::default();
    let root = if cfg!(windows) { "C:/work" } else { "/work" };
    let dir = PathBuf::from(format!("{root}/build"));
    let mut graphs = Vec::new();
    for config in ["Debug", "Release", "RelWithDebInfo"] {
        let mut graph = BuildGraph::default();
        graph.config.0 = config.into();
        graph.targets.push(GraphTarget {
            name: "app".into(),
            in_all: true,
            ..GraphTarget::default()
        });
        graphs.push(graph);
    }
    let result = GenerationResult {
        configs: graphs.iter().map(|graph| graph.config.clone()).collect(),
        graphs,
        ..GenerationResult::default()
    };
    host.seed_file(
        &dir.join("CMakeCache.txt"),
        b"CMAKE_CROSS_CONFIGS:STRING=all\nCMAKE_DEFAULT_CONFIGS:STRING=Debug;Release\n",
    )?;
    generate_multi_config(&result, &host, &dir)?;

    let entry = read_in(&host, &dir, "build.ninja");
    for utility in ["install", "install/local", "test"] {
        assert!(entry.contains(&format!(
            "build {utility}: phony {utility}$:Debug {utility}$:Release\n"
        )));
        assert!(entry.contains(&format!(
            "build {utility}$:all: phony {utility}$:Debug {utility}$:RelWithDebInfo {utility}$:Release\n"
        )));
    }
    assert!(!entry.contains("CMakeFiles/install.util"));

    let implementation = read_in(&host, &dir, "CMakeFiles/impl-Debug.ninja");
    for config in ["Debug", "Release", "RelWithDebInfo"] {
        assert!(implementation.contains(&format!(
            "build CMakeFiles/{config}/install.util: CUSTOM_COMMAND all$:{config}\n"
        )));
        assert!(implementation.contains(&format!(
            "build install$:{config}: phony CMakeFiles/{config}/install.util\n"
        )));
    }
    assert!(implementation.contains("-DCMAKE_INSTALL_CONFIG_NAME=Release -P cmake_install.cmake"));

    let release = read_in(&host, &dir, "build-Release.ninja");
    assert!(release.contains("build install: phony install$:Release\n"));
    assert!(!release.contains("build install$:all:"));
    Ok(())
}

#[test]
fn custom_link_recipe_orders_utility_dependencies_on_the_link_edge()
-> Result<(), Box<dyn std::error::Error>> {
    let input = BuildGraph {
        steps: vec![
            BuildStep::Custom(CustomStep {
                outputs: vec!["bin/app".into()],
                byproducts: vec!["bin/note".into()],
                invocations: vec![Invocation {
                    argv: vec!["cc".into()],
                    shell: "cc".into(),
                    ..Default::default()
                }],
                ..Default::default()
            }),
            BuildStep::Custom(CustomStep {
                outputs: vec!["tool".into()],
                ..Default::default()
            }),
        ],
        targets: vec![
            GraphTarget {
                name: "app".into(),
                depends: vec![1],
                steps: vec![0],
                ..Default::default()
            },
            GraphTarget {
                name: "tool".into(),
                steps: vec![1],
                ..Default::default()
            },
        ],
        ..Default::default()
    };
    let artifacts = ArtifactSet {
        file: Some("bin/app".into()),
        ..Default::default()
    };
    let graph = Graph::new(
        &input,
        &[(b"app".as_slice(), &artifacts)].into(),
        Path::new("/work/build"),
        None,
    )?;
    let mut ninja = Ninja::default();
    graph.emit(&mut ninja, b"cmake", b"", &[])?;
    let manifest = String::from_utf8(ninja.0)?;
    assert!(
        manifest
            .lines()
            .any(|line| line.starts_with("build bin/app ")
                && line.contains("bin/note")
                && line.ends_with("|| tool")),
        "{manifest}"
    );
    Ok(())
}

#[test]
fn directory_all_omits_targets_excluded_by_expressions() -> Result<(), Box<dyn std::error::Error>> {
    use rcmake_model::{DirId, GeneratorKind, ProjectModel, Target, TargetKind, TargetOrigin};

    let root = if cfg!(windows) { "C:/work" } else { "/work" };
    let source = format!("{root}/src");
    let build = format!("{root}/build");
    let host = PureHost::default();
    let mut model = ProjectModel::default();
    model.set_root_dirs(source.as_str().into(), build.as_str().into());
    model.generator = Some(GeneratorKind::Ninja);
    let sub = model.add_directory(
        DirId::ROOT,
        format!("{source}/sub").into(),
        format!("{build}/sub").into(),
        Default::default(),
    )?;
    let mut exe = Target::new(
        "exe".into(),
        TargetKind::Executable,
        TargetOrigin::Normal,
        sub,
        Default::default(),
        Default::default(),
    );
    exe.sources
        .push(rcmake_model::BT::new("main.c".into(), Default::default()));
    exe.props.set("EXCLUDE_FROM_ALL".into(), "$<BOOL:1>".into());
    model.add_target(exe)?;
    host.seed_file(&Path::new(&source).join("sub/main.c"), b"")?;
    let result = rcmake_resolve::resolve(&model, &host, &[])?;
    let dir = Path::new(&build);
    super::generate_single_config_with_model(&result, &host, dir, &model)?;
    let manifest = read_in(&host, dir, "build.ninja");
    assert!(
        manifest.lines().any(|line| line == "build sub/all: phony"),
        "{manifest}"
    );
    Ok(())
}

#[test]
fn custom_command_restat_depends_on_symbolic_outputs_and_byproducts() {
    let host = PureHost::default();
    let steps = [
        ("ordinary", false, false, Vec::new()),
        ("symbolic", true, false, Vec::new()),
        ("later-output", false, true, Vec::new()),
        (
            "symbolic-with-byproduct",
            true,
            false,
            vec!["side-effect".into()],
        ),
    ]
    .into_iter()
    .map(|(name, symbolic, later_output, byproducts)| {
        let mut outputs = vec![name.into()];
        let mut symbolic_outputs = Vec::new();
        if later_output {
            outputs.push("second".into());
            symbolic_outputs.push("second".into());
        }
        BuildStep::Custom(CustomStep {
            outputs,
            symbolic,
            symbolic_outputs,
            byproducts,
            invocations: vec![Invocation {
                shell: "touch output".into(),
                ..Invocation::default()
            }],
            ..CustomStep::default()
        })
    })
    .collect();
    let result = GenerationResult {
        graphs: vec![BuildGraph {
            steps,
            ..BuildGraph::default()
        }],
        ..GenerationResult::default()
    };
    generate_single_config(&result, &host, Path::new("/work/build"))
        .unwrap_or_else(|error| panic!("{error}"));
    let build = read(&host, "build.ninja");
    for (output, restat) in [
        ("ordinary", true),
        ("symbolic", false),
        ("later-output", false),
        ("symbolic-with-byproduct", true),
        ("CMakeFiles/test.util", true),
    ] {
        let edge = build
            .split("build ")
            .find(|edge| {
                edge.strip_prefix(output)
                    .is_some_and(|tail| tail.starts_with([' ', ':']))
            })
            .unwrap_or_else(|| panic!("Missing edge for {output}"));
        assert_eq!(edge.contains("  restat = 1\n"), restat, "{edge}");
    }
}

#[test]
fn ninja_test_depends_on_all_only_if_explicitly_enabled() {
    use rcmake_model::ProjectModel;

    for (skip, dependency) in [(None, ""), (Some("ON"), ""), (Some("OFF"), " all")] {
        let mut model = ProjectModel::default();
        model
            .dir_mut(rcmake_model::DirId::ROOT)
            .vars
            .insert("CMAKE_TESTING_ENABLED".into(), "1".into());
        if let Some(skip) = skip {
            model
                .dir_mut(rcmake_model::DirId::ROOT)
                .vars
                .insert("CMAKE_SKIP_TEST_ALL_DEPENDENCY".into(), skip.into());
        }
        let host = PureHost::default();
        let result = GenerationResult {
            graphs: vec![BuildGraph::default()],
            ..GenerationResult::default()
        };
        super::generate_single_config_with_model(&result, &host, Path::new("/work/build"), &model)
            .unwrap_or_else(|error| panic!("{error}"));
        let build = read(&host, "build.ninja");
        assert!(
            build.contains(&format!(
                "build CMakeFiles/test.util: CUSTOM_COMMAND{dependency}\n"
            )),
            "{build}"
        );
    }
}

#[test]
fn compile_launcher_with_ctest_launch_splits_launcher_binding() {
    use rcmake_model::ProjectModel;

    let mut model = ProjectModel::default();
    model.global.props.set(
        "RULE_LAUNCH_COMPILE".into(),
        "/usr/bin/ctest --launch".into(),
    );
    model
        .dir_mut(rcmake_model::DirId::ROOT)
        .vars
        .insert("CMAKE_CTEST_COMMAND".into(), "/usr/bin/ctest".into());
    let mut graph = BuildGraph {
        steps: vec![BuildStep::Compile {
            byproducts: Vec::new(),
            compile_input: None,
            object: "CMakeFiles/app.dir/main.c.o".into(),
            source: "/work/src/main.c".into(),
            invocation: Invocation {
                shell: "\"/usr/bin/ctest\" --launch --target-name app -- env USED_LAUNCHER=1 cc -c /work/src/main.c -o CMakeFiles/app.dir/main.c.o".into(),
                ..Invocation::default()
            },
            depfile: None,
            modmap: None,
            dyndep: None,
        }],
        targets: vec![GraphTarget {
            name: "app".into(),
            steps: vec![0],
            in_all: true,
            ..GraphTarget::default()
        }],
        ..BuildGraph::default()
    };
    graph.compile_launchers.insert(
        "CMakeFiles/app.dir/main.c.o".into(),
        "env USED_LAUNCHER=1".into(),
    );
    let result = GenerationResult {
        graphs: vec![graph],
        ..GenerationResult::default()
    };
    let host = PureHost::default();
    super::generate_single_config_with_model(&result, &host, Path::new("/work/build"), &model)
        .unwrap_or_else(|error| panic!("{error}"));
    let rules = read(&host, "CMakeFiles/rules.ninja");
    let build = read(&host, "build.ninja");
    assert!(
        rules.contains("rule COMPILE_LAUNCHED\n")
            && rules.contains("\"/usr/bin/ctest\" --launch $LAUNCH_ARGS -- $LAUNCHER $COMMAND"),
        "{rules}"
    );
    assert!(
        build.contains("  LAUNCHER = env USED_LAUNCHER=1\n")
            && build.contains("  LAUNCH_ARGS = --target-name app\n"),
        "{build}"
    );
}

#[test]
fn compile_launcher_preserves_distinct_programs_quoted_separator_and_prefix() {
    use rcmake_model::ProjectModel;

    let mut model = ProjectModel::default();
    model
        .dir_mut(rcmake_model::DirId::ROOT)
        .vars
        .insert("CMAKE_NINJA_OUTPUT_PATH_PREFIX".into(), "sub".into());
    let mut graph = BuildGraph {
        steps: vec![
            BuildStep::Compile {
                byproducts: Vec::new(),
                compile_input: None,
                object: "CMakeFiles/a.dir/a.c.o".into(),
                source: "/work/src/a.c".into(),
                invocation: Invocation {
                    shell: "\"/usr/bin/ctest\" --launch --option \"foo -- bar\" -- env USED_LAUNCHER=1 cc -c /work/src/a.c -o CMakeFiles/a.dir/a.c.o".into(),
                    ..Invocation::default()
                },
                depfile: None,
                modmap: None,
                dyndep: None,
            },
            BuildStep::Compile {
                byproducts: Vec::new(),
                compile_input: None,
                object: "CMakeFiles/b.dir/b.c.o".into(),
                source: "/work/src/b.c".into(),
                invocation: Invocation {
                    shell: "\"/opt/custom/launcher\" --launch --target-name b -- env USED_LAUNCHER=2 cc -c /work/src/b.c -o CMakeFiles/b.dir/b.c.o".into(),
                    ..Invocation::default()
                },
                depfile: None,
                modmap: None,
                dyndep: None,
            },
        ],
        targets: vec![
            GraphTarget {
                name: "a".into(),
                steps: vec![0],
                in_all: true,
                ..GraphTarget::default()
            },
            GraphTarget {
                name: "b".into(),
                steps: vec![1],
                in_all: true,
                ..GraphTarget::default()
            },
        ],
        ..BuildGraph::default()
    };
    graph.compile_launchers.insert(
        "CMakeFiles/a.dir/a.c.o".into(),
        "env USED_LAUNCHER=1".into(),
    );
    graph.compile_launchers.insert(
        "CMakeFiles/b.dir/b.c.o".into(),
        "env USED_LAUNCHER=2".into(),
    );
    let result = GenerationResult {
        graphs: vec![graph],
        ..GenerationResult::default()
    };
    let host = PureHost::default();
    super::generate_single_config_with_model(&result, &host, Path::new("/work/build"), &model)
        .unwrap_or_else(|error| panic!("{error}"));
    let rules = read(&host, "CMakeFiles/rules.ninja");
    let build = read(&host, "build.ninja");
    assert!(
        rules.contains("rule COMPILE_LAUNCHED\n")
            && rules.contains(
                "cd -- /work/build && \"/usr/bin/ctest\" --launch $LAUNCH_ARGS -- $LAUNCHER $COMMAND"
            ),
        "{rules}"
    );
    assert!(
        rules.contains("rule COMPILE_LAUNCHED_1\n")
            && rules.contains(
                "cd -- /work/build && \"/opt/custom/launcher\" --launch $LAUNCH_ARGS -- $LAUNCHER $COMMAND"
            ),
        "{rules}"
    );
    assert!(
        build.contains("build sub/CMakeFiles/a.dir/a.c.o: COMPILE_LAUNCHED /work/src/a.c\n")
            && build.contains("  LAUNCH_ARGS = --option \"foo -- bar\"\n")
            && build.contains("  LAUNCHER = env USED_LAUNCHER=1\n"),
        "{build}"
    );
    assert!(
        build.contains("build sub/CMakeFiles/b.dir/b.c.o: COMPILE_LAUNCHED_1 /work/src/b.c\n")
            && build.contains("  LAUNCH_ARGS = --target-name b\n")
            && build.contains("  LAUNCHER = env USED_LAUNCHER=2\n"),
        "{build}"
    );
}

#[test]
fn compile_launcher_preserves_shell_expansion_in_program_word() {
    use rcmake_model::ProjectModel;

    let mut model = ProjectModel::default();
    model.set_root_dirs("/work/src".into(), "/work/build".into());
    let step = |object: &str, source: &str, shell: &str| BuildStep::Compile {
        object: object.into(),
        byproducts: Vec::new(),
        source: source.into(),
        compile_input: None,
        invocation: Invocation {
            shell: shell.into(),
            ..Invocation::default()
        },
        depfile: None,
        modmap: None,
        dyndep: None,
    };
    let graph = BuildGraph {
        steps: vec![
            step(
                "CMakeFiles/a.dir/a.c.o",
                "/work/src/a.c",
                "$MY_LAUNCHER --launch --target a -- /usr/bin/cc -c a.c -o CMakeFiles/a.dir/a.c.o",
            ),
            step(
                "CMakeFiles/b.dir/b.c.o",
                "/work/src/b.c",
                "~/bin/launcher --launch --target b -- /usr/bin/cc -c b.c -o CMakeFiles/b.dir/b.c.o",
            ),
            step(
                "CMakeFiles/c.dir/c.c.o",
                "/work/src/c.c",
                "\"$QUOTED_VAR\" --launch --target c -- /usr/bin/cc -c c.c -o CMakeFiles/c.dir/c.c.o",
            ),
            step(
                "CMakeFiles/d.dir/d.c.o",
                "/work/src/d.c",
                "/usr/bin/ctest --launch --target d -- /usr/bin/cc -c d.c -o CMakeFiles/d.dir/d.c.o",
            ),
        ],
        targets: vec![GraphTarget {
            name: "demo".into(),
            steps: vec![0, 1, 2, 3],
            in_all: true,
            ..GraphTarget::default()
        }],
        ..BuildGraph::default()
    };
    let result = GenerationResult {
        graphs: vec![graph],
        ..GenerationResult::default()
    };
    let host = PureHost::default();
    super::generate_single_config_with_model(&result, &host, Path::new("/work/build"), &model)
        .unwrap_or_else(|error| panic!("{error}"));
    let rules = read(&host, "CMakeFiles/rules.ninja");
    let build = read(&host, "build.ninja");
    assert!(
        rules.contains("rule COMPILE_LAUNCHED\n")
            && rules.contains("\"/usr/bin/ctest\" --launch $LAUNCH_ARGS -- $LAUNCHER $COMMAND")
            && !rules.contains("COMPILE_LAUNCHED_1")
            && !rules.contains("MY_LAUNCHER")
            && !rules.contains("~/bin/launcher")
            && !rules.contains("QUOTED_VAR"),
        "{rules}"
    );
    assert!(
        build.contains("build CMakeFiles/a.dir/a.c.o: COMPILE /work/src/a.c\n")
            && build.contains("  COMMAND = $$MY_LAUNCHER --launch --target a -- /usr/bin/cc"),
        "{build}"
    );
    assert!(
        build.contains("build CMakeFiles/b.dir/b.c.o: COMPILE /work/src/b.c\n")
            && build.contains("  COMMAND = ~/bin/launcher --launch --target b -- /usr/bin/cc"),
        "{build}"
    );
    assert!(
        build.contains("build CMakeFiles/c.dir/c.c.o: COMPILE /work/src/c.c\n")
            && build.contains("  COMMAND = \"$$QUOTED_VAR\" --launch --target c -- /usr/bin/cc"),
        "{build}"
    );
    assert!(
        build.contains("build CMakeFiles/d.dir/d.c.o: COMPILE_LAUNCHED /work/src/d.c\n"),
        "{build}"
    );
}

#[test]
fn link_depfile_survives_response_file_selection() -> Result<(), Box<dyn std::error::Error>> {
    let dir = Path::new("/work/build");
    let depfile = "CMakeFiles/app.dir/link.d";
    let flag = format!("-Wl,--dependency-file={depfile}");
    let result = GenerationResult {
        graphs: vec![BuildGraph {
            steps: vec![BuildStep::Link {
                inputs: vec!["app.o".into()],
                implicit_inputs: Vec::new(),
                step: LinkStep {
                    kind: LinkKind::Executable,
                    language: "C".into(),
                    fragments: Vec::new(),
                    output: "app".into(),
                },
                invocations: vec![Invocation {
                    argv: vec![
                        "cc".into(),
                        flag.clone().into(),
                        "app.o".into(),
                        "-o".into(),
                        "app".into(),
                    ],
                    shell: format!("cc {flag} app.o -o app").into(),
                    ..Invocation::default()
                }],
            }],
            targets: vec![GraphTarget {
                name: "app".into(),
                steps: vec![0],
                in_all: true,
                ..GraphTarget::default()
            }],
            link_depfiles: [("app".into(), depfile.into())].into(),
            ..BuildGraph::default()
        }],
        ..GenerationResult::default()
    };
    for (force, rule) in [(false, "LINK_DEPFILE"), (true, "LINK_DEPFILE_RSP")] {
        let host = PureHost::default();
        if force {
            host.env_set(b"CMAKE_NINJA_FORCE_RESPONSE_FILE", Some(b"1"));
        }
        generate_single_config(&result, &host, dir)?;
        let build = read(&host, "build.ninja");
        assert!(build.contains(&format!("build app: {rule} app.o")));
        assert!(build.contains(&format!("  DEP_FILE = {depfile}\n")));
        assert!(build.contains(&flag));
        let rules = read(&host, "CMakeFiles/rules.ninja");
        let rule = rules
            .split_once(&format!("\nrule {rule}\n"))
            .ok_or("Missing link rule")?
            .1;
        let rule = rule.split("\n\n").next().ok_or("Empty link rule")?;
        assert!(rule.contains("depfile = $DEP_FILE\n  deps = gcc"));
        assert_eq!(rule.contains("rspfile = $RSP_FILE"), force);
    }
    Ok(())
}

#[test]
fn parallel_emit_matches_serial_across_80_targets() -> Result<(), Box<dyn std::error::Error>> {
    use std::collections::BTreeMap;

    let mut steps = Vec::new();
    let mut targets = Vec::new();
    for i in 0..80usize {
        let obj_a = BString::from(format!("obj/t_{i:02}_a.o"));
        let obj_b = BString::from(format!("obj/t_{i:02}_b.o"));
        let lib = BString::from(format!("libt_{i:02}.a"));
        let step_a = steps.len();
        steps.push(BuildStep::Compile {
            byproducts: Vec::new(),
            compile_input: None,
            object: obj_a.clone(),
            source: format!("/work/src/t_{i:02}_a.c").into(),
            invocation: Invocation {
                shell: format!("cc -c /work/src/t_{i:02}_a.c -o {obj_a}").into(),
                ..Invocation::default()
            },
            depfile: Some(format!("obj/t_{i:02}_a.d").into()),
            modmap: None,
            dyndep: None,
        });
        let step_b = steps.len();
        steps.push(BuildStep::Compile {
            byproducts: Vec::new(),
            compile_input: None,
            object: obj_b.clone(),
            source: format!("/work/src/t_{i:02}_b.c").into(),
            invocation: Invocation {
                shell: format!("cc -c /work/src/t_{i:02}_b.c -o {obj_b}").into(),
                ..Invocation::default()
            },
            depfile: Some(format!("obj/t_{i:02}_b.d").into()),
            modmap: None,
            dyndep: None,
        });
        let link_idx = steps.len();
        steps.push(BuildStep::Link {
            implicit_inputs: Vec::new(),
            inputs: vec![obj_a, obj_b],
            step: LinkStep {
                kind: LinkKind::StaticArchive,
                language: "C".into(),
                fragments: Vec::new(),
                output: lib,
            },
            invocations: vec![Invocation {
                shell: format!("ar qc libt_{i:02}.a obj/t_{i:02}_a.o obj/t_{i:02}_b.o").into(),
                ..Invocation::default()
            }],
        });
        targets.push(GraphTarget {
            name: format!("t_{i:02}").into(),
            steps: vec![step_a, step_b, link_idx],
            depends: if i > 0 { vec![i - 1] } else { Vec::new() },
            in_all: true,
        });
    }
    let build_graph = BuildGraph {
        steps,
        targets,
        ..BuildGraph::default()
    };
    let artifacts = BTreeMap::new();
    let graph = Graph::new(&build_graph, &artifacts, Path::new("/work/build"), None)?;
    let custom_inputs = BTreeMap::new();

    let mut serial = graph.manifest();
    graph.emit_into_with_workers(
        &mut serial,
        b"cmake",
        b"",
        &[],
        None,
        &custom_inputs,
        Some(1),
    )?;
    let mut parallel = graph.manifest();
    graph.emit_into_with_workers(
        &mut parallel,
        b"cmake",
        b"",
        &[],
        None,
        &custom_inputs,
        Some(4),
    )?;

    assert_eq!(serial.0, parallel.0);
    Ok(())
}

#[test]
fn parallel_emit_with_shared_outputs_byproducts_and_aliases_is_deterministic()
-> Result<(), Box<dyn std::error::Error>> {
    use std::collections::{BTreeMap, HashSet};

    let mut steps = Vec::new();
    let mut targets = Vec::new();
    let mut artifact_sets = Vec::new();
    for i in 0..70usize {
        if i == 10 {
            let idx = steps.len();
            steps.push(BuildStep::Custom(CustomStep {
                outputs: vec!["gen_10.c".into()],
                byproducts: vec!["shared_custom_byproduct.h".into()],
                invocations: vec![Invocation {
                    shell: "echo gen".into(),
                    ..Invocation::default()
                }],
                ..CustomStep::default()
            }));
            targets.push(GraphTarget {
                name: format!("t_{i:02}").into(),
                steps: vec![idx],
                depends: Vec::new(),
                in_all: true,
            });
            continue;
        }
        let obj = BString::from(format!("obj/t_{i:02}.o"));
        let lib = BString::from(format!("libt_{i:02}.so.1.0"));
        let compile_idx = steps.len();
        steps.push(BuildStep::Compile {
            byproducts: Vec::new(),
            compile_input: None,
            object: obj.clone(),
            source: format!("/work/src/t_{i:02}.c").into(),
            invocation: Invocation {
                shell: format!("cc -c /work/src/t_{i:02}.c -o {obj}").into(),
                ..Invocation::default()
            },
            depfile: None,
            modmap: None,
            dyndep: None,
        });
        let link_idx = steps.len();
        steps.push(BuildStep::Link {
            implicit_inputs: Vec::new(),
            inputs: vec![obj],
            step: LinkStep {
                kind: LinkKind::SharedLibrary,
                language: "C".into(),
                fragments: Vec::new(),
                output: lib.clone(),
            },
            invocations: vec![Invocation {
                shell: format!("cc -shared -o {lib} obj/t_{i:02}.o").into(),
                ..Invocation::default()
            }],
        });
        if i == 20 {
            artifact_sets.push((
                BString::from(format!("t_{i:02}")),
                ArtifactSet {
                    file: Some(lib),
                    soname_file: Some("libt_20.so.1".into()),
                    linker_file: Some("libt_20.so".into()),
                    ..ArtifactSet::default()
                },
            ));
        }
        targets.push(GraphTarget {
            name: format!("t_{i:02}").into(),
            steps: vec![compile_idx, link_idx],
            depends: Vec::new(),
            in_all: true,
        });
    }
    let build_graph = BuildGraph {
        steps,
        targets,
        ..BuildGraph::default()
    };
    let artifacts: BTreeMap<&[u8], &ArtifactSet> = artifact_sets
        .iter()
        .map(|(name, set)| (name.as_slice(), set))
        .collect();
    let graph = Graph::new(&build_graph, &artifacts, Path::new("/work/build"), None)?;
    let custom_inputs = BTreeMap::new();

    let pre_emitted: HashSet<BString> = [
        BString::from("obj/t_03.o"),
        BString::from("shared_custom_byproduct.h"),
        BString::from("libt_20.so.1"),
        BString::from("t_30"),
    ]
    .into_iter()
    .collect();
    let mut emitted_serial = pre_emitted.clone();
    let mut emitted_parallel = pre_emitted;

    let mut serial = graph.manifest();
    graph.emit_into_with_workers(
        &mut serial,
        b"cmake",
        b"",
        &[],
        Some(&mut emitted_serial),
        &custom_inputs,
        Some(1),
    )?;
    let mut parallel = graph.manifest();
    graph.emit_into_with_workers(
        &mut parallel,
        b"cmake",
        b"",
        &[],
        Some(&mut emitted_parallel),
        &custom_inputs,
        Some(4),
    )?;

    assert_eq!(serial.0, parallel.0);
    assert_eq!(emitted_serial, emitted_parallel);
    let text = String::from_utf8(parallel.0)?;
    assert!(!text.contains("build obj/t_03.o:"));
    assert!(!text.contains("build gen_10.c"));
    assert!(text.contains("build libt_20.so.1.0 | libt_20.so:"));
    assert!(!text.contains("build t_30: phony"));
    Ok(())
}
