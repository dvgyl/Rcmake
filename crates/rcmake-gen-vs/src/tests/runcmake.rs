//! Solution and project behavior that the CMake 4.5.1 `RunCMake.VSSolution`
//! and `RunCMake.VS10Project` suites check.

use rcmake_diag::{PolicyId, PolicyMap, PolicyStatus};
use rcmake_host::Stream;
use rcmake_model::{CustomCommand, SourceGroupDef};
use rcmake_resolve::CustomOutputOwners;

use super::*;

fn dirs() -> (std::path::PathBuf, std::path::PathBuf) {
    let root = if cfg!(windows) { "C:/work" } else { "/work" };
    (Path::new(root).join("src"), Path::new(root).join("build"))
}

fn new_model(source: &Path, bin: &Path) -> ProjectModel {
    let mut model = ProjectModel::default();
    model.generator = Some(GeneratorKind::VisualStudio(VsVersion::Vs17));
    model.set_root_dirs(
        rcmake_host::path::from_native_cmake(source),
        rcmake_host::path::from_native_cmake(bin),
    );
    let root = model.dir_mut(DirId::ROOT);
    root.vars
        .insert("CMAKE_VS_PLATFORM_NAME".into(), "x64".into());
    root.vars
        .insert("CMAKE_CONFIGURATION_TYPES".into(), "Debug;Release".into());
    root.vars.insert("PROJECT_NAME".into(), "Demo".into());
    model
}

fn src(source: &Path, name: &str) -> String {
    format!("{}/{name}", string(&from_native(source)))
}

fn compile_source(path: &str, language: &str) -> IdeSource {
    IdeSource {
        skip_precompile_headers: false,
        path: path.into(),
        kind: IdeSourceKind::Compile {
            language: language.into(),
        },
        group: BString::default(),
        generated: false,
        excluded_configs: Vec::new(),
        file_set: None,
        package_location: None,
        compile: None,
    }
}

fn settings(language: &str, flags: &[&str], defines: &[&str]) -> IdeCompileSettings {
    IdeCompileSettings {
        language: language.into(),
        flags: flags.iter().map(|flag| bt(flag)).collect(),
        defines: defines.iter().map(|define| bt(define)).collect(),
        includes: Vec::new(),
    }
}

/// Add IDE facts of a target for each configuration of `result`.
fn facts(
    result: &mut GenerationResult,
    id: TargetId,
    name: &str,
    kind: TargetKind,
    ide: impl Fn(&str) -> IdeFacts,
) {
    if result.configs.is_empty() {
        result.configs.resize_with(2, Default::default);
        for (config, name) in result.configs.iter_mut().zip(["Debug", "Release"]) {
            config.0 = name.into();
        }
    }
    for config in result.configs.clone() {
        let ide = ide(&string(&config.0));
        result.targets.push(ResolvedTarget {
            fortran: None,
            id,
            config,
            metadata: TargetMetadata {
                name: name.into(),
                kind,
                source_dir: Default::default(),
                binary_dir: Default::default(),
                backtrace: Default::default(),
                folder: None,
                exclude_from_all: false,
                generator_provided: false,
            },
            artifacts: Default::default(),
            compile_groups: Vec::new(),
            compile_units: Vec::new(),
            link: None,
            dependencies: Vec::new(),
            cxx_modules: None,
            ide: Some(Box::new(ide)),
        });
    }
}

