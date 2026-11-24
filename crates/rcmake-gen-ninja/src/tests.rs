#![allow(clippy::panic)]

use std::path::Path;

use rcmake_base::BString;
use rcmake_host::{Follow, Host, PureHost, ReadRange};
use rcmake_resolve::{
    ArtifactSet, BuildGraph, BuildStep, CustomStep, GraphTarget, Invocation, LinkKind, LinkStep,
};

use super::{
    GenerationResult, Graph, Ninja, generate_multi_config, generate_single_config, qualified,
};

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
                    modmap: None,
                    dyndep: None,
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
    assert!(rules.contains("rule COMPILE_DEPFILE\n  command = $COMMAND\n  description = $DESC\n  depfile = $DEP_FILE\n  deps = gcc"));
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
        "build obj/a$ $$$:file.o: COMPILE_DEPFILE /work/src/a$ $$$:file.c || generated$ $$$:file",
        "  DEP_FILE = obj/a$ $$$:file.d",
        "build libshared.so: LINK_SHARED_LIBRARY obj/a$ $$$:file.o",
        "  DESC = Linking$ C$ shared$ library$ libshared.so",
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
fn link_byproducts_use_unique_paths_on_the_link_edge() -> Result<(), Box<dyn std::error::Error>> {
    let input = BuildGraph {
        steps: vec![
            BuildStep::Link {
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
        graph.emit(&mut ninja, b"cmake", b"", &[]);
        graph.emit_entry_aliases(&mut ninja);
        let manifest = String::from_utf8(ninja.0)?;
        assert_eq!(
            manifest,
            concat!(
                "build libdemo.so.1 | libdemo.so libdemo.lib libdemo.pdb: LINK_SHARED_LIBRARY\n",
                "  COMMAND = $:\n",
                "  DESC = Linking$ C$ shared$ library$ libdemo.so.1\n",
                "build check: phony libdemo.so\n",
                "build libdemo.so$:Debug: phony libdemo.so.1 libdemo.so libdemo.lib libdemo.pdb\n",
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
        graph.emit(&mut ninja, b"cmake", b"", &[]);
        if config.is_some() {
            graph.emit_entry_aliases(&mut ninja);
        }
        let manifest = String::from_utf8(ninja.0)?;
        let executable_node = executable.replace(':', "$:");
        let suffix = config.map_or("", |_| "$:Debug");
        for line in [
            format!("build {library_alias}: phony libsqlite3.a"),
            "build shell.o: COMPILE shell.c".to_owned(),
            format!("build {executable_node}: LINK_EXECUTABLE shell.o libsqlite3.a"),
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
        "  COMMAND = '/cache$ tools/cmake'$ --regenerate-during-build$",
        "  COMMAND = '/cache$ tools/cmake'$ -P$ cmake_install.cmake",
        "  COMMAND = '/cache$ tools/cmake'$ -S",
        "  COMMAND = '/cache$ tools/ctest'$ --force-new-ctest-process",
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
        for line in [
            "  COMMAND = '/current$ tools/rcmake'$ -DCMAKE_INSTALL_CONFIG_NAME=Debug",
            "  COMMAND = '/current$ tools/rcmake'$ -S",
            "  COMMAND = '/current$ tools/ctest'$ --force-new-ctest-process$ -C$ Debug",
        ] {
            assert!(build.contains(line), "Missing {line:?} in:\n{build}");
        }
    }
    let implementation = read(&host, "CMakeFiles/impl-Debug.ninja");
    assert!(
        implementation.contains("  COMMAND = '/current$ tools/rcmake'$ --regenerate-during-build$")
    );
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
    assert!(build.contains("'/tool$ bin/cpack'$ --config$ ./CPackSourceConfig.cmake"));

    host.seed_file(
        &dir.join("CMakeCache.txt"),
        b"CMAKE_CPACK_COMMAND:FILEPATH=/tools/packer\nCMAKE_SKIP_PACKAGE_ALL_DEPENDENCY:BOOL=ON\n",
    )?;
    host.seed_file(&dir.join("CPackConfig.cmake"), b"")?;
    generate_single_config(&result, &host, dir)?;
    let build = read(&host, "build.ninja");
    assert!(build.contains("build package: phony CMakeFiles/package.util"));
    assert!(build.contains("build CMakeFiles/package.util: CUSTOM_COMMAND\n"));
    assert!(build.contains("/tools/packer$ --config$ ./CPackConfig.cmake"));
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
                    source: source.clone(),
                    object: object.clone(),
                    ddi: ddi.clone(),
                    invocation: Invocation::default(),
                    depfile: Some(format!("{base}/unit.o.ddi.d").into()),
                },
                BuildStep::Collate {
                    target: name.into(),
                    ddis: vec![ddi],
                    dd: dd.clone(),
                    modmaps: vec![map.clone()],
                    tdi: format!("{base}/CXXDependInfo.json").into(),
                    depend_info: Box::default(),
                    modmap_format: rcmake_cxxmodules::ModmapFormat::Clang,
                    implicit_inputs,
                },
                BuildStep::Compile {
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
        });
        result.targets.push(ResolvedTarget {
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
