use super::*;

#[test]
fn writes_windows_link_artifacts_and_resource_options() {
    let host = PureHost::default();
    let bin = Path::new("/build tree");
    host.seed_file(&bin.join("CPackConfig.cmake"), b"").unwrap();
    let (model, mut result) = fixture(VsVersion::Vs17, "x64", bin, &host);
    let shared = model.find_target(DirId::ROOT, b"shared_lib").unwrap();
    let app = model.find_target(DirId::ROOT, b"app").unwrap();
    let release = result
        .configs
        .iter()
        .find(|config| config.0 == b"Release")
        .unwrap()
        .clone();
    let client = result
        .targets
        .iter_mut()
        .find(|fact| fact.id == app && fact.config == release)
        .unwrap();
    client.dependencies.push(shared);
    client
        .ide
        .as_mut()
        .unwrap()
        .link_items
        .push(IdeLinkItem::Target(shared));
    let source = |path: &str, kind| IdeSource {
        skip_precompile_headers: false,
        path: path.into(),
        kind,
        group: BString::default(),
        generated: false,
        excluded_configs: Vec::new(),
        file_set: None,
        package_location: None,
        compile: None,
    };
    result.targets.push(ResolvedTarget {
        fortran: None,
        id: shared,
        config: release,
        metadata: TargetMetadata {
            name: "shared_lib".into(),
            kind: TargetKind::SharedLibrary,
            source_dir: "/source tree".into(),
            binary_dir: from_native(bin),
            backtrace: Default::default(),
            folder: None,
            exclude_from_all: false,
            generator_provided: false,
        },
        artifacts: rcmake_resolve::ArtifactSet {
            file: Some("runtime files/shared.dll".into()),
            import_file: Some("archive files/shared import.lib".into()),
            pdb_file: Some("debug files/shared.pdb".into()),
            ..Default::default()
        },
        compile_groups: Vec::new(),
        compile_units: Vec::new(),
        link: None,
        dependencies: Vec::new(),
        cxx_modules: None,
        ide: Some(Box::new(IdeFacts {
            sources: vec![
                source(
                    "/source tree/shared.cpp",
                    IdeSourceKind::Compile {
                        language: "CXX".into(),
                    },
                ),
                source(
                    "/source tree/version.RC",
                    IdeSourceKind::Compile {
                        language: "RC".into(),
                    },
                ),
                source("/source tree/exports file.def", IdeSourceKind::Other),
            ],
            compile: vec![
                IdeCompileSettings {
                    language: "CXX".into(),
                    flags: vec![bt("/std:c++20")],
                    defines: vec![bt("DLL_EXPORT")],
                    ..Default::default()
                },
                IdeCompileSettings {
                    language: "RC".into(),
                    includes: vec![IdeInclude {
                        path: "/build tree/generated headers".into(),
                        system: false,
                        framework: false,
                    }],
                    defines: vec![bt("RESOURCE_VERSION=1")],
                    ..Default::default()
                },
            ],
            ..Default::default()
        })),
    });
    generate(VsVersion::Vs17, &result, &model, &host, bin).unwrap();
    let read_project = |name: &str| {
        string(
            &host
                .read(&bin.join(name), ReadRange::All { limit: 65536 })
                .unwrap(),
        )
    };
    let dll = read_project("shared_lib.vcxproj");
    for property in [
        r"<ImportLibrary>\build tree\archive files\shared import.lib</ImportLibrary>",
        r"<ProgramDatabaseFile>\build tree\debug files\shared.pdb</ProgramDatabaseFile>",
        r"<ModuleDefinitionFile>\source tree\exports file.def</ModuleDefinitionFile>",
        r"<LanguageStandard>stdcpp20</LanguageStandard>",
        r#"<ResourceCompile Include="\source tree\version.RC">"#,
        r#"<AdditionalIncludeDirectories>\build tree\generated headers;%(AdditionalIncludeDirectories)</AdditionalIncludeDirectories>"#,
        r"<PreprocessorDefinitions>%(PreprocessorDefinitions);RESOURCE_VERSION=1;DLL_EXPORT</PreprocessorDefinitions>",
        r#"Directories="\build tree\archive files;\build tree\debug files""#,
    ] {
        assert!(dll.contains(property), "{property}\n{dll}");
    }
    let client = read_project("app.vcxproj");
    assert!(client.contains(r"\build tree\archive files\shared import.lib"));
    assert!(client.contains("<LinkLibraryDependencies>false</LinkLibraryDependencies>"));
}

