//! Render the project plan as MSBuild XML.

use rcmake_model::ParsedToolset;

use crate::{
    MSBUILD_NS,
    cuda::{self, ImportKind},
    plan::Flavor,
    xml::{Writer, is_name},
};

use super::*;

pub(crate) mod nuget;

const EVENTS: [&str; 3] = ["PreBuildEvent", "PreLinkEvent", "PostBuildEvent"];

/// The `CMAKE_VS_SDK_*_DIRECTORIES` variables and their MSBuild properties.
const SDK_DIRECTORIES: [(&[u8], &str); 7] = [
    (b"CMAKE_VS_SDK_EXECUTABLE_DIRECTORIES", "ExecutablePath"),
    (b"CMAKE_VS_SDK_INCLUDE_DIRECTORIES", "IncludePath"),
    (b"CMAKE_VS_SDK_REFERENCE_DIRECTORIES", "ReferencePath"),
    (b"CMAKE_VS_SDK_LIBRARY_DIRECTORIES", "LibraryPath"),
    (b"CMAKE_VS_SDK_LIBRARY_WINRT_DIRECTORIES", "LibraryWPath"),
    (b"CMAKE_VS_SDK_SOURCE_DIRECTORIES", "SourcePath"),
    (b"CMAKE_VS_SDK_EXCLUDE_DIRECTORIES", "ExcludePath"),
];

