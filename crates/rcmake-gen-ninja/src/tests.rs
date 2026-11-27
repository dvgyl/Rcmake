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
    assert!(manifest.contains("build sub/CMakeFiles/edit_cache.util: CUSTOM_COMMAND"));
    assert!(manifest.contains("build sub/edit_cache: phony sub/CMakeFiles/edit_cache.util"));
    assert!(manifest.contains("cd /work/build/sub && /tools/ccmake -S/work/src -B/work/build"));
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
    assert!(rules.contains("rule RERUN_CMAKE\n  command = $COMMAND\n  description = Re-running CMake...\n  generator = 1\n"));
    assert!(!rules.contains("rule RERUN_CMAKE\n  command = $COMMAND\n  description = Re-running CMake...\n  generator = 1\n  pool = console"));
    assert!(!rules.contains("rule RERUN_CMAKE\n  command = $COMMAND\n  description = Re-running CMake...\n  generator = 1\n  restat = 1"));
    assert!(rules.contains("rule CLEAN\n  command = ninja $FILE_ARG -t clean $TARGETS\n  description = Cleaning all built files..."));
    assert!(rules.contains(
        "rule HELP\n  command = ninja -t targets\n  description = All primary targets available:"
    ));
    assert!(rules.contains("pool slow\n  depth = 3"));
    assert!(!rules.contains("pool ignored"));
    let build = read(&host, "build.ninja");
    for line in [
        "include CMakeFiles/rules.ninja",
        "build generated$ $$$:file include${ninja_pipe}generated.h | /work/build/generated$ $$$:file /work/build/include${ninja_pipe}generated.h: CUSTOM_COMMAND /work/config.in",
        r#"  COMMAND = cd "/work/a dir" && printf '%s' '$$value'"#,
        "  DESC = Generate $$: file",
        "  depfile = a $$:file.d",
        "  pool = console",
        "  pool = slow",
        "build cmake_object_order_depends_target_shared: phony || generate",
        "build obj/a$ $$$:file.o: COMPILE_DEPFILE /work/src/a$ $$$:file.c || cmake_object_order_depends_target_shared",
        "  DEP_FILE = obj/a $$:file.d",
        "build libshared.so: LINK_SHARED_LIBRARY obj/a$ $$$:file.o || generate",
        "  DESC = Linking C shared library libshared.so",
        "build CMakeFiles/optional | /work/build/CMakeFiles/optional: CUSTOM_COMMAND",
        "build shared: phony libshared.so",
        "build generate: phony generated$ $$$:file include${ninja_pipe}generated.h",
        "build optional: phony CMakeFiles/optional",
        "build all: phony libshared.so",
        "build build.ninja: RERUN_CMAKE | /work/source$ dir/CMakeLists.txt /work/source$ dir/options$$$:file.cmake",
        "  COMMAND = cmake --regenerate-during-build -S'/work/source dir' -B/work/build",
        "build /work/source$ dir/CMakeLists.txt /work/source$ dir/options$$$:file.cmake: phony",
        "build CMakeFiles/install.util: CUSTOM_COMMAND all",
        "  COMMAND = cd /work/build && cmake -P cmake_install.cmake",
        "build install: phony CMakeFiles/install.util",
        "  COMMAND = cd /work/build && ctest",
        "build test: phony CMakeFiles/test.util",
        "build clean: CLEAN",
        "build help: HELP",
        "  COMMAND = cd /work/build && cmake --regenerate-during-build -S'/work/source dir' -B/work/build",
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
    let rules_file = dir.join("CMakeFiles/rules.ninja");
    let original_manifest_mtime = host
        .metadata(&manifest, Follow::Yes)
        .map(|metadata| metadata.mtime);
    let original_rules_mtime = host
        .metadata(&rules_file, Follow::Yes)
        .map(|metadata| metadata.mtime);
    host.sleep(std::time::Duration::from_secs(1));
    generate_single_config(&result, &host, dir).unwrap_or_else(|error| panic!("{error}"));
    // `build.ninja` is rewritten unconditionally (matching cmGlobalNinjaGenerator::OpenFileStream)
    // so RERUN_CMAKE always advances its mtime past touched inputs, while rules.ninja stays stable.
    assert_ne!(
        original_manifest_mtime,
        host.metadata(&manifest, Follow::Yes)
            .map(|metadata| metadata.mtime)
    );
    assert_eq!(
        original_rules_mtime,
        host.metadata(&rules_file, Follow::Yes)
            .map(|metadata| metadata.mtime)
    );
    assert_eq!(read(&host, "/work/build/build.ninja"), build);
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
                "build cmake_object_order_depends_target_libdemo.so_DEBUG: phony || .\n",
                "build libdemo.so.1 | libdemo.so libdemo.lib: LINK_SHARED_LIBRARY\n",
                "  COMMAND = : && :\n",
                "  DESC = Linking C shared library libdemo.so.1\n",
                "build check | /work/build/check: phony libdemo.so\n",
                "build libdemo.so$:Debug: phony libdemo.so\n",
                "build all$:Debug: phony libdemo.so\n",
                "build all: phony all$:Debug\n"
            )
        );
    }
    Ok(())
}