#[test]
fn writes_the_configured_module_definition_pre_link_command()
-> Result<(), Box<dyn std::error::Error>> {
    let root = if cfg!(windows) { "C:/work" } else { "/work" };
    let bin = Path::new(root).join("build");
    let source = Path::new(root).join("src");
    let host = PureHost::new(bin.clone(), bin.join("cmake"));
    let mut model = ProjectModel::default();
    model.set_root_dirs(
        rcmake_host::path::from_native_cmake(&source),
        rcmake_host::path::from_native_cmake(&bin),
    );
    let id = model.add_target(target("dll", TargetKind::SharedLibrary, ""))?;
    let mut config = BuildGraph::default().config;
    config.0 = "Debug".into();
    let result = GenerationResult {
        configs: vec![config.clone()],
        targets: vec![ResolvedTarget {
            id,
            config: config.clone(),
            metadata: TargetMetadata {
                name: "dll".into(),
                kind: TargetKind::SharedLibrary,
                source_dir: rcmake_host::path::from_native_cmake(&source),
                binary_dir: rcmake_host::path::from_native_cmake(&bin),
                backtrace: Default::default(),
                folder: None,
                exclude_from_all: false,
                generator_provided: false,
            },
            ide: Some(Box::new(IdeFacts::default())),
            artifacts: Default::default(),
            compile_groups: Vec::new(),
            compile_units: Vec::new(),
            link: None,
            dependencies: Vec::new(),
            cxx_modules: None,
            fortran: None,
        }],
        graphs: vec![BuildGraph {
            config,
            steps: vec![BuildStep::Link {
                step: rcmake_resolve::LinkStep {
                    kind: rcmake_resolve::LinkKind::SharedLibrary,
                    language: "C".into(),
                    fragments: Vec::new(),
                    output: "dll.dll".into(),
                },
                inputs: Vec::new(),
                implicit_inputs: Vec::new(),
                invocations: vec![Invocation {
                    argv: [
                        "cmake",
                        "-E",
                        "__create_def",
                        "dll.dir/exports.def",
                        "dll.dir/exports.def.objs",
                    ]
                    .into_iter()
                    .map(BString::from)
                    .collect(),
                    ..Default::default()
                }],
            }],
            targets: vec![GraphTarget {
                name: "dll".into(),
                steps: vec![0],
                ..Default::default()
            }],
            ..BuildGraph::default()
        }],
        ..Default::default()
    };
    generate(VsVersion::Vs17, &result, &model, &host, &bin)?;
    let xml = host.read(&bin.join("dll.vcxproj"), ReadRange::All { limit: 65_536 })?;
    let xml = String::from_utf8(xml)?;
    assert!(xml.contains("ModuleDefinitionFile"));
    assert!(xml.contains(r"dll.dir\exports.def"));
    assert!(xml.contains("PreLinkEvent"));
    assert!(xml.contains("__create_def dll.dir/exports.def dll.dir/exports.def.objs"));
    Ok(())
}

#[test]
fn relative_solution_paths_keep_windows_drive_and_share_roots() {
    assert_eq!(relative(r"C:\work", "c:/work/lib"), "lib");
    assert_eq!(relative(r"C:\work", "D:/work/lib"), "D:/work/lib");
    assert_eq!(
        relative(r"\\server\one\work", "//SERVER/ONE/work/lib"),
        "lib"
    );
    assert_eq!(
        relative("//server/one/work", "//server/two/lib"),
        "//server/two/lib"
    );
    assert_eq!(relative("/work", "D:/work/lib"), "D:/work/lib");
    assert_eq!(relative("/work", "/work/lib"), "lib");
}

#[test]
fn cxx23_standard_switches_follow_toolset() {
    for toolset in ["v143", "v145"] {
        let preview = flags_table::compile(["/std:c++23preview".into()], toolset);
        assert!(preview.has("LanguageStandard"));
        let stable = flags_table::compile(["/std:c++23".into()], toolset);
        assert_eq!(stable.has("LanguageStandard"), toolset == "v145");
    }
}

#[test]
fn whole_archive_uses_final_library_path_and_keeps_project_reference() {
    let host = PureHost::default();
    let bin = Path::new("/build tree");
    host.create_dir_all(bin).expect("build directory");
    let version = VsVersion::Vs17;
    let (model, mut result) = fixture(version, "x64", bin, &host);
    let library = model
        .find_target(DirId::ROOT, b"static_lib")
        .expect("static library");
    let feature = |item| IdeLinkItem::VisualStudioFeature {
        item: Box::new(item),
        template: "LINKER:/WHOLEARCHIVE:<LIBRARY>".into(),
    };
    for target in result
        .targets
        .iter_mut()
        .filter(|target| target.metadata.name == "app")
    {
        let ide = target.ide.as_mut().expect("app IDE facts");
        ide.link_items = vec![
            feature(IdeLinkItem::Target(library)),
            feature(IdeLinkItem::File("C:/vendor files/external.lib".into())),
            IdeLinkItem::Library("kernel32".into()),
        ];
    }
    generate(version, &result, &model, &host, bin).expect("generate projects");
    let project = contents(&host, bin, "app.vcxproj");
    assert!(project.contains(r"/WHOLEARCHIVE:C:\vendor files\external.lib;kernel32.lib"));
    assert!(project.contains(r"Debug\static_lib.lib;/WHOLEARCHIVE:"));
    assert!(project.contains(r"Release\static_lib.lib;/WHOLEARCHIVE:"));
    assert!(project.contains(r#"<ProjectReference Include="static_lib.vcxproj">"#));
}
