use std::path::Path;

use rcmake_gen_common::ide::{BuildRequest, Jobs};
use rcmake_gen_common::write_if_changed;
use rcmake_host::{NativeHost, PureHost, ReadRange, path::to_native};
use rcmake_model::{
    BT, CacheEntry, CacheType, CustomCommand, CustomCommandKind, GeneratorKind, GeneratorSpec,
    InstallKind, InstallRule, Target, TargetEvent, TargetKind, TargetOrigin, UsageField,
};
use rcmake_resolve::{
    BuildGraph, BuildStep, CustomStep, GraphTarget, Invocation, ResolvedTarget, TargetMetadata,
    ide::{
        IdeCompileSettings, IdeCustomPhase, IdeCustomStep, IdeFacts, IdeInclude, IdeLinkItem,
        IdeSource, IdeSourceKind, OutputNaming,
    },
};

use super::*;

fn bt(value: &str) -> BT<BString> {
    BT::new(value.into(), Default::default())
}

fn target(name: &str, kind: TargetKind, source: &str) -> Target {
    let mut target = Target::new(
        name.into(),
        kind,
        TargetOrigin::Normal,
        DirId::ROOT,
        Default::default(),
        Default::default(),
    );
    if !source.is_empty() {
        target.sources.push(bt(source));
    }
    target
}

#[test]
fn uuid_matches_cmake_namespace_and_reuses_cache() {
    let mut model = ProjectModel::default();
    assert_eq!(
        guid::for_name(&model, "/out", "app"),
        "{C14FBFFA-70CA-3300-849E-3C063E50CF05}"
    );
    assert_ne!(
        guid::for_name(&model, "/out/other", "app"),
        guid::for_name(&model, "/out", "app")
    );
    model.cache.insert(
        "app_GUID_CMAKE".into(),
        CacheEntry {
            value: "deadbeef-0000-4000-8000-000000000000".into(),
            ty: CacheType::Internal,
            help: Default::default(),
            advanced: false,
            props: Default::default(),
        },
    );
    assert_eq!(
        guid::for_name(&model, "/out", "app"),
        "{DEADBEEF-0000-4000-8000-000000000000}"
    );
}

#[test]
fn xml_preserves_utf8_and_escapes_values() {
    let mut xml = xml::Writer::new(true, "utf-8");
    xml.start("Project", &[("Name", "a<&>\"\t\n")]);
    xml.text("Message", &[], "café & <ok>\r\n");
    xml.empty("Empty", &[]);
    assert_eq!(
        xml.finish(),
        "\u{feff}<?xml version=\"1.0\" encoding=\"utf-8\"?>\r\n<Project Name=\"a&lt;&amp;&gt;&quot;&#x9;&#xA;\">\r\n  <Message>café &amp; &lt;ok&gt;&#xD;&#xA;</Message>\r\n  <Empty />\r\n</Project>\r\n"
    );
}

