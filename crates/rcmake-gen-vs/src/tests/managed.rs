//! External MSBuild projects, C# projects, and `CMAKE_MSVCIDE_RUN_PATH`.

use rcmake_diag::{PolicyId, PolicyStatus};

use super::*;

fn work_dirs() -> (std::path::PathBuf, std::path::PathBuf) {
    let root = if cfg!(windows) { "C:/work" } else { "/work" };
    (Path::new(root).join("src"), Path::new(root).join("build"))
}

fn model_for(version: VsVersion, source: &Path, bin: &Path) -> ProjectModel {
    let mut model = ProjectModel::default();
    model.generator = Some(GeneratorKind::VisualStudio(version));
    model.set_root_dirs(
        rcmake_host::path::from_native_cmake(source),
        rcmake_host::path::from_native_cmake(bin),
    );
    model
        .dir_mut(DirId::ROOT)
        .vars
        .insert("CMAKE_VS_PLATFORM_NAME".into(), "x64".into());
    // The RunCMake projects use policy CMP0143 OLD, so no solution folders.
    model.global.props.set("USE_FOLDERS".into(), "OFF".into());
    model
}

fn exists(host: &dyn Host, path: &Path) -> bool {
    host.metadata(path, Follow::Yes).is_ok()
}

/// The lines of one `.slnx` project element after its opening tag, up to and
/// including `</Project>`, without indentation.
fn slnx_project_lines<'a>(solution: &'a str, path: &str) -> Vec<&'a str> {
    let start = format!("<Project Path=\"{path}\"");
    let mut lines = solution
        .lines()
        .map(str::trim)
        .skip_while(|line| !line.starts_with(&start));
    let mut out = Vec::new();
    if lines.next().is_some_and(|line| line.ends_with("/>")) {
        return out;
    }
    for line in lines {
        out.push(line);
        if line == "</Project>" {
            break;
        }
    }
    out
}

fn cache_entry(value: &str) -> CacheEntry {
    CacheEntry {
        value: value.into(),
        ty: CacheType::Internal,
        help: "Stored GUID".into(),
        advanced: false,
        props: Default::default(),
    }
}

/// Add a target as `include_external_msproject()` records it.
fn external(
    model: &mut ProjectModel,
    name: &str,
    location: &str,
    options: &[(&str, &str)],
) -> TargetId {
    let mut target = target(name, TargetKind::Utility, "");
    target.props.set("GENERATOR_FILE_NAME".into(), name.into());
    target
        .props
        .set("EXTERNAL_MSPROJECT".into(), location.into());
    for (key, value) in options {
        if *key == "GUID" {
            model
                .cache
                .insert(format!("{name}_GUID_CMAKE").into(), cache_entry(value));
        } else {
            target.props.set((*key).into(), (*value).into());
        }
    }
    model.add_target(target).expect("external project")
}

/// Find the `.sln` entry of a project: `(type, path, GUID)`, without braces.
fn sln_entry<'a>(solution: &'a str, name: &str) -> Option<(&'a str, &'a str, &'a str)> {
    solution.lines().find_map(|line| {
        let (kind, rest) = line.strip_prefix("Project(\"{")?.split_once("}\") = \"")?;
        let (found, rest) = rest.split_once("\", \"")?;
        let (path, guid) = rest.split_once("\", \"{")?;
        let guid = guid.strip_suffix("}\"")?;
        (found == name).then_some((kind, path, guid))
    })
}