pub(crate) fn render(ctx: &Context<'_>, plan: &Plan) -> String {
    let settings = ctx.settings;
    let project = ctx.project;
    let run_path = ctx.run_path();
    let mut xml = Writer::new(true, "utf-8");
    xml.start(
        "Project",
        &[
            ("DefaultTargets", "Build"),
            ("ToolsVersion", settings.version.tools_version()),
            ("xmlns", MSBUILD_NS),
        ],
    );
    if settings.tool_host.is_some() || settings.tool_version.is_some() {
        xml.start("PropertyGroup", &[]);
        if let Some(host) = &settings.tool_host {
            xml.text("PreferredToolArchitecture", &[], host);
        }
        if let Some(version) = &settings.tool_version {
            xml.text("VCToolsVersion", &[], version);
        }
        xml.end();
    }
    // These projects import the common targets, which resolve NuGet package
    // assets when the solution has .NET SDK-style projects. They have no
    // packages, so CMake turns the resolution off.
    if matches!(
        project.utility,
        Some(UtilityKind::AllBuild | UtilityKind::Package | UtilityKind::ZeroCheck)
    ) {
        xml.start("PropertyGroup", &[]);
        xml.text("ResolveNugetPackages", &[], "false");
        xml.end();
    }
    xml.start("ItemGroup", &[("Label", "ProjectConfigurations")]);
    for config in &settings.configs {
        let key = format!("{config}|{}", settings.platform);
        xml.start("ProjectConfiguration", &[("Include", &key)]);
        xml.text("Configuration", &[], config);
        xml.text("Platform", &[], &settings.platform);
        xml.end();
    }
    xml.end();
    xml.start("PropertyGroup", &[("Label", "Globals")]);
    xml.text("ProjectGuid", &[], &project.guid);
    xml.text("Keyword", &[], "Win32Proj");
    if let Some(sdk) = &settings.sdk {
        xml.text("WindowsTargetPlatformVersion", &[], sdk);
    }
    xml.text("Platform", &[], &settings.platform);
    xml.text("ProjectName", &[], &project.name);
    write_target_framework(&mut xml, ctx);
    xml.text("VCProjectUpgraderObjectName", &[], "NoUpgrade");
    if let ParsedToolset::Vs(toolset) = &ctx.model.generator_spec.parsed
        && let Some(path) = &toolset.vctargets
    {
        xml.text("VCTargetsPath", &[], &string(path));
    }
    if plan.configs.values().any(|config| config.cl.managed) {
        write_managed_globals(&mut xml, ctx);
    }
    if let Some(root) = &settings.cuda_toolkit
        && plan.configs.values().any(|config| config.cuda.is_some())
    {
        cuda::cuda_toolkit_custom_dir(&mut xml, root);
    }
    if let Some(target) = project.target.and_then(|id| ctx.model.target(id)) {
        for key in target.props.names() {
            let name = string(&key);
            if let Some(property) = name.strip_prefix("VS_GLOBAL_").filter(|key| is_name(key))
                && let Some(value) = target.props.get(&key)
            {
                xml.text(property, &[], &string(value));
            }
        }
    }
    // CMake sets `VS_GLOBAL_DisableFastUpToDateCheck` on `ZERO_CHECK` when it
    // verifies `CONFIGURE_DEPENDS` globs, so MSBuild always runs its
    // pre-build event.
    if project.utility == Some(UtilityKind::ZeroCheck) && !ctx.model.glob_checks.is_empty() {
        xml.text("DisableFastUpToDateCheck", &[], "true");
    }
    xml.end();
    xml.empty(
        "Import",
        &[("Project", r"$(VCTargetsPath)\Microsoft.Cpp.Default.props")],
    );
    let configuration_type = match project.kind {
        TargetKind::Executable => "Application",
        TargetKind::StaticLibrary => "StaticLibrary",
        TargetKind::SharedLibrary | TargetKind::ModuleLibrary => "DynamicLibrary",
        _ => "Utility",
    };
    for name in &settings.configs {
        xml.start(
            "PropertyGroup",
            &[
                ("Condition", &settings.condition(name)),
                ("Label", "Configuration"),
            ],
        );
        let config = plan.configs.get(name);
        let kind = ctx
            .property(name, b"VS_CONFIGURATION_TYPE")
            .map(|value| string(value));
        xml.text(
            "ConfigurationType",
            &[],
            kind.as_deref().unwrap_or(configuration_type),
        );
        xml.text(
            "CharacterSet",
            &[],
            config
                .and_then(|config| config.cl.char_set)
                .unwrap_or("MultiByte"),
        );
        let toolset = ctx
            .property(name, b"VS_PLATFORM_TOOLSET")
            .map(|value| string(value));
        xml.text(
            "PlatformToolset",
            &[],
            toolset.as_deref().unwrap_or(&settings.toolset),
        );
        if let Some(debug) = ctx.debug_libraries(name, runtime_is_debug(project.kind, plan, name)) {
            xml.text(
                "UseDebugLibraries",
                &[],
                if debug { "true" } else { "false" },
            );
        }
        if let Some(spectre) = config.and_then(|config| config.cl.spectre.as_deref()) {
            xml.text("SpectreMitigation", &[], spectre);
        }
        xml.end();
    }
    xml.empty(
        "Import",
        &[("Project", r"$(VCTargetsPath)\Microsoft.Cpp.props")],
    );
    xml.start("ImportGroup", &[("Label", "ExtensionSettings")]);
    ctx.tool_imports(&mut xml, plan, ImportKind::Props);
    xml.end();
    xml.start("ImportGroup", &[("Label", "PropertySheets")]);
    let user_props = project
        .target
        .and_then(|id| ctx.model.target(id))
        .and_then(|target| target.props.get(b"VS_USER_PROPS"))
        .map_or_else(
            || r"$(UserRootDir)\Microsoft.Cpp.$(Platform).user.props".to_owned(),
            |value| windows(&string(value)),
        );
    if !user_props.is_empty() {
        write_props_import(&mut xml, &user_props);
    }
    xml.end();
    xml.empty("PropertyGroup", &[("Label", "UserMacros")]);
    xml.start("PropertyGroup", &[]);
    xml.text("_ProjectFileVersion", &[], "10.0.20506.1");
    if plan.items.values().any(|item| item.kind == Kind::Custom) {
        xml.text("CustomBuildBeforeTargets", &[], "ClCompile;ResourceCompile");
    }
    for name in &settings.configs {
        if let Some(config) = plan.configs.get(name) {
            let condition = settings.condition(name);
            let attrs = [("Condition", condition.as_str())];
            xml.text(
                "OutDir",
                &attrs,
                &format!("{}\\", msbuild(&windows(&config.dir))),
            );
            xml.text(
                "IntDir",
                &attrs,
                &format!("{}\\", msbuild(&windows(&config.int_dir))),
            );
            if project.target.is_some() {
                for (key, element) in SDK_DIRECTORIES {
                    if let Some(value) = ctx.definition(key) {
                        xml.text(element, &attrs, &string(value));
                    }
                }
            }
            xml.text("TargetName", &attrs, &msbuild(&config.name));
            xml.text("TargetExt", &attrs, &msbuild(&config.extension));
            if let Some(value) = &config.link_incremental {
                xml.text("LinkIncremental", &attrs, value);
            }
            if let Some(value) = &config.generate_manifest {
                xml.text("GenerateManifest", &attrs, value);
            }
            if !config.debugger.is_empty() {
                xml.text("DebuggerFlavor", &attrs, "WindowsLocalDebugger");
                for (property, value) in &config.debugger {
                    xml.text(property, &attrs, value);
                }
            }
        }
    }
    xml.end();
    for name in &settings.configs {
        let Some(config) = plan.configs.get(name) else {
            continue;
        };
        xml.start(
            "ItemDefinitionGroup",
            &[("Condition", &settings.condition(name))],
        );
        xml.start("ClCompile", &[]);
        config.compile.write(&mut xml);
        if matches!(
            project.kind,
            TargetKind::Executable
                | TargetKind::StaticLibrary
                | TargetKind::SharedLibrary
                | TargetKind::ModuleLibrary
                | TargetKind::ObjectLibrary
        ) {
            xml.text("ObjectFileName", &[], object_file_name(ctx, name));
        }
        xml.end();
        xml.start("ResourceCompile", &[]);
        config.resource.write(&mut xml);
        xml.end();
        xml.empty("Midl", &[]);
        if let Some(cuda) = &config.cuda {
            ctx.write_cuda_config(&mut xml, cuda);
        }
        if project.kind == TargetKind::StaticLibrary {
            xml.start("Lib", &[]);
            config.link.write(&mut xml);
            xml.end();
        } else if matches!(
            project.kind,
            TargetKind::Executable | TargetKind::SharedLibrary | TargetKind::ModuleLibrary
        ) {
            xml.start("Link", &[]);
            config.link.write(&mut xml);
            xml.end();
        }
        if let Some(value) = config.dpi_aware {
            xml.start("Manifest", &[]);
            xml.text("EnableDpiAwareness", &[], value);
            xml.end();
        }
        for (event, rules) in EVENTS.iter().zip(&config.events) {
            if !rules.is_empty() {
                xml.start(event, &[]);
                let commands = rules
                    .iter()
                    .flat_map(|rule| rule.commands.iter().cloned())
                    .collect::<Vec<_>>();
                xml.text(
                    "Command",
                    &[],
                    &rules::script(&commands, run_path.as_deref()),
                );
                if let Some(message) = rules.iter().find(|rule| !rule.message.is_empty()) {
                    xml.text("Message", &[], &msbuild(&message.message));
                }
                xml.end();
            }
        }
        xml.end();
    }
    write_items(&mut xml, ctx, plan, true, run_path.as_deref());
    // As in CMake, the unity support of MSBuild is on before the sources.
    if ctx.unity_build() {
        xml.start("PropertyGroup", &[]);
        xml.text("EnableUnitySupport", &[], "true");
        xml.end();
    }
    write_items(&mut xml, ctx, plan, false, run_path.as_deref());
    if let Some(target) = project.target.and_then(|id| ctx.model.target(id)) {
        write_framework_references(&mut xml, target);
        nuget::write(&mut xml, target);
        write_project_imports(&mut xml, ctx, target);
    }
    if !project.dependencies.is_empty() {
        xml.start("ItemGroup", &[]);
        for dep in project
            .dependencies
            .iter()
            .filter_map(|&index| ctx.projects.get(index))
        {
            let path = msbuild(&windows(&dep.path_from(&project.dir)));
            let mut attrs = vec![("Include", path.as_str())];
            let condition;
            if project.utility == Some(UtilityKind::AllBuild)
                && !dep.in_all.iter().all(|&included| included)
            {
                condition = settings
                    .configs
                    .iter()
                    .zip(&dep.in_all)
                    .filter(|(_, included)| **included)
                    .map(|(name, _)| format!("({})", settings.condition(name)))
                    .collect::<Vec<_>>()
                    .join(" Or ");
                attrs.push(("Condition", &condition));
            }
            xml.start("ProjectReference", &attrs);
            xml.text("Project", &[], &dep.guid);
            xml.text("Name", &[], &dep.name);
            // A C# project reference must not read the target framework of
            // the C++ project. CMake keeps its output assembly.
            if dep.flavor == Flavor::CSharp || dep.path().ends_with("csproj") {
                xml.text("SkipGetTargetFrameworkProperties", &[], "true");
            } else {
                xml.text("ReferenceOutputAssembly", &[], "false");
                xml.text("CopyToOutputDirectory", &[], "Never");
            }
            xml.text("LinkLibraryDependencies", &[], "false");
            xml.end();
        }
        xml.end();
    }
    xml.empty(
        "Import",
        &[("Project", r"$(VCTargetsPath)\Microsoft.Cpp.targets")],
    );
    if plan
        .configs
        .values()
        .any(|config| !config.link_output_dirs.is_empty())
    {
        xml.start(
            "Target",
            &[
                ("Name", "RcmakePrepareLinkDirectories"),
                ("BeforeTargets", "Link"),
            ],
        );
        for name in &settings.configs {
            if let Some(config) = plan.configs.get(name)
                && !config.link_output_dirs.is_empty()
            {
                let dirs = config.link_output_dirs.join(";");
                xml.empty(
                    "MakeDir",
                    &[
                        ("Condition", &settings.condition(name)),
                        ("Directories", &dirs),
                    ],
                );
            }
        }
        xml.end();
    }
    if project.kind == TargetKind::ObjectLibrary {
        xml.empty(
            "Target",
            &[
                ("Name", "RcmakeCompileObjects"),
                ("BeforeTargets", "Build"),
                ("DependsOnTargets", "ClCompile"),
            ],
        );
    }
    xml.start("ImportGroup", &[("Label", "ExtensionTargets")]);
    write_targets_files(&mut xml, ctx, plan);
    ctx.tool_imports(&mut xml, plan, ImportKind::Targets);
    xml.end();
    xml.finish()
}