#[test]
fn target_dependencies_use_the_library_when_its_name_matches_an_executable_output()
-> Result<(), Box<dyn std::error::Error>> {
    for (config, executable, library_alias, check_library) in [
        (None, "sqlite3", None, "libsqlite3.a"),
        (
            Some("Debug"),
            "sqlite3",
            Some("sqlite3$:Debug"),
            "libsqlite3.a",
        ),
        (Some("Debug"), "sqlite3:Debug", None, "libsqlite3.a"),
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
        let order_suffix = config.map_or("", |_| "_DEBUG");
        assert!(
            !manifest.contains("CMakeFiles/rcmake-target-0-2.util"),
            "Must not invent synthetic .util alias for colliding library target:\n{manifest}"
        );
        if let Some(alias) = library_alias {
            assert!(
                manifest
                    .lines()
                    .any(|candidate| candidate == format!("build {alias}: phony libsqlite3.a")),
                "{manifest}"
            );
        } else {
            assert!(
                !manifest.lines().any(|candidate| {
                    candidate.starts_with(&format!("build sqlite3{suffix}: phony"))
                }),
                "Colliding short alias must be omitted:\n{manifest}"
            );
        }
        for line in [
            format!(
                "build cmake_object_order_depends_target_sqlite_shell{order_suffix}: phony || cmake_object_order_depends_target_sqlite3{order_suffix}"
            ),
            format!(
                "build shell.o: COMPILE shell.c || cmake_object_order_depends_target_sqlite_shell{order_suffix}"
            ),
            format!(
                "build {executable_node}: LINK_EXECUTABLE shell.o | libsqlite3.a || libsqlite3.a"
            ),
            format!("build all{suffix}: phony libsqlite3.a {executable_node}"),
        ] {
            assert!(
                manifest.lines().any(|candidate| candidate == line),
                "{manifest}"
            );
        }
        assert!(
            manifest.lines().any(|line| {
                line.starts_with("build check CMakeFiles/rcmake-target-0-0.util")
                    && line.ends_with(&format!(": phony {check_library} {executable_node}"))
            }),
            "{manifest}"
        );
        if config.is_some() && executable != "sqlite3" {
            assert!(
                manifest.contains(&format!("build sqlite3: phony {check_library}\n")),
                "{manifest}"
            );
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
        "  COMMAND = cd /work/build && '/cache tools/cmake' -P cmake_install.cmake",
        "  COMMAND = cd /work/build && '/cache tools/cmake' --regenerate-during-build -S",
        "  COMMAND = cd /work/build && '/cache tools/ctest'\n",
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
        let line =
            "  COMMAND = cd /work/build && '/current tools/rcmake' --regenerate-during-build -S";
        assert!(build.contains(line), "Missing {line:?} in:\n{build}");
    }
    // The configuration utilities are in the implementation file.
    let implementation = read(&host, "CMakeFiles/impl-Debug.ninja");
    for line in [
        "  COMMAND = '/current tools/rcmake' --regenerate-during-build ",
        "  COMMAND = cd /work/build && '/current tools/rcmake' -DCMAKE_INSTALL_CONFIG_NAME=Debug",
        "  COMMAND = cd /work/build && '/current tools/ctest' -C Debug",
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
    let padded = format!("\n{build}");
    for (output, restat) in [
        ("ordinary", true),
        ("symbolic", false),
        ("later-output", false),
        ("symbolic-with-byproduct", true),
        ("CMakeFiles/test.util", true),
    ] {
        let edge = padded
            .split("\nbuild ")
            .find(|edge| {
                edge.strip_prefix(output)
                    .is_some_and(|tail| tail.starts_with([' ', ':']))
            })
            .unwrap_or_else(|| panic!("Missing edge for {output}"));
        assert_eq!(
            edge.lines().any(|line| line == "  restat = 1"),
            restat,
            "{edge}"
        );
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
        build.contains(
            "build sub/CMakeFiles/a.dir/a.c.o: COMPILE_LAUNCHED /work/src/a.c || sub/cmake_object_order_depends_target_a\n"
        )
            && build.contains("  LAUNCH_ARGS = --option \"foo -- bar\"\n")
            && build.contains("  LAUNCHER = env USED_LAUNCHER=1\n"),
        "{build}"
    );
    assert!(
        build.contains(
            "build sub/CMakeFiles/b.dir/b.c.o: COMPILE_LAUNCHED_1 /work/src/b.c || sub/cmake_object_order_depends_target_b\n"
        )
            && build.contains("  LAUNCH_ARGS = --target-name b\n")
            && build.contains("  LAUNCHER = env USED_LAUNCHER=2\n"),
        "{build}"
    );
    assert!(
        build.contains("build sub/all: phony sub/a sub/b\n"),
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
        build.contains(
            "build CMakeFiles/a.dir/a.c.o: COMPILE /work/src/a.c || cmake_object_order_depends_target_demo\n"
        )
            && build.contains("  COMMAND = $$MY_LAUNCHER --launch --target a -- /usr/bin/cc"),
        "{build}"
    );
    assert!(
        build.contains(
            "build CMakeFiles/b.dir/b.c.o: COMPILE /work/src/b.c || cmake_object_order_depends_target_demo\n"
        )
            && build.contains("  COMMAND = ~/bin/launcher --launch --target b -- /usr/bin/cc"),
        "{build}"
    );
    assert!(
        build.contains(
            "build CMakeFiles/c.dir/c.c.o: COMPILE /work/src/c.c || cmake_object_order_depends_target_demo\n"
        )
            && build.contains("  COMMAND = \"$$QUOTED_VAR\" --launch --target c -- /usr/bin/cc"),
        "{build}"
    );
    assert!(
        build.contains(
            "build CMakeFiles/d.dir/d.c.o: COMPILE_LAUNCHED /work/src/d.c || cmake_object_order_depends_target_demo\n"
        ),
        "{build}"
    );
    assert!(build.contains("build all: phony demo\n"), "{build}");
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

#[test]
fn wave_c2_regeneration_globs_and_builtin_utilities() -> Result<(), Box<dyn std::error::Error>> {
    use rcmake_model::{
        DirId, GlobCheck, GlobKey, InstallKind, InstallOptions, InstallRule, ProjectModel,
        TestEntry, TestKind,
    };

    let host = PureHost::default();
    let dir = Path::new("/work/build");
    host.seed_file(&dir.join("CMakeCache.txt"), b"")?;

    let mut model = ProjectModel::default();
    model.set_root_dirs("/work/src".into(), "/work/build".into());
    model
        .dir_mut(DirId::ROOT)
        .vars
        .insert("CMAKE_MAKE_PROGRAM".into(), "/opt/bin/ninja".into());
    model
        .dir_mut(DirId::ROOT)
        .vars
        .insert("CMAKE_TESTING_ENABLED".into(), "1".into());
    let sub = model.add_directory(
        DirId::ROOT,
        "/work/src/sub".into(),
        "/work/build/sub".into(),
        Default::default(),
    )?;
    model.add_test(TestEntry {
        name: "root_test".into(),
        dir: DirId::ROOT,
        backtrace: Default::default(),
        command: vec!["true".into()],
        configurations: vec![],
        working_directory: None,
        command_expand_lists: false,
        old_style: false,
        policies: Default::default(),
        props: Default::default(),
        build_depends: vec![],
        kind: TestKind::Single,
    })?;
    model.add_test(TestEntry {
        name: "sub_test".into(),
        dir: sub,
        backtrace: Default::default(),
        command: vec!["true".into()],
        configurations: vec![],
        working_directory: None,
        command_expand_lists: false,
        old_style: false,
        policies: Default::default(),
        props: Default::default(),
        build_depends: vec![],
        kind: TestKind::Single,
    })?;
    for install_dir in [DirId::ROOT, sub] {
        model.add_install(InstallRule {
            dir: install_dir,
            backtrace: Default::default(),
            kind: InstallKind::Files(vec![rcmake_model::BT::new(
                "/work/src/file.txt".into(),
                Default::default(),
            )]),
            options: InstallOptions::default(),
        });
    }
    model.glob_checks.insert(
        GlobKey {
            expression: "/work/src/*.c".into(),
            recursive: false,
            directories: false,
            follow: false,
            relative: None,
        },
        GlobCheck {
            files: "/work/src/a.c".into(),
            calls: Vec::new(),
        },
    );

    let mut result = GenerationResult {
        graphs: vec![BuildGraph::default()],
        ..GenerationResult::default()
    };
    result.install = rcmake_resolve::lower::lower_install_manifest(&model, &result.targets);
    result.metadata.listfiles = vec![
        "/work/src/CMakeLists.txt".into(),
        "/work/src/sub/CMakeLists.txt".into(),
    ];

    super::generate_single_config_with_model(&result, &host, dir, &model)?;
    let rules = read(&host, "CMakeFiles/rules.ninja");
    let build = read(&host, "build.ninja");

    // E10 + E12 rule shapes.
    assert!(
        rules.contains("rule VERIFY_GLOBS\n  command = $COMMAND\n  description = Re-checking globbed directories...\n  generator = 1\n"),
        "{rules}"
    );
    assert!(
        rules.contains("rule CLEAN\n  command = /opt/bin/ninja $FILE_ARG -t clean $TARGETS\n  description = Cleaning all built files...\n"),
        "{rules}"
    );
    assert!(
        rules.contains("rule HELP\n  command = /opt/bin/ninja -t targets\n  description = All primary targets available:\n"),
        "{rules}"
    );
    assert!(!rules.contains("rule CLEAN_ADDITIONAL"), "{rules}");

    // E2 + E12 regeneration edges.
    assert!(
        build.contains(
            "build CMakeFiles/VerifyGlobs.cmake_force: phony\nbuild CMakeFiles/cmake.verify_globs: VERIFY_GLOBS | CMakeFiles/VerifyGlobs.cmake_force\n"
        ),
        "{build}"
    );
    assert!(
        build.contains("build build.ninja cmake_install.cmake sub/cmake_install.cmake CTestTestfile.cmake sub/CTestTestfile.cmake: RERUN_CMAKE CMakeFiles/cmake.verify_globs | /work/src/CMakeLists.txt /work/src/sub/CMakeLists.txt CMakeCache.txt CMakeFiles/VerifyGlobs.cmake\n"),
        "{build}"
    );
    assert!(
        !build.contains("build build.ninja CMakeFiles/rules.ninja:"),
        "{build}"
    );

    // E1 combined multi-output phony for regeneration inputs.
    assert!(
        build.contains(
            "build /work/src/CMakeLists.txt /work/src/sub/CMakeLists.txt CMakeCache.txt CMakeFiles/VerifyGlobs.cmake: phony\n"
        ),
        "{build}"
    );

    // E4 + E10 utilities.
    for line in [
        "build list_install_components: phony",
        "build sub/list_install_components: phony",
        "build sub/CMakeFiles/test.util: CUSTOM_COMMAND",
        "  COMMAND = cd /work/build/sub && ctest",
        "build sub/CMakeFiles/rebuild_cache.util: CUSTOM_COMMAND",
        "  COMMAND = cd /work/build/sub && cmake --regenerate-during-build -S/work/src -B/work/build",
        "build sub/CMakeFiles/install.util: CUSTOM_COMMAND sub/all",
        "  COMMAND = cd /work/build/sub && cmake -P cmake_install.cmake",
        "build clean: CLEAN",
        "build help: HELP",
    ] {
        assert!(
            build.lines().any(|candidate| candidate == line),
            "Missing {line:?} in:\n{build}"
        );
    }
    assert!(!build.contains("--force-new-ctest-process"), "{build}");

    // E4 edit_cache fallback: escaped spaces and NO pool = console.
    let padded = format!("\n{build}");
    let edit_edge = padded
        .split("\nbuild ")
        .find(|edge| edge.starts_with("CMakeFiles/edit_cache.util:"))
        .ok_or("Missing edit_cache.util edge")?;
    assert!(
        edit_edge.contains(
            r"  COMMAND = cd /work/build && cmake -E echo No\ interactive\ CMake\ dialog\ available."
        ),
        "{edit_edge}"
    );
    assert!(!edit_edge.contains("pool = console"), "{edit_edge}");

    // Configured edit_cache keeps pool = console.
    model
        .dir_mut(DirId::ROOT)
        .vars
        .insert("CMAKE_EDIT_COMMAND".into(), "/tools/ccmake".into());
    super::generate_single_config_with_model(&result, &host, dir, &model)?;
    let configured = format!("\n{}", read(&host, "build.ninja"));
    let configured_edit = configured
        .split("\nbuild ")
        .find(|edge| edge.starts_with("CMakeFiles/edit_cache.util:"))
        .ok_or("Missing configured edit_cache.util edge")?;
    assert!(
        configured_edit.contains("pool = console"),
        "{configured_edit}"
    );
    Ok(())
}

#[test]
fn wave_c2_targets_aliases_order_depends_and_link_edges() -> Result<(), Box<dyn std::error::Error>>
{
    use rcmake_genex::ConfigId;
    use rcmake_model::{
        DirId, InstallKind, InstallOptions, InstallRule, ProjectModel, Target, TargetKind,
        TargetOrigin,
    };
    use rcmake_resolve::{
        FileSetOrder, InstallAction, InstallActionKind, LinkFragment, LinkFragmentRole,
        ResolvedTarget, TargetMetadata,
    };

    let host = PureHost::default();
    let dir = Path::new("/work/build");
    let mut model = ProjectModel::default();
    model.set_root_dirs("/work/src".into(), "/work/build".into());
    let sub = model.add_directory(
        DirId::ROOT,
        "/work/src/sub".into(),
        "/work/build/sub".into(),
        Default::default(),
    )?;

    let mk = |model: &mut ProjectModel, name: &str, kind: TargetKind, target_dir: DirId| {
        model.add_target(Target::new(
            name.into(),
            kind,
            TargetOrigin::Normal,
            target_dir,
            Default::default(),
            Default::default(),
        ))
    };
    let id_util = mk(&mut model, "sub_util", TargetKind::Utility, sub)?;
    let id_obj = mk(&mut model, "sub_obj", TargetKind::ObjectLibrary, sub)?;
    let id_sh = mk(&mut model, "sh", TargetKind::SharedLibrary, sub)?;
    let id_st = mk(&mut model, "st", TargetKind::StaticLibrary, sub)?;
    let id_app = mk(&mut model, "app", TargetKind::Executable, DirId::ROOT)?;
    model.add_install(InstallRule {
        dir: sub,
        backtrace: Default::default(),
        kind: InstallKind::Targets {
            targets: vec![id_sh],
            export: None,
        },
        options: InstallOptions {
            component: Some("Runtime".into()),
            ..InstallOptions::default()
        },
    });

    let mut graph = BuildGraph {
        steps: vec![
            // 0: sub_util empty-output custom command (E3)
            BuildStep::Custom(CustomStep {
                invocations: vec![Invocation {
                    argv: vec!["true".into()],
                    ..Invocation::default()
                }],
                ..CustomStep::default()
            }),
            // 1: sub_obj compile
            BuildStep::Compile {
                byproducts: Vec::new(),
                compile_input: None,
                object: "sub/CMakeFiles/sub_obj.dir/o.c.o".into(),
                source: "/work/src/sub/o.c".into(),
                invocation: Invocation {
                    shell: "cc -c /work/src/sub/o.c -o sub/CMakeFiles/sub_obj.dir/o.c.o".into(),
                    ..Invocation::default()
                },
                depfile: None,
                modmap: None,
                dyndep: None,
            },
            // 2: sh compile
            BuildStep::Compile {
                byproducts: Vec::new(),
                compile_input: None,
                object: "sub/CMakeFiles/sh.dir/sh.c.o".into(),
                source: "/work/src/sub/sh.c".into(),
                invocation: Invocation {
                    shell: "cc -c /work/src/sub/sh.c -o sub/CMakeFiles/sh.dir/sh.c.o".into(),
                    ..Invocation::default()
                },
                depfile: None,
                modmap: None,
                dyndep: None,
            },
            // 3: sh link (real versioned output)
            BuildStep::Link {
                implicit_inputs: Vec::new(),
                inputs: vec!["sub/CMakeFiles/sh.dir/sh.c.o".into()],
                step: LinkStep {
                    kind: LinkKind::SharedLibrary,
                    language: "C".into(),
                    fragments: Vec::new(),
                    output: "sub/libsh.1.2.3.dylib".into(),
                },
                invocations: vec![Invocation {
                    shell: "cc -shared -o sub/libsh.1.2.3.dylib sub/CMakeFiles/sh.dir/sh.c.o"
                        .into(),
                    ..Invocation::default()
                }],
            },
            // 4: sh library symlink (E9: 2 outputs -> double space + && :)
            BuildStep::Custom(CustomStep {
                outputs: vec!["sub/libsh.1.dylib".into(), "sub/libsh.dylib".into()],
                depends: vec!["sub/libsh.1.2.3.dylib".into()],
                invocations: vec![Invocation {
                    argv: vec![
                        "cmake".into(),
                        "-E".into(),
                        "cmake_symlink_library".into(),
                        "sub/libsh.1.2.3.dylib".into(),
                        "sub/libsh.1.dylib".into(),
                        "sub/libsh.dylib".into(),
                    ],
                    ..Invocation::default()
                }],
                ..CustomStep::default()
            }),
            // 5: st public generated header
            BuildStep::Custom(CustomStep {
                outputs: vec!["sub/pub.h".into()],
                invocations: vec![Invocation {
                    argv: vec!["touch".into(), "sub/pub.h".into()],
                    ..Invocation::default()
                }],
                ..CustomStep::default()
            }),
            // 6: st private generated header (CMP0154 _private phony)
            BuildStep::Custom(CustomStep {
                outputs: vec!["sub/priv.h".into()],
                invocations: vec![Invocation {
                    argv: vec!["touch".into(), "sub/priv.h".into()],
                    ..Invocation::default()
                }],
                ..CustomStep::default()
            }),
            // 7: st compile
            BuildStep::Compile {
                byproducts: Vec::new(),
                compile_input: None,
                object: "sub/CMakeFiles/st.dir/st.c.o".into(),
                source: "/work/src/sub/st.c".into(),
                invocation: Invocation {
                    shell: "cc -c /work/src/sub/st.c -o sub/CMakeFiles/st.dir/st.c.o".into(),
                    ..Invocation::default()
                },
                depfile: None,
                modmap: None,
                dyndep: None,
            },
            // 8: st archive
            BuildStep::Link {
                implicit_inputs: Vec::new(),
                inputs: vec!["sub/CMakeFiles/st.dir/st.c.o".into()],
                step: LinkStep {
                    kind: LinkKind::StaticArchive,
                    language: "C".into(),
                    fragments: Vec::new(),
                    output: "sub/libst.a".into(),
                },
                invocations: vec![Invocation {
                    shell: "ar qc sub/libst.a sub/CMakeFiles/st.dir/st.c.o".into(),
                    ..Invocation::default()
                }],
            },
            // 9: app compile
            BuildStep::Compile {
                byproducts: Vec::new(),
                compile_input: None,
                object: "CMakeFiles/app.dir/main.c.o".into(),
                source: "/work/src/main.c".into(),
                invocation: Invocation {
                    shell: "cc -c /work/src/main.c -o CMakeFiles/app.dir/main.c.o".into(),
                    ..Invocation::default()
                },
                depfile: None,
                modmap: None,
                dyndep: None,
            },
            // 10: app link (E8: explicit objs, implicit libs + LINK_DEPENDS + abs full-path lib, order-only + dup sh symlink)
            BuildStep::Link {
                implicit_inputs: vec!["/work/src/script.ld".into()],
                inputs: vec![
                    "sub/CMakeFiles/sub_obj.dir/o.c.o".into(),
                    "CMakeFiles/app.dir/main.c.o".into(),
                    "sub/libsh.1.2.3.dylib".into(),
                    "sub/libst.a".into(),
                ],
                step: LinkStep {
                    kind: LinkKind::Executable,
                    language: "C".into(),
                    fragments: vec![LinkFragment {
                        role: LinkFragmentRole::Libraries,
                        text: rcmake_model::BT::new("/opt/ext/libfoo.a".into(), Default::default()),
                    }],
                    output: "app".into(),
                },
                invocations: vec![Invocation {
                    shell: "cc -o app CMakeFiles/app.dir/main.c.o sub/libsh.1.2.3.dylib sub/libst.a /opt/ext/libfoo.a".into(),
                    ..Invocation::default()
                }],
            },
        ],
        targets: vec![
            GraphTarget {
                name: "sub_util".into(),
                steps: vec![0],
                in_all: true,
                ..GraphTarget::default()
            },
            GraphTarget {
                name: "sub_obj".into(),
                steps: vec![1],
                in_all: true,
                ..GraphTarget::default()
            },
            GraphTarget {
                name: "sh".into(),
                steps: vec![2, 3, 4],
                in_all: true,
                ..GraphTarget::default()
            },
            GraphTarget {
                name: "st".into(),
                steps: vec![5, 6, 7, 8],
                in_all: true,
                ..GraphTarget::default()
            },
            GraphTarget {
                name: "app".into(),
                steps: vec![9, 10],
                depends: vec![0, 1, 2, 3],
                in_all: true,
            },
        ],
        target_ids: vec![id_util, id_obj, id_sh, id_st, id_app],
        utility_steps: [0].into_iter().collect(),
        ..BuildGraph::default()
    };
    graph.file_set_order.insert(
        3,
        FileSetOrder {
            public: [BString::from("sub/pub.h")].into_iter().collect(),
            independent: Default::default(),
        },
    );

    let resolved = |id, name: &str, kind: TargetKind, target_dir: &str, artifacts: ArtifactSet| {
        ResolvedTarget {
            id,
            config: ConfigId::default(),
            metadata: TargetMetadata {
                name: name.into(),
                kind,
                source_dir: format!("/work/src{target_dir}").into(),
                binary_dir: format!("/work/build{target_dir}").into(),
                backtrace: Default::default(),
                folder: None,
                exclude_from_all: false,
                generator_provided: false,
            },
            artifacts,
            compile_units: vec![],
            compile_groups: vec![],
            link: None,
            dependencies: vec![],
            cxx_modules: None,
            fortran: None,
            ide: None,
        }
    };
    let mut result = GenerationResult {
        graphs: vec![graph],
        targets: vec![
            resolved(
                id_util,
                "sub_util",
                TargetKind::Utility,
                "/sub",
                ArtifactSet::default(),
            ),
            resolved(
                id_obj,
                "sub_obj",
                TargetKind::ObjectLibrary,
                "/sub",
                ArtifactSet::default(),
            ),
            resolved(
                id_sh,
                "sh",
                TargetKind::SharedLibrary,
                "/sub",
                ArtifactSet {
                    file: Some("sub/libsh.1.2.3.dylib".into()),
                    soname_file: Some("sub/libsh.1.dylib".into()),
                    linker_file: Some("sub/libsh.dylib".into()),
                    ..ArtifactSet::default()
                },
            ),
            resolved(
                id_st,
                "st",
                TargetKind::StaticLibrary,
                "/sub",
                ArtifactSet {
                    file: Some("sub/libst.a".into()),
                    ..ArtifactSet::default()
                },
            ),
            resolved(
                id_app,
                "app",
                TargetKind::Executable,
                "",
                ArtifactSet {
                    file: Some("app".into()),
                    ..ArtifactSet::default()
                },
            ),
        ],
        ..GenerationResult::default()
    };
    result.install.actions.push(InstallAction {
        all_components: false,
        exclude_from_all: false,
        component: "Runtime".into(),
        destination: "lib".into(),
        configurations: Vec::new(),
        kind: InstallActionKind::Files {
            file_type: "SHARED_LIBRARY".into(),
            files: vec![
                "/work/build/sub/libsh.1.2.3.dylib".into(),
                "/work/build/sub/libsh.1.dylib".into(),
                "/work/build/sub/libsh.dylib".into(),
            ],
            rename: None,
            permissions: Vec::new(),
        },
        optional: false,
        message: BString::default(),
    });

    super::generate_single_config_with_model(&result, &host, dir, &model)?;
    let build = read(&host, "build.ninja");
    let padded = format!("\n{build}");

    // E3: utility custom step output is sub/CMakeFiles/sub_util + abs alias, no restat.
    let util_edge = padded
        .split("\nbuild ")
        .find(|edge| edge.starts_with("sub/CMakeFiles/sub_util |"))
        .ok_or("Missing sub/CMakeFiles/sub_util edge")?;
    assert!(
        util_edge.starts_with(
            "sub/CMakeFiles/sub_util | /work/build/sub/CMakeFiles/sub_util: CUSTOM_COMMAND\n"
        ),
        "{util_edge}"
    );
    assert!(!util_edge.contains("restat = 1"), "{util_edge}");

    // E9: library symlink double space + && :, no implicit outputs, no restat.
    let sym_edge = padded
        .split("\nbuild ")
        .find(|edge| edge.starts_with("sub/libsh.1.dylib sub/libsh.dylib:"))
        .ok_or("Missing library symlink edge")?;
    assert!(
        sym_edge.starts_with(
            "sub/libsh.1.dylib sub/libsh.dylib: CMAKE_SYMLINK_LIBRARY sub/libsh.1.2.3.dylib\n"
        ),
        "{sym_edge}"
    );
    assert!(
        sym_edge.contains(
            "  COMMAND = cmake -E cmake_symlink_library sub/libsh.1.2.3.dylib  sub/libsh.1.dylib sub/libsh.dylib && :\n"
        ),
        "{sym_edge}"
    );
    assert!(!sym_edge.contains("restat = 1"), "{sym_edge}");

    // E5, E6, E7, E8, E11, E14 assertions.
    for line in [
        "build cmake_object_order_depends_target_sub_obj: phony || .",
        "build cmake_object_order_depends_target_sh: phony || .",
        "build cmake_object_order_depends_target_st: phony || sub/pub.h",
        "build cmake_object_order_depends_target_st_private: phony || cmake_object_order_depends_target_st sub/priv.h",
        "build sub/CMakeFiles/st.dir/st.c.o: COMPILE /work/src/sub/st.c || cmake_object_order_depends_target_st_private",
        "build cmake_object_order_depends_target_app: phony || cmake_object_order_depends_target_sh cmake_object_order_depends_target_st cmake_object_order_depends_target_sub_obj sub/sub_util",
        "build CMakeFiles/app.dir/main.c.o: COMPILE /work/src/main.c || cmake_object_order_depends_target_app",
        "build app: LINK_EXECUTABLE sub/CMakeFiles/sub_obj.dir/o.c.o CMakeFiles/app.dir/main.c.o | sub/libsh.1.2.3.dylib sub/libst.a /work/src/script.ld /opt/ext/libfoo.a || sub/libsh.dylib sub/libst.a sub/sub_obj sub/sub_util sub/libsh.dylib",
        "build sub/sub_util: phony sub/CMakeFiles/sub_util",
        "build sub_util: phony sub/sub_util",
        "build sub/sub_obj: phony sub/CMakeFiles/sub_obj.dir/o.c.o",
        "build sub_obj: phony sub/sub_obj",
        "build sh: phony sub/libsh.dylib",
        "build st: phony sub/libst.a",
        "build sub/all: phony sub/sub_util sub/sub_obj sub/libsh.dylib sub/libst.a",
        "build all: phony app sub/all",
        "build install_prep/Runtime: phony sub/libsh.dylib",
    ] {
        assert!(
            build.lines().any(|candidate| candidate == line),
            "Missing {line:?} in:\n{build}"
        );
    }
    Ok(())
}

#[test]
fn wave_c2_executable_symlinks_and_single_call_archive_response()
-> Result<(), Box<dyn std::error::Error>> {
    use rcmake_resolve::{LinkResponse, ResponseQuoting, ResponseRole, ResponseSpan};

    let host = PureHost::default();
    let dir = Path::new("/work/build");

    // 1. Both inline-split and pre-split versioned executables + 1-output library symlink.
    let result = GenerationResult {
        graphs: vec![BuildGraph {
            steps: vec![
                // Inline cmake_symlink_executable + post-build inside BuildStep::Link.
                BuildStep::Link {
                    implicit_inputs: Vec::new(),
                    inputs: vec!["a.o".into()],
                    step: LinkStep {
                        kind: LinkKind::Executable,
                        language: "C".into(),
                        fragments: Vec::new(),
                        output: "bin/inline-1.0".into(),
                    },
                    invocations: vec![
                        Invocation {
                            argv: vec![
                                "cc".into(),
                                "a.o".into(),
                                "-o".into(),
                                "bin/inline-1.0".into(),
                            ],
                            shell: "cc a.o -o bin/inline-1.0".into(),
                            ..Invocation::default()
                        },
                        Invocation {
                            argv: vec![
                                "cmake".into(),
                                "-E".into(),
                                "cmake_symlink_executable".into(),
                                "bin/inline-1.0".into(),
                                "bin/inline".into(),
                            ],
                            shell: "cmake -E cmake_symlink_executable bin/inline-1.0 bin/inline"
                                .into(),
                            ..Invocation::default()
                        },
                        Invocation {
                            argv: vec!["echo".into(), "post".into()],
                            shell: "echo post".into(),
                            ..Invocation::default()
                        },
                    ],
                },
                // Pre-split versioned executable link + separate Custom symlink step (no post-build).
                BuildStep::Link {
                    implicit_inputs: Vec::new(),
                    inputs: vec!["b.o".into()],
                    step: LinkStep {
                        kind: LinkKind::Executable,
                        language: "C".into(),
                        fragments: Vec::new(),
                        output: "bin/split-2.0".into(),
                    },
                    invocations: vec![Invocation {
                        shell: "cc b.o -o bin/split-2.0".into(),
                        ..Invocation::default()
                    }],
                },
                BuildStep::Custom(CustomStep {
                    outputs: vec!["bin/split".into()],
                    depends: vec!["bin/split-2.0".into()],
                    invocations: vec![Invocation {
                        argv: vec![
                            "cmake".into(),
                            "-E".into(),
                            "cmake_symlink_executable".into(),
                            "bin/split-2.0".into(),
                            "bin/split".into(),
                        ],
                        ..Invocation::default()
                    }],
                    ..CustomStep::default()
                }),
                // 1-output library symlink (single space + && :).
                BuildStep::Custom(CustomStep {
                    outputs: vec!["libone.so".into()],
                    depends: vec!["libone.so.1".into()],
                    invocations: vec![Invocation {
                        argv: vec![
                            "cmake".into(),
                            "-E".into(),
                            "cmake_symlink_library".into(),
                            "libone.so.1".into(),
                            "libone.so.1".into(),
                            "libone.so".into(),
                        ],
                        ..Invocation::default()
                    }],
                    ..CustomStep::default()
                }),
            ],
            targets: vec![
                GraphTarget {
                    name: "inline_exe".into(),
                    steps: vec![0],
                    in_all: true,
                    ..GraphTarget::default()
                },
                GraphTarget {
                    name: "split_exe".into(),
                    steps: vec![1, 2],
                    in_all: true,
                    ..GraphTarget::default()
                },
            ],
            ..BuildGraph::default()
        }],
        ..GenerationResult::default()
    };
    generate_single_config(&result, &host, dir)?;
    let rules = read(&host, "CMakeFiles/rules.ninja");
    let build = read(&host, "build.ninja");
    assert!(
        rules.contains("rule CMAKE_SYMLINK_LIBRARY\n  command = $COMMAND\n  description = $DESC\n")
            && !rules.contains(
                "rule CMAKE_SYMLINK_LIBRARY\n  command = $COMMAND\n  description = $DESC\n  restat"
            ),
        "{rules}"
    );
    assert!(
        rules.contains(
            "rule CMAKE_SYMLINK_EXECUTABLE\n  command = $COMMAND\n  description = $DESC\n"
        ),
        "{rules}"
    );
    for line in [
        "build bin/inline-1.0: LINK_EXECUTABLE a.o",
        "  COMMAND = : && cc a.o -o bin/inline-1.0 && :",
        "build bin/inline: CMAKE_SYMLINK_EXECUTABLE bin/inline-1.0",
        "  COMMAND = cmake -E cmake_symlink_executable bin/inline-1.0 bin/inline && echo post",
        "build bin/split: CMAKE_SYMLINK_EXECUTABLE bin/split-2.0",
        "  COMMAND = cmake -E cmake_symlink_executable bin/split-2.0 bin/split && :",
        "build libone.so: CMAKE_SYMLINK_LIBRARY libone.so.1",
        "  COMMAND = cmake -E cmake_symlink_library libone.so.1 libone.so.1 libone.so && :",
        "build inline_exe: phony bin/inline",
        "build split_exe: phony bin/split",
        "build all: phony bin/inline bin/split",
    ] {
        assert!(
            build.lines().any(|candidate| candidate == line),
            "Missing {line:?} in:\n{build}"
        );
    }

    // 2. Single-call static archive response-file sizing:
    //    (a) Forced response file with flag "@" -> STATIC_ARCHIVE_RSP.
    //    (b) Empty flag "" -> no "@" emitted when forced; errors cleanly when oversized.
    let make_archive = |count: usize, flag: &str| {
        let objects: Vec<BString> = (0..count)
            .map(|i| format!("CMakeFiles/big.dir/very_long_archive_unit_name_{i:04}.c.o").into())
            .collect();
        let mut argv = vec![BString::from("ar"), "qc".into(), "libbig.a".into()];
        argv.extend(objects.iter().cloned());
        let prefix_len = b"ar qc libbig.a ".len();
        let mut shell = b"ar qc libbig.a".to_vec();
        for obj in &objects {
            shell.push(b' ');
            shell.extend_from_slice(obj);
        }
        let shell_len = shell.len();
        let argv_end = argv.len();
        GenerationResult {
            graphs: vec![BuildGraph {
                steps: vec![BuildStep::Link {
                    implicit_inputs: Vec::new(),
                    inputs: objects,
                    step: LinkStep {
                        kind: LinkKind::StaticArchive,
                        language: "C".into(),
                        fragments: Vec::new(),
                        output: "libbig.a".into(),
                    },
                    invocations: vec![Invocation {
                        argv,
                        shell: shell.into(),
                        link_response: Some(Box::new(LinkResponse {
                            flag: flag.into(),
                            quoting: ResponseQuoting::Gnu,
                            objects: None,
                            libraries: None,
                            spans: vec![ResponseSpan {
                                role: ResponseRole::Objects,
                                argv: 3..argv_end,
                                shell: prefix_len..shell_len,
                            }],
                            archive_batch: false,
                        })),
                        ..Invocation::default()
                    }],
                }],
                targets: vec![GraphTarget {
                    name: "big".into(),
                    steps: vec![0],
                    in_all: true,
                    ..GraphTarget::default()
                }],
                ..BuildGraph::default()
            }],
            ..GenerationResult::default()
        }
    };

    // (a) Forced response file on single-call archive switches to STATIC_ARCHIVE_RSP.
    let rsp_host = PureHost::default();
    rsp_host.env_set(b"CMAKE_NINJA_FORCE_RESPONSE_FILE", Some(b"1"));
    generate_single_config(&make_archive(4, "@"), &rsp_host, dir)?;
    let rsp_build = read(&rsp_host, "build.ninja");
    assert!(
        rsp_build.contains("build libbig.a: STATIC_ARCHIVE_RSP"),
        "{rsp_build}"
    );
    assert!(rsp_build.contains("  RSP_FILE = "), "{rsp_build}");
    assert!(
        rsp_build
            .contains(r#"  RSP_ARGS = "CMakeFiles/big.dir/very_long_archive_unit_name_0000.c.o""#),
        "{rsp_build}"
    );

    // (b) Toolchain with no response-file flag ("") never emits "@" when forced,
    //     and returns an explicit error instead of panicking when oversized.
    let no_flag_host = PureHost::default();
    no_flag_host.env_set(b"CMAKE_NINJA_FORCE_RESPONSE_FILE", Some(b"1"));
    generate_single_config(&make_archive(4, ""), &no_flag_host, dir)?;
    let no_flag_build = read(&no_flag_host, "build.ninja");
    assert!(
        no_flag_build.contains("build libbig.a: STATIC_ARCHIVE "),
        "{no_flag_build}"
    );
    assert!(!no_flag_build.contains("RSP_FILE"), "{no_flag_build}");
    assert!(!no_flag_build.contains('@'), "{no_flag_build}");

    let tiny_limit_host = PureHost::default();
    let mut commands = crate::commands::Commands::default();
    commands.posix_limit = 128;
    let oversized =
        super::generate_single(&make_archive(10, ""), &tiny_limit_host, dir, None, commands);
    assert!(
        oversized.is_err(),
        "expected oversized single archive without response flag to fail"
    );
    Ok(())
}

#[test]
fn link_absolute_library_fragments_deduplicate_in_insertion_order()
-> Result<(), Box<dyn std::error::Error>> {
    use rcmake_genex::ConfigId;
    use rcmake_resolve::{LinkFragment, LinkFragmentRole};

    let dir = Path::new("/work/build");
    let mut fragments = Vec::new();
    for idx in 0..400 {
        fragments.push(LinkFragment {
            role: LinkFragmentRole::Libraries,
            text: rcmake_model::BT::new(
                format!("/opt/ext/lib{}.a", idx % 50).into(),
                Default::default(),
            ),
        });
    }
    fragments.push(LinkFragment {
        role: LinkFragmentRole::Libraries,
        text: rcmake_model::BT::new("/work/src/script.ld".into(), Default::default()),
    });
    fragments.push(LinkFragment {
        role: LinkFragmentRole::Libraries,
        text: rcmake_model::BT::new("/opt/ext/lib0.a".into(), Default::default()),
    });
    let result = GenerationResult {
        configs: vec![ConfigId("".into())],
        graphs: vec![BuildGraph {
            config: ConfigId("".into()),
            steps: vec![BuildStep::Link {
                implicit_inputs: vec!["/work/src/script.ld".into()],
                inputs: vec!["CMakeFiles/app.dir/main.c.o".into()],
                step: LinkStep {
                    kind: LinkKind::Executable,
                    language: "C".into(),
                    fragments,
                    output: "app".into(),
                },
                invocations: vec![Invocation {
                    shell: "cc -o app CMakeFiles/app.dir/main.c.o".into(),
                    ..Invocation::default()
                }],
            }],
            targets: vec![GraphTarget {
                name: "app".into(),
                steps: vec![0],
                in_all: true,
                ..GraphTarget::default()
            }],
            ..BuildGraph::default()
        }],
        ..GenerationResult::default()
    };
    let host = PureHost::default();
    generate_single_config(&result, &host, dir)?;
    let build = read(&host, "build.ninja");
    let link_line = build
        .lines()
        .find(|line| line.starts_with("build app: "))
        .ok_or("missing app link line")?;
    let (_, implicit_part) = link_line
        .split_once(" | ")
        .ok_or("missing implicit inputs on link line")?;
    let implicit_nodes: Vec<&str> = implicit_part
        .split(" || ")
        .next()
        .unwrap_or_default()
        .split_whitespace()
        .collect();
    let mut expected = vec!["/work/src/script.ld".to_owned()];
    expected.extend((0..50).map(|idx| format!("/opt/ext/lib{idx}.a")));
    let expected_refs: Vec<&str> = expected.iter().map(String::as_str).collect();
    assert_eq!(implicit_nodes, expected_refs);
    Ok(())
}

#[test]
fn utility_custom_target_inherits_transitive_dependency_artifacts_as_order_only()
-> Result<(), Box<dyn std::error::Error>> {
    use rcmake_genex::ConfigId;
    use rcmake_model::{DirId, ProjectModel, Target, TargetKind, TargetOrigin};
    use rcmake_resolve::{ResolvedTarget, TargetMetadata};

    let dir = Path::new("/work/build");
    let mut model = ProjectModel::default();
    model.set_root_dirs("/work/src".into(), "/work/build".into());
    let mk = |model: &mut ProjectModel, name: &str, kind: TargetKind| {
        model.add_target(Target::new(
            name.into(),
            kind,
            TargetOrigin::Normal,
            DirId::ROOT,
            Default::default(),
            Default::default(),
        ))
    };
    let lib_id = mk(&mut model, "jansson", TargetKind::StaticLibrary)?;
    let test_id = mk(&mut model, "test_array", TargetKind::Executable)?;
    let check_id = mk(&mut model, "check", TargetKind::Utility)?;
    let graph = BuildGraph {
        config: ConfigId::default(),
        steps: vec![
            BuildStep::Link {
                implicit_inputs: vec![],
                inputs: vec!["CMakeFiles/jansson.dir/j.c.o".into()],
                step: LinkStep {
                    kind: LinkKind::StaticArchive,
                    language: "C".into(),
                    fragments: vec![],
                    output: "libjansson.a".into(),
                },
                invocations: vec![Invocation {
                    shell: "ar qc libjansson.a CMakeFiles/jansson.dir/j.c.o".into(),
                    ..Invocation::default()
                }],
            },
            BuildStep::Link {
                implicit_inputs: vec!["libjansson.a".into()],
                inputs: vec!["CMakeFiles/test_array.dir/t.c.o".into()],
                step: LinkStep {
                    kind: LinkKind::Executable,
                    language: "C".into(),
                    fragments: vec![],
                    output: "test_array".into(),
                },
                invocations: vec![Invocation {
                    shell: "cc -o test_array CMakeFiles/test_array.dir/t.c.o libjansson.a".into(),
                    ..Invocation::default()
                }],
            },
            BuildStep::Custom(CustomStep {
                outputs: vec!["CMakeFiles/check".into()],
                byproducts: vec![],
                depends: vec!["test_array".into()],
                invocations: vec![Invocation {
                    shell: "ctest".into(),
                    ..Invocation::default()
                }],
                comment: Some("Running tests".into()),
                uses_terminal: false,
                job_pool: None,
                depfile: None,
                symbolic: true,
                symbolic_outputs: Vec::new(),
                target_dependencies: Vec::new(),
            }),
        ],
        targets: vec![
            GraphTarget {
                name: "jansson".into(),
                steps: vec![0],
                depends: vec![],
                in_all: true,
            },
            GraphTarget {
                name: "test_array".into(),
                steps: vec![1],
                depends: vec![0],
                in_all: true,
            },
            GraphTarget {
                name: "check".into(),
                steps: vec![2],
                depends: vec![1],
                in_all: false,
            },
        ],
        target_ids: vec![lib_id, test_id, check_id],
        utility_steps: [2].into_iter().collect(),
        ..BuildGraph::default()
    };
    let resolved = |id, name: &str, kind: TargetKind, output: Option<&str>| ResolvedTarget {
        id,
        config: ConfigId::default(),
        metadata: TargetMetadata {
            name: name.into(),
            kind,
            source_dir: "/work/src".into(),
            binary_dir: "/work/build".into(),
            backtrace: Default::default(),
            folder: None,
            exclude_from_all: false,
            generator_provided: false,
        },
        artifacts: output.map_or_else(ArtifactSet::default, |path| ArtifactSet {
            file: Some(path.into()),
            ..ArtifactSet::default()
        }),
        compile_units: vec![],
        compile_groups: vec![],
        link: None,
        dependencies: vec![],
        cxx_modules: None,
        fortran: None,
        ide: None,
    };
    let result = GenerationResult {
        configs: vec![ConfigId::default()],
        graphs: vec![graph],
        targets: vec![
            resolved(
                lib_id,
                "jansson",
                TargetKind::StaticLibrary,
                Some("libjansson.a"),
            ),
            resolved(
                test_id,
                "test_array",
                TargetKind::Executable,
                Some("test_array"),
            ),
            resolved(check_id, "check", TargetKind::Utility, None),
        ],
        ..GenerationResult::default()
    };
    let host = PureHost::default();
    generate_single_config(&result, &host, dir)?;
    let build = read(&host, "build.ninja");
    let check_line = build
        .lines()
        .find(|line| {
            line.starts_with("build CMakeFiles/check:")
                || line.starts_with("build CMakeFiles/check ")
        })
        .ok_or("missing CMakeFiles/check edge")?;
    assert!(
        check_line.ends_with(": CUSTOM_COMMAND test_array || libjansson.a test_array"),
        "unexpected check edge: {check_line}"
    );
    Ok(())
}

#[test]
fn swift_split_build_emits_all_three_audit_11_ninja_edges_and_rsp()
-> Result<(), Box<dyn std::error::Error>> {
    use rcmake_diag::{PolicyId, PolicyMap, PolicyStatus};
    use rcmake_genex::ConfigId;
    use rcmake_model::{
        BT, DirId, GeneratorKind, LinkItem, ProjectModel, Target, TargetKind, TargetOrigin,
        UsageScope,
    };
    use rcmake_resolve::resolve;

    let host = PureHost::default();
    host.create_dir_all(Path::new("/work/src"))?;
    for src in ["a.swift", "b.swift", "main.swift"] {
        host.write(
            &Path::new("/work/src").join(src),
            b"",
            rcmake_host::WriteMode::Truncate,
        )?;
    }
    let dir = Path::new("/work/build");
    let mut model = ProjectModel::default();
    model.set_root_dirs("/work/src".into(), "/work/build".into());
    model
        .dir_mut(DirId::ROOT)
        .vars
        .insert("CMAKE_HOST_SYSTEM_NAME".into(), "Linux".into());
    model.generator = Some(GeneratorKind::Ninja);
    model
        .global
        .props
        .set("ENABLED_LANGUAGES".into(), "Swift".into());
    let root = model.dir_mut(DirId::ROOT);
    root.vars.insert("PROJECT_NAME".into(), "SwiftApp".into());
    root.vars
        .insert("CMAKE_Swift_COMPILER".into(), "/usr/bin/swiftc".into());
    root.vars
        .insert("CMAKE_Swift_COMPILER_ID".into(), "Apple".into());
    root.vars
        .insert("CMAKE_Swift_COMPILER_VERSION".into(), "6.0".into());
    root.vars.insert(
        "CMAKE_Swift_MODULE_TRIPLE".into(),
        "arm64-apple-macos".into(),
    );
    root.vars
        .insert("CMAKE_Swift_SOURCE_FILE_EXTENSIONS".into(), "swift".into());
    root.vars
        .insert("CMAKE_Swift_OUTPUT_EXTENSION".into(), ".o".into());
    root.vars
        .insert("CMAKE_Swift_DEFINE_FLAG".into(), "-D".into());
    root.vars
        .insert("CMAKE_INCLUDE_FLAG_Swift".into(), "-I ".into());
    root.vars
        .insert("CMAKE_Swift_RESPONSE_FILE_FLAG".into(), "@".into());
    root.vars
        .insert("CMAKE_NINJA_FORCE_RESPONSE_FILE".into(), "1".into());
    root.vars.insert(
        "CMAKE_Swift_COMPILATION_MODE_DEFAULT".into(),
        "incremental".into(),
    );
    root.vars
        .insert("CMAKE_Swift_NUM_THREADS".into(), "4".into());
    root.vars.insert("CMAKE_AR".into(), "/usr/bin/ar".into());
    root.vars.insert(
        "CMAKE_Swift_COMPILE_OBJECT".into(),
        "<CMAKE_Swift_COMPILER> -j 4 -num-threads 4 <DEFINES> <FLAGS> <INCLUDES> <SOURCE>".into(),
    );
    root.vars.insert(
        "CMAKE_Swift_ARCHIVE_CREATE".into(),
        "<CMAKE_AR> crs <TARGET> <OBJECTS>".into(),
    );
    root.vars.insert(
        "CMAKE_Swift_LINK_EXECUTABLE".into(),
        "<CMAKE_Swift_COMPILER> -j 4 -num-threads 4 -emit-executable -o <TARGET> <FLAGS> <OBJECTS> <LINK_FLAGS> <LINK_LIBRARIES>".into(),
    );
    let mut policies = PolicyMap::default();
    for id in [157u16, 195, 214, 215, 216] {
        let _ = policies.set(PolicyId::new(id), PolicyStatus::New);
    }

    let mut lib_a = Target::new(
        "libA".into(),
        TargetKind::StaticLibrary,
        TargetOrigin::Normal,
        DirId::ROOT,
        Default::default(),
        policies,
    );
    lib_a
        .sources
        .push(BT::new("a.swift".into(), Default::default()));
    lib_a
        .props
        .set("Swift_MODULE_DIRECTORY".into(), "/work/build/mods".into());
    model.add_target(lib_a)?;

    let mut lib_b = Target::new(
        "libB".into(),
        TargetKind::StaticLibrary,
        TargetOrigin::Normal,
        DirId::ROOT,
        Default::default(),
        policies,
    );
    let link_item = |name: &str| LinkItem {
        opens_lookup_scope: false,
        closes_lookup_scope: false,
        scope: Some(UsageScope::Private),
        text: BT::new(name.into(), Default::default()),
        lookup_dir: DirId::ROOT,
    };

    lib_b
        .sources
        .push(BT::new("b.swift".into(), Default::default()));
    lib_b
        .usage
        .link_libraries
        .get_or_insert_with(Vec::new)
        .push(link_item("libA"));
    model.add_target(lib_b)?;

    let mut app = Target::new(
        "app".into(),
        TargetKind::Executable,
        TargetOrigin::Normal,
        DirId::ROOT,
        Default::default(),
        policies,
    );
    app.sources
        .push(BT::new("main.swift".into(), Default::default()));
    app.usage
        .link_libraries
        .get_or_insert_with(Vec::new)
        .push(link_item("libB"));
    model.add_target(app)?;

    let result = resolve(&model, &host, &[ConfigId::default()])?;
    super::generate_single_config_with_model(&result, &host, dir, &model)?;
    let build = read(&host, "build.ninja");
    let rules = read(&host, "CMakeFiles/rules.ninja");

    // restat = 1 and response file on Swift rule.
    assert!(rules.contains("restat = 1"), "{rules}");
    assert!(rules.contains("rspfile = $RSP_FILE"), "{rules}");

    // Edge #1: upstream .swiftmodule implicit input on libB module, libB objects, AND consumer app objects!
    let mod_a = "mods/libA.swiftmodule/arm64-apple-macos.swiftmodule";
    let mod_b = "libB.swiftmodule/arm64-apple-macos.swiftmodule";
    let line_matching = |needle: &str| {
        build
            .lines()
            .find(|line| line.starts_with("build ") && line.contains(needle))
            .unwrap_or("")
            .to_owned()
    };
    let lib_a_mod = line_matching(&format!("build {mod_a}:"));
    let lib_a_obj = line_matching("build CMakeFiles/libA.dir/a.swift.o:");
    let lib_a_link = line_matching("build liblibA.a:");
    let lib_b_mod = line_matching(&format!("build {mod_b}:"));
    let lib_b_obj = line_matching("build CMakeFiles/libB.dir/b.swift.o:");
    let lib_b_link = line_matching("build liblibB.a:");
    let app_obj = line_matching("build CMakeFiles/app.dir/main.swift.o:");

    // Edge #1: imported upstream .swiftmodule as implicit inputs (`|`).
    assert!(lib_b_mod.contains(&format!("| {mod_a}")), "{lib_b_mod}");
    assert!(lib_b_obj.contains(&format!("| {mod_a}")), "{lib_b_obj}");
    assert!(app_obj.contains(&format!("| {mod_b}")), "{app_obj}");
    // Edge #2: object-order on both + own module order-only on split objects edge.
    assert!(
        lib_a_mod.contains("|| cmake_object_order_depends_target_libA"),
        "{lib_a_mod}"
    );
    assert!(
        lib_a_obj.contains("||")
            && lib_a_obj.contains("cmake_object_order_depends_target_libA")
            && lib_a_obj.contains(mod_a),
        "{lib_a_obj}"
    );
    assert!(
        lib_b_obj.contains("||")
            && lib_b_obj.contains("cmake_object_order_depends_target_libB")
            && lib_b_obj.contains(mod_b),
        "{lib_b_obj}"
    );
    // Edge #3: final link/archive implicitly depends on own .swiftmodule.
    assert!(lib_a_link.contains(&format!("| {mod_a}")), "{lib_a_link}");
    assert!(lib_b_link.contains(&format!("| {mod_b}")), "{lib_b_link}");
    Ok(())
}

#[test]
fn subdirectory_package_targets_emit_in_single_and_multi_config()
-> Result<(), Box<dyn std::error::Error>> {
    use rcmake_genex::ConfigId;
    use rcmake_model::{DirId, ProjectModel};

    let host = PureHost::default();
    let dir = Path::new("/work/build");
    let mut model = ProjectModel::default();
    model.set_root_dirs("/work/src".into(), "/work/build".into());
    model.add_directory(
        DirId::ROOT,
        "/work/src/sub".into(),
        "/work/build/sub".into(),
        Default::default(),
    )?;
    host.seed_file(&dir.join("CPackConfig.cmake"), b"")?;
    host.seed_file(&dir.join("CPackSourceConfig.cmake"), b"")?;

    let mut result = GenerationResult {
        graphs: vec![BuildGraph::default()],
        ..GenerationResult::default()
    };
    result.metadata.cmake_command = Some("/tools/cmake".into());
    super::generate_single_config_with_model(&result, &host, dir, &model)?;
    let single = read(&host, "build.ninja");
    for line in [
        "build CMakeFiles/package.util: CUSTOM_COMMAND all",
        "  COMMAND = cd /work/build && /tools/cpack --config ./CPackConfig.cmake",
        "build package: phony CMakeFiles/package.util",
        "build CMakeFiles/package_source.util: CUSTOM_COMMAND",
        "  COMMAND = cd /work/build && /tools/cpack --config ./CPackSourceConfig.cmake",
        "build package_source: phony CMakeFiles/package_source.util",
        "build sub/CMakeFiles/package.util: CUSTOM_COMMAND sub/all",
        "build sub/package: phony sub/CMakeFiles/package.util",
        "build sub/CMakeFiles/package_source.util: CUSTOM_COMMAND",
        "build sub/package_source: phony sub/CMakeFiles/package_source.util",
    ] {
        assert!(
            single.lines().any(|candidate| candidate == line),
            "Missing {line:?} in:\n{single}"
        );
    }

    let mut graphs = Vec::new();
    for config in ["Debug", "Release"] {
        graphs.push(BuildGraph {
            config: ConfigId(config.into()),
            ..BuildGraph::default()
        });
    }
    let multi_result = GenerationResult {
        configs: graphs.iter().map(|g| g.config.clone()).collect(),
        graphs,
        metadata: result.metadata,
        ..GenerationResult::default()
    };
    host.seed_file(
        &dir.join("CMakeCache.txt"),
        b"CMAKE_CROSS_CONFIGS:STRING=all\nCMAKE_DEFAULT_CONFIGS:STRING=Debug\n",
    )?;
    super::generate_multi_config_with_model(&multi_result, &host, dir, &model)?;
    let entry = read(&host, "build.ninja");
    for line in [
        "build package: phony package$:Debug",
        "build package$:all: phony package$:Debug package$:Release",
        "build package_source: phony package_source$:Debug",
        "build package_source$:all: phony package_source$:Debug package_source$:Release",
        "build sub/package: phony sub/package$:Debug",
        "build sub/package$:all: phony sub/package$:Debug sub/package$:Release",
        "build sub/package_source: phony sub/package_source$:Debug",
        "build sub/package_source$:all: phony sub/package_source$:Debug sub/package_source$:Release",
    ] {
        assert!(
            entry.lines().any(|candidate| candidate == line),
            "Missing {line:?} in:\n{entry}"
        );
    }
    let impl_debug = read(&host, "CMakeFiles/impl-Debug.ninja");
    assert!(
        impl_debug.contains("build sub/CMakeFiles/Debug/package.util: CUSTOM_COMMAND sub/all$:Debug\n  COMMAND = cd /work/build && /tools/cpack -C Debug --config ./CPackConfig.cmake\n"),
        "{impl_debug}"
    );
    assert!(
        impl_debug.contains("build sub/CMakeFiles/Debug/package_source.util: CUSTOM_COMMAND\n  COMMAND = cd /work/build && /tools/cpack --config ./CPackSourceConfig.cmake\n"),
        "{impl_debug}"
    );
    Ok(())
}

#[test]
fn install_prep_filters_unused_groups_interface_empty_files_and_excluded_subdirs()
-> Result<(), Box<dyn std::error::Error>> {
    use rcmake_model::{
        DirId, InstallKind, InstallOptions, InstallRule, ProjectModel, Target, TargetKind,
        TargetOrigin,
    };
    use rcmake_resolve::{InstallAction, InstallActionKind, InstallEntry};

    let host = PureHost::default();
    let dir = Path::new("/work/build");
    let mut model = ProjectModel::default();
    model.set_root_dirs("/work/src".into(), "/work/build".into());
    let sub_excl = model.add_directory(
        DirId::ROOT,
        "/work/src/excl".into(),
        "/work/build/excl".into(),
        Default::default(),
    )?;
    model.dir_mut(sub_excl).exclude_from_all = true;

    let iface = model.add_target(Target::new(
        "iface".into(),
        TargetKind::InterfaceLibrary,
        TargetOrigin::Normal,
        DirId::ROOT,
        Default::default(),
        Default::default(),
    ))?;
    let stlib = model.add_target(Target::new(
        "stlib".into(),
        TargetKind::StaticLibrary,
        TargetOrigin::Normal,
        DirId::ROOT,
        Default::default(),
        Default::default(),
    ))?;
    let excl_lib = model.add_target(Target::new(
        "excl_lib".into(),
        TargetKind::StaticLibrary,
        TargetOrigin::Normal,
        sub_excl,
        Default::default(),
        Default::default(),
    ))?;

    // Rule 0: interface library with no file sets -> must NOT emit IfaceOnly.
    model.add_install(InstallRule {
        dir: DirId::ROOT,
        backtrace: Default::default(),
        kind: InstallKind::Targets {
            targets: vec![iface],
            export: None,
        },
        options: InstallOptions {
            component: Some("IfaceOnly".into()),
            ..InstallOptions::default()
        },
    });
    // Rule 1: static library naming unused RUNTIME COMPONENT SleefRuntime -> must NOT emit SleefRuntime.
    model.add_install(InstallRule {
        dir: DirId::ROOT,
        backtrace: Default::default(),
        kind: InstallKind::Targets {
            targets: vec![stlib],
            export: None,
        },
        options: InstallOptions {
            component: Some("StlibArchive".into()),
            ..InstallOptions::default()
        },
    });
    for (idx, arg) in [
        "TARGETS",
        "stlib",
        "ARCHIVE",
        "DESTINATION",
        "lib",
        "COMPONENT",
        "StlibArchive",
        "RUNTIME",
        "DESTINATION",
        "bin",
        "COMPONENT",
        "SleefRuntime",
    ]
    .into_iter()
    .enumerate()
    {
        model
            .global
            .props
            .set(format!("_RCMAKE_INSTALL_1_ARG{idx}").into(), arg.into());
    }
    // Rule 2: empty FILES list -> must NOT emit EmptyFiles.
    model.add_install(InstallRule {
        dir: DirId::ROOT,
        backtrace: Default::default(),
        kind: InstallKind::Files(Vec::new()),
        options: InstallOptions {
            component: Some("EmptyFiles".into()),
            ..InstallOptions::default()
        },
    });
    // Rule 3: static library in EXCLUDE_FROM_ALL subdir -> emits empty ExclSub, no deps in all.
    model.add_install(InstallRule {
        dir: sub_excl,
        backtrace: Default::default(),
        kind: InstallKind::Targets {
            targets: vec![excl_lib],
            export: None,
        },
        options: InstallOptions {
            component: Some("ExclSub".into()),
            ..InstallOptions::default()
        },
    });

    let mut result = GenerationResult {
        graphs: vec![BuildGraph {
            steps: vec![
                BuildStep::Link {
                    implicit_inputs: Vec::new(),
                    inputs: Vec::new(),
                    step: LinkStep {
                        kind: LinkKind::StaticArchive,
                        language: "C".into(),
                        fragments: Vec::new(),
                        output: "libstlib.a".into(),
                    },
                    invocations: Vec::new(),
                },
                BuildStep::Link {
                    implicit_inputs: Vec::new(),
                    inputs: Vec::new(),
                    step: LinkStep {
                        kind: LinkKind::StaticArchive,
                        language: "C".into(),
                        fragments: Vec::new(),
                        output: "excl/libexcl_lib.a".into(),
                    },
                    invocations: Vec::new(),
                },
            ],
            targets: vec![
                GraphTarget {
                    name: "stlib".into(),
                    steps: vec![0],
                    in_all: true,
                    ..GraphTarget::default()
                },
                GraphTarget {
                    name: "excl_lib".into(),
                    steps: vec![1],
                    in_all: false,
                    ..GraphTarget::default()
                },
            ],
            ..BuildGraph::default()
        }],
        ..GenerationResult::default()
    };
    result.install.actions = vec![
        InstallAction {
            all_components: false,
            exclude_from_all: false,
            component: "StlibArchive".into(),
            destination: "lib".into(),
            configurations: vec![],
            kind: InstallActionKind::Files {
                file_type: "STATIC_LIBRARY".into(),
                files: vec!["/work/build/libstlib.a".into()],
                rename: None,
                permissions: vec![],
            },
            optional: false,
            message: BString::default(),
        },
        InstallAction {
            all_components: false,
            exclude_from_all: false,
            component: "EmptyFiles".into(),
            destination: "share".into(),
            configurations: vec![],
            kind: InstallActionKind::Files {
                file_type: "FILE".into(),
                files: vec![],
                rename: None,
                permissions: vec![],
            },
            optional: false,
            message: BString::default(),
        },
        InstallAction {
            all_components: false,
            exclude_from_all: false,
            component: "ExclSub".into(),
            destination: "lib".into(),
            configurations: vec![],
            kind: InstallActionKind::Files {
                file_type: "STATIC_LIBRARY".into(),
                files: vec!["/work/build/excl/libexcl_lib.a".into()],
                rename: None,
                permissions: vec![],
            },
            optional: false,
            message: BString::default(),
        },
    ];
    result
        .install
        .directories
        .insert(DirId::ROOT, vec![InstallEntry::Actions(0..2)]);
    result
        .install
        .directories
        .insert(sub_excl, vec![InstallEntry::Actions(2..3)]);

    super::generate_single_config_with_model(&result, &host, dir, &model)?;
    let build = read(&host, "build.ninja");
    assert!(!build.contains("install_prep/IfaceOnly"), "{build}");
    assert!(!build.contains("install_prep/SleefRuntime"), "{build}");
    assert!(!build.contains("install_prep/EmptyFiles"), "{build}");
    assert!(
        build.contains("build install_prep/StlibArchive: phony libstlib.a\n"),
        "{build}"
    );
    assert!(
        build.contains("build install_prep/ExclSub: phony\n"),
        "{build}"
    );
    assert!(
        build.contains("build install_prep/all: phony libstlib.a\n"),
        "{build}"
    );
    Ok(())
}

#[test]
fn custom_command_order_only_phony_depends_cross_dir_and_repeated_link_libs()
-> Result<(), Box<dyn std::error::Error>> {
    use rcmake_model::{
        CustomCommand, CustomCommandKind, DirId, ProjectModel, Target, TargetKind, TargetOrigin,
    };
    use rcmake_resolve::{CustomOutputOwners, LinkFragment, LinkFragmentRole};

    let host = PureHost::default();
    let dir = Path::new("/work/build");
    let mut model = ProjectModel::default();
    model.set_root_dirs("/work/src".into(), "/work/build".into());
    let sub_a = model.add_directory(
        DirId::ROOT,
        "/work/src/subA".into(),
        "/work/build/subA".into(),
        Default::default(),
    )?;
    let sub_b = model.add_directory(
        DirId::ROOT,
        "/work/src/subB".into(),
        "/work/build/subB".into(),
        Default::default(),
    )?;

    let tool_id = model.add_target(Target::new(
        "tool".into(),
        TargetKind::Executable,
        TargetOrigin::Normal,
        DirId::ROOT,
        Default::default(),
        Default::default(),
    ))?;
    let a_hier_id = model.add_target(Target::new(
        "a_hier".into(),
        TargetKind::Utility,
        TargetOrigin::Normal,
        sub_a,
        Default::default(),
        Default::default(),
    ))?;
    let b_hier_id = model.add_target(Target::new(
        "b_hier".into(),
        TargetKind::Utility,
        TargetOrigin::Normal,
        sub_b,
        Default::default(),
        Default::default(),
    ))?;
    let lib_a_id = model.add_target(Target::new(
        "lib_a".into(),
        TargetKind::StaticLibrary,
        TargetOrigin::Normal,
        DirId::ROOT,
        Default::default(),
        Default::default(),
    ))?;
    let app_id = model.add_target(Target::new(
        "app".into(),
        TargetKind::Executable,
        TargetOrigin::Normal,
        DirId::ROOT,
        Default::default(),
        Default::default(),
    ))?;

    let make_cmd = |dir, out: &str| CustomCommand {
        dir,
        kind: CustomCommandKind::Output,
        backtrace: Default::default(),
        policies: Default::default(),
        outputs: vec![rcmake_model::BT::new(out.into(), Default::default())],
        byproducts: Vec::new(),
        depends: Vec::new(),
        main_dependency: None,
        command_lines: Vec::new(),
        working_directory: None,
        comment: None,
        depfile: None,
        job_pool: None,
        implicit_depends: Vec::new(),
        verbatim: false,
        uses_terminal: false,
        command_expand_lists: false,
        job_server_aware: false,
        codegen: false,
    };
    let cmd_a = model.add_command(make_cmd(sub_a, "/work/build/a.txt"))?;
    let cmd_b = model.add_command(make_cmd(sub_b, "/work/build/b.txt"))?;

    let mut graph = BuildGraph {
        steps: vec![
            // 0: tool executable
            BuildStep::Link {
                implicit_inputs: Vec::new(),
                inputs: Vec::new(),
                step: LinkStep {
                    kind: LinkKind::Executable,
                    language: "C".into(),
                    fragments: Vec::new(),
                    output: "tool".into(),
                },
                invocations: Vec::new(),
            },
            // 1: a.txt (defined in subA)
            BuildStep::Custom(CustomStep {
                outputs: vec!["a.txt".into()],
                depends: vec!["tool".into()],
                invocations: vec![Invocation {
                    argv: vec!["tool".into()],
                    ..Invocation::default()
                }],
                ..CustomStep::default()
            }),
            // 2: subA/CMakeFiles/a_hier (utility phony)
            BuildStep::Custom(CustomStep {
                depends: vec!["a.txt".into()],
                ..CustomStep::default()
            }),
            // 3: b.txt (defined in subB, depends on a.txt and tool)
            BuildStep::Custom(CustomStep {
                outputs: vec!["b.txt".into()],
                depends: vec!["tool".into(), "a.txt".into()],
                invocations: vec![Invocation {
                    argv: vec!["tool".into()],
                    ..Invocation::default()
                }],
                ..CustomStep::default()
            }),
            // 4: subB/CMakeFiles/b_hier (utility phony with target-only depend a_hier)
            BuildStep::Custom(CustomStep {
                depends: vec!["b.txt".into(), "a_hier".into()],
                ..CustomStep::default()
            }),
            // 5: lib_a.a
            BuildStep::Link {
                implicit_inputs: Vec::new(),
                inputs: Vec::new(),
                step: LinkStep {
                    kind: LinkKind::StaticArchive,
                    language: "C".into(),
                    fragments: Vec::new(),
                    output: "liba.a".into(),
                },
                invocations: Vec::new(),
            },
            // 6: app linking liba.a twice (e.g. static cycle)
            BuildStep::Link {
                implicit_inputs: Vec::new(),
                inputs: vec!["main.o".into(), "liba.a".into(), "liba.a".into()],
                step: LinkStep {
                    kind: LinkKind::Executable,
                    language: "C".into(),
                    fragments: vec![
                        LinkFragment {
                            role: LinkFragmentRole::Libraries,
                            text: rcmake_model::BT::new("liba.a".into(), Default::default()),
                        },
                        LinkFragment {
                            role: LinkFragmentRole::Libraries,
                            text: rcmake_model::BT::new("liba.a".into(), Default::default()),
                        },
                    ],
                    output: "app".into(),
                },
                invocations: Vec::new(),
            },
        ],
        targets: vec![
            GraphTarget {
                name: "tool".into(),
                steps: vec![0],
                in_all: true,
                ..GraphTarget::default()
            },
            GraphTarget {
                name: "a_hier".into(),
                steps: vec![1, 2],
                depends: vec![0],
                in_all: false,
            },
            GraphTarget {
                name: "b_hier".into(),
                // Resolver traces cross-dir step 1 into b_hier.steps; Ninja must filter by dir.
                steps: vec![1, 3, 4],
                depends: vec![0, 1],
                in_all: false,
            },
            GraphTarget {
                name: "lib_a".into(),
                steps: vec![5],
                in_all: true,
                ..GraphTarget::default()
            },
            GraphTarget {
                name: "app".into(),
                steps: vec![6],
                depends: vec![3],
                in_all: true,
            },
        ],
        target_ids: vec![tool_id, a_hier_id, b_hier_id, lib_a_id, app_id],
        ..BuildGraph::default()
    };
    graph.utility_steps.insert(2);
    graph.utility_steps.insert(4);
    graph.custom_output_owners.insert(
        1,
        CustomOutputOwners {
            command: cmd_a,
            targets: [a_hier_id].into_iter().collect(),
            explicit_only: false,
            shared_output: false,
        },
    );
    graph.custom_output_owners.insert(
        3,
        CustomOutputOwners {
            command: cmd_b,
            targets: [b_hier_id].into_iter().collect(),
            explicit_only: false,
            shared_output: false,
        },
    );

    let result = GenerationResult {
        graphs: vec![graph],
        ..GenerationResult::default()
    };
    super::generate_single_config_with_model(&result, &host, dir, &model)?;
    let build = read(&host, "build.ninja");
    // 1. Custom command keeps tool in BOTH explicit inputs and order_only.
    assert!(
        build.contains("build a.txt | /work/build/a.txt: CUSTOM_COMMAND tool || tool\n"),
        "{build}"
    );
    // 2. Phony utility step moves target-only depends (subA/a_hier) into order_only.
    assert!(
        build.contains(
            "build subB/CMakeFiles/b_hier | /work/build/subB/CMakeFiles/b_hier: phony b.txt || subA/a_hier tool\n"
        ),
        "{build}"
    );
    // 3. Cross-directory custom command output (a.txt from subA) is excluded from subB/b_hier phony.
    assert!(
        build.contains("build subB/b_hier: phony subB/CMakeFiles/b_hier b.txt tool subA/a_hier\n"),
        "{build}"
    );
    // 4. Link edge preserves repeated static library implicit_inputs.
    assert!(
        build.contains("build app: LINK_EXECUTABLE main.o | liba.a liba.a || liba.a\n"),
        "{build}"
    );
    Ok(())
}

#[test]
fn link_custom_step_keeps_pre_link_colon_prefix_when_post_build_present()
-> Result<(), Box<dyn std::error::Error>> {
    use rcmake_genex::ConfigId;
    use rcmake_model::{DirId, ProjectModel, Target, TargetKind, TargetOrigin};
    use rcmake_resolve::{ResolvedTarget, TargetMetadata};

    let host = PureHost::default();
    let dir = Path::new("/work/build");
    let mut model = ProjectModel::default();
    let id = model.add_target(Target::new(
        "btop_test".into(),
        TargetKind::Executable,
        TargetOrigin::Normal,
        DirId::ROOT,
        Default::default(),
        Default::default(),
    ))?;
    let graph = BuildGraph {
        steps: vec![BuildStep::Custom(CustomStep {
            outputs: vec!["tests/btop_test".into()],
            byproducts: vec!["tests/btop.stamp".into()],
            depends: vec!["CMakeFiles/btop_test.dir/main.cpp.o".into()],
            invocations: vec![
                Invocation {
                    argv: vec![
                        "/usr/bin/c++".into(),
                        "CMakeFiles/btop_test.dir/main.cpp.o".into(),
                        "-o".into(),
                        "tests/btop_test".into(),
                    ],
                    shell: "/usr/bin/c++ CMakeFiles/btop_test.dir/main.cpp.o -o tests/btop_test"
                        .into(),
                    working_dir: None,
                    link_response: None,
                },
                Invocation {
                    argv: vec![
                        "cmake".into(),
                        "-E".into(),
                        "touch".into(),
                        "tests/btop.stamp".into(),
                    ],
                    shell: "cmake -E touch tests/btop.stamp".into(),
                    working_dir: Some("/work/build".into()),
                    link_response: None,
                },
            ],
            ..CustomStep::default()
        })],
        targets: vec![GraphTarget {
            name: "btop_test".into(),
            steps: vec![0],
            in_all: true,
            ..GraphTarget::default()
        }],
        ..BuildGraph::default()
    };
    let result = GenerationResult {
        targets: vec![ResolvedTarget {
            fortran: None,
            id,
            config: ConfigId::default(),
            metadata: TargetMetadata {
                name: "btop_test".into(),
                kind: TargetKind::Executable,
                source_dir: "/work/src".into(),
                binary_dir: "/work/build".into(),
                backtrace: Default::default(),
                folder: None,
                exclude_from_all: false,
                generator_provided: false,
            },
            artifacts: ArtifactSet {
                file: Some("/work/build/tests/btop_test".into()),
                ..ArtifactSet::default()
            },
            compile_groups: Vec::new(),
            compile_units: Vec::new(),
            link: None,
            dependencies: Vec::new(),
            cxx_modules: None,
            ide: None,
        }],
        graphs: vec![graph],
        ..GenerationResult::default()
    };
    generate_single_config(&result, &host, dir)?;
    let build = read(&host, "build.ninja");
    assert!(
        build.contains("  COMMAND = : && /usr/bin/c++ CMakeFiles/btop_test.dir/main.cpp.o -o tests/btop_test && cd /work/build && cmake -E touch tests/btop.stamp\n"),
        "custom-lowered link step must preserve `: && ` PRE_LINK prefix:\n{build}"
    );
    assert!(
        build.contains(
            "build tests/btop_test tests/btop.stamp | /work/build/tests/btop.stamp: CUSTOM_COMMAND"
        ),
        "byproduct keeps workdir alias while link output does not:\n{build}"
    );
    Ok(())
}

#[test]
fn single_config_utility_events_emit_util_suffix_and_per_event_cd_chaining()
-> Result<(), Box<dyn std::error::Error>> {
    use rcmake_model::{
        CustomCommand, CustomCommandKind, DirId, GeneratorKind, ProjectModel, Target, TargetEvent,
        TargetKind, TargetOrigin,
    };

    let host = PureHost::default();
    let dir = Path::new("/work/build");
    let mut model = ProjectModel::default();
    model.set_root_dirs("/work/src".into(), "/work/build".into());
    model
        .dir_mut(DirId::ROOT)
        .vars
        .insert("CMAKE_HOST_SYSTEM_NAME".into(), "Linux".into());
    model.generator = Some(GeneratorKind::Ninja);

    let u0 = model.add_target(Target::new(
        "u0".into(),
        TargetKind::Utility,
        TargetOrigin::Normal,
        DirId::ROOT,
        Default::default(),
        Default::default(),
    ))?;
    let mut ev1 = CustomCommand::new(
        DirId::ROOT,
        CustomCommandKind::TargetEvent {
            target: u0,
            event: TargetEvent::PostBuild,
        },
        Default::default(),
        Default::default(),
    );
    ev1.command_lines
        .push(vec!["cmake".into(), "-E".into(), "echo".into(), "a".into()]);
    ev1.command_lines
        .push(vec!["cmake".into(), "-E".into(), "echo".into(), "b".into()]);
    model.add_command(ev1)?;
    let mut ev2 = CustomCommand::new(
        DirId::ROOT,
        CustomCommandKind::TargetEvent {
            target: u0,
            event: TargetEvent::PostBuild,
        },
        Default::default(),
        Default::default(),
    );
    ev2.command_lines
        .push(vec!["cmake".into(), "-E".into(), "echo".into(), "c".into()]);
    model.add_command(ev2)?;

    let u2 = model.add_target(Target::new(
        "u2".into(),
        TargetKind::Utility,
        TargetOrigin::Normal,
        DirId::ROOT,
        Default::default(),
        Default::default(),
    ))?;
    let mut base = CustomCommand::new(
        DirId::ROOT,
        CustomCommandKind::Utility(u2),
        Default::default(),
        Default::default(),
    );
    base.command_lines.push(vec![
        "cmake".into(),
        "-E".into(),
        "echo".into(),
        "main".into(),
    ]);
    model.add_command(base)?;
    let mut post2 = CustomCommand::new(
        DirId::ROOT,
        CustomCommandKind::TargetEvent {
            target: u2,
            event: TargetEvent::PostBuild,
        },
        Default::default(),
        Default::default(),
    );
    post2.command_lines.push(vec![
        "cmake".into(),
        "-E".into(),
        "echo".into(),
        "post2".into(),
    ]);
    model.add_command(post2)?;

    let result = rcmake_resolve::resolve(&model, &host, &[])?;
    super::generate_single_config_with_model(&result, &host, dir, &model)?;
    let build = read(&host, "build.ninja");
    assert!(
        build.contains("build CMakeFiles/u0.util: CUSTOM_COMMAND\n  restat = 1\n  COMMAND = cd /work/build && cmake -E echo a && cmake -E echo b && cd /work/build && cmake -E echo c\n"),
        "u0 must emit .util without workdir alias and chain per-event cd:\n{build}"
    );
    assert!(
        build.contains("build u0: phony CMakeFiles/u0.util\n"),
        "u0 phony must point to CMakeFiles/u0.util:\n{build}"
    );
    assert!(
        build.contains("build CMakeFiles/u2 | /work/build/CMakeFiles/u2: CUSTOM_COMMAND\n"),
        "u2 base utility keeps CMakeFiles/u2 with workdir alias:\n{build}"
    );
    assert!(
        build.contains("build CMakeFiles/u2.util: CUSTOM_COMMAND CMakeFiles/u2\n  restat = 1\n"),
        "u2 post-build event emits CMakeFiles/u2.util depending on CMakeFiles/u2:\n{build}"
    );
    assert!(
        build.contains("build u2: phony CMakeFiles/u2.util\n"),
        "u2 phony must point only to CMakeFiles/u2.util:\n{build}"
    );
    Ok(())
}

#[test]
fn orphan_custom_output_command_emits_no_ninja_edge() -> Result<(), Box<dyn std::error::Error>> {
    use rcmake_model::{
        BT, CustomCommand, CustomCommandKind, DirId, GeneratorKind, ProjectModel, Target,
        TargetKind, TargetOrigin,
    };

    let host = PureHost::default();
    let dir = Path::new("/work/build");
    let mut model = ProjectModel::default();
    model.set_root_dirs("/work/src".into(), "/work/build".into());
    model
        .dir_mut(DirId::ROOT)
        .vars
        .insert("CMAKE_HOST_SYSTEM_NAME".into(), "Linux".into());
    model.generator = Some(GeneratorKind::Ninja);

    let mut orphan = CustomCommand::new(
        DirId::ROOT,
        CustomCommandKind::Output,
        Default::default(),
        Default::default(),
    );
    orphan
        .outputs
        .push(BT::new("/work/build/orphan.txt".into(), Default::default()));
    orphan.command_lines.push(vec![
        "cmake".into(),
        "-E".into(),
        "touch".into(),
        "orphan.txt".into(),
    ]);
    model.add_command(orphan)?;

    let mut used = CustomCommand::new(
        DirId::ROOT,
        CustomCommandKind::Output,
        Default::default(),
        Default::default(),
    );
    used.outputs
        .push(BT::new("/work/build/used.txt".into(), Default::default()));
    used.command_lines.push(vec![
        "cmake".into(),
        "-E".into(),
        "touch".into(),
        "used.txt".into(),
    ]);
    model.add_command(used)?;

    let consumer = model.add_target(Target::new(
        "consumer".into(),
        TargetKind::Utility,
        TargetOrigin::Normal,
        DirId::ROOT,
        Default::default(),
        Default::default(),
    ))?;
    let mut util = CustomCommand::new(
        DirId::ROOT,
        CustomCommandKind::Utility(consumer),
        Default::default(),
        Default::default(),
    );
    util.depends
        .push(BT::new("/work/build/used.txt".into(), Default::default()));
    model.add_command(util)?;

    let result = rcmake_resolve::resolve(&model, &host, &[])?;
    super::generate_single_config_with_model(&result, &host, dir, &model)?;
    let build = read(&host, "build.ninja");
    assert!(
        !build.contains("orphan.txt"),
        "unreferenced custom output must not emit a Ninja edge:\n{build}"
    );
    assert!(
        build.contains("build used.txt | /work/build/used.txt: CUSTOM_COMMAND"),
        "referenced custom output must still emit its Ninja edge:\n{build}"
    );
    Ok(())
}

#[test]
fn link_depends_no_shared_and_single_config_clean_additional()
-> Result<(), Box<dyn std::error::Error>> {
    use rcmake_model::{DirId, ImportedVisibility, ProjectModel, Target, TargetKind, TargetOrigin};
    use rcmake_resolve::{LinkFragment, LinkFragmentRole};

    let host = PureHost::default();
    let dir = Path::new("/work/build");
    let mut model = ProjectModel::default();
    model.set_root_dirs("/work/src".into(), "/work/build".into());
    model
        .dir_mut(DirId::ROOT)
        .props
        .set("ADDITIONAL_CLEAN_FILES".into(), "a b.tmp;c.tmp".into());

    let s1_id = model.add_target(Target::new(
        "s1".into(),
        TargetKind::SharedLibrary,
        TargetOrigin::Normal,
        DirId::ROOT,
        Default::default(),
        Default::default(),
    ))?;
    let s2_id = model.add_target(Target::new(
        "s2".into(),
        TargetKind::SharedLibrary,
        TargetOrigin::Normal,
        DirId::ROOT,
        Default::default(),
        Default::default(),
    ))?;
    let mut imp_sh = Target::new(
        "imp_sh".into(),
        TargetKind::SharedLibrary,
        TargetOrigin::Imported(ImportedVisibility::Global),
        DirId::ROOT,
        Default::default(),
        Default::default(),
    );
    imp_sh
        .props
        .set("IMPORTED_LOCATION".into(), "/opt/lib/libimp.so".into());
    model.add_target(imp_sh)?;
    let mut imp_unk = Target::new(
        "imp_unk".into(),
        TargetKind::UnknownLibrary,
        TargetOrigin::Imported(ImportedVisibility::Global),
        DirId::ROOT,
        Default::default(),
        Default::default(),
    );
    imp_unk
        .props
        .set("IMPORTED_LOCATION".into(), "/opt/lib/libunk.so".into());
    model.add_target(imp_unk)?;
    let stat_id = model.add_target(Target::new(
        "stat".into(),
        TargetKind::StaticLibrary,
        TargetOrigin::Normal,
        DirId::ROOT,
        Default::default(),
        Default::default(),
    ))?;
    let mut app_raw = Target::new(
        "app".into(),
        TargetKind::Executable,
        TargetOrigin::Normal,
        DirId::ROOT,
        Default::default(),
        Default::default(),
    );
    app_raw
        .props
        .set("LINK_DEPENDS_NO_SHARED".into(), "ON".into());
    let app_id = model.add_target(app_raw)?;

    let graph = BuildGraph {
        steps: vec![
            BuildStep::Link {
                implicit_inputs: Vec::new(),
                inputs: vec!["s1.o".into()],
                step: LinkStep {
                    kind: LinkKind::SharedLibrary,
                    language: "C".into(),
                    fragments: Vec::new(),
                    output: "libs1.so".into(),
                },
                invocations: vec![Invocation::default()],
            },
            BuildStep::Link {
                implicit_inputs: Vec::new(),
                inputs: vec!["s2.o".into()],
                step: LinkStep {
                    kind: LinkKind::SharedLibrary,
                    language: "C".into(),
                    fragments: Vec::new(),
                    output: "libs2.so".into(),
                },
                invocations: vec![Invocation::default()],
            },
            BuildStep::Link {
                implicit_inputs: Vec::new(),
                inputs: vec!["stat.o".into()],
                step: LinkStep {
                    kind: LinkKind::StaticArchive,
                    language: "C".into(),
                    fragments: Vec::new(),
                    output: "libstat.a".into(),
                },
                invocations: vec![Invocation::default()],
            },
            BuildStep::Link {
                implicit_inputs: vec!["libs2.so".into()],
                inputs: vec![
                    "main.o".into(),
                    "libs1.so".into(),
                    "libs2.so".into(),
                    "/opt/lib/libimp.so".into(),
                    "/opt/lib/libunk.so".into(),
                    "libstat.a".into(),
                ],
                step: LinkStep {
                    kind: LinkKind::Executable,
                    language: "C".into(),
                    fragments: vec![LinkFragment {
                        role: LinkFragmentRole::Libraries,
                        text: rcmake_model::BT::new(
                            "/opt/lib/libext.so.1".into(),
                            Default::default(),
                        ),
                    }],
                    output: "app".into(),
                },
                invocations: vec![Invocation::default()],
            },
        ],
        targets: vec![
            GraphTarget {
                name: "s1".into(),
                steps: vec![0],
                in_all: true,
                ..GraphTarget::default()
            },
            GraphTarget {
                name: "s2".into(),
                steps: vec![1],
                in_all: true,
                ..GraphTarget::default()
            },
            GraphTarget {
                name: "stat".into(),
                steps: vec![2],
                in_all: true,
                ..GraphTarget::default()
            },
            GraphTarget {
                name: "app".into(),
                steps: vec![3],
                depends: vec![0, 1, 2],
                in_all: true,
            },
        ],
        target_ids: vec![s1_id, s2_id, stat_id, app_id],
        ..BuildGraph::default()
    };
    let result = GenerationResult {
        graphs: vec![graph],
        ..GenerationResult::default()
    };
    super::generate_single_config_with_model(&result, &host, dir, &model)?;
    let build = read(&host, "build.ninja");
    let rules = read(&host, "CMakeFiles/rules.ninja");
    let script = read(&host, "CMakeFiles/clean_additional.cmake");

    let app_edge = build
        .lines()
        .find(|line| line.starts_with("build app: LINK_EXECUTABLE"))
        .ok_or("missing app link edge")?;
    let before_oo = app_edge.split("||").next().unwrap_or("");
    let implicit_part = before_oo.split('|').nth(1).unwrap_or("");
    let oo = app_edge.split("||").nth(1).unwrap_or("");

    // Normal and imported SHARED targets without explicit LINK_DEPENDS move to order-only.
    assert!(
        !implicit_part.contains("libs1.so") && !implicit_part.contains("/opt/lib/libimp.so"),
        "shared library targets must not appear in implicit inputs when LINK_DEPENDS_NO_SHARED=ON:\n{app_edge}"
    );
    assert!(
        oo.contains("libs1.so") && oo.contains("/opt/lib/libimp.so"),
        "shared library targets must move to order-only inputs:\n{app_edge}"
    );
    // Static libraries, direct raw shared library paths, UNKNOWN imported targets, and
    // explicit LINK_DEPENDS on a linked shared target all stay in implicit inputs.
    for expected in [
        "libstat.a",
        "libs2.so",
        "/opt/lib/libunk.so",
        "/opt/lib/libext.so.1",
    ] {
        assert!(
            implicit_part.contains(expected),
            "{expected} must stay in implicit inputs:\n{app_edge}"
        );
    }

    assert!(
        rules.contains("rule CLEAN_ADDITIONAL\n  command = cmake -DCONFIG=$CONFIG -P CMakeFiles/clean_additional.cmake\n  description = Cleaning additional files...\n"),
        "{rules}"
    );
    assert!(
        build.contains("build CMakeFiles/clean.additional: CLEAN_ADDITIONAL\n")
            && build.contains("build clean: CLEAN CMakeFiles/clean.additional\n"),
        "{build}"
    );
    assert!(
        script.contains("# Additional clean files\ncmake_minimum_required(VERSION 3.16)\n\nif(\"${CONFIG}\" STREQUAL \"\" OR \"${CONFIG}\" STREQUAL \"\")\n  file(REMOVE_RECURSE\n  \"a b.tmp\"\n  \"c.tmp\"\n  )\nendif()\n"),
        "{script}"
    );
    Ok(())
}

#[test]
fn prefixed_clean_additional_and_cache_only_testing_enabled()
-> Result<(), Box<dyn std::error::Error>> {
    use rcmake_model::{CacheEntry, CacheType, DirId, ProjectModel};

    // 4a: CMAKE_NINJA_OUTPUT_PATH_PREFIX prefixes and shell-quotes CLEAN_ADDITIONAL script path.
    for (prefix, expected_arg) in [
        ("sub/", "sub/CMakeFiles/clean_additional.cmake"),
        ("sub dir/", "'sub dir/CMakeFiles/clean_additional.cmake'"),
    ] {
        let host = PureHost::default();
        let dir = Path::new("/work/build");
        let mut model = ProjectModel::default();
        model.set_root_dirs("/work/src".into(), "/work/build".into());
        model
            .dir_mut(DirId::ROOT)
            .vars
            .insert("CMAKE_NINJA_OUTPUT_PATH_PREFIX".into(), prefix.into());
        model
            .dir_mut(DirId::ROOT)
            .props
            .set("ADDITIONAL_CLEAN_FILES".into(), "extra.tmp".into());
        let result = GenerationResult {
            graphs: vec![BuildGraph::default()],
            ..GenerationResult::default()
        };
        super::generate_single_config_with_model(&result, &host, dir, &model)?;
        let rules = read(&host, "CMakeFiles/rules.ninja");
        let expected = format!(
            "rule CLEAN_ADDITIONAL\n  command = cmake -DCONFIG=$CONFIG -P {expected_arg}\n  description = Cleaning additional files...\n"
        );
        assert!(
            rules.contains(&expected),
            "expected {expected:?} in:\n{rules}"
        );
    }

    // 4b: cache-only -DCMAKE_TESTING_ENABLED:BOOL=ON activates CTestTestfile.cmake even when
    // dir.vars is populated without a normal CMAKE_TESTING_ENABLED variable.
    let host = PureHost::default();
    let dir = Path::new("/work/build");
    host.seed_file(&dir.join("CMakeCache.txt"), b"")?;
    let mut model = ProjectModel::default();
    model.set_root_dirs("/work/src".into(), "/work/build".into());
    model
        .dir_mut(DirId::ROOT)
        .vars
        .insert("CMAKE_CURRENT_SOURCE_DIR".into(), "/work/src".into());
    let sub = model.add_directory(
        DirId::ROOT,
        "/work/src/sub".into(),
        "/work/build/sub".into(),
        Default::default(),
    )?;
    model
        .dir_mut(sub)
        .vars
        .insert("CMAKE_CURRENT_SOURCE_DIR".into(), "/work/src/sub".into());
    model.cache.insert(
        "CMAKE_TESTING_ENABLED".into(),
        CacheEntry {
            value: "ON".into(),
            ty: CacheType::Bool,
            help: BString::default(),
            advanced: false,
            props: Default::default(),
        },
    );
    let mut result = GenerationResult {
        graphs: vec![BuildGraph::default()],
        ..GenerationResult::default()
    };
    result.metadata.listfiles = vec!["/work/src/CMakeLists.txt".into()];
    super::generate_single_config_with_model(&result, &host, dir, &model)?;
    let build = read(&host, "build.ninja");
    let rerun = build
        .lines()
        .find(|line| line.contains(": RERUN_CMAKE"))
        .ok_or("missing RERUN_CMAKE edge")?;
    assert!(
        rerun.contains("CTestTestfile.cmake") && rerun.contains("sub/CTestTestfile.cmake"),
        "cache-only CMAKE_TESTING_ENABLED=ON must list CTestTestfile.cmake for root and sub:\n{rerun}"
    );
    Ok(())
}

#[test]
fn verify_globs_refreshes_stale_manifest_outputs_and_scopes_ctest_files()
-> Result<(), Box<dyn std::error::Error>> {
    use rcmake_host::FileTime;
    use rcmake_model::{
        DirId, GlobCheck, GlobKey, InstallKind, InstallOptions, InstallRule, ProjectModel,
    };

    let host = PureHost::default();
    let dir = Path::new("/work/build");
    host.seed_file(&dir.join("CMakeCache.txt"), b"")?;
    host.seed_file(&dir.join("cmake_install.cmake"), b"# install\n")?;
    host.seed_file(&dir.join("CTestTestfile.cmake"), b"# ctest\n")?;
    let old_time = FileTime {
        secs: 100,
        nanos: 0,
    };
    let new_time = FileTime {
        secs: 500,
        nanos: 0,
    };
    host.set_times(&dir.join("cmake_install.cmake"), None, Some(old_time))?;
    host.set_times(&dir.join("CTestTestfile.cmake"), None, Some(old_time))?;
    host.seed_file(&dir.join("CMakeFiles/cmake.verify_globs"), b"")?;
    host.set_times(
        &dir.join("CMakeFiles/cmake.verify_globs"),
        None,
        Some(new_time),
    )?;

    let mut model = ProjectModel::default();
    model.set_root_dirs("/work/src".into(), "/work/build".into());
    model
        .dir_mut(DirId::ROOT)
        .vars
        .insert("CMAKE_TESTING_ENABLED".into(), "1".into());
    // Subdirectory added BEFORE enable_testing() has populated dir.vars without CMAKE_TESTING_ENABLED.
    let before = model.add_directory(
        DirId::ROOT,
        "/work/src/before".into(),
        "/work/build/before".into(),
        Default::default(),
    )?;
    model
        .dir_mut(before)
        .vars
        .insert("CMAKE_CURRENT_SOURCE_DIR".into(), "/work/src/before".into());
    // Subdirectory added AFTER enable_testing() inherits CMAKE_TESTING_ENABLED=1.
    let after = model.add_directory(
        DirId::ROOT,
        "/work/src/after".into(),
        "/work/build/after".into(),
        Default::default(),
    )?;
    model
        .dir_mut(after)
        .vars
        .insert("CMAKE_TESTING_ENABLED".into(), "1".into());
    model.add_install(InstallRule {
        dir: DirId::ROOT,
        backtrace: Default::default(),
        kind: InstallKind::Files(vec![rcmake_model::BT::new(
            "/work/src/a.txt".into(),
            Default::default(),
        )]),
        options: InstallOptions::default(),
    });
    model.glob_checks.insert(
        GlobKey {
            expression: "/work/src/*.c".into(),
            recursive: false,
            directories: false,
            follow: false,
            relative: None,
        },
        GlobCheck {
            files: "/work/src/a.c".into(),
            calls: Vec::new(),
        },
    );

    let mut result = GenerationResult {
        graphs: vec![BuildGraph::default()],
        ..GenerationResult::default()
    };
    result.install = rcmake_resolve::lower::lower_install_manifest(&model, &result.targets);
    super::generate_single_config_with_model(&result, &host, dir, &model)?;

    let build = read(&host, "build.ninja");
    let rerun = build
        .lines()
        .find(|line| line.contains(": RERUN_CMAKE"))
        .ok_or("missing RERUN_CMAKE edge")?;
    assert!(rerun.contains("CTestTestfile.cmake"), "{rerun}");
    assert!(rerun.contains("after/CTestTestfile.cmake"), "{rerun}");
    assert!(
        !rerun.contains("before/CTestTestfile.cmake"),
        "subdirectory configured before enable_testing() must not list CTestTestfile.cmake: {rerun}"
    );
    assert!(
        host.metadata(&dir.join("cmake_install.cmake"), rcmake_host::Follow::Yes)?
            .mtime
            >= new_time,
        "stale cmake_install.cmake mtime must be refreshed to >= cmake.verify_globs"
    );
    assert!(
        host.metadata(&dir.join("CTestTestfile.cmake"), rcmake_host::Follow::Yes)?
            .mtime
            >= new_time,
        "stale CTestTestfile.cmake mtime must be refreshed to >= cmake.verify_globs"
    );
    Ok(())
}

#[test]
fn link_event_byproducts_custom_byproducts_dup_depends_and_assumed_source()
-> Result<(), Box<dyn std::error::Error>> {
    use rcmake_model::{DirId, ProjectModel, Target, TargetKind, TargetOrigin};

    let host = PureHost::default();
    let dir = Path::new("/work/build");
    let mut model = ProjectModel::default();
    model.set_root_dirs("/work/src".into(), "/work/build".into());
    let app_id = model.add_target(Target::new(
        "btop_test".into(),
        TargetKind::Executable,
        TargetOrigin::Normal,
        DirId::ROOT,
        Default::default(),
        Default::default(),
    ))?;
    model
        .dir_mut(DirId::ROOT)
        .source_props
        .entry("/work/build/gen/sqlite3.c".into())
        .or_default()
        .set("GENERATED".into(), "ON".into());

    let link_event_byproducts = [(
        BString::from("btop_test"),
        vec![BString::from("btop_test_tests.cmake")],
    )]
    .into_iter()
    .collect();
    let graph = BuildGraph {
        steps: vec![
            BuildStep::Custom(CustomStep {
                outputs: vec!["ep-complete".into()],
                byproducts: vec!["libcurl.a".into()],
                depends: vec!["ep-install".into(), "ep-mkdir".into(), "ep-install".into()],
                invocations: vec![Invocation {
                    shell: "cmake -E touch ep-complete".into(),
                    ..Default::default()
                }],
                ..Default::default()
            }),
            BuildStep::Compile {
                object: "CMakeFiles/btop_test.dir/sqlite3.c.o".into(),
                byproducts: Vec::new(),
                source: "/work/build/gen/sqlite3.c".into(),
                compile_input: None,
                invocation: Invocation {
                    shell: "cc -c gen/sqlite3.c -o CMakeFiles/btop_test.dir/sqlite3.c.o".into(),
                    ..Default::default()
                },
                depfile: None,
                modmap: None,
                dyndep: None,
            },
            BuildStep::Link {
                inputs: vec![
                    "CMakeFiles/btop_test.dir/sqlite3.c.o".into(),
                    "libbtop.a".into(),
                    "/usr/lib/libIOReport.tbd".into(),
                ],
                implicit_inputs: vec!["/SDK/OpenGL.framework".into()],
                step: LinkStep {
                    kind: LinkKind::Executable,
                    language: "CXX".into(),
                    fragments: vec![],
                    output: "btop_test".into(),
                },
                invocations: vec![Invocation {
                    shell: "c++ CMakeFiles/btop_test.dir/sqlite3.c.o -o btop_test libbtop.a".into(),
                    ..Default::default()
                }],
            },
        ],
        targets: vec![GraphTarget {
            name: "btop_test".into(),
            steps: vec![0, 1, 2],
            depends: Vec::new(),
            in_all: true,
        }],
        target_ids: vec![app_id],
        link_event_byproducts,
        ..BuildGraph::default()
    };
    let result = GenerationResult {
        graphs: vec![graph],
        ..GenerationResult::default()
    };
    super::generate_single_config_with_model(&result, &host, dir, &model)?;
    let build = read(&host, "build.ninja");

    // Custom command preserves duplicate inputs and puts byproducts in explicit outputs.
    assert!(
        build.contains(
            "build ep-complete libcurl.a | /work/build/ep-complete /work/build/libcurl.a: CUSTOM_COMMAND ep-install ep-mkdir ep-install\n"
        ),
        "{build}"
    );
    // Link edge with POST_BUILD BYPRODUCTS keeps LINK_EXECUTABLE, puts byproduct in explicit
    // outputs with workdir alias, keeps libraries + framework in implicit inputs, and binds restat.
    assert!(
        build.contains(
            "build btop_test btop_test_tests.cmake | /work/build/btop_test_tests.cmake: LINK_EXECUTABLE CMakeFiles/btop_test.dir/sqlite3.c.o | libbtop.a /usr/lib/libIOReport.tbd /SDK/OpenGL.framework"
        ),
        "{build}"
    );
    assert!(
        build.contains("  restat = 1\n"),
        "link edge with event byproducts must set restat = 1:\n{build}"
    );
    // Assumed generated source has 1 output and depends on cmake_object_order_depends_target_<name>.
    assert!(
        build.contains(
            "build gen/sqlite3.c: phony || cmake_object_order_depends_target_btop_test\n"
        ),
        "{build}"
    );
    Ok(())
}