#[test]
fn external_projects_keep_type_guid_platform_and_configurations_in_sln() {
    let (source, bin) = work_dirs();
    let host = PureHost::new(bin.clone(), bin.join("cmake"));
    let mut model = model_for(VsVersion::Vs17, &source, &bin);
    // AutoType: the extension selects the type.
    for (name, extension) in [
        ("externalCS", "csproj"),
        ("externalDB", "dbproj"),
        ("externalFS", "fsproj"),
        ("externalPy", "pyproj"),
        ("externalNJS", "njsproj"),
        ("externalVB", "vbproj"),
        ("externalVD", "vdproj"),
        ("externalVF", "vfproj"),
        ("externalWAP", "wapproj"),
        ("externalWiX", "wixproj"),
    ] {
        external(&mut model, name, &format!("external.{extension}"), &[]);
    }
    // ProjType: an explicit type.
    external(
        &mut model,
        "DotNetCore",
        "DotNetCore.project",
        &[("VS_PROJECT_TYPE", "9A19103F-16F7-4668-BE54-9A1E7A4F7556")],
    );
    // CustomGuid, CustomTypePlatform, and CustomGuidTypePlatform.
    external(
        &mut model,
        "customGuid",
        "guid.project",
        &[("GUID", "aaa-bbb-ccc-000")],
    );
    external(
        &mut model,
        "customTypePlatform",
        "type.project",
        &[
            ("VS_PROJECT_TYPE", "aaa-bbb-ccc-ddd-eee"),
            ("VS_PLATFORM_MAPPING", "Custom Platform"),
        ],
    );
    external(
        &mut model,
        "customAll",
        "sub/all.project",
        &[
            ("GUID", "aaa-bbb-ccc-111"),
            ("VS_PROJECT_TYPE", "aaa-bbb-ccc-ddd-eee"),
            ("VS_PLATFORM_MAPPING", "Custom Platform"),
        ],
    );
    // CustomConfig: mapped configurations.
    external(
        &mut model,
        "customConfig",
        "config.project",
        &[
            ("GUID", "aaa-bbb-ccc-222"),
            ("MAP_IMPORTED_CONFIG_RELEASE", "Custom - Release;Ignored"),
            ("MAP_IMPORTED_CONFIG_MINSIZEREL", "Custom - MinSizeRel"),
        ],
    );
    generate(
        VsVersion::Vs17,
        &GenerationResult::default(),
        &model,
        &host,
        &bin,
    )
    .expect("generate solution");
    let solution = contents(&host, &bin, "Project.sln");

    for (name, extension, kind) in [
        (
            "externalCS",
            "csproj",
            "FAE04EC0-301F-11D3-BF4B-00C04F79EFBC",
        ),
        (
            "externalDB",
            "dbproj",
            "C8D11400-126E-41CD-887F-60BD40844F9E",
        ),
        (
            "externalFS",
            "fsproj",
            "F2A71F9B-5D33-465A-A702-920D77279786",
        ),
        (
            "externalNJS",
            "njsproj",
            "9092AA53-FB77-4645-B42D-1CCCA6BD08BD",
        ),
        (
            "externalPy",
            "pyproj",
            "888888A0-9F3D-457C-B088-3A5042F75D52",
        ),
        (
            "externalVB",
            "vbproj",
            "F184B08F-C81C-45F6-A57F-5ABD9991F28F",
        ),
        (
            "externalVD",
            "vdproj",
            "54435603-DBB4-11D2-8724-00A0C9A8B90C",
        ),
        (
            "externalVF",
            "vfproj",
            "6989167D-11E4-40FE-8C1A-2192A86A7E90",
        ),
        (
            "externalWAP",
            "wapproj",
            "C7167F0D-BC9F-4E6E-AFE1-012C56B48DB5",
        ),
        (
            "externalWiX",
            "wixproj",
            "930C7802-8A8C-48F9-8165-68863BCCD9DD",
        ),
    ] {
        let (found, path, guid) = sln_entry(&solution, name).expect(name);
        assert_eq!(found, kind, "{name}");
        assert_eq!(path, format!("external.{extension}"), "{name}");
        let active = format!("{{{guid}}}.Release|x64.ActiveCfg = Release|x64");
        assert!(solution.contains(&active), "{active}\n{solution}");
        // No project file is written for an external project.
        assert!(
            !exists(&host, &bin.join(format!("{name}.vcxproj"))),
            "{name}"
        );
    }
    let (kind, _, _) = sln_entry(&solution, "DotNetCore").expect("DotNetCore");
    assert_eq!(kind, "9A19103F-16F7-4668-BE54-9A1E7A4F7556");

    let (kind, _, guid) = sln_entry(&solution, "customGuid").expect("customGuid");
    assert_eq!((kind, guid), (project_type::DEFAULT, "aaa-bbb-ccc-000"));

    let (kind, _, guid) = sln_entry(&solution, "customTypePlatform").expect("type");
    assert_eq!(kind, "aaa-bbb-ccc-ddd-eee");
    assert!(solution.contains(&format!(
        "\t\t{{{guid}}}.Debug|x64.ActiveCfg = Debug|Custom Platform\r\n"
    )));

    let (kind, path, guid) = sln_entry(&solution, "customAll").expect("customAll");
    assert_eq!(
        (kind, path, guid),
        ("aaa-bbb-ccc-ddd-eee", r"sub\all.project", "aaa-bbb-ccc-111")
    );
    assert!(
        solution.contains("\t\t{aaa-bbb-ccc-111}.Debug|x64.ActiveCfg = Debug|Custom Platform\r\n")
    );
    assert!(
        solution.contains("\t\t{aaa-bbb-ccc-111}.Debug|x64.Build.0 = Debug|Custom Platform\r\n")
    );

    for line in [
        "\t\t{aaa-bbb-ccc-222}.Debug|x64.ActiveCfg = Debug|x64\r\n",
        "\t\t{aaa-bbb-ccc-222}.Release|x64.ActiveCfg = Custom - Release|x64\r\n",
        "\t\t{aaa-bbb-ccc-222}.Release|x64.Build.0 = Custom - Release|x64\r\n",
        "\t\t{aaa-bbb-ccc-222}.MinSizeRel|x64.ActiveCfg = Custom - MinSizeRel|x64\r\n",
        "\t\t{aaa-bbb-ccc-222}.RelWithDebInfo|x64.ActiveCfg = RelWithDebInfo|x64\r\n",
    ] {
        assert!(solution.contains(line), "{line}\n{solution}");
    }

    // The external projects depend on ZERO_CHECK, and ALL_BUILD references
    // them by their location.
    let (_, _, zero) = sln_entry(&solution, "ZERO_CHECK").expect("ZERO_CHECK");
    let section = solution
        .split("Project(")
        .find(|entry| entry.contains("\"customGuid\""))
        .expect("customGuid entry");
    assert!(section.contains(&format!("\t\t{{{zero}}} = {{{zero}}}\r\n")));
    let all_build = contents(&host, &bin, "ALL_BUILD.vcxproj");
    assert!(all_build.contains(r#"<ProjectReference Include="sub\all.project">"#));
    let csharp = all_build
        .split("<ProjectReference ")
        .find(|entry| entry.starts_with(r#"Include="external.csproj""#))
        .expect("C# reference");
    assert!(csharp.contains("<SkipGetTargetFrameworkProperties>true"));
    assert!(!csharp.contains("<ReferenceOutputAssembly>"));
    let other = all_build
        .split("<ProjectReference ")
        .find(|entry| entry.starts_with(r#"Include="guid.project""#))
        .expect("project reference");
    assert!(!other.contains("SkipGetTargetFrameworkProperties"));
    assert!(other.contains("<ReferenceOutputAssembly>false"));
}

#[test]
fn external_projects_write_types_platforms_and_build_types_in_slnx() {
    let (source, bin) = work_dirs();
    let host = PureHost::new(bin.clone(), bin.join("cmake"));
    let mut model = model_for(VsVersion::Vs18, &source, &bin);
    external(&mut model, "externalCS", "external.csproj", &[]);
    external(&mut model, "externalPy", "external.pyproj", &[]);
    external(&mut model, "externalNJS", "external.njsproj", &[]);
    external(
        &mut model,
        "DotNetCore",
        "DotNetCore.project",
        &[("VS_PROJECT_TYPE", "9A19103F-16F7-4668-BE54-9A1E7A4F7556")],
    );
    external(
        &mut model,
        "external",
        "dir/external.project",
        &[
            ("GUID", "AAA-bbb-ccc-111"),
            ("VS_PROJECT_TYPE", "AAA-bbb-ccc-ddd-eee"),
            ("VS_PLATFORM_MAPPING", "Custom Platform"),
            ("MAP_IMPORTED_CONFIG_RELEASE", "Custom - Release"),
        ],
    );
    generate(
        VsVersion::Vs18,
        &GenerationResult::default(),
        &model,
        &host,
        &bin,
    )
    .expect("generate solution");
    let solution = contents(&host, &bin, "Project.slnx");

    let expected = [
        r#"  <Project Path="ALL_BUILD.vcxproj" Type="8bc9ceb8-8b4a-11d0-8d11-00a0c91bc942" Id="#,
        r#"    <BuildDependency Project="DotNetCore.project"/>"#,
        r#"    <BuildDependency Project="ZERO_CHECK.vcxproj"/>"#,
        r#"    <BuildDependency Project="dir/external.project"/>"#,
        r#"    <BuildDependency Project="external.csproj"/>"#,
        r#"    <BuildDependency Project="external.njsproj"/>"#,
        r#"    <BuildDependency Project="external.pyproj"/>"#,
        r#"  <Project Path="DotNetCore.project" Type="9a19103f-16f7-4668-be54-9a1e7a4f7556" Id="#,
        r#"    <Platform Project="x64"/>"#,
        r#"  <Project Path="ZERO_CHECK.vcxproj" Type="8bc9ceb8-8b4a-11d0-8d11-00a0c91bc942" Id="#,
        r#"  <Project Path="dir/external.project" Type="aaa-bbb-ccc-ddd-eee" Id="aaa-bbb-ccc-111">"#,
        r#"    <BuildType Solution="Release|*" Project="Custom - Release"/>"#,
        r#"    <Platform Project="Custom Platform"/>"#,
        r#"  <Project Path="external.csproj" Type="fae04ec0-301f-11d3-bf4b-00c04f79efbc" Id="#,
        r#"    <Platform Project="x64"/>"#,
        r#"  <Project Path="external.njsproj" Type="9092aa53-fb77-4645-b42d-1ccca6bd08bd" Id="#,
        r#"  <Project Path="external.pyproj" Type="888888a0-9f3d-457c-b088-3a5042f75d52" Id="#,
    ];
    let mut rest = solution.as_str();
    for line in expected {
        let index = rest
            .find(line)
            .unwrap_or_else(|| panic!("{line}\n{solution}"));
        rest = rest.get(index + line.len()..).unwrap_or_default();
    }
    // The project with an explicit type and platform lists them in order.
    assert_eq!(
        slnx_project_lines(&solution, "dir/external.project"),
        [
            r#"<BuildDependency Project="ZERO_CHECK.vcxproj"/>"#,
            r#"<BuildType Solution="Release|*" Project="Custom - Release"/>"#,
            r#"<Platform Project="Custom Platform"/>"#,
            "</Project>",
        ]
    );
    // A plain external project only depends on ZERO_CHECK.
    assert_eq!(
        slnx_project_lines(&solution, "external.pyproj"),
        [
            r#"<BuildDependency Project="ZERO_CHECK.vcxproj"/>"#,
            "</Project>"
        ]
    );
    assert!(slnx_project_lines(&solution, "ZERO_CHECK.vcxproj").is_empty());
}

#[test]
fn cached_guids_are_used_exactly_as_given() {
    let mut model = ProjectModel::default();
    model
        .cache
        .insert("ext_GUID_CMAKE".into(), cache_entry("aaa-bbb-ccc-000"));
    assert_eq!(guid::for_name(&model, "/out", "ext"), "{aaa-bbb-ccc-000}");
    model
        .cache
        .insert("empty_GUID_CMAKE".into(), cache_entry(""));
    assert_eq!(
        guid::for_name(&model, "/out", "empty"),
        guid::for_name(&ProjectModel::default(), "/out", "empty")
    );
}

#[test]
fn custom_scripts_prepend_the_ide_run_path_inside_setlocal() {
    let commands = [
        rules::Command {
            argv: vec!["echo_message".into()],
            dir: "/work/build".into(),
        },
        rules::Command {
            argv: vec!["tool.bat".into(), "a b".into()],
            dir: String::new(),
        },
    ];
    assert_eq!(
        rules::script(&commands, Some("/work/src;/work/tools")),
        [
            "setlocal DisableDelayedExpansion",
            r"cd /D \work\build",
            "if errorlevel 1 exit /b %25errorlevel%25",
            "set \"PATH=/work/src%3B/work/tools%3B%25PATH%25\"",
            "echo_message",
            "if errorlevel 1 exit /b %25errorlevel%25",
            "call tool.bat \"a b\"",
            "if errorlevel 1 exit /b %25errorlevel%25",
            "endlocal",
        ]
        .join("\r\n")
    );
    assert!(!rules::script(&commands, None).contains("set \"PATH"));
}

#[test]
fn run_path_line_keeps_batch_characters_as_text() {
    assert_eq!(
        rules::set_path(r"C:\R&D\tools;%TEMP%\bin"),
        r#"set "PATH=C:\R&D\tools;%%TEMP%%\bin;%PATH%""#
    );
    assert_eq!(
        rules::set_path(r#"C:\a|b<c>d^e;C:\"quoted""#),
        r#"set "PATH=C:\a|b<c>d^e;C:\quoted;%PATH%""#
    );
    // MSBuild escapes `%` and `;` in the stored command. MSBuild removes the
    // escapes before it runs the script.
    let commands = [rules::Command {
        argv: vec!["tool".into()],
        dir: String::new(),
    }];
    assert!(
        rules::script(&commands, Some(r"C:\R&D\tools;%TEMP%"))
            .contains(r#"set "PATH=C:\R&D\tools%3B%25%25TEMP%25%25%3B%25PATH%25""#)
    );
}

#[test]
fn project_commands_use_the_directory_run_path() {
    let host = PureHost::default();
    let bin = Path::new(if cfg!(windows) { "C:/out" } else { "/out" });
    host.create_dir_all(bin).expect("build directory");
    let (mut model, result) = fixture(VsVersion::Vs17, "x64", bin, &host);
    // A configure-time cache entry, as from the environment, is the fallback.
    model.cache.insert(
        "CMAKE_MSVCIDE_RUN_PATH".into(),
        cache_entry("C:/cached/tools"),
    );
    model
        .dir_mut(DirId::ROOT)
        .vars
        .insert("CMAKE_MSVCIDE_RUN_PATH".into(), "C:/tools".into());
    generate(VsVersion::Vs17, &result, &model, &host, bin).expect("generate project");
    let project = contents(&host, bin, "app.vcxproj");
    let commands = project
        .split("<Command")
        .skip(1)
        .filter(|element| element.starts_with('>') || element.starts_with(" Condition="))
        .map(|element| element.split("</Command>").next().unwrap_or_default())
        .collect::<Vec<_>>();
    assert!(!commands.is_empty(), "{project}");
    for command in commands {
        let (_, script) = command.split_once('>').expect("command text");
        let lines = script.split("&#xD;&#xA;").collect::<Vec<_>>();
        assert_eq!(lines.first(), Some(&"setlocal DisableDelayedExpansion"));
        let set = lines
            .iter()
            .position(|line| *line == "set \"PATH=C:/tools%3B%25PATH%25\"")
            .unwrap_or_else(|| panic!("{script}"));
        // Only a change of directory can come before the new `PATH`.
        assert!(
            lines.get(1..set).is_some_and(|head| head
                .iter()
                .all(|line| line.starts_with("cd /D ") || line.starts_with("if errorlevel"))),
            "{script}"
        );
        assert!(!script.contains("cached"));
    }
}

fn ide_source(path: &str, language: Option<&str>) -> IdeSource {
    IdeSource {
        skip_precompile_headers: false,
        path: path.into(),
        kind: language.map_or(IdeSourceKind::Other, |language| IdeSourceKind::Compile {
            language: language.into(),
        }),
        group: BString::default(),
        generated: false,
        excluded_configs: Vec::new(),
        file_set: None,
        package_location: None,
        compile: None,
    }
}

fn step(phase: IdeCustomPhase, argv: &[&str], outputs: &[&str], depends: &[&str]) -> IdeCustomStep {
    IdeCustomStep {
        phase,
        commands: vec![argv.iter().map(|arg| (*arg).into()).collect()],
        working_dir: BString::default(),
        outputs: outputs.iter().map(|path| (*path).into()).collect(),
        byproducts: Vec::new(),
        depends: depends.iter().map(|path| (*path).into()).collect(),
        comment: None,
        always_run: false,
    }
}

/// Facts of a target for `Debug` and `Release`.
fn target_facts(
    result: &mut GenerationResult,
    id: TargetId,
    name: &str,
    kind: TargetKind,
    bin: &Path,
    ide: impl Fn(&str) -> IdeFacts,
) {
    if result.configs.is_empty() {
        result.configs.resize_with(2, Default::default);
        for (config, name) in result.configs.iter_mut().zip(["Debug", "Release"]) {
            config.0 = name.into();
        }
    }
    for config in result.configs.clone() {
        let facts = ide(&string(&config.0));
        result.targets.push(ResolvedTarget {
            fortran: None,
            id,
            config,
            metadata: TargetMetadata {
                name: name.into(),
                kind,
                source_dir: Default::default(),
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
            dependencies: Vec::new(),
            cxx_modules: None,
            ide: Some(Box::new(facts)),
        });
    }
}

fn csharp_compile(config: &str) -> IdeCompileSettings {
    let debug = config == "Debug";
    IdeCompileSettings {
        language: "CSharp".into(),
        flags: vec![
            bt("/platform:x64"),
            bt("/define:TRACE"),
            bt(if debug { "/debug:full" } else { "/debug:none" }),
            bt(if debug { "/optimize-" } else { "/optimize" }),
        ],
        defines: vec![
            bt("MY_FOO_DEFINE"),
            bt("MY_BAR_ASSIGNMENT=bar"),
            bt(if debug {
                "DEFINE_ONLY_FOR_DEBUG"
            } else {
                "DEFINE_ONLY_FOR_RELEASE"
            }),
        ],
        includes: Vec::new(),
    }
}

/// The text between an opening tag that starts with `start` and the next
/// closing tag `end`.
fn element<'a>(text: &'a str, start: &str, end: &str) -> &'a str {
    let index = text
        .find(start)
        .unwrap_or_else(|| panic!("{start}\n{text}"));
    let rest = text.get(index..).unwrap_or_default();
    rest.get(..rest.find(end).map_or(rest.len(), |at| at + end.len()))
        .unwrap_or_default()
}

#[test]
fn sdk_style_csharp_projects_follow_the_dotnet_sdk() {
    let (source, bin) = work_dirs();
    let host = PureHost::new(bin.clone(), bin.join("cmake"));
    let mut model = model_for(VsVersion::Vs17, &source, &bin);
    let src = from_native(&source);
    let out = from_native(&bin);
    let src = string(&src);
    let out = string(&out);
    let mut app = target("foo", TargetKind::Executable, "");
    for (key, value) in [
        ("DOTNET_SDK", "Microsoft.NET.Sdk"),
        ("VS_DOTNET_STARTUP_OBJECT", "CSharpOnly.CSharpOnly"),
        ("VS_GLOBAL_RuntimeIdentifier", "win10-x64"),
        ("VS_DOTNET_REFERENCE_SomeDll", "SomeDll.dll"),
        ("VS_DOTNET_REFERENCES", "System;System.Xml"),
        ("VS_PACKAGE_REFERENCES", "Newtonsoft.Json_13.0.3"),
    ] {
        app.props.set(key.into(), value.into());
    }
    let app = model.add_target(app).expect("C# executable");
    let mut xaml = rcmake_model::PropertyMap::default();
    xaml.set("VS_XAML_TYPE".into(), "ApplicationDefinition".into());
    let mut designer = rcmake_model::PropertyMap::default();
    designer.set("VS_CSHARP_DependentUpon".into(), "App.xaml".into());
    designer.set("VS_CSHARP_SubType".into(), "Code".into());
    let root = model.dir_mut(DirId::ROOT);
    root.source_props
        .insert(format!("{src}/App.xaml").into(), xaml);
    root.source_props
        .insert(format!("{src}/App.xaml.cs").into(), designer);
    let mut result = GenerationResult::default();
    let generated = format!("{out}/bar.cs");
    let lib1 = format!("{src}/lib1.cs");
    target_facts(
        &mut result,
        app,
        "foo",
        TargetKind::Executable,
        &bin,
        |config| {
            let mut only = ide_source(&format!("{src}/release_only.cs"), Some("CSharp"));
            only.excluded_configs = vec!["Debug".into()];
            IdeFacts {
                naming: OutputNaming {
                    dir: format!("{out}/{config}").into(),
                    base_name: "longer name".into(),
                    suffix: ".exe".into(),
                    ..Default::default()
                },
                sources: vec![
                    ide_source(&format!("{src}/csharponly.cs"), Some("CSharp")),
                    ide_source(&format!("{src}/App.xaml"), None),
                    ide_source(&format!("{src}/App.xaml.cs"), Some("CSharp")),
                    ide_source(&format!("{src}/Resources.resx"), None),
                    ide_source(&format!("{src}/notes.txt"), None),
                    ide_source(&generated, Some("CSharp")),
                    only,
                ],
                compile: vec![csharp_compile(config)],
                custom: vec![
                    step(
                        IdeCustomPhase::PreBuild,
                        &["echo", "This should happen!"],
                        &[],
                        &[],
                    ),
                    step(
                        IdeCustomPhase::Rule,
                        &["cmake", "-E", "copy", &lib1, "bar.cs"],
                        &[&generated],
                        &[&lib1],
                    ),
                ],
                ..Default::default()
            }
        },
    );
    generate(VsVersion::Vs17, &result, &model, &host, &bin).expect("generate");
    assert!(!exists(&host, &bin.join("foo.vcxproj")));
    assert!(!exists(&host, &bin.join("foo.csproj.filters")));
    let project = contents(&host, &bin, "foo.csproj");
    assert!(
        project.starts_with("\u{feff}<Project Sdk=\"Microsoft.NET.Sdk\">\r\n"),
        "{project}"
    );
    let windows_src = windows(&src);
    let windows_out = windows(&out);
    let globals = element(
        &project,
        "<PropertyGroup Label=\"Globals\">",
        "</PropertyGroup>",
    );
    for line in [
        "<Keyword>Win32Proj</Keyword>",
        "<RuntimeIdentifier>win10-x64</RuntimeIdentifier>",
        "<Configurations>Debug;Release</Configurations>",
        "<EnableDefaultItems>false</EnableDefaultItems>",
        "<ManagedAssembly>true</ManagedAssembly>",
        "<TargetFramework>net5.0</TargetFramework>",
        "<AppendTargetFrameworkToOutputPath>false</AppendTargetFrameworkToOutputPath>",
        "<OutputType>Exe</OutputType>",
        "<StartupObject>CSharpOnly.CSharpOnly</StartupObject>",
    ] {
        assert!(globals.contains(line), "{line}\n{globals}");
    }
    let debug = element(
        &project,
        "<PropertyGroup Condition=\"'$(Configuration)' == 'Debug'\">",
        "</PropertyGroup>",
    );
    for line in [
        format!("<OutputPath>{windows_out}\\Debug\\</OutputPath>"),
        "<AssemblyName>longer name</AssemblyName>".into(),
        "<DebugType>full</DebugType>".into(),
        "<DefineConstants>TRACE;MY_FOO_DEFINE;DEFINE_ONLY_FOR_DEBUG</DefineConstants>".into(),
        "<Optimize>false</Optimize>".into(),
        "<Platform>x64</Platform>".into(),
    ] {
        assert!(debug.contains(&line), "{line}\n{debug}");
    }
    assert!(!debug.contains("MY_BAR_ASSIGNMENT"));
    let release = element(
        &project,
        "<PropertyGroup Condition=\"'$(Configuration)' == 'Release'\">",
        "</PropertyGroup>",
    );
    assert!(release.contains("<DefineConstants>TRACE;MY_FOO_DEFINE;DEFINE_ONLY_FOR_RELEASE<"));
    let event = element(
        &project,
        "<Target Condition=\"'$(Configuration)' == 'Debug'\" Name=\"PreBuildDebug\" BeforeTargets=\"PreBuildEvent\">",
        "</Target>",
    );
    assert!(
        event.contains("<Exec Command=\"setlocal DisableDelayedExpansion&#xD;&#xA;echo &quot;This should happen!&quot;"),
        "{event}"
    );

    // The generated source belongs to its custom command.
    let custom = element(&project, "<None Include=", "</ItemGroup>");
    assert!(
        custom.contains(&format!("{windows_out}\\bar.cs")),
        "{custom}"
    );
    assert!(custom.contains("<Link>bar.cs</Link>"), "{custom}");
    assert!(project.contains(&format!(
        "<UpToDateCheckInput Include=\"{windows_src}\\lib1.cs\" />"
    )));
    let rule = element(
        &project,
        "<Target Condition=\"'$(Configuration)' == 'Debug'\" Name=\"CustomCommand_Debug_",
        "</Target>",
    );
    for text in [
        format!("Inputs=\"{windows_out}\\bar.cs;{windows_src}\\lib1.cs\""),
        format!("Outputs=\"{windows_out}\\bar.cs\""),
        "BeforeTargets=\"CoreCompile\" DependsOnTargets=\"PrepareForBuild\">".into(),
        "<Exec Command=\"echo Generating bar.cs\" />".into(),
    ] {
        assert!(rule.contains(&text), "{text}\n{rule}");
    }
    assert!(!project.contains(&format!("<Compile Include=\"{windows_out}\\bar.cs\"")));

    for text in [
        format!("<Compile Include=\"{windows_src}\\csharponly.cs\">\r\n      <Link>csharponly.cs</Link>"),
        format!("<Compile Include=\"{windows_src}\\App.xaml.cs\">\r\n      <Link>App.xaml.cs</Link>\r\n      <DependentUpon>App.xaml</DependentUpon>\r\n      <SubType>Code</SubType>"),
        format!("<Compile Condition=\"('$(Configuration)|$(Platform)'=='Release|x64')\" Include=\"{windows_src}\\release_only.cs\">"),
        format!("<None Include=\"{windows_src}\\notes.txt\">\r\n      <Link>notes.txt</Link>"),
        format!("<EmbeddedResource Include=\"{windows_src}\\Resources.resx\">\r\n      <Link>Resources.resx</Link>"),
        format!("<ApplicationDefinition Include=\"{windows_src}\\App.xaml\">\r\n      <Link>App.xaml</Link>\r\n      <SubType>Designer</SubType>"),
        "<Reference Include=\"System\">\r\n      <CopyLocalSatelliteAssemblies>true</CopyLocalSatelliteAssemblies>\r\n      <ReferenceOutputAssembly>true</ReferenceOutputAssembly>\r\n    </Reference>".into(),
        format!("<Reference Include=\"SomeDll\">\r\n      <CopyLocalSatelliteAssemblies>true</CopyLocalSatelliteAssemblies>\r\n      <ReferenceOutputAssembly>true</ReferenceOutputAssembly>\r\n      <Private>True</Private>\r\n      <HintPath>{windows_src}\\SomeDll.dll</HintPath>"),
        "<PackageReference Include=\"Newtonsoft.Json\" Version=\"13.0.3\" />".into(),
        format!("<ProjectReference Include=\"{windows_out}\\ZERO_CHECK.vcxproj\">"),
    ] {
        assert!(project.contains(&text), "{text}\n{project}");
    }
    let zero = element(&project, "<ProjectReference ", "</ProjectReference>");
    assert!(zero.contains("<ReferenceOutputAssembly>false</ReferenceOutputAssembly>"));
    // SDK-style projects have no classic imports.
    assert!(!project.contains("Microsoft.CSharp.targets"));

    let solution = contents(&host, &bin, "Project.sln");
    let (kind, path, guid) = sln_entry(&solution, "foo").expect("C# project");
    assert_eq!((kind, path), (project_type::CSHARP, "foo.csproj"));
    assert!(solution.contains(&format!(
        "\t\t{{{guid}}}.Debug|x64.ActiveCfg = Debug|Any CPU\r\n"
    )));
    assert!(solution.contains(&format!(
        "\t\t{{{guid}}}.Debug|x64.Build.0 = Debug|Any CPU\r\n"
    )));
}

#[test]
fn classic_csharp_projects_use_msbuild_targets_and_the_solution_platform() {
    let (source, bin) = work_dirs();
    let host = PureHost::new(bin.clone(), bin.join("cmake"));
    for version in [VsVersion::Vs17, VsVersion::Vs18] {
        let mut model = model_for(version, &source, &bin);
        let src = string(&from_native(&source));
        // An explicitly empty `DOTNET_SDK` selects a classic project.
        let mut lib = target("baz", TargetKind::SharedLibrary, "");
        lib.props.set("DOTNET_SDK".into(), "".into());
        lib.props
            .set("VS_GLOBAL_ROOTNAMESPACE".into(), "Baz.Space".into());
        let lib = model.add_target(lib).expect("C# library");
        let mut sdk = target("platform", TargetKind::Executable, "");
        sdk.props
            .set("DOTNET_SDK".into(), "Microsoft.NET.Sdk".into());
        sdk.props
            .set("VS_GLOBAL_PlatformTarget".into(), "x64".into());
        let sdk = model.add_target(sdk).expect("C# executable");
        let mut result = GenerationResult::default();
        let ide = |config: &str| IdeFacts {
            sources: vec![ide_source(&format!("{src}/lib1.cs"), Some("CSharp"))],
            compile: vec![csharp_compile(config)],
            custom: vec![step(IdeCustomPhase::PostBuild, &["echo", "done"], &[], &[])],
            ..Default::default()
        };
        target_facts(
            &mut result,
            lib,
            "baz",
            TargetKind::SharedLibrary,
            &bin,
            ide,
        );
        target_facts(
            &mut result,
            sdk,
            "platform",
            TargetKind::Executable,
            &bin,
            ide,
        );
        generate(version, &result, &model, &host, &bin).expect("generate");
        let project = contents(&host, &bin, "baz.csproj");
        for text in [
            format!(
                "\u{feff}<?xml version=\"1.0\" encoding=\"utf-8\"?>\r\n<Project DefaultTargets=\"Build\" ToolsVersion=\"{}\"",
                version.tools_version()
            ),
            "<RootNamespace>Baz.Space</RootNamespace>".into(),
            "<Platform>x64</Platform>".into(),
            "<ProjectName>baz</ProjectName>".into(),
            "<TargetFrameworkVersion>v4.7.2</TargetFrameworkVersion>".into(),
            "<OutputType>Library</OutputType>".into(),
            "<AppDesignerFolder>Properties</AppDesignerFolder>".into(),
            r#"<Import Project="$(MSBuildExtensionsPath)\$(MSBuildToolsVersion)\Microsoft.Common.props" Condition="Exists('$(MSBuildExtensionsPath)\$(MSBuildToolsVersion)\Microsoft.Common.props')" />"#.into(),
            r#"<Import Project="$(UserRootDir)\Microsoft.CSharp.$(Platform).user.props" Condition="exists('$(UserRootDir)\Microsoft.CSharp.$(Platform).user.props')" Label="LocalAppDataPlatform" />"#.into(),
            r#"<Import Project="$(MSBuildToolsPath)\Microsoft.CSharp.targets" />"#.into(),
            "<BuildDependsOn>&#xA;      $(BuildDependsOn)&#xA;</BuildDependsOn>".into(),
        ] {
            assert!(project.contains(&text), "{text}\n{project}");
        }
        // `VS_GLOBAL_ROOTNAMESPACE` is written once, as `RootNamespace`.
        assert!(!project.contains("<ROOTNAMESPACE>"));
        let debug = element(
            &project,
            "<PropertyGroup Condition=\"'$(Configuration)|$(Platform)'=='Debug|x64'\" Label=\"Configuration\">",
            "</PropertyGroup>",
        );
        let windows_out = windows(&string(&from_native(&bin)));
        let lines = debug
            .lines()
            .skip(1)
            .take(4)
            .map(|line| line.trim().to_owned())
            .collect::<Vec<_>>();
        assert_eq!(
            lines,
            [
                "<DebugSymbols>true</DebugSymbols>".to_owned(),
                "<DefineDebug>true</DefineDebug>".to_owned(),
                format!("<OutputPath>{windows_out}\\Debug\\</OutputPath>"),
                "<PlatformTarget>x64</PlatformTarget>".to_owned(),
            ],
            "{debug}"
        );
        assert!(debug.contains("<AssemblyName>baz</AssemblyName>"));
        assert!(debug.contains("<DefineConstants>TRACE;MY_FOO_DEFINE;DEFINE_ONLY_FOR_DEBUG<"));
        assert!(!debug.contains("<Platform>"));
        let release = element(
            &project,
            "<PropertyGroup Condition=\"'$(Configuration)|$(Platform)'=='Release|x64'\" Label=\"Configuration\">",
            "</PropertyGroup>",
        );
        assert!(!release.contains("DebugSymbols"));
        let event = element(
            &project,
            "<PropertyGroup Condition=\"'$(Configuration)' == 'Release'\">",
            "</PropertyGroup>",
        );
        assert!(
            event.contains("<PostBuildEvent>setlocal DisableDelayedExpansion"),
            "{event}"
        );

        if version.writes_slnx() {
            let solution = contents(&host, &bin, "Project.slnx");
            for entry in ["baz.csproj", "platform.csproj"] {
                let project = solution
                    .split("<Project ")
                    .find(|project| project.starts_with(&format!("Path=\"{entry}\"")))
                    .expect(entry);
                assert!(project.contains("Type=\"fae04ec0-301f-11d3-bf4b-00c04f79efbc\""));
                assert!(project.contains("<Platform Project=\"x64\"/>"), "{project}");
            }
        } else {
            let solution = contents(&host, &bin, "Project.sln");
            assert!(!solution.contains("Any CPU"), "{solution}");
            let (kind, _, guid) = sln_entry(&solution, "baz").expect("baz");
            assert_eq!(kind, project_type::CSHARP);
            assert!(solution.contains(&format!("{{{guid}}}.Debug|x64.ActiveCfg = Debug|x64")));
        }
    }
}

#[test]
fn msbuild_builds_csharp_and_external_projects_by_solution_entry() {
    let host = PureHost::default();
    let dir = to_native(b"/out");
    host.create_dir_all(&dir).expect("build directory");
    write_if_changed(
        &host,
        &dir.join("Demo.sln"),
        concat!(
            "Project(\"{2150E333-8FDC-42A3-9474-1A3956D46DE8}\") = \"foo\", \"foo\", \"{0}\"\r\nEndProject\r\n",
            "Project(\"{FAE04EC0-301F-11D3-BF4B-00C04F79EFBC}\") = \"foo\", \"sub\\foo.csproj\", \"{1}\"\r\nEndProject\r\n",
            "Project(\"{aaa-bbb}\") = \"ext\", \"..\\ext.project\", \"{2}\"\r\nEndProject\r\n",
        )
        .as_bytes(),
    )
    .expect("solution");
    write_if_changed(
        &host,
        &dir.join("Demo.slnx"),
        b"<Solution>\n  <Folder Name=\"/foo/\">\n  <Project Path=\"sub/foo.csproj\" Type=\"x\"/>\n  <Project Path=\"../ext.project\"/>\n</Solution>\n",
    )
    .expect("solution");
    let targets = ["foo".into(), "ext".into(), "missing".into()];
    for version in [VsVersion::Vs17, VsVersion::Vs18] {
        let commands = build_commands(
            &BuildRequest {
                generator: GeneratorKind::VisualStudio(version),
                build_dir: b"/out",
                project_name: b"Demo",
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
            [r"sub\foo.csproj", r"..\ext.project", "missing.vcxproj"],
            "{version:?}"
        );
    }
}

/// The `UseDebugLibraries` value in the `Configuration` property group of one
/// configuration, as the RunCMake `check-common.cmake` reads it.
fn use_debug_libraries(project: &str, config: &str) -> Option<String> {
    let group = element(
        project,
        &format!(
            "<PropertyGroup Condition=\"'$(Configuration)|$(Platform)'=='{config}|x64'\" Label=\"Configuration\">"
        ),
        "</PropertyGroup>",
    );
    let (_, rest) = group.split_once("<UseDebugLibraries>")?;
    let (value, _) = rest.split_once("</UseDebugLibraries>")?;
    Some(value.to_owned())
}

/// How one `VS10ProjectUseDebugLibraries` target is set up.
struct DebugCase {
    name: String,
    kind: TargetKind,
    /// Runtime library flags for `Debug` and `Release`.
    runtime: [&'static str; 2],
    /// `VS_USE_DEBUG_LIBRARIES` and its values for `Debug` and `Release`.
    explicit: Option<(&'static str, [&'static str; 2])>,
    /// Expected values for `Debug` and `Release`.
    expected: [Option<&'static str>; 2],
}

/// Generate the targets of one case and check `UseDebugLibraries`.
///
/// `policy` is the `CMP0162` setting, `top_level` is the last top-level
/// `CMAKE_VS_USE_DEBUG_LIBRARIES` value, and `builtin` is the expected value
/// of `ALL_BUILD` and `ZERO_CHECK` for `Debug` and `Release`.
fn check_debug_libraries(
    policy: Option<PolicyStatus>,
    top_level: Option<&str>,
    cases: &[DebugCase],
    builtin: [Option<&str>; 2],
) {
    let (source, bin) = work_dirs();
    let host = PureHost::new(bin.clone(), bin.join("cmake"));
    let mut model = model_for(VsVersion::Vs17, &source, &bin);
    let src = string(&from_native(&source));
    let mut policies = rcmake_diag::PolicyMap::default();
    if let Some(policy) = policy {
        policies
            .set(PolicyId::new(162), policy)
            .expect("policy CMP0162");
    }
    let root = model.dir_mut(DirId::ROOT);
    root.final_policies = Some(policies);
    if let Some(value) = top_level {
        root.vars
            .insert("CMAKE_VS_USE_DEBUG_LIBRARIES".into(), value.into());
    }
    let mut result = GenerationResult::default();
    for case in cases {
        let mut item = target(&case.name, case.kind, "");
        item.policies.policies = policies;
        if let Some((raw, _)) = case.explicit {
            item.props.set("VS_USE_DEBUG_LIBRARIES".into(), raw.into());
        }
        let id = model.add_target(item).expect("target");
        target_facts(&mut result, id, &case.name, case.kind, &bin, |config| {
            let index = usize::from(config != "Debug");
            let mut facts = IdeFacts::default();
            if case.kind != TargetKind::Utility {
                facts.sources = vec![ide_source(&format!("{src}/empty.cxx"), Some("CXX"))];
                facts.compile = vec![IdeCompileSettings {
                    language: "CXX".into(),
                    flags: case
                        .runtime
                        .get(index)
                        .map(|flag| bt(flag))
                        .into_iter()
                        .collect(),
                    ..Default::default()
                }];
            }
            if let Some((_, values)) = case.explicit
                && let Some(value) = values.get(index)
            {
                facts
                    .attributes
                    .insert("VS_USE_DEBUG_LIBRARIES".into(), (*value).into());
            }
            facts
        });
    }
    generate(VsVersion::Vs17, &result, &model, &host, &bin).expect("generate");
    let check = |name: &str, expected: [Option<&str>; 2]| {
        let project = contents(&host, &bin, &format!("{name}.vcxproj"));
        for (config, expected) in ["Debug", "Release"].into_iter().zip(expected) {
            assert_eq!(
                use_debug_libraries(&project, config).as_deref(),
                expected,
                "{name} {config}"
            );
        }
    };
    for case in cases {
        check(&case.name, case.expected);
    }
    for name in ["ALL_BUILD", "ZERO_CHECK"] {
        check(name, builtin);
    }
}

/// The `Default-common.cmake` targets. `rtl` is the runtime library of
/// `defaultRTL` for `Debug` and `Release`, and `rtl_expected` its result.
fn default_cases(
    rtl: [&'static str; 2],
    rtl_expected: [Option<&'static str>; 2],
) -> [DebugCase; 4] {
    let debug = [Some("true"), Some("false")];
    [
        DebugCase {
            name: "default".into(),
            kind: TargetKind::StaticLibrary,
            runtime: ["/MDd", "/MD"],
            explicit: None,
            expected: debug,
        },
        DebugCase {
            name: "defaultCLR".into(),
            kind: TargetKind::StaticLibrary,
            runtime: ["/MDd", "/MD"],
            explicit: None,
            expected: debug,
        },
        DebugCase {
            name: "defaultRTL".into(),
            kind: TargetKind::StaticLibrary,
            runtime: rtl,
            explicit: None,
            expected: rtl_expected,
        },
        DebugCase {
            name: "defaultUtil".into(),
            kind: TargetKind::Utility,
            runtime: ["", ""],
            explicit: None,
            expected: debug,
        },
    ]
}

/// The `Explicit-common.cmake` targets.
fn explicit_cases() -> Vec<DebugCase> {
    let mut cases = Vec::new();
    for (prefix, raw, values, expected) in [
        ("empty", "", ["", ""], [None, None]),
        (
            "genex",
            "$<CONFIG:Debug>",
            ["1", "0"],
            [Some("true"), Some("false")],
        ),
    ] {
        for (suffix, kind) in [
            ("", TargetKind::StaticLibrary),
            ("CLR", TargetKind::StaticLibrary),
            ("Util", TargetKind::Utility),
        ] {
            cases.push(DebugCase {
                name: format!("{prefix}{suffix}"),
                kind,
                runtime: ["/MDd", "/MD"],
                explicit: Some((raw, values)),
                expected,
            });
        }
    }
    cases
}

#[test]
fn use_debug_libraries_default_cmp0162_new() {
    // With CMP0091 NEW, `MSVC_RUNTIME_LIBRARY` selects `/MD` for `defaultRTL`.
    check_debug_libraries(
        Some(PolicyStatus::New),
        None,
        &default_cases(["/MD", "/MD"], [Some("false"), Some("false")]),
        [Some("true"), Some("false")],
    );
}

#[test]
fn use_debug_libraries_cmp0091_old() {
    // With CMP0091 OLD, the configuration flags select the runtime library.
    check_debug_libraries(
        Some(PolicyStatus::New),
        None,
        &default_cases(["/MDd", "/MD"], [Some("true"), Some("false")]),
        [Some("true"), Some("false")],
    );
}

#[test]
fn use_debug_libraries_default_cmp0162_warn() {
    let none = [None, None];
    let mut cases = default_cases(["/MD", "/MD"], none);
    for case in &mut cases {
        case.expected = none;
    }
    check_debug_libraries(None, None, &cases, none);
}

#[test]
fn use_debug_libraries_explicit_cmp0162_new() {
    check_debug_libraries(
        Some(PolicyStatus::New),
        Some("0"),
        &explicit_cases(),
        [Some("false"), Some("false")],
    );
}

#[test]
fn use_debug_libraries_explicit_cmp0162_old() {
    check_debug_libraries(
        Some(PolicyStatus::Old),
        Some("0"),
        &explicit_cases(),
        [Some("false"), Some("false")],
    );
}

#[test]
fn invalid_xml_names_fall_back_to_the_default_elements() {
    assert!(xml::is_name("ApplicationDefinition"));
    assert!(xml::is_name("_Page.2-x"));
    for name in ["", "My Tool", "a:b", "1Page", "<x>", "Pa&ge"] {
        assert!(!xml::is_name(name), "{name}");
    }
    let (source, bin) = work_dirs();
    let host = PureHost::new(bin.clone(), bin.join("cmake"));
    let mut model = model_for(VsVersion::Vs17, &source, &bin);
    let src = string(&from_native(&source));
    let app = model
        .add_target(target("app", TargetKind::Executable, ""))
        .expect("C# executable");
    let mut bad_xaml = rcmake_model::PropertyMap::default();
    bad_xaml.set("VS_XAML_TYPE".into(), "Bad Type".into());
    let mut bad_tool = rcmake_model::PropertyMap::default();
    bad_tool.set("VS_TOOL_OVERRIDE".into(), "<Tool>".into());
    let mut good_tool = rcmake_model::PropertyMap::default();
    good_tool.set("VS_TOOL_OVERRIDE".into(), "Content".into());
    let root = model.dir_mut(DirId::ROOT);
    root.source_props
        .insert(format!("{src}/Main.xaml").into(), bad_xaml);
    root.source_props
        .insert(format!("{src}/bad.txt").into(), bad_tool);
    root.source_props
        .insert(format!("{src}/good.txt").into(), good_tool);
    let mut result = GenerationResult::default();
    target_facts(
        &mut result,
        app,
        "app",
        TargetKind::Executable,
        &bin,
        |_| IdeFacts {
            sources: vec![
                ide_source(&format!("{src}/main.cs"), Some("CSharp")),
                ide_source(&format!("{src}/Main.xaml"), None),
                ide_source(&format!("{src}/bad.txt"), None),
                ide_source(&format!("{src}/good.txt"), None),
            ],
            ..Default::default()
        },
    );
    generate(VsVersion::Vs17, &result, &model, &host, &bin).expect("generate");
    let project = contents(&host, &bin, "app.csproj");
    let windows_src = windows(&src);
    for text in [
        format!("<Page Include=\"{windows_src}\\Main.xaml\">"),
        format!("<None Include=\"{windows_src}\\bad.txt\">"),
        format!("<Content Include=\"{windows_src}\\good.txt\">"),
    ] {
        assert!(project.contains(&text), "{text}\n{project}");
    }
    assert!(!project.contains("Bad Type"));
    assert!(!project.contains("&lt;Tool"));
}