fn write_items(
    xml: &mut Writer,
    ctx: &Context<'_>,
    plan: &Plan,
    custom: bool,
    run_path: Option<&str>,
) {
    let settings = ctx.settings;
    let mut items = plan
        .items
        .values()
        .filter(|item| (item.kind == Kind::Custom) == custom)
        .peekable();
    if items.peek().is_none() {
        return;
    }
    xml.start("ItemGroup", &[]);
    for item in items {
        let path = crate::msbuild_item(&windows(&item.path));
        let attrs = [("Include", path.as_str())];
        // A source with another item type, such as a `VS_TOOL_OVERRIDE`
        // value, gets no compiler settings. This also applies to CUDA and
        // assembler sources, so the filters file names the same item type.
        if item.tool.is_some() || matches!(item.kind, Kind::Header | Kind::Other) {
            if item.settings.is_empty() {
                xml.empty(item.tag(), &attrs);
            } else {
                xml.start(item.tag(), &attrs);
                write_settings(xml, ctx, item);
                xml.end();
            }
            continue;
        }
        if matches!(item.kind, Kind::Cuda | Kind::Masm | Kind::Nasm) {
            ctx.write_tool_item(xml, item, plan);
            continue;
        }
        let mut item_attrs = attrs.to_vec();
        if let Some(unity) = &item.unity {
            item_attrs.push((
                "IncludeInUnityFile",
                if unity.include { "true" } else { "false" },
            ));
            item_attrs.push(("CustomUnityFile", "true"));
            item_attrs.push(("UnityFilesDirectory", unity.dir.as_str()));
        }
        xml.start(item.kind.tag(), &item_attrs);
        if item.utf8 {
            xml.text("UseUtf8Encoding", &[], "Always");
        }
        for name in &settings.configs {
            let condition = settings.condition(name);
            let attrs = [("Condition", condition.as_str())];
            let Some(config) = item.configs.get(name).filter(|config| !config.excluded) else {
                xml.text("ExcludedFromBuild", &attrs, "true");
                continue;
            };
            if let Some(rule) = &config.rule {
                rules::metadata(xml, rule, &condition, run_path);
            }
            if !config.object.is_empty() {
                xml.text("ObjectFileName", &attrs, &msbuild(&windows(&config.object)));
            }
            if let Some(mode) = config.compile_as
                && !config
                    .compile
                    .as_ref()
                    .is_some_and(|settings| settings.has("CompileAs"))
            {
                xml.text("CompileAs", &attrs, mode);
            }
            if let Some(compile) = &config.compile {
                compile.write_condition(xml, &attrs);
            }
        }
        write_settings(xml, ctx, item);
        xml.end();
    }
    xml.end();
}