#[test]
fn solutions_write_user_sections_items_and_cmake_defaults() {
    let (source, bin) = dirs();
    let host = PureHost::new(bin.clone(), bin.join("cmake"));
    let mut model = new_model(&source, &bin);
    let root = model.dir_mut(DirId::ROOT);
    root.props.set(
        "VS_GLOBAL_SECTION_PRE_Presec".into(),
        "Key1=Value1;Key2= Value with spaces ;NoValue".into(),
    );
    root.props.set(
        "VS_GLOBAL_SECTION_POST_ExtensibilityGlobals".into(),
        "Key=1".into(),
    );
    root.props
        .set("VS_GLOBAL_SECTION_POST_Empty".into(), BString::default());
    root.props
        .set("VS_SOLUTION_ITEMS".into(), "notes.txt;group/a.txt".into());
    // Each directory starts with the default groups of CMake.
    root.source_groups = rcmake_resolve::ide::default_source_groups();
    root.source_groups.push(SourceGroupDef {
        name: "Outer".into(),
        regex: None,
        files: Default::default(),
        children: vec![SourceGroupDef {
            name: "Inner".into(),
            regex: Some("a\\.txt$".into()),
            files: Default::default(),
            children: Vec::new(),
        }],
    });
    generate(
        VsVersion::Vs17,
        &GenerationResult::default(),
        &model,
        &host,
        &bin,
    )
    .expect("generate solution");
    let sln = contents(&host, &bin, "Demo.sln");
    for expected in [
        "\tGlobalSection(Empty) = postSolution\r\n\tEndGlobalSection\r\n",
        "\tGlobalSection(ExtensibilityGlobals) = postSolution\r\n\t\tKey = 1\r\n\t\tSolutionGuid = {",
        "\tGlobalSection(Presec) = preSolution\r\n\t\tKey1 = Value1\r\n\t\tKey2 = Value with spaces\r\n\tEndGlobalSection\r\n",
        "\tGlobalSection(ExtensibilityAddIns) = postSolution\r\n\tEndGlobalSection\r\n",
        r#"Project("{2150E333-8FDC-42A3-9474-1A3956D46DE8}") = "Inner", "Outer\Inner", "#,
        r#"Project("{2150E333-8FDC-42A3-9474-1A3956D46DE8}") = "Solution Items", "Solution Items", "#,
        "\tProjectSection(SolutionItems) = preProject\r\n",
    ] {
        assert!(sln.contains(expected), "{expected}\n{sln}");
    }
    // The user section replaces the default one.
    assert_eq!(
        sln.matches("GlobalSection(ExtensibilityGlobals)").count(),
        1
    );
    let notes = src(&source, "notes.txt");
    assert!(sln.contains(&format!("\t\t{notes} = {notes}\r\n")), "{sln}");

    generate(
        VsVersion::Vs18,
        &GenerationResult::default(),
        &model,
        &host,
        &bin,
    )
    .expect("generate slnx");
    let slnx = contents(&host, &bin, "Demo.slnx");
    for expected in [
        r#"<Properties Name="Empty" Scope="PostLoad"/>"#,
        r#"<Properties Name="ExtensibilityGlobals" Scope="PostLoad">"#,
        r#"<Properties Name="Key" Value="1"/>"#,
        r#"<Properties Name="Presec">"#,
        r#"<Folder Name="/Outer/Inner/">"#,
        &format!(r#"<File Path="{}"/>"#, src(&source, "group/a.txt")),
        &format!(r#"<File Path="{notes}"/>"#),
    ] {
        assert!(slnx.contains(expected), "{expected}\n{slnx}");
    }
    // The XML format has no default sections.
    assert!(!slnx.contains("SolutionGuid"), "{slnx}");
}

#[test]
fn startup_project_must_name_a_target() {
    let (source, bin) = dirs();
    let host = PureHost::new(bin.clone(), bin.join("cmake"));
    let mut model = new_model(&source, &bin);
    model
        .add_target(target("tool", TargetKind::Utility, ""))
        .expect("utility");
    model
        .dir_mut(DirId::ROOT)
        .props
        .set("VS_STARTUP_PROJECT".into(), "missing".into());
    generate(
        VsVersion::Vs17,
        &GenerationResult::default(),
        &model,
        &host,
        &bin,
    )
    .expect("generate solution");
    let stderr = string(&host.stream_bytes(Stream::Stderr));
    assert!(
        stderr.starts_with("CMake Warning (author) in CMakeLists.txt:\n"),
        "{stderr}"
    );
    assert!(stderr.contains("Directory property VS_STARTUP_PROJECT specifies target 'missing'"));
    let sln = contents(&host, &bin, "Demo.sln");
    let first = sln
        .lines()
        .find(|line| line.starts_with("Project("))
        .unwrap_or_default();
    assert!(first.contains("\"ALL_BUILD\""), "{sln}");

    let host = PureHost::new(bin.clone(), bin.join("cmake"));
    model
        .dir_mut(DirId::ROOT)
        .props
        .set("VS_STARTUP_PROJECT".into(), "tool".into());
    generate(
        VsVersion::Vs17,
        &GenerationResult::default(),
        &model,
        &host,
        &bin,
    )
    .expect("generate solution");
    assert!(host.stream_bytes(Stream::Stderr).is_empty());
    let sln = contents(&host, &bin, "Demo.sln");
    let first = sln
        .lines()
        .find(|line| line.starts_with("Project("))
        .unwrap_or_default();
    assert!(first.contains("\"tool\""), "{sln}");
}

#[test]
fn folders_follow_policy_cmp0143_without_use_folders() {
    let (source, bin) = dirs();
    let mut model = new_model(&source, &bin);
    let mut app = target("app", TargetKind::Utility, "");
    app.props.set("FOLDER".into(), "Tools".into());
    model.add_target(app).expect("utility");
    for (status, folders) in [(None, false), (Some(PolicyStatus::New), true)] {
        let host = PureHost::new(bin.clone(), bin.join("cmake"));
        model.dir_mut(DirId::ROOT).final_policies = status.map(|status| {
            let mut policies = PolicyMap::default();
            policies.set(PolicyId::new(143), status).expect("policy");
            policies
        });
        generate(
            VsVersion::Vs17,
            &GenerationResult::default(),
            &model,
            &host,
            &bin,
        )
        .expect("generate solution");
        let sln = contents(&host, &bin, "Demo.sln");
        assert_eq!(sln.contains("CMakePredefinedTargets"), folders, "{sln}");
        assert_eq!(sln.contains("\"Tools\", \"Tools\""), folders, "{sln}");
    }
}

#[test]
fn deploy_and_default_build_follow_cmake_rules() {
    let (source, bin) = dirs();
    let host = PureHost::new(bin.clone(), bin.join("cmake"));
    write_if_changed(&host, &bin.join("CPackConfig.cmake"), b"").expect("CPack configuration");
    let mut model = new_model(&source, &bin);
    model.dir_mut(DirId::ROOT).vars.insert(
        "CMAKE_VS_INCLUDE_PACKAGE_TO_DEFAULT_BUILD".into(),
        "ON".into(),
    );
    let mut lib = target("lib", TargetKind::SharedLibrary, "");
    lib.props.set(
        "VS_SOLUTION_DEPLOY".into(),
        "$<NOT:$<CONFIG:Release>>".into(),
    );
    let lib = model.add_target(lib).expect("library");
    // `add_custom_target()` without `ALL` excludes the target from `ALL_BUILD`.
    let mut tool = target("tool", TargetKind::Utility, "");
    tool.props.set("VS_SOLUTION_DEPLOY".into(), "ON".into());
    tool.props.set("EXCLUDE_FROM_ALL".into(), "TRUE".into());
    model.add_target(tool).expect("utility");
    let mut result = GenerationResult::default();
    facts(
        &mut result,
        lib,
        "lib",
        TargetKind::SharedLibrary,
        |config| {
            let mut ide = IdeFacts::default();
            ide.attributes.insert(
                "VS_SOLUTION_DEPLOY".into(),
                if config == "Release" { "0" } else { "1" }.into(),
            );
            ide
        },
    );
    generate(VsVersion::Vs17, &result, &model, &host, &bin).expect("generate solution");
    let sln = contents(&host, &bin, "Demo.sln");
    let guid = |name: &str| guid::for_name(&model, &string(&from_native(&bin)), name);
    let (lib, tool, all, package) = (
        guid("lib"),
        guid("tool"),
        guid("ALL_BUILD"),
        guid("PACKAGE"),
    );
    assert!(sln.contains(&format!("{lib}.Debug|x64.Deploy.0")), "{sln}");
    assert!(
        !sln.contains(&format!("{lib}.Release|x64.Deploy.0")),
        "{sln}"
    );
    assert!(
        sln.contains(&format!("{tool}.Release|x64.Deploy.0")),
        "{sln}"
    );
    // A utility that no project depends on is not in the default build.
    assert!(!sln.contains(&format!("{tool}.Debug|x64.Build.0")), "{sln}");
    // PACKAGE builds by default, and so does ALL_BUILD, which it needs.
    assert!(
        sln.contains(&format!("{package}.Debug|x64.Build.0")),
        "{sln}"
    );
    assert!(sln.contains(&format!("{all}.Debug|x64.Build.0")), "{sln}");
}

/// A C++ library with IDE facts that use many target properties.
fn property_project(source: &Path, bin: &Path) -> (ProjectModel, GenerationResult) {
    let mut model = new_model(source, bin);
    let root = model.dir_mut(DirId::ROOT);
    root.vars.insert(
        "CMAKE_VS_SDK_EXECUTABLE_DIRECTORIES".into(),
        r"$(VC_ExecutablePath_x86);C:\SDK\".into(),
    );
    root.vars
        .insert("CMAKE_IMPORT_LIBRARY_SUFFIX".into(), BString::default());
    let mut lib = target("lib", TargetKind::SharedLibrary, "");
    for (key, value) in [
        ("VS_CONFIGURATION_TYPE", "MyType $<CONFIG>"),
        ("VS_PLATFORM_TOOLSET", "MyToolset"),
        ("VS_NO_COMPILE_BATCHING", "ON"),
        ("VS_USER_PROPS", "C:/props/my.props"),
        ("VS_FILTER_PROPS", "C:/props/my.props"),
        ("VS_PROJECT_IMPORT", "imports/a.props"),
        (
            "VS_FRAMEWORK_REFERENCES",
            "Microsoft.WindowsDesktop.App.WPF",
        ),
        ("DOTNET_TARGET_FRAMEWORK_VERSION", "v4.7.2"),
        ("VERSION", "2.5.1"),
    ] {
        lib.props.set(key.into(), value.into());
    }
    let id = model.add_target(lib).expect("library");
    let main = src(source, "main.cpp");
    let mut result = GenerationResult::default();
    facts(
        &mut result,
        id,
        "lib",
        TargetKind::SharedLibrary,
        |config| {
            let mut ide = IdeFacts::default();
            ide.attributes.insert(
                "VS_CONFIGURATION_TYPE".into(),
                format!("MyType {config}").into(),
            );
            ide.sources.push(compile_source(&main, "CXX"));
            ide.compile.push(settings(
                "CXX",
                &["/Qspectre", "/FIfirst.h", "/FI", "second.h"],
                &["_UNICODE=1"],
            ));
            ide.link_items
                .push(IdeLinkItem::File("C:/deps/extra.targets".into()));
            ide
        },
    );
    (model, result)
}

#[test]
fn projects_write_target_properties_as_cmake_does() {
    let (source, bin) = dirs();
    let host = PureHost::new(bin.clone(), bin.join("cmake"));
    let (model, result) = property_project(&source, &bin);
    generate(VsVersion::Vs17, &result, &model, &host, &bin).expect("generate project");
    let project = contents(&host, &bin, "lib.vcxproj");
    for expected in [
        "<ConfigurationType>MyType Debug</ConfigurationType>",
        "<ConfigurationType>MyType Release</ConfigurationType>",
        "<CharacterSet>Unicode</CharacterSet>",
        "<PlatformToolset>MyToolset</PlatformToolset>",
        "<SpectreMitigation>Spectre</SpectreMitigation>",
        "<ObjectFileName>$(IntDir)%(filename).obj</ObjectFileName>",
        "<ForcedIncludeFiles>first.h;second.h</ForcedIncludeFiles>",
        "<PrecompiledHeader>NotUsing</PrecompiledHeader>",
        "<TargetFrameworkVersion>v4.7.2</TargetFrameworkVersion>",
        r#"<ExecutablePath Condition="'$(Configuration)|$(Platform)'=='Debug|x64'">$(VC_ExecutablePath_x86);C:\SDK\</ExecutablePath>"#,
        r#"<Import Project="C:\props\my.props" Condition="exists('C:\props\my.props')" Label="LocalAppDataPlatform" />"#,
        r#"<FrameworkReference Include="Microsoft.WindowsDesktop.App.WPF" />"#,
        r#"<Import Project="C:\deps\extra.targets" Condition="Exists('C:\deps\extra.targets') And ('$(Configuration)'=='Debug' Or '$(Configuration)'=='Release')" />"#,
        "<Version>2.5</Version>",
        "<SubSystem>Console</SubSystem>",
        "<GenerateDebugInformation>false</GenerateDebugInformation>",
        "<DataExecutionPrevention></DataExecutionPrevention>",
        "<IgnoreSpecificDefaultLibraries>%(IgnoreSpecificDefaultLibraries)</IgnoreSpecificDefaultLibraries>",
    ] {
        assert!(project.contains(expected), "{expected}\n{project}");
    }
    let import = format!(
        r#"<Import Project="{}" />"#,
        windows(&src(&source, "imports/a.props"))
    );
    assert!(project.contains(&import), "{import}\n{project}");
    // The Spectre setting is in the configuration group, not in ClCompile.
    let definitions = project
        .split_once("<ItemDefinitionGroup")
        .map_or("", |(_, rest)| rest);
    assert!(!definitions.contains("SpectreMitigation"), "{project}");
    // An empty import library suffix means that there is no import library,
    // and a `.targets` file is not a linker input.
    assert!(!project.contains("<ImportLibrary>"), "{project}");
    assert!(!project.contains("extra.targets;"), "{project}");
    let filters = contents(&host, &bin, "lib.vcxproj.filters");
    assert!(filters.contains(r"C:\props\my.props"), "{filters}");
}

#[test]
fn bad_dpi_values_report_all_targets_and_fail_generation() {
    let (source, bin) = dirs();
    let host = PureHost::new(bin.clone(), bin.join("cmake"));
    let mut model = new_model(&source, &bin);
    let mut result = GenerationResult::default();
    for (name, value) in [("a", "Foo"), ("b", "Bar")] {
        let mut app = target(name, TargetKind::Executable, "");
        app.props.set("VS_DPI_AWARE".into(), value.into());
        let id = model.add_target(app).expect("executable");
        facts(&mut result, id, name, TargetKind::Executable, |_| {
            IdeFacts::default()
        });
    }
    let error = generate(VsVersion::Vs17, &result, &model, &host, &bin)
        .expect_err("bad DPI values")
        .to_string();
    assert!(
        error.contains("Bad parameter for VS_DPI_AWARE: Bar"),
        "{error}"
    );
    assert_eq!(
        string(&host.stream_bytes(Stream::Stderr)),
        "CMake Error: Bad parameter for VS_DPI_AWARE: Foo\n"
    );
    // The other projects are still written.
    assert!(contents(&host, &bin, "b.vcxproj").contains("<ProjectName>b</ProjectName>"));
}

#[test]
fn custom_commands_use_cmake_rule_files_and_parallel_policy() {
    let (source, bin) = dirs();
    let host = PureHost::new(bin.clone(), bin.join("cmake"));
    let mut model = new_model(&source, &bin);
    let top = string(&from_native(&bin));
    let tool = model
        .add_target(target("tool", TargetKind::Utility, ""))
        .expect("utility");
    let mut policies = PolicyMap::default();
    policies
        .set(PolicyId::new(147), PolicyStatus::New)
        .expect("policy");
    let mut command = CustomCommand::new(
        DirId::ROOT,
        CustomCommandKind::Output,
        Default::default(),
        policies,
    );
    command
        .outputs
        .push(bt(&format!("{top}/out-$<CONFIG>.txt")));
    command
        .command_lines
        .push(vec!["cmake".into(), "-E".into(), "touch".into()]);
    let id = model.add_command(command).expect("custom command");
    let mut result = GenerationResult::default();
    facts(&mut result, tool, "tool", TargetKind::Utility, |config| {
        let mut ide = IdeFacts::default();
        ide.custom.push(IdeCustomStep {
            phase: IdeCustomPhase::Rule,
            commands: vec![vec!["cmake".into(), "-E".into(), "touch".into()]],
            working_dir: BString::default(),
            outputs: vec![format!("{top}/out-{config}.txt").into()],
            byproducts: Vec::new(),
            depends: Vec::new(),
            comment: None,
            always_run: false,
        });
        ide
    });
    for id_config in result.configs.clone() {
        let config = string(&id_config.0);
        let mut graph = BuildGraph {
            config: id_config,
            ..BuildGraph::default()
        };
        graph.steps.push(BuildStep::Custom(CustomStep {
            outputs: vec![format!("{top}/out-{config}.txt").into()],
            ..CustomStep::default()
        }));
        graph.custom_output_owners.insert(
            0,
            CustomOutputOwners {
                command: id,
                targets: Default::default(),
                explicit_only: false,
                shared_output: false,
            },
        );
        result.graphs.push(graph);
    }
    generate(VsVersion::Vs17, &result, &model, &host, &bin).expect("generate project");
    let project = contents(&host, &bin, "tool.vcxproj");
    assert!(project.contains(r#"\out-(CONFIG).txt.rule""#), "{project}");
    assert!(project.contains(">true</BuildInParallel>"), "{project}");
    // Each project also has its directory listfile as a custom build item.
    let listfile = windows(&src(&source, "CMakeLists.txt"));
    assert!(
        project.contains(&format!(r#"<CustomBuild Include="{listfile}">"#)),
        "{project}"
    );
    assert!(project.contains("Building Custom Rule "), "{project}");
}

#[test]
fn source_settings_and_tool_overrides_follow_cmake() {
    let (source, bin) = dirs();
    let host = PureHost::new(bin.clone(), bin.join("cmake"));
    let mut model = new_model(&source, &bin);
    let (main, shader, text) = (
        src(&source, "main.cpp"),
        src(&source, "shader.hlsl"),
        src(&source, "notes.txt"),
    );
    let mut lib = target("lib", TargetKind::StaticLibrary, "");
    lib.props.set(
        "VS_SOURCE_SETTINGS_FXCompile".into(),
        "ShaderType=Pixel".into(),
    );
    let id = model.add_target(lib).expect("library");
    let root = model.dir_mut(DirId::ROOT);
    let mut props = rcmake_model::PropertyMap::default();
    props.set("VS_SETTINGS".into(), "Custom=1;Empty=".into());
    root.source_props.insert(main.as_str().into(), props);
    let mut props = rcmake_model::PropertyMap::default();
    props.set("VS_TOOL_OVERRIDE".into(), "TextTool".into());
    root.source_props.insert(text.as_str().into(), props);
    let mut result = GenerationResult::default();
    facts(&mut result, id, "lib", TargetKind::StaticLibrary, |_| {
        let mut ide = IdeFacts::default();
        ide.sources.push(compile_source(&main, "CXX"));
        for path in [&shader, &text] {
            let mut other = compile_source(path, "CXX");
            other.kind = IdeSourceKind::Other;
            ide.sources.push(other);
        }
        ide.compile.push(settings("CXX", &[], &[]));
        ide
    });
    generate(VsVersion::Vs17, &result, &model, &host, &bin).expect("generate project");
    let project = contents(&host, &bin, "lib.vcxproj");
    for expected in [
        "<Custom>1</Custom>",
        &format!(r#"<FXCompile Include="{}">"#, windows(&shader)),
        "<ShaderType>Pixel</ShaderType>",
        &format!(r#"<TextTool Include="{}" />"#, windows(&text)),
    ] {
        assert!(project.contains(expected), "{expected}\n{project}");
    }
    // A setting without a value is not written.
    assert!(!project.contains("<Empty>"), "{project}");
    let filters = contents(&host, &bin, "lib.vcxproj.filters");
    assert!(filters.contains("<TextTool Include="), "{filters}");
}

/// Add output commands and their IDE and graph steps to a utility project.
/// Each entry is the output file name and the command words.
fn add_output_commands(
    model: &mut ProjectModel,
    result: &mut GenerationResult,
    tool: TargetId,
    top: &str,
    main: Option<&str>,
    commands: &[(&str, &[&str])],
) {
    let mut ids = Vec::new();
    for (output, words) in commands {
        let mut command = CustomCommand::new(
            DirId::ROOT,
            CustomCommandKind::Output,
            Default::default(),
            Default::default(),
        );
        command.outputs.push(bt(&format!("{top}/{output}")));
        command.main_dependency = main.map(BString::from);
        command
            .command_lines
            .push(words.iter().map(|word| BString::from(*word)).collect());
        ids.push(model.add_command(command).expect("custom command"));
    }
    facts(result, tool, "tool", TargetKind::Utility, |_| {
        let mut ide = IdeFacts::default();
        for (output, words) in commands {
            ide.custom.push(IdeCustomStep {
                phase: IdeCustomPhase::Rule,
                commands: vec![words.iter().map(|word| BString::from(*word)).collect()],
                working_dir: BString::default(),
                outputs: vec![format!("{top}/{output}").into()],
                byproducts: Vec::new(),
                depends: Vec::new(),
                comment: None,
                always_run: false,
            });
        }
        ide
    });
    for config in result.configs.clone() {
        let mut graph = BuildGraph {
            config,
            ..BuildGraph::default()
        };
        for (index, (id, (output, _))) in ids.iter().zip(commands).enumerate() {
            graph.steps.push(BuildStep::Custom(CustomStep {
                outputs: vec![format!("{top}/{output}").into()],
                ..CustomStep::default()
            }));
            graph.custom_output_owners.insert(
                index,
                CustomOutputOwners {
                    command: *id,
                    targets: Default::default(),
                    explicit_only: false,
                    shared_output: false,
                },
            );
        }
        result.graphs.push(graph);
    }
}

#[test]
fn commands_that_share_a_main_dependency_keep_both_rules() {
    let (source, bin) = dirs();
    let host = PureHost::new(bin.clone(), bin.join("cmake"));
    let mut model = new_model(&source, &bin);
    model.cache.insert(
        "_RCMAKE_NO_WARNING_AS_ERROR".into(),
        CacheEntry {
            value: "ON".into(),
            ty: CacheType::Internal,
            help: Default::default(),
            advanced: false,
            props: Default::default(),
        },
    );
    let top = string(&from_native(&bin));
    let tool = model
        .add_target(target("tool", TargetKind::Utility, ""))
        .expect("utility");
    let mut result = GenerationResult::default();
    add_output_commands(
        &mut model,
        &mut result,
        tool,
        &top,
        Some("in.txt"),
        &[
            ("a.txt", &["cmake", "-E", "echo", "first"]),
            ("b.txt", &["cmake", "-E", "echo", "second"]),
        ],
    );
    generate(VsVersion::Vs17, &result, &model, &host, &bin).expect("generate project");
    let project = contents(&host, &bin, "tool.vcxproj");
    // The first command owns the main dependency. The second command has
    // other command lines, so it gets its own rule file.
    let main = windows(&src(&source, "in.txt"));
    assert!(
        project.contains(&format!(r#"<CustomBuild Include="{main}">"#)),
        "{project}"
    );
    assert!(project.contains(r#"\b.txt.rule""#), "{project}");
    assert!(!project.contains(r#"\a.txt.rule""#), "{project}");
    assert!(project.contains("cmake -E echo first"), "{project}");
    assert!(project.contains("cmake -E echo second"), "{project}");
    // The listfile rule runs the stamp check of CMake for its directory.
    for expected in [
        format!("-S{} -B{top}", string(&from_native(&source))),
        format!("--check-stamp-file {top}/CMakeFiles/generate.stamp --compile-no-warning-as-error"),
        "<UseUtf8Encoding>Always</UseUtf8Encoding>".to_owned(),
    ] {
        assert!(project.contains(&expected), "{expected}\n{project}");
    }
}

#[test]
fn managed_targets_link_with_full_debug_information() {
    let (source, bin) = dirs();
    let host = PureHost::new(bin.clone(), bin.join("cmake"));
    let mut model = new_model(&source, &bin);
    let mut lib = target("lib", TargetKind::SharedLibrary, "");
    lib.props
        .set("COMMON_LANGUAGE_RUNTIME".into(), BString::default());
    let id = model.add_target(lib).expect("library");
    let main = src(&source, "main.cpp");
    let mut result = GenerationResult::default();
    facts(&mut result, id, "lib", TargetKind::SharedLibrary, |_| {
        let mut ide = IdeFacts::default();
        ide.sources.push(compile_source(&main, "CXX"));
        ide.compile.push(settings("CXX", &[], &[]));
        ide.link_options.push(bt("/DEBUG:FASTLINK"));
        ide
    });
    generate(VsVersion::Vs17, &result, &model, &host, &bin).expect("generate project");
    let project = contents(&host, &bin, "lib.vcxproj");
    for expected in [
        "<CompileAsManaged>true</CompileAsManaged>",
        "<ManagedAssembly>true</ManagedAssembly>",
        "<OutputType>Library</OutputType>",
        "<GenerateDebugInformation>Debug</GenerateDebugInformation>",
    ] {
        assert!(project.contains(expected), "{expected}\n{project}");
    }
    assert!(!project.contains("DebugFastLink"), "{project}");
}

#[test]
fn msvc_runtime_library_follows_the_language_flags() {
    let (source, bin) = dirs();
    let host = PureHost::new(bin.clone(), bin.join("cmake"));
    let mut model = new_model(&source, &bin);
    let root = model.dir_mut(DirId::ROOT);
    for (key, value) in [
        ("CMAKE_MSVC_RUNTIME_LIBRARY_DEFAULT", "MultiThreadedDLL"),
        (
            "CMAKE_CXX_COMPILE_OPTIONS_MSVC_RUNTIME_LIBRARY_MultiThreadedDLL",
            "-MD",
        ),
        ("CMAKE_CXX_FLAGS", "/MT /W3"),
    ] {
        root.vars.insert(key.into(), value.into());
    }
    let main = src(&source, "main.cpp");
    let mut result = GenerationResult::default();
    // The first target has `/MT` only in `CMAKE_CXX_FLAGS`. The second also
    // has `/MTd` in its compile options, which come after the runtime flag.
    for (name, flags) in [
        ("plain", &["SHELL:/MT /W3"][..]),
        ("option", &["SHELL:/MT /W3", "/MTd"][..]),
    ] {
        let id = model
            .add_target(target(name, TargetKind::StaticLibrary, ""))
            .expect("library");
        facts(&mut result, id, name, TargetKind::StaticLibrary, |_| {
            let mut ide = IdeFacts::default();
            ide.sources.push(compile_source(&main, "CXX"));
            ide.compile.push(settings("CXX", flags, &[]));
            ide
        });
    }
    generate(VsVersion::Vs17, &result, &model, &host, &bin).expect("generate projects");
    let plain = contents(&host, &bin, "plain.vcxproj");
    assert!(
        plain.contains("<RuntimeLibrary>MultiThreadedDLL</RuntimeLibrary>"),
        "{plain}"
    );
    assert!(
        !plain.contains("<RuntimeLibrary>MultiThreaded</RuntimeLibrary>"),
        "{plain}"
    );
    let option = contents(&host, &bin, "option.vcxproj");
    assert!(
        option.contains("<RuntimeLibrary>MultiThreadedDebug</RuntimeLibrary>"),
        "{option}"
    );
}

#[test]
fn package_default_build_is_evaluated_for_each_configuration() {
    let (source, bin) = dirs();
    let host = PureHost::new(bin.clone(), bin.join("cmake"));
    write_if_changed(&host, &bin.join("CPackConfig.cmake"), b"").expect("CPack configuration");
    let mut model = new_model(&source, &bin);
    model.dir_mut(DirId::ROOT).vars.insert(
        "CMAKE_VS_INCLUDE_PACKAGE_TO_DEFAULT_BUILD".into(),
        "$<CONFIG:Release>".into(),
    );
    model
        .add_target(target("lib", TargetKind::StaticLibrary, ""))
        .expect("library");
    generate(
        VsVersion::Vs17,
        &GenerationResult::default(),
        &model,
        &host,
        &bin,
    )
    .expect("generate solution");
    let sln = contents(&host, &bin, "Demo.sln");
    let package = guid::for_name(&model, &string(&from_native(&bin)), "PACKAGE");
    assert!(
        sln.contains(&format!("{package}.Release|x64.Build.0")),
        "{sln}"
    );
    assert!(
        !sln.contains(&format!("{package}.Debug|x64.Build.0")),
        "{sln}"
    );
}

#[test]
fn assembler_items_use_tool_overrides_and_settings() {
    let (source, bin) = dirs();
    let host = PureHost::new(bin.clone(), bin.join("cmake"));
    let mut model = new_model(&source, &bin);
    let (plain, other) = (src(&source, "plain.asm"), src(&source, "other.asm"));
    let id = model
        .add_target(target("lib", TargetKind::StaticLibrary, ""))
        .expect("library");
    let root = model.dir_mut(DirId::ROOT);
    let mut props = rcmake_model::PropertyMap::default();
    props.set("VS_SETTINGS".into(), "Extra=1".into());
    root.source_props.insert(plain.as_str().into(), props);
    let mut props = rcmake_model::PropertyMap::default();
    props.set("VS_TOOL_OVERRIDE".into(), "MyAsm".into());
    root.source_props.insert(other.as_str().into(), props);
    let mut result = GenerationResult::default();
    facts(&mut result, id, "lib", TargetKind::StaticLibrary, |_| {
        let mut ide = IdeFacts::default();
        ide.sources.push(compile_source(&plain, "ASM_MASM"));
        ide.sources.push(compile_source(&other, "ASM_MASM"));
        ide
    });
    generate(VsVersion::Vs17, &result, &model, &host, &bin).expect("generate project");
    let project = contents(&host, &bin, "lib.vcxproj");
    let filters = contents(&host, &bin, "lib.vcxproj.filters");
    let plain_item = format!(r#"<MASM Include="{}""#, windows(&plain));
    let other_item = format!(r#"<MyAsm Include="{}""#, windows(&other));
    for expected in [plain_item.as_str(), other_item.as_str(), "<Extra>1</Extra>"] {
        assert!(project.contains(expected), "{expected}\n{project}");
    }
    // The filters file names the same item types.
    for expected in [plain_item.as_str(), other_item.as_str()] {
        assert!(filters.contains(expected), "{expected}\n{filters}");
    }
    assert!(!project.contains(&format!(r#"<MASM Include="{}""#, windows(&other))));
}

#[test]
fn invalid_item_and_metadata_names_are_errors() {
    let (source, bin) = dirs();
    let host = PureHost::new(bin.clone(), bin.join("cmake"));
    let mut model = new_model(&source, &bin);
    let (main, notes) = (src(&source, "main.cpp"), src(&source, "notes.txt"));
    let id = model
        .add_target(target("lib", TargetKind::StaticLibrary, ""))
        .expect("library");
    let root = model.dir_mut(DirId::ROOT);
    let mut props = rcmake_model::PropertyMap::default();
    props.set("VS_SETTINGS".into(), "Bad Key=1;Good=2".into());
    root.source_props.insert(main.as_str().into(), props);
    let mut props = rcmake_model::PropertyMap::default();
    props.set("VS_TOOL_OVERRIDE".into(), "Bad Name".into());
    root.source_props.insert(notes.as_str().into(), props);
    let mut result = GenerationResult::default();
    facts(&mut result, id, "lib", TargetKind::StaticLibrary, |_| {
        let mut ide = IdeFacts::default();
        ide.sources.push(compile_source(&main, "CXX"));
        let mut other = compile_source(&notes, "CXX");
        other.kind = IdeSourceKind::Other;
        ide.sources.push(other);
        ide.compile.push(settings("CXX", &[], &[]));
        ide
    });
    let error = generate(VsVersion::Vs17, &result, &model, &host, &bin)
        .expect_err("invalid names")
        .to_string();
    assert!(
        error.contains(r#"VS_TOOL_OVERRIDE value "Bad Name""#),
        "{error}"
    );
    let stderr = string(&host.stream_bytes(Stream::Stderr));
    assert!(stderr.contains(r#"names "Bad Key""#), "{stderr}");
    // The project file stays valid XML and keeps the valid values.
    let project = contents(&host, &bin, "lib.vcxproj");
    assert!(project.contains("<Good>2</Good>"), "{project}");
    assert!(
        project.contains(&format!(r#"<None Include="{}" />"#, windows(&notes))),
        "{project}"
    );
    assert!(!project.contains("Bad"), "{project}");
}

#[test]
fn source_group_matching_reports_a_used_up_step_budget() {
    let mut matcher = crate::plan::RegexMatcher::with_budget(8);
    assert!(matcher.check().is_ok());
    let text = "a".repeat(64);
    assert!(!matcher.matches(b"(a|aa)*b", text.as_bytes()));
    let error = matcher.check().expect_err("used-up budget").to_string();
    assert!(error.contains("step limit"), "{error}");
    // After that, nothing matches.
    assert!(!matcher.matches(b"a", b"a"));
    let mut fresh = crate::plan::RegexMatcher::default();
    assert!(fresh.matches(b"a+", b"xaa"));
    assert!(fresh.check().is_ok());
}

/// A unity build library: each unity file is a path and its member sources.
/// `other` sources compile on their own.
fn unity_project(
    source: &Path,
    bin: &Path,
    unity_files: &[(String, Vec<String>)],
    other: &[String],
    toolset: Option<&str>,
) -> (ProjectModel, GenerationResult) {
    let mut model = new_model(source, bin);
    if let Some(toolset) = toolset {
        model
            .dir_mut(DirId::ROOT)
            .vars
            .insert("CMAKE_VS_PLATFORM_TOOLSET".into(), toolset.into());
    }
    let mut lib = target("tgt", TargetKind::SharedLibrary, "");
    lib.props.set("UNITY_BUILD".into(), "ON".into());
    let id = model.add_target(lib).expect("library");
    let mut result = GenerationResult::default();
    facts(&mut result, id, "tgt", TargetKind::SharedLibrary, |_| {
        let mut ide = IdeFacts::default();
        for (path, members) in unity_files {
            ide.sources.push(compile_source(path, "C"));
            ide.unity_files.push(rcmake_resolve::ide::IdeUnityFile {
                path: path.as_str().into(),
                language: "C".into(),
                member_sources: members
                    .iter()
                    .map(|member| member.as_str().into())
                    .collect(),
            });
        }
        for path in other {
            ide.sources.push(compile_source(path, "C"));
        }
        ide.compile.push(settings("C", &[], &[]));
        ide
    });
    (model, result)
}

/// The `ClCompile` start tags of a project file.
fn compile_items(project: &str) -> Vec<&str> {
    project
        .lines()
        .map(str::trim)
        .filter(|line| line.starts_with("<ClCompile Include="))
        .collect()
}

#[test]
fn unity_builds_use_native_msbuild_unity_files() {
    let (source, bin) = dirs();
    let top = string(&from_native(&bin));
    let dir = format!("{top}/CMakeFiles/tgt.dir/Unity");
    let unity = format!("{dir}/unity_0_c.c");
    let members: Vec<String> = (1..=3).map(|index| format!("{top}/s{index}.c")).collect();
    let host = PureHost::new(bin.clone(), bin.join("cmake"));
    let (model, result) = unity_project(
        &source,
        &bin,
        &[(unity.clone(), members.clone())],
        &[],
        None,
    );
    generate(VsVersion::Vs17, &result, &model, &host, &bin).expect("generate project");
    let project = contents(&host, &bin, "tgt.vcxproj");
    assert!(
        project.contains("<EnableUnitySupport>true</EnableUnitySupport>"),
        "{project}"
    );
    let items = compile_items(&project);
    let expected = format!(
        r#"<ClCompile Include="{}" IncludeInUnityFile="false" CustomUnityFile="true" UnityFilesDirectory="{dir}">"#,
        windows(&unity)
    );
    assert!(items.contains(&expected.as_str()), "{expected}\n{project}");
    for member in &members {
        let expected = format!(
            r#"<ClCompile Include="{}" IncludeInUnityFile="true" CustomUnityFile="true" UnityFilesDirectory="{dir}">"#,
            windows(member)
        );
        assert!(items.contains(&expected.as_str()), "{expected}\n{project}");
    }
    // A member compiles through the unity file, so it has no object file name.
    assert!(!project.contains(r"s1.obj"), "{project}");
    let filters = contents(&host, &bin, "tgt.vcxproj.filters");
    assert!(filters.contains(&windows(&members[0])), "{filters}");
}

#[test]
fn grouped_and_short_unity_builds_name_each_unity_directory() {
    let (source, bin) = dirs();
    let top = string(&from_native(&bin));
    // The SHORT strategy puts the unity files in `CMakeFiles/<hash>`.
    let dir = format!("{top}/CMakeFiles/9bc0d702/Unity");
    let pool_a: Vec<String> = (1..=4).map(|index| format!("{top}/s{index}.c")).collect();
    let pool_b: Vec<String> = (5..=7).map(|index| format!("{top}/s{index}.c")).collect();
    let alone = format!("{top}/s8.c");
    let host = PureHost::new(bin.clone(), bin.join("cmake"));
    let (model, result) = unity_project(
        &source,
        &bin,
        &[
            (format!("{dir}/unity_poolA_c.c"), pool_a),
            (format!("{dir}/unity_poolB_c.c"), pool_b),
        ],
        std::slice::from_ref(&alone),
        None,
    );
    generate(VsVersion::Vs17, &result, &model, &host, &bin).expect("generate project");
    let project = contents(&host, &bin, "tgt.vcxproj");
    let items = compile_items(&project);
    let count = |text: &str| items.iter().filter(|item| item.contains(text)).count();
    assert_eq!(count(r#"IncludeInUnityFile="false""#), 2, "{project}");
    assert_eq!(count(r#"IncludeInUnityFile="true""#), 7, "{project}");
    assert_eq!(
        count(&format!(r#"UnityFilesDirectory="{dir}""#)),
        9,
        "{project}"
    );
    // A source in no unity group compiles on its own.
    let alone_item = format!(r#"<ClCompile Include="{}">"#, windows(&alone));
    assert!(items.contains(&alone_item.as_str()), "{project}");
}

#[test]
fn old_toolsets_exclude_unity_members_from_the_build() {
    let (source, bin) = dirs();
    let top = string(&from_native(&bin));
    let unity = format!("{top}/CMakeFiles/tgt.dir/Unity/unity_0_c.c");
    let member = format!("{top}/s1.c");
    let host = PureHost::new(bin.clone(), bin.join("cmake"));
    let (model, result) = unity_project(
        &source,
        &bin,
        &[(unity, vec![member.clone()])],
        &[],
        Some("v140"),
    );
    generate(VsVersion::Vs17, &result, &model, &host, &bin).expect("generate project");
    let project = contents(&host, &bin, "tgt.vcxproj");
    assert!(!project.contains("IncludeInUnityFile"), "{project}");
    let (_, item) = project
        .split_once(&format!(r#"<ClCompile Include="{}">"#, windows(&member)))
        .expect("member item");
    let (item, _) = item.split_once("</ClCompile>").expect("member item end");
    assert_eq!(
        item.matches(">true</ExcludedFromBuild>").count(),
        2,
        "{item}"
    );
}

#[test]
fn each_directory_gets_generation_stamp_files() {
    let (source, bin) = dirs();
    let host = PureHost::new(bin.clone(), bin.join("cmake"));
    let mut model = new_model(&source, &bin);
    let (src_dir, bin_dir) = (string(&from_native(&source)), string(&from_native(&bin)));
    model.dir_mut(DirId::ROOT).listfiles = vec![
        format!("{src_dir}/CMakeLists.txt").into(),
        format!("{src_dir}/cmake/extra.cmake").into(),
        format!("{src_dir}/CMakeLists.txt").into(),
    ];
    let sub = model
        .add_directory(
            DirId::ROOT,
            format!("{src_dir}/sub").into(),
            format!("{bin_dir}/sub").into(),
            Default::default(),
        )
        .expect("subdirectory");
    model.dir_mut(sub).listfiles = vec![format!("{src_dir}/sub/CMakeLists.txt").into()];
    generate(
        VsVersion::Vs17,
        &GenerationResult::default(),
        &model,
        &host,
        &bin,
    )
    .expect("generate");
    assert_eq!(
        contents(&host, &bin, "CMakeFiles/generate.stamp"),
        "# CMake generation timestamp file for this directory.\n"
    );
    assert_eq!(
        contents(&host, &bin, "CMakeFiles/generate.stamp.depend"),
        format!(
            "# CMake generation dependency list for this directory.\n{src_dir}/CMakeLists.txt\n{src_dir}/cmake/extra.cmake\n"
        )
    );
    assert_eq!(
        contents(&host, &bin, "sub/CMakeFiles/generate.stamp.depend"),
        format!(
            "# CMake generation dependency list for this directory.\n{src_dir}/sub/CMakeLists.txt\n"
        )
    );
    assert!(contents(&host, &bin, "sub/CMakeFiles/generate.stamp").starts_with('#'));
    assert_eq!(
        contents(&host, &bin, "CMakeFiles/generate.stamp.list"),
        format!("{bin_dir}/CMakeFiles/generate.stamp\n{bin_dir}/sub/CMakeFiles/generate.stamp\n")
    );
}

/// The text of a `<tag Include="path">` element, up to its end tag.
fn item_element<'a>(project: &'a str, tag: &str, path: &str) -> &'a str {
    let start = format!(r#"<{tag} Include="{}""#, windows(path));
    project
        .split_once(&start)
        .and_then(|(_, rest)| rest.split_once(&format!("</{tag}>")))
        .map_or("", |(element, _)| element)
}

#[test]
fn cuda_unity_members_are_not_compiled_on_their_own() {
    let (source, bin) = dirs();
    let top = string(&from_native(&bin));
    let unity = format!("{top}/CMakeFiles/tgt.dir/Unity/unity_0_c.c");
    let kernel = format!("{top}/kernel.cu");
    let host = PureHost::new(bin.clone(), bin.join("cmake"));
    let mut model = new_model(&source, &bin);
    let mut lib = target("tgt", TargetKind::SharedLibrary, "");
    lib.props.set("UNITY_BUILD".into(), "ON".into());
    let id = model.add_target(lib).expect("library");
    let mut result = GenerationResult::default();
    facts(&mut result, id, "tgt", TargetKind::SharedLibrary, |_| {
        let mut ide = IdeFacts::default();
        ide.sources.push(compile_source(&unity, "C"));
        ide.sources.push(compile_source(&kernel, "CUDA"));
        ide.unity_files.push(rcmake_resolve::ide::IdeUnityFile {
            path: unity.as_str().into(),
            language: "C".into(),
            member_sources: vec![kernel.as_str().into()],
        });
        ide.compile.push(settings("C", &[], &[]));
        ide
    });
    generate(VsVersion::Vs17, &result, &model, &host, &bin).expect("generate project");
    let project = contents(&host, &bin, "tgt.vcxproj");
    // The member is still an item, and no configuration builds it.
    let element = item_element(&project, "CudaCompile", &kernel);
    assert_eq!(
        element.matches(">true</ExcludedFromBuild>").count(),
        2,
        "{project}"
    );
    assert_eq!(
        project
            .matches(&format!(r#"<CudaCompile Include="{}""#, windows(&kernel)))
            .count(),
        1,
        "{project}"
    );
    let filters = contents(&host, &bin, "tgt.vcxproj.filters");
    assert!(
        filters.contains(&format!(r#"<CudaCompile Include="{}""#, windows(&kernel))),
        "{filters}"
    );
}

#[test]
fn short_unity_members_keep_their_hashed_object_names() {
    let (source, bin) = dirs();
    let top = string(&from_native(&bin));
    let unity = format!("{top}/CMakeFiles/9bc0d702/Unity/unity_0_c.c");
    let member = format!("{top}/s1.c");
    let host = PureHost::new(bin.clone(), bin.join("cmake"));
    let (mut model, result) =
        unity_project(&source, &bin, &[(unity, vec![member.clone()])], &[], None);
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
    generate(VsVersion::Vs17, &result, &model, &host, &bin).expect("generate project");
    let project = contents(&host, &bin, "tgt.vcxproj");
    let element = item_element(&project, "ClCompile", &member);
    assert_eq!(element.matches("<ObjectFileName ").count(), 2, "{project}");
    assert!(!element.contains("s1.obj"), "{element}");
}

#[test]
fn generation_stamps_are_rewritten_on_each_generation() {
    let (source, bin) = dirs();
    let host = PureHost::new(bin.clone(), bin.join("cmake"));
    let model = new_model(&source, &bin);
    let modified = |name: &str| {
        host.metadata(&bin.join(name), Follow::Yes)
            .expect("stamp file")
            .mtime
    };
    let files = [
        "CMakeFiles/generate.stamp",
        "CMakeFiles/generate.stamp.depend",
        "CMakeFiles/generate.stamp.list",
    ];
    generate(
        VsVersion::Vs17,
        &GenerationResult::default(),
        &model,
        &host,
        &bin,
    )
    .expect("first generation");
    let first: Vec<_> = files.iter().map(|name| modified(name)).collect();
    let depend = contents(&host, &bin, "CMakeFiles/generate.stamp.depend");
    generate(
        VsVersion::Vs17,
        &GenerationResult::default(),
        &model,
        &host,
        &bin,
    )
    .expect("second generation");
    // The contents are the same, and each file is written again.
    assert_eq!(
        contents(&host, &bin, "CMakeFiles/generate.stamp.depend"),
        depend
    );
    for (name, before) in files.iter().zip(first) {
        assert_ne!(modified(name), before, "{name}");
    }
}