#[test]
fn compile_flags_use_last_values_and_inherit_options() {
    let mut xml = xml::Writer::new(false, "utf-8");
    flags_table::compile(flags_table::split(r#"/Od /O2 /W4 /WX /MDd /Zi /EHsc /GR- /std:c++20 /Zc:preprocessor /Zc:__cplusplus /D"NAME=two words" /I "C:\Program Files\SDK""#), "v143").write(&mut xml);
    let xml = xml.finish();
    for element in [
        "<Optimization>MaxSpeed",
        "<WarningLevel>Level4",
        "<TreatWarningAsError>true",
        "<RuntimeLibrary>MultiThreadedDebugDLL",
        "<DebugInformationFormat>ProgramDatabase",
        "<ExceptionHandling>Sync",
        "<RuntimeTypeInfo>false",
        "<LanguageStandard>stdcpp20",
        "<UseStandardPreprocessor>true",
        "<AdditionalOptions>%(AdditionalOptions) /Zc:__cplusplus",
        "<PreprocessorDefinitions>%(PreprocessorDefinitions);NAME=two words",
        "<AdditionalIncludeDirectories>C:\\Program Files\\SDK;%(AdditionalIncludeDirectories)",
    ] {
        assert!(xml.contains(element), "{element}: {xml}");
    }
}

#[test]
fn selects_msvc_language_standard_properties() {
    for (flag, property, value) in [
        ("std:c++14", "LanguageStandard", "stdcpp14"),
        ("std:c++20", "LanguageStandard", "stdcpp20"),
        ("std:c11", "LanguageStandard_C", "stdc11"),
        ("std:c17", "LanguageStandard_C", "stdc17"),
        ("std:clatest", "LanguageStandard_C", "stdclatest"),
    ] {
        for prefix in ["", "-", "/"] {
            let mut xml = xml::Writer::new(false, "utf-8");
            flags_table::compile(std::iter::once(format!("{prefix}{flag}")), "v143")
                .write(&mut xml);
            assert!(
                xml.finish()
                    .contains(&format!("<{property}>{value}</{property}>")),
                "{prefix}{flag}"
            );
        }
    }
}

#[test]
fn writes_visual_studio_standards_from_resolved_ide_settings()
-> Result<(), Box<dyn std::error::Error>> {
    let root = if cfg!(windows) { "C:/work" } else { "/work" };
    let source = Path::new(root).join("src");
    let bin = Path::new(root).join("build");
    let host = PureHost::new(bin.clone(), bin.join("cmake"));
    host.seed_file(
        &source.join("main.cpp"),
        b"consteval int value() { return 42; }",
    )?;
    let mut model = ProjectModel::default();
    model.set_root_dirs(from_native(&source), from_native(&bin));
    model.generator = Some(GeneratorKind::VisualStudio(VsVersion::Vs17));
    let vars = &mut model.dir_mut(DirId::ROOT).vars;
    for (name, value) in [
        ("CMAKE_BUILD_TYPE", "Debug"),
        ("CMAKE_SYSTEM_NAME", "Windows"),
        ("CMAKE_CXX_COMPILER", "cl.exe"),
        ("CMAKE_CXX_COMPILER_ID", "MSVC"),
        ("CMAKE_CXX_STANDARD_DEFAULT", "14"),
        ("CMAKE_CXX_COMPILE_FEATURES", "cxx_std_20"),
        ("CMAKE_CXX20_STANDARD_COMPILE_OPTION", "-std:c++20"),
        ("CMAKE_CXX20_EXTENSION_COMPILE_OPTION", "-std:c++20"),
    ] {
        vars.insert(name.into(), value.into());
    }
    for name in ["explicit", "feature", "override"] {
        let mut app = target(name, TargetKind::StaticLibrary, "main.cpp");
        if name == "feature" {
            app.usage
                .push(UsageField::CompileFeatures, bt("cxx_std_20"));
        } else {
            app.props.set("CXX_STANDARD".into(), "20".into());
        }
        if name == "override" {
            app.usage.push(UsageField::CompileOptions, bt("/std:c++17"));
        }
        model.add_target(app)?;
    }
    let result = rcmake_resolve::resolve_with_options(
        &model,
        &host,
        &[],
        &rcmake_resolve::ResolveOptions {
            ide: Some(rcmake_resolve::ide::IdeFlavor::VisualStudio),
            ..Default::default()
        },
    )?;
    generate(VsVersion::Vs17, &result, &model, &host, &bin)?;
    for (name, standard) in [("explicit", 20), ("feature", 20), ("override", 17)] {
        let file = host.read(
            &bin.join(format!("{name}.vcxproj")),
            ReadRange::All { limit: 65536 },
        )?;
        let project = string(&file);
        assert!(
            project.contains(&format!(
                "<LanguageStandard>stdcpp{standard}</LanguageStandard>"
            )),
            "{name}\n{project}"
        );
    }
    Ok(())
}

#[test]
fn writes_link_output_directories_and_cuda_and_nasm_tool_imports() {
    let host = PureHost::default();
    let bin = Path::new("/out");
    let mut model = ProjectModel::default();
    let root = model.dir_mut(DirId::ROOT);
    for (name, value) in [
        ("CMAKE_ROOT", "/cmake"),
        ("CMAKE_CUDA_COMPILER_VERSION", "12.6.91"),
        ("CMAKE_ASM_NASM_COMPILER", "C:/Assembler Tools/nasm.exe"),
        ("CMAKE_ASM_NASM_OBJECT_FORMAT", "win64"),
    ] {
        root.vars.insert(name.into(), value.into());
    }
    host.seed_file(
        Path::new("/cmake/Templates/MSBuild/nasm.props.in"),
        b"<CompilerNasm>@CMAKE_ASM_NASM_COMPILER@</CompilerNasm>",
    )
    .unwrap();
    let mut app = target("app", TargetKind::Executable, "kernel.cu");
    app.sources.push(bt("start.nasm"));
    app.props
        .set("CUDA_SEPARABLE_COMPILATION".into(), "ON".into());
    app.props.set("CUDA_ARCHITECTURES".into(), "75-real".into());
    app.props.set(
        "LINK_FLAGS".into(),
        r#"/DEBUG:FULL /PDB:"$(IntDir)symbols/app.pdb" /IMPLIB:"link files/app.lib""#.into(),
    );
    model.add_target(app).unwrap();

    generate(
        VsVersion::Vs17,
        &GenerationResult::default(),
        &model,
        &host,
        bin,
    )
    .unwrap();
    let file = bin.join("app.vcxproj");
    let text = string(&host.read(&file, ReadRange::All { limit: 65536 }).unwrap());
    for fragment in [
        "<GenerateDebugInformation>DebugFull</GenerateDebugInformation>",
        r"<ProgramDatabaseFile>$(IntDir)symbols\app.pdb</ProgramDatabaseFile>",
        r#"Directories="link files;$(IntDir)symbols""#,
        r"BuildCustomizations\CUDA 12.6.props",
        r"BuildCustomizations\CUDA 12.6.targets",
        r"app.dir\nasm.props",
        r"Templates\MSBuild\nasm.targets",
        "<GenerateRelocatableDeviceCode>true</GenerateRelocatableDeviceCode>",
        "<PerformDeviceLink>true</PerformDeviceLink>",
        "--generate-code=arch=compute_75,code=sm_75",
        "<CudaCompile Include=",
        "<NASM Include=",
        "<Outputswitch>1</Outputswitch>",
    ] {
        assert!(text.contains(fragment), "{fragment}\n{text}");
    }
    let props = bin.join("app.dir/nasm.props");
    assert_eq!(
        host.read(&props, ReadRange::All { limit: 65536 }).unwrap(),
        br"<CompilerNasm>C:\Assembler Tools\nasm.exe</CompilerNasm>"
    );
}

#[test]
fn writes_cuda_output_modes_and_split_toolkit_imports() -> Result<(), Box<dyn std::error::Error>> {
    let host = PureHost::default();
    let bin = Path::new("/out");
    let mut model = ProjectModel::default();
    let toolkit = "/toolkit/CUDAVisualStudioIntegration";
    host.create_dir_all(Path::new(toolkit))?;
    model.generator_spec = GeneratorSpec::parse(
        GeneratorKind::VisualStudio(VsVersion::Vs17),
        b"cuda=/toolkit",
        b"",
        b"",
        None,
    )?;
    let root = model.dir_mut(DirId::ROOT);
    root.vars
        .insert("CMAKE_CUDA_COMPILER_VERSION".into(), "12.6.91".into());
    let modes = [
        ("PTX", "ptx", "ptx"),
        ("CUBIN", "cubin", "cubin"),
        ("FATBIN", "fatbin", "fatbin"),
        ("OPTIX", "optix-ir", "optixir"),
    ];
    for (property, _, _) in modes {
        let mut app = target(property, TargetKind::ObjectLibrary, "kernel.cu");
        app.props
            .set(format!("CUDA_{property}_COMPILATION").into(), "ON".into());
        model.add_target(app).expect("Add CUDA object library");
    }
    generate(
        VsVersion::Vs17,
        &GenerationResult::default(),
        &model,
        &host,
        bin,
    )?;
    for (name, mode, extension) in modes {
        let file = contents(&host, bin, &format!("{name}.vcxproj"));
        for kind in ["props", "targets"] {
            assert!(file.contains(&format!(
                r"\toolkit\CUDAVisualStudioIntegration\extras\visual_studio_integration\MSBuildExtensions\CUDA 12.6.{kind}"
            )));
        }
        assert!(file.contains(&format!("<NvccCompilation>{mode}</NvccCompilation>")));
        assert!(file.contains(&format!(
            "<CompileOut>$(IntDir)%(Filename).{extension}</CompileOut>"
        )));
        let suffix = format!(".{extension}</CompileOut>");
        assert!(
            file.lines()
                .filter(|line| line.contains("<CompileOut>"))
                .count()
                > 1
        );
        assert!(
            file.lines()
                .filter(|line| line.contains("<CompileOut>"))
                .all(|line| line.ends_with(&suffix))
        );
    }
    Ok(())
}

#[test]
fn build_plans_support_clean_parallel_targets_and_devenv() {
    let host = PureHost::default();
    let targets = ["app".into(), "lib.vcxproj".into()];
    let req = BuildRequest {
        generator: GeneratorKind::VisualStudio(VsVersion::Vs18),
        build_dir: b"/out",
        project_name: b"Demo",
        make_program: b"",
        config: Some(b"Release"),
        targets: &targets,
        jobs: Jobs::Count(4),
        clean_first: true,
        verbose: true,
        native_args: &["/nr:false".into()],
        platform: b"ARM64",
    };
    let commands = build_commands(&req, &host).expect("MSBuild plan");
    assert_eq!(
        commands
            .iter()
            .map(|cmd| cmd.argv.get(1).map(|arg| string(arg)))
            .collect::<Vec<_>>(),
        [
            Some("Demo.slnx".into()),
            Some("app.vcxproj".into()),
            Some("lib.vcxproj".into())
        ]
    );
    assert_eq!(
        commands.first().and_then(|cmd| cmd.argv.get(2)),
        Some(&"/t:Clean".into())
    );
    assert!(!commands[0].argv.contains(&"/nr:false".into()));
    let build = commands.get(1).expect("build command");
    assert_eq!(
        build.argv.iter().map(|arg| string(arg)).collect::<Vec<_>>(),
        [
            "MSBuild.exe",
            "app.vcxproj",
            "/p:Configuration=Release",
            "/p:Platform=ARM64",
            "/p:VisualStudioVersion=18.0",
            "/m:4",
            "/v:n",
            "/nr:false",
        ]
    );
    assert_eq!(build.working_dir, "/out");

    let req = BuildRequest {
        generator: GeneratorKind::VisualStudio(VsVersion::Vs17),
        make_program: br"C:\Tools\devenv.com",
        clean_first: false,
        targets: &[],
        config: None,
        platform: b"Win32",
        native_args: &[],
        ..req
    };
    let commands = build_commands(&req, &host).expect("devenv plan");
    assert_eq!(
        commands
            .first()
            .expect("build command")
            .argv
            .iter()
            .map(|arg| string(arg))
            .collect::<Vec<_>>(),
        [
            r"C:\Tools\devenv.com",
            "Demo.sln",
            "/Build",
            "Debug|Win32",
            "/Project",
            "ALL_BUILD"
        ]
    );
}

#[test]
fn msbuild_builds_named_subdirectory_projects_in_both_solution_formats() {
    let host = PureHost::default();
    let bin = to_native(if cfg!(windows) { b"C:/out" } else { b"/out" });
    let mut model = ProjectModel::default();
    model.set_root_dirs("/source".into(), from_native(&bin));
    let sub = model
        .add_directory(
            DirId::ROOT,
            "/source/sub".into(),
            from_native(&bin.join("sub & child")),
            Default::default(),
        )
        .expect("add subdirectory");
    let mut app = target("app", TargetKind::Utility, "");
    app.dir = sub;
    app.props.set("FOLDER".into(), "Products/Tools".into());
    model.add_target(app).expect("add target");

    let build_dir = from_native(&bin);
    let targets = ["APP".into(), "custom/other".into(), "".into()];
    for version in [VsVersion::Vs17, VsVersion::Vs18] {
        generate(version, &GenerationResult::default(), &model, &host, &bin)
            .expect("generate solution");
        let commands = build_commands(
            &BuildRequest {
                generator: GeneratorKind::VisualStudio(version),
                build_dir: &build_dir,
                project_name: b"Project",
                make_program: b"MSBuild.exe",
                config: None,
                targets: &targets,
                jobs: Jobs::None,
                clean_first: false,
                verbose: false,
                native_args: &[],
                platform: b"x64",
            },
            &host,
        )
        .expect("plan build");
        assert_eq!(
            commands
                .iter()
                .map(|command| string(&command.argv[1]))
                .collect::<Vec<_>>(),
            [
                r"sub & child\app.vcxproj",
                r"custom\other.vcxproj",
                "ALL_BUILD.vcxproj"
            ],
        );
    }
}

fn fixture(
    version: VsVersion,
    platform: &str,
    bin: &Path,
    host: &dyn Host,
) -> (ProjectModel, GenerationResult) {
    let kind = GeneratorKind::VisualStudio(version);
    let mut model = ProjectModel::default();
    model.generator = Some(kind);
    model.generator_spec = GeneratorSpec::parse(
        kind,
        b"host=x64,version=14.44",
        format!("{platform},version=10.0").as_bytes(),
        b"",
        None,
    )
    .expect("generator options");
    model.set_root_dirs("/source tree".into(), from_native(bin));
    let root = model.dir_mut(DirId::ROOT);
    root.vars.insert("PROJECT_NAME".into(), "Demo".into());
    root.vars
        .insert("CMAKE_CONFIGURATION_TYPES".into(), "Debug;Release".into());
    root.props.set("CMAKE_TESTING_ENABLED".into(), "ON".into());
    root.props.set("VS_STARTUP_PROJECT".into(), "app".into());
    model.add_install(InstallRule {
        dir: DirId::ROOT,
        backtrace: Default::default(),
        kind: InstallKind::Code("".into()),
        options: Default::default(),
    });
    write_if_changed(host, &bin.join("CPackConfig.cmake"), b"").expect("CPack configuration");
    let lib = model
        .add_target(target(
            "static_lib",
            TargetKind::StaticLibrary,
            "lib.c;lib.hpp",
        ))
        .expect("static library");
    model
        .add_target(target(
            "shared_lib",
            TargetKind::SharedLibrary,
            "shared.cpp",
        ))
        .expect("shared library");
    let object = model
        .add_target(target("objects", TargetKind::ObjectLibrary, "object.cpp"))
        .expect("object library");
    model
        .add_target(target(
            "headers",
            TargetKind::InterfaceLibrary,
            "interface.hpp",
        ))
        .expect("interface library");
    model
        .add_target(target("unused_interface", TargetKind::InterfaceLibrary, ""))
        .expect("empty interface library");
    let job = model
        .add_target(target("run", TargetKind::Utility, ""))
        .expect("utility");
    let mut app = target("app", TargetKind::Executable, "main.cpp");
    app.props.set("FOLDER".into(), "Products/Tools".into());
    app.props
        .set("EXCLUDE_FROM_DEFAULT_BUILD_RELEASE".into(), "ON".into());
    app.utility_deps.push(bt("run"));
    let app = model.add_target(app).expect("executable");
    let mut fallback = target("fallback", TargetKind::Executable, "fallback.c");
    fallback.usage.push(UsageField::CompileOptions, bt("/W3"));
    let fallback = model.add_target(fallback).expect("fallback executable");

    let mut event = CustomCommand::new(
        DirId::ROOT,
        CustomCommandKind::TargetEvent {
            target: fallback,
            event: TargetEvent::PreLink,
        },
        Default::default(),
        Default::default(),
    );
    event.command_lines.push(vec![
        "cmake".into(),
        "-E".into(),
        "echo".into(),
        "linking".into(),
    ]);
    model.add_command(event).expect("pre-link command");

    let mut result = GenerationResult::default();
    result.configs.resize_with(2, Default::default);
    for (config, name) in result.configs.iter_mut().zip(["Debug", "Release"]) {
        config.0 = name.into();
        let make = |phase, args: &[&str], outputs: &[&str]| IdeCustomStep {
            phase,
            commands: vec![args.iter().map(|arg| (*arg).into()).collect()],
            working_dir: from_native(bin),
            outputs: outputs.iter().map(|path| (*path).into()).collect(),
            byproducts: Vec::new(),
            depends: vec!["/source tree/script.cmake".into()],
            comment: Some("Run <step> & check".into()),
            always_run: false,
        };
        result.targets.push(ResolvedTarget {
            fortran: None,
            id: app,
            config: config.clone(),
            metadata: TargetMetadata {
                name: "app".into(),
                kind: TargetKind::Executable,
                source_dir: "/source tree".into(),
                binary_dir: from_native(bin),
                backtrace: Default::default(),
                folder: None,
                exclude_from_all: false,
                generator_provided: false,
            },
            artifacts: Default::default(),
            compile_groups: Vec::new(),
            compile_units: Vec::new(),
            link: None,
            dependencies: vec![lib, object, job],
            cxx_modules: None,
            ide: Some(Box::new(IdeFacts {
                naming: OutputNaming {
                    dir: from_native(&bin.join(name)),
                    base_name: "application".into(),
                    suffix: ".exe".into(),
                    ..Default::default()
                },
                sources: vec![
                    IdeSource {
                        skip_precompile_headers: false,
                        path: "/source tree/main.cpp".into(),
                        kind: IdeSourceKind::Compile {
                            language: "CXX".into(),
                        },
                        group: r"Code\Client".into(),
                        generated: false,
                        excluded_configs: Vec::new(),
                        file_set: None,
                        package_location: None,
                        compile: (name == "Debug").then(|| IdeCompileSettings {
                            language: "CXX".into(),
                            defines: vec![bt(r#"SOURCE="$(OutDir);file""#)],
                            includes: vec![IdeInclude {
                                path: "$(VCInstallDir)/include;extra".into(),
                                system: false,
                                framework: false,
                            }],
                            flags: vec![bt("/W3"), bt("/FI$(ProjectDir)pch.h")],
                        }),
                    },
                    IdeSource {
                        skip_precompile_headers: false,
                        path: from_native(&bin.join("generated.cpp")),
                        kind: IdeSourceKind::Compile {
                            language: "CXX".into(),
                        },
                        group: BString::default(),
                        generated: true,
                        excluded_configs: vec!["Release".into()],
                        file_set: None,
                        package_location: None,
                        compile: None,
                    },
                    IdeSource {
                        skip_precompile_headers: false,
                        path: "/source tree/main.hpp".into(),
                        kind: IdeSourceKind::Header,
                        group: BString::default(),
                        generated: false,
                        excluded_configs: Vec::new(),
                        file_set: None,
                        package_location: None,
                        compile: None,
                    },
                ],
                compile: vec![IdeCompileSettings {
                    language: "CXX".into(),
                    flags: vec![
                        bt("/W4"),
                        bt("/TC"),
                        bt("/GS-"),
                        bt("/Zc:__cplusplus"),
                        bt(if name == "Debug" { "/Od" } else { "/O2" }),
                    ],
                    defines: vec![bt("PROJECT_NAME=Demo")],
                    ..Default::default()
                }],
                link_items: vec![
                    IdeLinkItem::Target(lib),
                    IdeLinkItem::Target(object),
                    IdeLinkItem::Library("kernel32".into()),
                ],
                link_options: vec![bt("/DEBUG"), bt("/SUBSYSTEM:CONSOLE")],
                custom: vec![
                    make(
                        IdeCustomPhase::Rule,
                        &["cmake", "-E", "touch", "generated.cpp"],
                        &["generated.cpp"],
                    ),
                    make(
                        IdeCustomPhase::PreBuild,
                        &["cmake", "-E", "echo", "starting"],
                        &[],
                    ),
                    make(
                        IdeCustomPhase::PostBuild,
                        &["cmake", "-E", "echo", "done"],
                        &[],
                    ),
                ],
                ..Default::default()
            })),
        });
        result.graphs.push(BuildGraph {
            config: config.clone(),
            targets: vec![GraphTarget {
                name: "fallback".into(),
                steps: vec![0, 1],
                ..Default::default()
            }],
            steps: fallback_steps(bin, name),
            ..BuildGraph::default()
        });
    }
    result.metadata.listfiles = vec![
        "/source tree/CMakeLists.txt".into(),
        "/source tree/script.cmake".into(),
        "/source tree/script.cmake".into(),
    ];
    (model, result)
}

fn fallback_steps(bin: &Path, name: &str) -> Vec<BuildStep> {
    vec![
        BuildStep::Compile {
            byproducts: Vec::new(),
            compile_input: None,
            object: format!("{name}/fallback.obj").into(),
            source: "/source tree/fallback.c".into(),
            invocation: Invocation {
                argv: vec![
                    "cl.exe".into(),
                    "/W3".into(),
                    "/MDd".into(),
                    "/c".into(),
                    "/source tree/fallback.c".into(),
                    format!("/Fo{name}/fallback.obj").into(),
                ],
                ..Default::default()
            },
            depfile: None,
            modmap: None,
            dyndep: None,
        },
        BuildStep::Custom(CustomStep {
            outputs: vec!["graph.out".into()],
            invocations: vec![Invocation {
                argv: vec![
                    "cmake".into(),
                    "-E".into(),
                    "touch".into(),
                    "graph.out".into(),
                ],
                working_dir: Some(from_native(bin)),
                ..Default::default()
            }],
            ..Default::default()
        }),
    ]
}

fn contents(host: &dyn Host, bin: &Path, filename: &str) -> String {
    string(
        &host
            .read(&bin.join(filename), ReadRange::All { limit: 256 * 1024 })
            .expect("generated file"),
    )
}

fn check_fixture(version: VsVersion, platform: &str, host: &dyn Host, bin: &Path) {
    host.create_dir_all(bin).expect("build directory");
    let (model, result) = fixture(version, platform, bin, host);
    generate(version, &result, &model, host, bin).expect("generate project");
    let solution = contents(host, bin, &format!("Demo.{}", solution_ext(version)));
    if version.writes_slnx() {
        assert!(solution.contains("<Folder Name=\"/Products/Tools/\""));
        assert!(solution.contains("DefaultStartup=\"true\""));
        assert!(solution.contains(&format!("<Platform Name=\"{platform}\"")));
        assert!(solution.contains("<Build Solution=\"Release|*\" Project=\"false\""));
    } else {
        assert!(solution.starts_with("\u{feff}"));
        assert!(solution.contains("# Visual Studio Version 17"));
        assert!(solution.contains(
            r#"Project("{2150E333-8FDC-42A3-9474-1A3956D46DE8}") = "Tools", "Products\Tools", "#
        ));
        assert!(solution.contains("GlobalSection(NestedProjects)"));
        assert!(solution.contains("ProjectSection(ProjectDependencies)"));
        assert!(solution.contains(&format!(".Release|{platform}.ActiveCfg")));
        assert!(solution.contains(&format!("Debug|{platform} = Debug|{platform}")));
    }
    let app = contents(host, bin, "app.vcxproj");
    for text in [
        "<ConfigurationType>Application",
        "<TargetName Condition=",
        ">application</TargetName>",
        "<PreferredToolArchitecture>x64",
        "<VCToolsVersion>14.44",
        "<WindowsTargetPlatformVersion>10.0",
        "<PreBuildEvent>",
        "<PostBuildEvent>",
        ".rule\">",
        "<CustomBuild",
        "Run &lt;step&gt; &amp; check",
        "<ExcludedFromBuild",
        "<Optimization>Disabled",
        "<Optimization>MaxSpeed",
        "<WarningLevel>Level4",
        "<GenerateDebugInformation>true",
        "<SubSystem>Console",
        "<ProjectReference Include=\"static_lib.vcxproj\"",
        "objects.dir",
        "kernel32.lib;%(AdditionalDependencies)",
    ] {
        assert!(app.contains(text), "{text}: {app}");
    }
    assert!(app.contains(version.default_toolset()));
    assert!(solution.contains("ZERO_CHECK.vcxproj"));
    for name in ["ALL_BUILD", "INSTALL", "RUN_TESTS", "PACKAGE"] {
        assert!(solution.contains(&format!("{name}.vcxproj")));
    }
    for (name, kind) in [
        ("static_lib", "StaticLibrary"),
        ("shared_lib", "DynamicLibrary"),
        ("objects", "Utility"),
        ("headers", "Utility"),
        ("run", "Utility"),
    ] {
        assert!(
            contents(host, bin, &format!("{name}.vcxproj"))
                .contains(&format!("<ConfigurationType>{kind}</ConfigurationType>"))
        );
    }
    assert!(!solution.contains("unused_interface"));
    assert!(contents(host, bin, "objects.vcxproj").contains("DependsOnTargets=\"ClCompile\""));
    let fallback = contents(host, bin, "fallback.vcxproj");
    assert!(fallback.contains("graph.out"));
    assert!(fallback.contains("<PreLinkEvent>"));
    assert!(fallback.contains("<WarningLevel Condition="));
    assert!(fallback.contains(">Level3</WarningLevel>"));
    assert!(fallback.contains(">MultiThreadedDebugDLL</RuntimeLibrary>"));
    assert!(!fallback.contains("/Fo"));
    let zero = contents(host, bin, "ZERO_CHECK.vcxproj");
    assert!(zero.contains("--regenerate-during-build"));
    assert!(zero.contains("CMakeLists.txt"));
    assert!(zero.contains("generate.stamp"));
    let filters = contents(host, bin, "app.vcxproj.filters");
    for group in [
        "Code",
        r"Code\Client",
        "CMake Rules",
        "Header Files",
        "Source Files",
    ] {
        assert!(filters.contains(&format!("<Filter Include=\"{group}\"")));
    }

    generate(version, &result, &model, host, bin).expect("repeat generation");
    assert_eq!(
        contents(host, bin, &format!("Demo.{}", solution_ext(version))),
        solution
    );
    assert_eq!(contents(host, bin, "app.vcxproj"), app);
}

#[test]
fn both_solution_formats_cover_three_platforms() {
    let native = NativeHost::new().expect("host");
    let output = native.env_get(b"RCMAKE_VS_TEST_OUTPUT");
    let pure = PureHost::default();
    for version in VsVersion::ALL {
        for platform in ["x64", "ARM64", "Win32"] {
            let name = format!("{}-{platform}", solution_ext(version));
            if let Some(output) = &output {
                check_fixture(version, platform, &native, &to_native(output).join(name));
            } else {
                let dir = if cfg!(windows) { "C:/out" } else { "/out" };
                check_fixture(version, platform, &pure, &Path::new(dir).join(name));
            }
        }
    }
}

#[test]
fn clcompile_emits_defaults_and_source_overrides_with_msbuild_properties() {
    let host = PureHost::default();
    let bin = Path::new(if cfg!(windows) { "C:/out" } else { "/out" });
    host.create_dir_all(bin).expect("build directory");
    let version = VsVersion::Vs17;
    let (model, result) = fixture(version, "x64", bin, &host);
    generate(version, &result, &model, &host, bin).expect("generate projects");
    let project = contents(&host, bin, "app.vcxproj");
    let (_, definition) = project
        .split_once("<ClCompile>")
        .expect("compile definitions");
    let (definition, _) = definition
        .split_once("</ClCompile>")
        .expect("compile definitions");
    for value in [
        "<BasicRuntimeChecks>Default</BasicRuntimeChecks>",
        "<BufferSecurityCheck>false</BufferSecurityCheck>",
        "<CallingConvention></CallingConvention>",
        "<FloatingPointModel></FloatingPointModel>",
        "<ForceConformanceInForLoopScope></ForceConformanceInForLoopScope>",
        "<MinimalRebuild></MinimalRebuild>",
        "<Optimization>Disabled</Optimization>",
        "<RemoveUnreferencedCodeData></RemoveUnreferencedCodeData>",
        "<RuntimeLibrary></RuntimeLibrary>",
        "<SupportJustMyCode></SupportJustMyCode>",
        "<TreatWChar_tAsBuiltInType></TreatWChar_tAsBuiltInType>",
        r#"CMAKE_INTDIR="$(Configuration)""#,
    ] {
        assert!(definition.contains(value), "{value}: {definition}");
    }
    assert!(!definition.contains("CompileAs"));
    let (_, source) = project.split_once(r#"main.cpp">"#).expect("main.cpp item");
    let (source, _) = source.split_once("</ClCompile>").expect("main.cpp item");
    for (element, value) in [
        ("WarningLevel", "Level3"),
        (
            "PreprocessorDefinitions",
            r#"%(PreprocessorDefinitions);SOURCE="$(OutDir)%3Bfile""#,
        ),
        (
            "AdditionalIncludeDirectories",
            r"$(VCInstallDir)\include%3Bextra;%(AdditionalIncludeDirectories)",
        ),
        (
            "AdditionalOptions",
            r#"%(AdditionalOptions) "/FI$(ProjectDir)pch.h""#,
        ),
    ] {
        let expected = format!(
            "<{element} Condition=\"'$(Configuration)|$(Platform)'=='Debug|x64'\">{value}</{element}>"
        );
        assert!(source.contains(&expected), "{expected}: {source}");
        assert_eq!(source.matches(&format!("<{element} ")).count(), 1);
    }
}

#[test]
fn v142_conditions_suppress_startup_banner_on_toolset_version() {
    let mut settings = flags_table::compile(["/nologo".into()], "v143");
    settings.compiler_defaults("v142");
    let mut xml = xml::Writer::new(false, "utf-8");
    settings.write(&mut xml);
    assert!(xml.finish().contains(
        r#"<SuppressStartupBanner Condition="'$(VCToolsVersion)' &gt;= '14.29'">true</SuppressStartupBanner>"#
    ));
}

#[test]
fn regeneration_can_be_disabled_and_empty_projects_generate() {
    let host = PureHost::default();
    let dir = if cfg!(windows) { "C:/out" } else { "/out" };
    let bin = Path::new(dir);
    let mut model = ProjectModel::default();
    model.set_root_dirs(dir.into(), dir.into());
    model
        .dir_mut(DirId::ROOT)
        .props
        .set("CMAKE_SUPPRESS_REGENERATION".into(), "ON".into());
    generate(
        VsVersion::Vs18,
        &GenerationResult::default(),
        &model,
        &host,
        bin,
    )
    .expect("generate empty solution");
    let solution = contents(&host, bin, "Project.slnx");
    assert!(solution.contains("ALL_BUILD.vcxproj"));
    assert!(!solution.contains("ZERO_CHECK"));
    assert!(solution.contains("RelWithDebInfo"));
}

#[test]
fn nested_solutions_build_only_local_targets_and_their_dependencies() {
    let host = PureHost::default();
    let dir = if cfg!(windows) { "C:/out" } else { "/out" };
    let bin = Path::new(dir);
    let mut model = ProjectModel::default();
    model.set_root_dirs(dir.into(), dir.into());
    let subdir = model
        .add_directory(
            DirId::ROOT,
            format!("{dir}/sub").into(),
            format!("{dir}/sub").into(),
            Default::default(),
        )
        .expect("subdirectory");
    let sub = model.dir_mut(subdir);
    sub.vars.insert("PROJECT_NAME".into(), "Child".into());
    sub.vars
        .insert("PROJECT_SOURCE_DIR".into(), sub.source_dir.clone());
    model
        .add_target(target("dependency", TargetKind::Utility, ""))
        .expect("dependency");
    model
        .add_target(target("unrelated", TargetKind::Utility, ""))
        .expect("unrelated target");
    let mut child = target("child", TargetKind::Executable, "child.cpp");
    child.dir = subdir;
    child.utility_deps.push(bt("dependency"));
    child
        .props
        .set("EXCLUDE_FROM_ALL_RELEASE".into(), "ON".into());
    model.add_target(child).expect("child target");

    generate(
        VsVersion::Vs18,
        &GenerationResult::default(),
        &model,
        &host,
        bin,
    )
    .expect("generate solutions");
    let solution = contents(&host, bin, "sub/Child.slnx");
    assert!(solution.contains("Path=\"..\\dependency.vcxproj\""));
    assert!(solution.contains("Path=\"child.vcxproj\""));
    assert!(solution.contains("Path=\"ALL_BUILD.vcxproj\""));
    assert!(!solution.contains("unrelated"));
    let aggregate = contents(&host, bin, "sub/ALL_BUILD.vcxproj");
    let reference = aggregate
        .lines()
        .find(|line| line.contains("ProjectReference Include=\"child.vcxproj\""))
        .expect("child reference");
    assert!(reference.contains("Debug"));
    assert!(!reference.contains("Release|"));
}

#[test]
fn short_intermediate_directory_strategy_uses_hashed_intdir() {
    let host = PureHost::default();
    let dir = if cfg!(windows) { "C:/out" } else { "/out" };
    let src = if cfg!(windows) { "C:/src" } else { "/src" };
    let bin = Path::new(dir);
    host.seed_file(
        &Path::new(src).join("main.cpp"),
        b"int main() { return 0; }\n",
    )
    .expect("seed source");
    let mut model = ProjectModel::default();
    model.generator = Some(GeneratorKind::VisualStudio(VsVersion::Vs17));
    model.set_root_dirs(src.into(), dir.into());
    model.cache.insert(
        "CMAKE_INTERMEDIATE_DIR_STRATEGY".into(),
        CacheEntry {
            value: "SHORT".into(),
            ty: CacheType::String,
            help: Default::default(),
            advanced: false,
            props: Default::default(),
        },
    );
    model
        .dir_mut(DirId::ROOT)
        .vars
        .insert("CMAKE_CONFIGURATION_TYPES".into(), "Debug".into());
    model
        .dir_mut(DirId::ROOT)
        .vars
        .insert("CMAKE_BUILD_TYPE".into(), "Debug".into());
    model
        .dir_mut(DirId::ROOT)
        .vars
        .insert("CMAKE_CXX_COMPILER".into(), "cl.exe".into());
    model
        .dir_mut(DirId::ROOT)
        .vars
        .insert("CMAKE_CXX_SOURCE_FILE_EXTENSIONS".into(), "cpp".into());
    model
        .add_target(target("app", TargetKind::Executable, "main.cpp"))
        .expect("app");
    model
        .add_target(target("util", TargetKind::Utility, ""))
        .expect("util");
    let result = rcmake_resolve::resolve_with_options(
        &model,
        &host,
        &[],
        &rcmake_resolve::ResolveOptions {
            ide: Some(rcmake_resolve::ide::IdeFlavor::VisualStudio),
            ..Default::default()
        },
    )
    .expect("resolve");
    generate(VsVersion::Vs17, &result, &model, &host, bin).expect("generate");
    let vcxproj = contents(&host, bin, "app.vcxproj");
    assert!(
        vcxproj.contains("<IntDir Condition=\"'$(Configuration)|$(Platform)'=='Debug|x64'\">")
            && vcxproj.contains(r"\.o\")
            && !vcxproj.contains(r"app.dir\Debug\"),
        "{vcxproj}"
    );
    let util_vcxproj = contents(&host, bin, "util.vcxproj");
    assert!(
        util_vcxproj.contains("<IntDir Condition=\"'$(Configuration)|$(Platform)'=='Debug|x64'\">")
            && util_vcxproj.contains(r"\.o\")
            && !util_vcxproj.contains(r"util.dir\Debug\"),
        "{util_vcxproj}"
    );
}

#[test]
fn generate_stamp_depend_lists_glob_stamp_once() {
    let host = PureHost::default();
    let bin = Path::new("/out");
    let mut model = ProjectModel::default();
    model.set_root_dirs("/src".into(), "/out".into());
    model.glob_checks.insert(
        rcmake_model::GlobKey {
            expression: "/src/*.cpp".into(),
            recursive: false,
            directories: false,
            follow: false,
            relative: None,
        },
        rcmake_model::GlobCheck {
            files: "/src/main.cpp".into(),
            calls: vec![rcmake_model::GlobCall {
                variable: "SOURCES".into(),
                backtrace: Default::default(),
            }],
        },
    );
    model
        .add_target(target("app", TargetKind::Executable, "main.cpp"))
        .expect("app");

    generate(
        VsVersion::Vs17,
        &GenerationResult::default(),
        &model,
        &host,
        bin,
    )
    .expect("generate");

    let depend = contents(&host, bin, "CMakeFiles/generate.stamp.depend");
    let stamp_count = depend
        .lines()
        .filter(|line| line.ends_with(rcmake_gen_common::glob::STAMP))
        .count();
    assert_eq!(stamp_count, 1, "{depend}");
}

#[test]
fn make_dir_preserves_configuration_order() {
    let host = PureHost::default();
    let bin = Path::new("/out");
    let mut model = ProjectModel::default();
    model.set_root_dirs("/src".into(), "/out".into());
    model.dir_mut(DirId::ROOT).vars.insert(
        "CMAKE_CONFIGURATION_TYPES".into(),
        "Debug;Release;MinSizeRel;RelWithDebInfo".into(),
    );
    let mut app = target("app", TargetKind::Executable, "main.cpp");
    app.props.set(
        "LINK_FLAGS".into(),
        r#"/PDB:"$(IntDir)symbols/app.pdb""#.into(),
    );
    model.add_target(app).expect("app");

    generate(
        VsVersion::Vs17,
        &GenerationResult::default(),
        &model,
        &host,
        bin,
    )
    .expect("generate");

    let vcxproj = contents(&host, bin, "app.vcxproj");
    let (_, section) = vcxproj
        .split_once("Name=\"RcmakePrepareLinkDirectories\"")
        .expect("link directory target");
    let (target_xml, _) = section.split_once("</Target>").expect("target end");
    let positions: Vec<_> = ["Debug", "Release", "MinSizeRel", "RelWithDebInfo"]
        .iter()
        .map(|cfg| {
            target_xml
                .find(&format!("'$(Configuration)|$(Platform)'=='{cfg}|x64'"))
                .unwrap_or_else(|| panic!("missing {cfg} in:\n{target_xml}"))
        })
        .collect();
    assert!(
        positions.windows(2).all(|pair| pair[0] < pair[1]),
        "expected MakeDir order Debug < Release < MinSizeRel < RelWithDebInfo, got {positions:?}\n{target_xml}"
    );
}

mod link;