/// Write the `VS_SETTINGS` metadata of a source item.
fn write_settings(xml: &mut Writer, ctx: &Context<'_>, item: &Item) {
    for setting in &item.settings {
        match &setting.config {
            Some(config) => {
                let condition = ctx.settings.condition(config);
                xml.text(&setting.name, &[("Condition", &condition)], &setting.value);
            }
            None => xml.text(&setting.name, &[], &setting.value),
        }
    }
}

/// Tell if the MSVC runtime library of a compiled target is a debug library
/// (`MultiThreadedDebug` or `MultiThreadedDebugDLL`). Other targets, and
/// targets without a runtime library setting, give `None`.
fn runtime_is_debug(kind: TargetKind, plan: &Plan, config: &str) -> Option<bool> {
    if !matches!(
        kind,
        TargetKind::Executable
            | TargetKind::StaticLibrary
            | TargetKind::SharedLibrary
            | TargetKind::ModuleLibrary
            | TargetKind::ObjectLibrary
    ) {
        return None;
    }
    plan.configs
        .get(config)?
        .compile
        .get("RuntimeLibrary")
        .filter(|library| !library.is_empty())
        .map(|library| library.contains("Debug"))
}

/// The target-level `ObjectFileName`, as CMake writes it.
///
/// With `VS_NO_COMPILE_BATCHING`, or with a Clang toolset, each source gets
/// its own object file name. Otherwise MSBuild can compile sources in batches.
fn object_file_name(ctx: &Context<'_>, config: &str) -> &'static str {
    let clang = ctx
        .settings
        .toolset
        .strip_prefix('v')
        .and_then(|rest| rest.split_once('_'))
        .is_some_and(|(digits, rest)| {
            !digits.is_empty()
                && digits.bytes().all(|byte| byte.is_ascii_digit())
                && rest.starts_with("clang_")
        });
    let batching_off = ctx
        .property(config, b"VS_NO_COMPILE_BATCHING")
        .is_some_and(|value| truth::is_on(value));
    if batching_off || clang {
        "$(IntDir)%(filename).obj"
    } else {
        "$(IntDir)"
    }
}

/// Write a property sheet import, as CMake writes `VS_USER_PROPS` and
/// `VS_FILTER_PROPS`.
pub(crate) fn write_props_import(xml: &mut Writer, props: &str) {
    let condition = format!("exists('{props}')");
    xml.empty(
        "Import",
        &[
            ("Project", props),
            ("Condition", &condition),
            ("Label", "LocalAppDataPlatform"),
        ],
    );
}

/// Write the .NET target framework of a C++ project in the `Globals` group.
///
/// `DOTNET_TARGET_FRAMEWORK` gives `TargetFramework`, or `TargetFrameworks`
/// for a list. Otherwise `VS_DOTNET_TARGET_FRAMEWORK_VERSION`, then
/// `DOTNET_TARGET_FRAMEWORK_VERSION`, gives `TargetFrameworkVersion`. A
/// project that CMake adds, such as `INSTALL`, uses the top-level
/// `CMAKE_DOTNET_TARGET_FRAMEWORK[_VERSION]` variables.
fn write_target_framework(xml: &mut Writer, ctx: &Context<'_>) {
    let target = ctx.project.target.and_then(|id| ctx.model.target(id));
    let value = |property: &[u8], variable: &[u8]| match target {
        Some(target) => target.props.get(property),
        None => ctx.model.definition(DirId::ROOT, variable),
    };
    if let Some(framework) = value(b"DOTNET_TARGET_FRAMEWORK", b"CMAKE_DOTNET_TARGET_FRAMEWORK") {
        let framework = string(framework);
        let element = if framework.contains(';') {
            "TargetFrameworks"
        } else {
            "TargetFramework"
        };
        xml.text(element, &[], &framework);
    } else if let Some(version) = target
        .and_then(|target| target.props.get(b"VS_DOTNET_TARGET_FRAMEWORK_VERSION"))
        .or_else(|| {
            value(
                b"DOTNET_TARGET_FRAMEWORK_VERSION",
                b"CMAKE_DOTNET_TARGET_FRAMEWORK_VERSION",
            )
        })
    {
        xml.text("TargetFrameworkVersion", &[], &string(version));
    }
}

/// Write the `VS_FRAMEWORK_REFERENCES` items.
fn write_framework_references(xml: &mut Writer, target: &Target) {
    let references = target
        .props
        .get(b"VS_FRAMEWORK_REFERENCES")
        .map(|value| rcmake_base::list::split(value, rcmake_base::list::Empty::Drop))
        .unwrap_or_default();
    if references.is_empty() {
        return;
    }
    xml.start("ItemGroup", &[]);
    for reference in references {
        xml.empty("FrameworkReference", &[("Include", &string(&reference))]);
    }
    xml.end();
}

/// Write the `VS_PROJECT_IMPORT` files. A relative path is relative to the
/// source directory of the target.
fn write_project_imports(xml: &mut Writer, ctx: &Context<'_>, target: &Target) {
    let Some(imports) = target.props.get(b"VS_PROJECT_IMPORT") else {
        return;
    };
    for path in rcmake_base::list::split(imports, rcmake_base::list::Empty::Drop) {
        let path = windows(&absolute(&ctx.project.source, &string(&path)));
        xml.empty("Import", &[("Project", &path)]);
    }
}

/// Import the `.targets` files that the target links, each with the
/// configurations that link it, as CMake does.
fn write_targets_files(xml: &mut Writer, ctx: &Context<'_>, plan: &Plan) {
    let mut files: Vec<(&str, Vec<&str>)> = Vec::new();
    for name in &ctx.settings.configs {
        for file in plan
            .configs
            .get(name)
            .into_iter()
            .flat_map(|config| &config.targets_files)
        {
            match files.iter_mut().find(|(known, _)| *known == file.as_str()) {
                Some((_, configs)) => configs.push(name),
                None => files.push((file, vec![name])),
            }
        }
    }
    for (file, configs) in files {
        let configs = configs
            .iter()
            .map(|config| format!("'$(Configuration)'=='{config}'"))
            .collect::<Vec<_>>()
            .join(" Or ");
        let condition = format!("Exists('{file}') And ({configs})");
        xml.empty("Import", &[("Project", file), ("Condition", &condition)]);
    }
}

/// Write the `Globals` settings of a C++/CLI target, as CMake writes them
/// for Visual Studio 17 and later.
fn write_managed_globals(xml: &mut Writer, ctx: &Context<'_>) {
    xml.text("ManagedAssembly", &[], "true");
    let output = match ctx.project.kind {
        TargetKind::ObjectLibrary | TargetKind::StaticLibrary | TargetKind::SharedLibrary => {
            "Library"
        }
        TargetKind::ModuleLibrary => "Module",
        TargetKind::Executable => {
            let windows_app = ctx
                .project
                .target
                .and_then(|id| ctx.model.target(id))
                .and_then(|target| target.props.get(b"WIN32_EXECUTABLE"))
                .is_some_and(|value| truth::is_on(value));
            if windows_app { "WinExe" } else { "Exe" }
        }
        _ => "Utility",
    };
    xml.text("OutputType", &[], output);
    xml.text("AppDesignerFolder", &[], "Properties");
}
