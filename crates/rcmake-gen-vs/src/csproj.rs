//! MSBuild C# project output (`.csproj`).
//!
//! A target that compiles only C# gets a `.csproj` project instead of a
//! `.vcxproj` project. A target with a non-empty `DOTNET_SDK` gets a .NET
//! SDK-style project (`<Project Sdk="Microsoft.NET.Sdk">`). Other C# targets
//! get a classic MSBuild project.

mod flags;

use std::collections::BTreeSet;

use md5::{Digest, Md5};
use rcmake_base::{list, truth};
use rcmake_gen_common::GenError;
use rcmake_host::{Follow, Host, path::to_native};
use rcmake_model::{PropertyMap, Target, TargetKind};
use rcmake_resolve::{
    ResolvedTarget,
    ide::{IdeCustomPhase, IdeCustomStep, IdeSource, IdeSourceKind},
};

use crate::{
    MSBUILD_NS, absolute, join, msbuild,
    plan::{Flavor, Project, dotnet_sdk},
    relative,
    rules::{self, Rule},
    string,
    vcxproj::{Context, ide_flags, nuget},
    windows,
    xml::{Writer, is_name},
};

use flags::Flags;

/// Build events in CMake order: the phase, the classic property name, the
/// name prefix of the SDK-style target, its target attribute, and the MSBuild
/// target that it runs before or after.
const EVENTS: [(IdeCustomPhase, &str, &str, &str, &str); 3] = [
    (
        IdeCustomPhase::PreLink,
        "PreLinkEvent",
        "PreLink",
        "BeforeTargets",
        "Link",
    ),
    (
        IdeCustomPhase::PreBuild,
        "PreBuildEvent",
        "PreBuild",
        "BeforeTargets",
        "PreBuildEvent",
    ),
    (
        IdeCustomPhase::PostBuild,
        "PostBuildEvent",
        "PostBuild",
        "AfterTargets",
        "PostBuildEvent",
    ),
];

const COMMON_PROPS: &str =
    r"$(MSBuildExtensionsPath)\$(MSBuildToolsVersion)\Microsoft.Common.props";
const USER_PROPS: &str = r"$(UserRootDir)\Microsoft.CSharp.$(Platform).user.props";
const CSHARP_TARGETS: &str = r"$(MSBuildToolsPath)\Microsoft.CSharp.targets";
/// Default `TargetFrameworkVersion` of a classic project for Visual Studio 17
/// and later.
const DEFAULT_FRAMEWORK_VERSION: &str = "v4.7.2";
/// Default `TargetFramework` of an SDK-style project.
const DEFAULT_FRAMEWORK: &str = "net5.0";

/// Render the `.csproj` file of a C# project.
///
/// # Errors
///
/// Returns an error if the target type or `WIN32_EXECUTABLE` value cannot
/// make a managed binary.
pub(crate) fn render(ctx: &Context<'_>, host: &dyn Host) -> Result<String, GenError> {
    let target = ctx
        .project
        .target
        .and_then(|id| ctx.model.target(id))
        .ok_or_else(|| {
            GenError::Invalid(format!("C# project {} has no target", ctx.project.name).into())
        })?;
    let project = Csharp::new(ctx, target, host);
    let output_type = project.output_type()?;
    if dotnet_sdk(target).is_some() {
        Ok(project.sdk_style(&output_type))
    } else {
        Ok(project.classic(&output_type))
    }
}

/// Settings of one configuration.
struct Config {
    name: String,
    flags: Flags,
    /// Output directory with `\` separators and a final `\`.
    output: String,
    assembly: String,
    /// Build event rules, in [`EVENTS`] order.
    events: [Vec<Rule>; 3],
}

/// One source file and the configurations that build it.
struct Source<'a> {
    path: String,
    source: &'a IdeSource,
    configs: Vec<bool>,
}

/// One custom command and the configurations that run it.
struct Custom<'a> {
    /// The file that the command makes, or its first input.
    path: String,
    steps: Vec<(usize, &'a IdeCustomStep)>,
}

struct Csharp<'a> {
    ctx: &'a Context<'a>,
    project: &'a Project,
    target: &'a Target,
    host: &'a dyn Host,
    configs: Vec<Config>,
    sources: Vec<Source<'a>>,
    customs: Vec<Custom<'a>>,
    run_path: Option<String>,
    in_source: bool,
}

impl<'a> Csharp<'a> {
    fn new(ctx: &'a Context<'a>, target: &'a Target, host: &'a dyn Host) -> Self {
        let project = ctx.project;
        let mut sources: Vec<Source<'a>> = Vec::new();
        let mut customs: Vec<Custom<'a>> = Vec::new();
        let count = ctx.settings.configs.len();
        let mut configs = Vec::with_capacity(count);
        for (index, name) in ctx.settings.configs.iter().enumerate() {
            let fact = ctx.facts.and_then(|facts| facts.get(name)).copied();
            let ide = fact.and_then(|fact| fact.ide.as_deref());
            let mut events: [Vec<Rule>; 3] = Default::default();
            for source in ide.into_iter().flat_map(|ide| &ide.sources) {
                let path = absolute(&project.source, &string(&source.path));
                let position = match sources.iter().position(|entry| entry.path == path) {
                    Some(position) => position,
                    None => {
                        sources.push(Source {
                            path,
                            source,
                            configs: vec![false; count],
                        });
                        sources.len() - 1
                    }
                };
                if let Some(slot) = sources
                    .get_mut(position)
                    .and_then(|entry| entry.configs.get_mut(index))
                {
                    *slot = !source
                        .excluded_configs
                        .iter()
                        .any(|config| config.as_slice() == name.as_bytes());
                }
            }
            for step in ide.into_iter().flat_map(|ide| &ide.custom) {
                if step.commands.iter().all(Vec::is_empty) {
                    continue;
                }
                if let Some(slot) = EVENTS
                    .iter()
                    .position(|(phase, ..)| *phase == step.phase)
                    .and_then(|slot| events.get_mut(slot))
                {
                    slot.push(Rule::from(step));
                    continue;
                }
                let path = custom_source(project, step, customs.len());
                match customs.iter().position(|custom| custom.path == path) {
                    Some(position) => {
                        if let Some(custom) = customs.get_mut(position) {
                            custom.steps.push((index, step));
                        }
                    }
                    None => customs.push(Custom {
                        path,
                        steps: vec![(index, step)],
                    }),
                }
            }
            configs.push(Config {
                name: name.clone(),
                flags: compile_flags(ctx, fact, name),
                output: output_dir(project, fact, name),
                assembly: assembly_name(target, fact, name),
                events,
            });
        }
        let dir = ctx.model.dir(target.dir);
        Self {
            ctx,
            project,
            target,
            host,
            configs,
            sources,
            customs,
            run_path: ctx.run_path(),
            in_source: dir.source_dir == dir.binary_dir,
        }
    }

    fn property(&self, key: &[u8]) -> Option<String> {
        self.target.props.get(key).map(|value| string(value))
    }

    fn output_type(&self) -> Result<String, GenError> {
        let sdk = dotnet_sdk(self.target).is_some();
        let kind = match self.target.kind {
            TargetKind::SharedLibrary => "Library",
            TargetKind::StaticLibrary | TargetKind::ObjectLibrary if !sdk => "Library",
            TargetKind::ModuleLibrary if !sdk => "Module",
            TargetKind::Executable => {
                let win32 = self.property(b"WIN32_EXECUTABLE").unwrap_or_default();
                if win32.contains("$<") {
                    return Err(GenError::Invalid(
                        format!(
                            "Target \"{}\" has a generator expression in its WIN32_EXECUTABLE property. This is not supported on managed executables.",
                            self.project.name
                        )
                        .into(),
                    ));
                }
                if truth::is_on(win32.as_bytes()) {
                    "WinExe"
                } else {
                    "Exe"
                }
            }
            TargetKind::StaticLibrary | TargetKind::ObjectLibrary | TargetKind::ModuleLibrary => {
                return Err(GenError::Invalid(
                    format!(
                        "Target \"{}\" is of a type not supported for managed binaries.",
                        self.project.name
                    )
                    .into(),
                ));
            }
            _ => "Utility",
        };
        Ok(kind.into())
    }

    /// The condition of one configuration. A C# project for the `Win32`
    /// platform also accepts the `x86` platform.
    fn condition(&self, config: &str) -> String {
        let mut condition = self.ctx.settings.condition(config);
        if self.ctx.settings.platform == "Win32" {
            condition.push_str(&format!(
                " Or '$(Configuration)|$(Platform)'=='{}|x86'",
                msbuild(config)
            ));
        }
        condition
    }

    fn sdk_style(&self, output_type: &str) -> String {
        let settings = self.ctx.settings;
        let sdk = dotnet_sdk(self.target).map(string).unwrap_or_default();
        let mut xml = Writer::without_declaration(true);
        xml.start("Project", &[("Sdk", &sdk)]);
        xml.start("PropertyGroup", &[("Label", "Globals")]);
        self.globals(&mut xml);
        xml.text("Configurations", &[], &settings.configs.join(";"));
        xml.text("EnableDefaultItems", &[], "false");
        xml.text("VCProjectUpgraderObjectName", &[], "NoUpgrade");
        xml.text("ManagedAssembly", &[], "true");
        match self.property(b"DOTNET_TARGET_FRAMEWORK") {
            Some(framework) if framework.contains(';') => {
                xml.text("TargetFrameworks", &[], &framework);
            }
            framework => {
                let framework = framework.unwrap_or_else(|| DEFAULT_FRAMEWORK.into());
                xml.text("TargetFramework", &[], &framework);
                xml.text("AppendTargetFrameworkToOutputPath", &[], "false");
            }
        }
        xml.text("OutputType", &[], output_type);
        if let Some(object) = self.property(b"VS_DOTNET_STARTUP_OBJECT") {
            xml.text("StartupObject", &[], &object);
        }
        xml.end();
        for config in &self.configs {
            xml.start(
                "PropertyGroup",
                &[("Condition", &configuration(&config.name))],
            );
            xml.text("OutputPath", &[], &msbuild(&config.output));
            xml.text("AssemblyName", &[], &msbuild(&config.assembly));
            write_flags(&mut xml, &config.flags);
            xml.end();
        }
        for config in &self.configs {
            self.sdk_events(&mut xml, config);
        }
        self.documentation(&mut xml);
        self.custom_commands(&mut xml, true);
        self.sources(&mut xml);
        self.resources(&mut xml);
        self.xaml(&mut xml);
        self.references(&mut xml);
        nuget::write(&mut xml, self.target);
        self.imports(&mut xml);
        self.project_references(&mut xml);
        xml.finish()
    }

    fn classic(&self, output_type: &str) -> String {
        let settings = self.ctx.settings;
        let mut xml = Writer::new(true, "utf-8");
        xml.start(
            "Project",
            &[
                ("DefaultTargets", "Build"),
                ("ToolsVersion", settings.version.tools_version()),
                ("xmlns", MSBUILD_NS),
            ],
        );
        if let Some(host) = &settings.tool_host {
            xml.start("PropertyGroup", &[]);
            xml.text("PreferredToolArchitecture", &[], host);
            xml.end();
        }
        xml.start("PropertyGroup", &[("Label", "Globals")]);
        self.globals(&mut xml);
        self.application_type(&mut xml);
        xml.text("Platform", &[], &settings.platform);
        let label = self
            .property(b"PROJECT_LABEL")
            .unwrap_or_else(|| self.project.name.clone());
        xml.text("ProjectName", &[], &label);
        if let Some(framework) = self.property(b"DOTNET_TARGET_FRAMEWORK") {
            let tag = if framework.contains(';') {
                "TargetFrameworks"
            } else {
                "TargetFramework"
            };
            xml.text(tag, &[], &framework);
        } else {
            let version = self
                .property(b"VS_DOTNET_TARGET_FRAMEWORK_VERSION")
                .or_else(|| self.property(b"DOTNET_TARGET_FRAMEWORK_VERSION"))
                .unwrap_or_else(|| DEFAULT_FRAMEWORK_VERSION.into());
            xml.text("TargetFrameworkVersion", &[], &version);
        }
        xml.text("VCProjectUpgraderObjectName", &[], "NoUpgrade");
        xml.text("ManagedAssembly", &[], "true");
        xml.text("OutputType", &[], output_type);
        xml.text("AppDesignerFolder", &[], "Properties");
        xml.end();
        if let Some(object) = self.property(b"VS_DOTNET_STARTUP_OBJECT") {
            xml.start("PropertyGroup", &[]);
            xml.text("StartupObject", &[], &object);
            xml.end();
        }
        let condition = format!("Exists('{COMMON_PROPS}')");
        xml.empty(
            "Import",
            &[("Project", COMMON_PROPS), ("Condition", &condition)],
        );
        for config in &self.configs {
            let condition = self.condition(&config.name);
            xml.start(
                "PropertyGroup",
                &[("Condition", &condition), ("Label", "Configuration")],
            );
            self.configuration_values(&mut xml, config);
            xml.end();
        }
        xml.start("ImportGroup", &[("Label", "ExtensionSettings")]);
        xml.end();
        xml.start("ImportGroup", &[("Label", "PropertySheets")]);
        let props = self
            .property(b"VS_USER_PROPS")
            .map_or_else(|| USER_PROPS.to_owned(), |props| windows(&props));
        if !props.is_empty() {
            let condition = format!("exists('{props}')");
            xml.empty(
                "Import",
                &[
                    ("Project", &props),
                    ("Condition", &condition),
                    ("Label", "LocalAppDataPlatform"),
                ],
            );
        }
        xml.end();
        self.documentation(&mut xml);
        xml.empty("PropertyGroup", &[("Label", "UserMacros")]);
        let names = self.custom_commands(&mut xml, false);
        self.sources(&mut xml);
        self.references(&mut xml);
        self.framework_references(&mut xml);
        nuget::write(&mut xml, self.target);
        self.imports(&mut xml);
        self.resources(&mut xml);
        self.xaml(&mut xml);
        self.winrt_references(&mut xml);
        self.project_references(&mut xml);
        xml.empty("Import", &[("Project", CSHARP_TARGETS)]);
        xml.start("ImportGroup", &[("Label", "ExtensionTargets")]);
        xml.end();
        for config in &self.configs {
            xml.start(
                "PropertyGroup",
                &[("Condition", &configuration(&config.name))],
            );
            for ((_, property, ..), rules) in EVENTS.iter().zip(&config.events) {
                if let Some(text) = self.event_text(rules) {
                    xml.text(property, &[], &text);
                }
            }
            xml.end();
        }
        // Run the custom commands before the build.
        let mut depends = String::from("\n");
        for name in &names {
            depends.push_str(&format!("      {name};\n"));
        }
        depends.push_str("      $(BuildDependsOn)\n");
        xml.start("PropertyGroup", &[]);
        xml.text("BuildDependsOn", &[], &depends);
        xml.end();
        xml.finish()
    }

    /// Write the `Globals` properties that both project styles share.
    fn globals(&self, xml: &mut Writer) {
        xml.text("ProjectGuid", &[], &self.project.guid);
        if let Some(types) = self.property(b"VS_GLOBAL_PROJECT_TYPES") {
            xml.text("ProjectTypeGuids", &[], &types);
        }
        let keyword = self
            .property(b"VS_GLOBAL_KEYWORD")
            .unwrap_or_else(|| "Win32Proj".into());
        xml.text("Keyword", &[], &keyword);
        if let Some(namespace) = self.property(b"VS_GLOBAL_ROOTNAMESPACE") {
            xml.text("RootNamespace", &[], &namespace);
        }
        for (key, value) in sorted(&self.target.props) {
            let Some(name) = key.strip_prefix("VS_GLOBAL_") else {
                continue;
            };
            if name.is_empty() || matches!(name, "PROJECT_TYPES" | "ROOTNAMESPACE" | "KEYWORD") {
                continue;
            }
            if is_name(name) {
                xml.text(name, &[], &value);
            }
        }
    }

    /// Write the desktop platform settings of a classic project.
    fn application_type(&self, xml: &mut Writer) {
        let settings = self.ctx.settings;
        match settings.platform.as_str() {
            "ARM64" => xml.text("WindowsSDKDesktopARM64Support", &[], "true"),
            "ARM" => xml.text("WindowsSDKDesktopARMSupport", &[], "true"),
            _ => {}
        }
        if let Some(sdk) = &settings.sdk {
            xml.text("WindowsTargetPlatformVersion", &[], sdk);
        }
        if let Some(version) = self.property(b"VS_WINDOWS_TARGET_PLATFORM_MIN_VERSION") {
            xml.text("WindowsTargetPlatformMinVersion", &[], &version);
        }
        if self
            .target
            .props
            .get(b"VS_IOT_STARTUP_TASK")
            .is_some_and(|value| truth::is_on(value))
        {
            xml.text("ContainsStartupTask", &[], "true");
        }
    }

    /// Write the per-configuration values of a classic managed project.
    fn configuration_values(&self, xml: &mut Writer, config: &Config) {
        let mut flags = config.flags.clone();
        if flags.debug_info() {
            xml.text("DebugSymbols", &[], "true");
            xml.text("DefineDebug", &[], "true");
        }
        xml.text("OutputPath", &[], &msbuild(&config.output));
        if let Some(platform) = flags.take("Platform") {
            xml.text("PlatformTarget", &[], &platform);
        }
        let toolset = self
            .property(b"VS_PLATFORM_TOOLSET")
            .unwrap_or_else(|| self.ctx.settings.toolset.clone());
        xml.text("PlatformToolset", &[], &toolset);
        // A C# project has no MSVC runtime library setting.
        if let Some(debug) = self.ctx.debug_libraries(&config.name, None) {
            xml.text(
                "UseDebugLibraries",
                &[],
                if debug { "true" } else { "false" },
            );
        }
        xml.text("AssemblyName", &[], &msbuild(&config.assembly));
        if self.target.kind == TargetKind::Executable {
            xml.text("StartAction", &[], "Program");
            let program = format!("{}{}.exe", config.output, config.assembly);
            xml.text("StartProgram", &[], &msbuild(&program));
        }
        write_flags(xml, &flags);
    }

    fn documentation(&self, xml: &mut Writer) {
        if let Some(file) = self
            .property(b"VS_DOTNET_DOCUMENTATION_FILE")
            .filter(|file| !file.is_empty())
        {
            xml.start("PropertyGroup", &[]);
            xml.text("DocumentationFile", &[], &file);
            xml.end();
        }
    }

    /// Write the build event targets of an SDK-style project.
    fn sdk_events(&self, xml: &mut Writer, config: &Config) {
        for ((_, _, prefix, when, msbuild_target), rules) in EVENTS.iter().zip(&config.events) {
            let Some(text) = self.event_text(rules) else {
                continue;
            };
            let name = format!("{prefix}{}", config.name);
            xml.start(
                "Target",
                &[
                    ("Condition", &configuration(&config.name)),
                    ("Name", &name),
                    (*when, *msbuild_target),
                ],
            );
            xml.empty("Exec", &[("Command", &text)]);
            xml.end();
        }
    }

    /// The text of one build event: an `echo` of the comments, then the
    /// script, each followed by a new line.
    fn event_text(&self, rules: &[Rule]) -> Option<String> {
        if rules.is_empty() {
            return None;
        }
        let commands = rules
            .iter()
            .flat_map(|rule| rule.commands.iter().cloned())
            .collect::<Vec<_>>();
        let comment = rules
            .iter()
            .map(|rule| echo_text(&rule.message))
            .collect::<Vec<_>>()
            .join("\t");
        let mut text = String::new();
        if !comment.replace('\t', "").is_empty() {
            text.push_str(&format!("echo {}\n", msbuild(&comment)));
        }
        text.push_str(&rules::script(&commands, self.run_path.as_deref()));
        text.push('\n');
        Some(text)
    }

    /// Write the custom commands and return the names of their MSBuild
    /// targets.
    fn custom_commands(&self, xml: &mut Writer, sdk: bool) -> BTreeSet<String> {
        let mut names = BTreeSet::new();
        for custom in &self.customs {
            xml.start("ItemGroup", &[]);
            xml.start("None", &[("Include", &include(&custom.path))]);
            self.link(xml, &custom.path);
            xml.end();
            xml.end();
            let hash = Md5::digest(custom.path.as_bytes())
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>();
            for &(index, step) in &custom.steps {
                let Some(config) = self.configs.get(index) else {
                    continue;
                };
                let mut inputs = vec![include(&custom.path)];
                let mut seen = BTreeSet::from([custom.path.clone()]);
                for depend in &step.depends {
                    let depend = string(depend);
                    if !seen.insert(depend.clone()) {
                        continue;
                    }
                    let depend = include(&depend);
                    if sdk {
                        xml.start("ItemGroup", &[]);
                        xml.empty("UpToDateCheckInput", &[("Include", &depend)]);
                        xml.end();
                    }
                    inputs.push(depend);
                }
                let outputs = step
                    .outputs
                    .iter()
                    .map(|path| include(&string(path)))
                    .collect::<Vec<_>>()
                    .join(";");
                let name = format!("CustomCommand_{}_{hash}", config.name);
                let rule = Rule::from(step);
                xml.start(
                    "Target",
                    &[
                        ("Condition", &configuration(&config.name)),
                        ("Name", &name),
                        ("Inputs", &inputs.join(";")),
                        ("Outputs", &outputs),
                        ("BeforeTargets", "CoreCompile"),
                        ("DependsOnTargets", "PrepareForBuild"),
                    ],
                );
                let comment = step
                    .comment
                    .as_ref()
                    .map_or_else(|| self.default_comment(step), |comment| string(comment));
                let comment = echo_text(&comment);
                if !comment.is_empty() {
                    xml.empty(
                        "Exec",
                        &[("Command", &format!("echo {}", msbuild(&comment)))],
                    );
                }
                xml.empty(
                    "Exec",
                    &[(
                        "Command",
                        &rules::script(&rule.commands, self.run_path.as_deref()),
                    )],
                );
                xml.end();
                names.insert(name);
            }
        }
        names
    }

    /// CMake's comment for a custom command without `COMMENT`: the outputs
    /// relative to the binary directory, as in `Generating bar.cs`.
    fn default_comment(&self, step: &IdeCustomStep) -> String {
        if step.outputs.is_empty() {
            return String::new();
        }
        let outputs = step
            .outputs
            .iter()
            .map(|path| {
                let path = string(path);
                relative(&self.project.dir, &path)
            })
            .collect::<Vec<_>>()
            .join(", ");
        format!("Generating {outputs}")
    }

    /// Write the sources that MSBuild compiles or copies.
    fn sources(&self, xml: &mut Writer) {
        xml.start("ItemGroup", &[]);
        // A file with a custom command is written with the command.
        for source in self
            .sources
            .iter()
            .filter(|source| !self.is_custom_output(&source.path))
        {
            let csharp = matches!(
                &source.source.kind,
                IdeSourceKind::Compile { language } if language.as_slice() == b"CSharp"
            );
            if csharp {
                let mut attrs = Vec::new();
                let condition;
                if source.configs.iter().any(|&included| !included) {
                    condition = self
                        .configs
                        .iter()
                        .zip(&source.configs)
                        .filter(|&(_, &included)| included)
                        .map(|(config, _)| {
                            format!(
                                "('$(Configuration)|$(Platform)'=='{}|{}')",
                                msbuild(&config.name),
                                msbuild(&self.ctx.settings.platform)
                            )
                        })
                        .collect::<Vec<_>>()
                        .join(" Or ");
                    attrs.push(("Condition", condition.as_str()));
                }
                let item = include(&source.path);
                attrs.push(("Include", item.as_str()));
                xml.start("Compile", &attrs);
                self.link(xml, &source.path);
                for (tag, value) in self.source_tags(&source.path) {
                    xml.text(&tag, &[], &value);
                }
                xml.end();
            } else if !matches!(extension(&source.path).as_str(), "xaml" | "resx") {
                self.extra_source(xml, &source.path);
            }
        }
        xml.end();
    }

    fn is_custom_output(&self, path: &str) -> bool {
        self.customs.iter().any(|custom| custom.path == path)
    }

    /// Write a source that MSBuild does not compile, such as a text file.
    fn extra_source(&self, xml: &mut Writer, path: &str) {
        let props = self.source_props(path);
        let get = |key: &[u8]| {
            props
                .and_then(|props| props.get(key))
                .filter(|value| !value.is_empty())
                .map(|value| string(value))
        };
        let ext = extension(path);
        let mut tool = match ext.as_str() {
            "hlsl" => "FXCompile",
            "jpg" | "png" => "Image",
            "resw" => "PRIResource",
            "xml" => "XML",
            "natvis" => "Natvis",
            _ => "None",
        }
        .to_owned();
        let mut settings = !self.in_source;
        let mut generator = None;
        if ext == "settings" {
            let name = path.rsplit('/').next().unwrap_or(path);
            generator = Some(name.replacen(".settings", ".Designer.cs", 1));
            settings = true;
        }
        let sub_type = (ext == "vsixmanifest").then_some("Designer");
        let copy = get(b"VS_COPY_TO_OUT_DIR");
        if copy.is_some() {
            tool = "Content".into();
            settings = true;
        }
        let vsix = props
            .and_then(|props| props.get(b"VS_INCLUDE_IN_VSIX"))
            .is_some_and(|value| truth::is_on(value));
        if vsix {
            tool = "Content".into();
            settings = true;
        }
        // An override that cannot be an element name keeps the default tool.
        if let Some(name) = get(b"VS_TOOL_OVERRIDE").filter(|name| is_name(name)) {
            tool = name;
        }
        let item = include(path);
        if !settings {
            xml.empty(&tool, &[("Include", &item)]);
            return;
        }
        xml.start(&tool, &[("Include", &item)]);
        self.link(xml, path);
        if let Some(output) = generator {
            xml.text("Generator", &[], "SettingsSingleFileGenerator");
            xml.text("LastGenOutput", &[], &output);
        }
        if let Some(sub_type) = sub_type {
            xml.text("SubType", &[], sub_type);
        }
        if let Some(copy) = copy {
            xml.text("CopyToOutputDirectory", &[], &copy);
        }
        if vsix {
            xml.text("IncludeInVSIX", &[], "True");
        }
        for (tag, value) in self.source_tags(path) {
            xml.text(&tag, &[], &value);
        }
        xml.end();
    }

    /// Write `.resx` files as embedded resources.
    fn resources(&self, xml: &mut Writer) {
        let resources = self
            .sources
            .iter()
            .filter(|source| extension(&source.path) == "resx")
            .collect::<Vec<_>>();
        if resources.is_empty() {
            return;
        }
        let source_dir = &self.project.source;
        let binary_dir = &self.project.dir;
        xml.start("ItemGroup", &[]);
        for resource in resources {
            let path = &resource.path;
            let relative_include = self.in_source && strip_dir(path, source_dir).is_some();
            let item = if relative_include {
                include(&relative(binary_dir, path))
            } else {
                include(path)
            };
            xml.start("EmbeddedResource", &[("Include", &item)]);
            if !relative_include {
                let link = self
                    .source_link(path)
                    .unwrap_or_else(|| file_name(path).to_owned());
                xml.text("Link", &[], &link);
            }
            let stem = path
                .rsplit_once('.')
                .map_or(path.as_str(), |(stem, _)| stem);
            let designer = format!("{stem}.Designer.cs");
            if self.exists(&designer) {
                let generator = self
                    .source_props(path)
                    .and_then(|props| props.get(b"VS_RESOURCE_GENERATOR"))
                    .map_or_else(
                        || "PublicResXFileCodeGenerator".to_owned(),
                        |value| string(value),
                    );
                if !generator.is_empty() {
                    xml.text("Generator", &[], &generator);
                    let output = designer
                        .strip_prefix(source_dir.as_str())
                        .or_else(|| designer.strip_prefix(binary_dir.as_str()))
                        .unwrap_or_else(|| file_name(&designer));
                    xml.text("LastGenOutput", &[], &windows(output));
                }
            }
            for (tag, value) in self.source_tags(path) {
                xml.text(&tag, &[], &value);
            }
            xml.end();
        }
        xml.end();
    }

    /// Write `.xaml` files with their `VS_XAML_TYPE` element, `Page` by default.
    fn xaml(&self, xml: &mut Writer) {
        let files = self
            .sources
            .iter()
            .filter(|source| extension(&source.path) == "xaml")
            .collect::<Vec<_>>();
        if files.is_empty() {
            return;
        }
        xml.start("ItemGroup", &[]);
        for file in files {
            // A type that cannot be an element name gives the default `Page`.
            let kind = self
                .source_props(&file.path)
                .and_then(|props| props.get(b"VS_XAML_TYPE"))
                .map(|value| string(value))
                .filter(|kind| is_name(kind))
                .unwrap_or_else(|| "Page".to_owned());
            xml.start(&kind, &[("Include", &include(&file.path))]);
            self.link(xml, &file.path);
            xml.text("SubType", &[], "Designer");
            xml.end();
        }
        xml.end();
    }

    /// Write `VS_DOTNET_REFERENCES` and `VS_DOTNET_REFERENCE_<name>` items.
    fn references(&self, xml: &mut Writer) {
        let mut plain = Vec::new();
        let mut hints = Vec::new();
        if let Some(value) = self.target.props.get(b"VS_DOTNET_REFERENCES") {
            for reference in list::split(value, list::Empty::Drop) {
                let reference = string(&reference);
                if self.exists(&reference) {
                    let name = file_name(&reference);
                    let name = name.rsplit_once('.').map_or(name, |(stem, _)| stem);
                    hints.push((name.to_owned(), windows(&reference)));
                } else {
                    plain.push(reference);
                }
            }
        }
        let mut direct = Vec::new();
        for (key, value) in sorted(&self.target.props) {
            if let Some(name) = key.strip_prefix("VS_DOTNET_REFERENCE_") {
                let path = absolute(&self.project.source, &value);
                direct.push((name.to_owned(), windows(&path)));
            }
        }
        direct.append(&mut hints);
        if plain.is_empty() && direct.is_empty() {
            return;
        }
        xml.start("ItemGroup", &[]);
        for name in &plain {
            self.reference(xml, name, None);
        }
        for (name, hint) in &direct {
            self.reference(xml, name, Some(hint.as_str()));
        }
        xml.end();
    }

    fn reference(&self, xml: &mut Writer, name: &str, hint: Option<&str>) {
        xml.start("Reference", &[("Include", name)]);
        xml.text("CopyLocalSatelliteAssemblies", &[], "true");
        xml.text("ReferenceOutputAssembly", &[], "true");
        if let Some(hint) = hint {
            let local = self
                .target
                .props
                .get(b"VS_DOTNET_REFERENCES_COPY_LOCAL")
                .is_none_or(|value| !truth::is_off(value));
            xml.text("Private", &[], if local { "True" } else { "False" });
            xml.text("HintPath", &[], hint);
        }
        self.reference_tags(xml, name);
        xml.end();
    }

    /// Write the `VS_DOTNET_REFERENCEPROP_<name>_TAG_<tag>` values of a
    /// reference.
    fn reference_tags(&self, xml: &mut Writer, name: &str) {
        let prefix = format!("VS_DOTNET_REFERENCEPROP_{name}_TAG_");
        for (key, value) in sorted(&self.target.props) {
            if let Some(tag) = key.strip_prefix(prefix.as_str())
                && !value.is_empty()
                && is_name(tag)
            {
                xml.text(tag, &[], &value);
            }
        }
    }

    fn framework_references(&self, xml: &mut Writer) {
        let references = self
            .target
            .props
            .get(b"VS_FRAMEWORK_REFERENCES")
            .map(|value| list::split(value, list::Empty::Drop))
            .unwrap_or_default();
        if references.is_empty() {
            xml.empty("ItemGroup", &[]);
            return;
        }
        xml.start("ItemGroup", &[]);
        for reference in references {
            xml.empty("FrameworkReference", &[("Include", &string(&reference))]);
        }
        xml.end();
    }

    fn winrt_references(&self, xml: &mut Writer) {
        let Some(value) = self.target.props.get(b"VS_WINRT_REFERENCES") else {
            return;
        };
        let references = list::split(value, list::Empty::Drop);
        if references.is_empty() {
            return;
        }
        xml.start("ItemGroup", &[]);
        for reference in references {
            xml.start("Reference", &[("Include", &string(&reference))]);
            xml.text("IsWinMDFile", &[], "true");
            xml.end();
        }
        xml.end();
    }

    /// Write the `VS_PROJECT_IMPORT` imports.
    fn imports(&self, xml: &mut Writer) {
        let Some(value) = self.target.props.get(b"VS_PROJECT_IMPORT") else {
            return;
        };
        for path in list::split(value, list::Empty::Drop) {
            let path = absolute(&self.project.source, &string(&path));
            xml.empty("Import", &[("Project", &windows(&path))]);
        }
    }

    /// Write one `ProjectReference` for each direct project dependency.
    ///
    /// `ZERO_CHECK` comes first, then the other projects by name.
    fn project_references(&self, xml: &mut Writer) {
        let mut dependencies = self
            .project
            .dependencies
            .iter()
            .filter_map(|&index| self.ctx.projects.get(index))
            .collect::<Vec<_>>();
        dependencies.sort_by_key(|dep| (dep.name != "ZERO_CHECK", dep.name.clone()));
        xml.start("ItemGroup", &[]);
        for dep in dependencies {
            let path = windows(&dep.path());
            xml.start("ProjectReference", &[("Include", &msbuild(&path))]);
            xml.text("Project", &[], &dep.guid);
            xml.text("Name", &[], &dep.name);
            self.reference_tags(xml, &dep.name);
            if dep.flavor == Flavor::CSharp || path.ends_with("csproj") {
                xml.text("SkipGetTargetFrameworkProperties", &[], "true");
            } else {
                xml.text("ReferenceOutputAssembly", &[], "false");
                xml.text("CopyToOutputDirectory", &[], "Never");
            }
            xml.end();
        }
        xml.end();
    }

    /// Write the `Link` element of an out-of-source file, so that the IDE
    /// shows it.
    fn link(&self, xml: &mut Writer, path: &str) {
        if self.in_source {
            return;
        }
        let link = self
            .source_link(path)
            .unwrap_or_else(|| file_name(path).to_owned());
        xml.text("Link", &[], &link);
    }

    /// CMake's link name of a source: its source group path, or its path
    /// below the source or binary directory, or `VS_CSHARP_Link`.
    fn source_link(&self, path: &str) -> Option<String> {
        let group = self
            .sources
            .iter()
            .find(|source| source.path == path)
            .map(|source| string(&source.source.group).replace('\\', "/"))
            .filter(|group| !group.is_empty())
            .map(|group| format!("{group}/{}", file_name(path)));
        let link = if let Some(group) = group.filter(|group| path.ends_with(group.as_str())) {
            group
        } else if let Some(rest) = strip_dir(path, &self.project.source) {
            rest.to_owned()
        } else if let Some(rest) =
            strip_dir(path, &self.project.dir).filter(|_| !path.ends_with(".cs"))
        {
            rest.to_owned()
        } else {
            self.source_props(path)
                .and_then(|props| props.get(b"VS_CSHARP_Link"))
                .map(|value| string(value))?
        };
        let link = windows(&link);
        (!link.is_empty()).then_some(link)
    }

    /// The non-empty `VS_CSHARP_<tag>` properties of a source, by tag.
    fn source_tags(&self, path: &str) -> Vec<(String, String)> {
        self.source_props(path)
            .map(|props| {
                sorted(props)
                    .into_iter()
                    .filter_map(|(key, value)| {
                        let tag = key.strip_prefix("VS_CSHARP_")?;
                        (!tag.is_empty() && !value.is_empty() && is_name(tag))
                            .then(|| (tag.to_owned(), value))
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Find the properties of a source in the target directory.
    fn source_props(&self, path: &str) -> Option<&'a PropertyMap> {
        let dir = self.ctx.model.dir(self.target.dir);
        dir.source_props.get(path.as_bytes()).or_else(|| {
            let rest = strip_dir(path, &self.project.dir)?;
            dir.source_props
                .get(join(&self.project.source, rest).as_bytes())
        })
    }

    fn exists(&self, path: &str) -> bool {
        !path.is_empty()
            && self
                .host
                .metadata(&to_native(path.as_bytes()), Follow::Yes)
                .is_ok()
    }
}

/// Collect the MSBuild flags of one configuration.
///
/// The flags come from the C# compile settings of the target. The C#
/// definitions without `=` are added to `DefineConstants`, as in CMake.
fn compile_flags(ctx: &Context<'_>, fact: Option<&ResolvedTarget>, config: &str) -> Flags {
    let ide = fact
        .and_then(|fact| fact.ide.as_deref())
        .and_then(|ide| {
            ide.compile
                .iter()
                .find(|settings| settings.language == b"CSharp")
        })
        .map(|settings| (&settings.flags, &settings.defines));
    let group = fact
        .and_then(|fact| {
            fact.compile_groups
                .iter()
                .find(|group| group.language == b"CSharp")
        })
        .map(|group| (&group.flags, &group.defines));
    let (mut flags, defines) = match ide.or(group) {
        Some((flags, defines)) => (
            Flags::parse(
                flags
                    .iter()
                    .flat_map(|flag| ide_flags(&string(&flag.value))),
            ),
            defines
                .iter()
                .map(|define| string(&define.value))
                .collect::<Vec<_>>(),
        ),
        None => {
            // Without resolved settings, use the language flag variables.
            let dir = ctx
                .project
                .target
                .and_then(|id| ctx.model.target(id))
                .map(|target| target.dir);
            let var = |key: String| {
                dir.and_then(|dir| ctx.model.definition(dir, key.as_bytes()))
                    .map(|value| crate::flags_table::split(&string(value)))
                    .unwrap_or_default()
            };
            let mut words = var("CMAKE_CSharp_FLAGS".into());
            words.extend(var(format!(
                "CMAKE_CSharp_FLAGS_{}",
                config.to_ascii_uppercase()
            )));
            (Flags::parse(words), Vec::new())
        }
    };
    flags.append(
        "DefineConstants",
        defines.into_iter().filter(|define| !define.contains('=')),
    );
    flags
}

/// The output directory of one configuration, with a final `\`.
fn output_dir(project: &Project, fact: Option<&ResolvedTarget>, config: &str) -> String {
    let naming = fact
        .and_then(|fact| fact.ide.as_deref())
        .map(|ide| &ide.naming)
        .filter(|naming| !naming.dir.is_empty())
        .map(|naming| string(&naming.dir));
    let file = || {
        let file = string(fact?.artifacts.file.as_ref()?);
        let dir = file.rsplit_once('/').map(|(dir, _)| dir.to_owned())?;
        Some(absolute(&project.dir, &dir))
    };
    let dir = naming
        .or_else(file)
        .unwrap_or_else(|| join(&project.dir, config));
    format!("{}\\", windows(dir.trim_end_matches(['/', '\\'])))
}

/// The assembly name of one configuration: the output name and the
/// `<CONFIG>_POSTFIX` value.
fn assembly_name(target: &Target, fact: Option<&ResolvedTarget>, config: &str) -> String {
    if let Some(name) = fact
        .and_then(|fact| fact.ide.as_deref())
        .map(|ide| &ide.naming.base_name)
        .filter(|name| !name.is_empty())
    {
        return string(name);
    }
    let upper = config.to_ascii_uppercase();
    let property = |key: &str| {
        target
            .props
            .get(key.as_bytes())
            .filter(|value| !value.is_empty())
            .map(|value| string(value))
    };
    let mut name = property(&format!("OUTPUT_NAME_{upper}"))
        .or_else(|| property("OUTPUT_NAME"))
        .unwrap_or_else(|| string(&target.name));
    if let Some(postfix) = property(&format!("{upper}_POSTFIX")) {
        name.push_str(&postfix);
    }
    name
}

/// The file that a custom command attaches to: its first output, its first
/// input, or a rule file in the target support directory.
fn custom_source(project: &Project, step: &IdeCustomStep, index: usize) -> String {
    step.outputs
        .first()
        .or(step.depends.first())
        .map(|path| absolute(&project.dir, &string(path)))
        .unwrap_or_else(|| {
            join(
                &project.dir,
                &format!("CMakeFiles/{}.dir/custom-{index}.rule", project.name),
            )
        })
}

/// A path as an MSBuild item, with `\` separators and MSBuild escapes.
fn include(path: &str) -> String {
    msbuild(&windows(path))
}

fn write_flags(xml: &mut Writer, flags: &Flags) {
    for (name, value) in flags.properties() {
        xml.text(name, &[], &value);
    }
}

fn configuration(config: &str) -> String {
    format!("'$(Configuration)' == '{}'", msbuild(config))
}

/// Encode a comment for `echo`, as CMake does: a new line becomes a tab,
/// and `^` comes before each of `"|&<>^`.
fn echo_text(comment: &str) -> String {
    let mut out = String::with_capacity(comment.len());
    for ch in comment.chars() {
        match ch {
            '\r' => {}
            '\n' => out.push('\t'),
            '"' | '|' | '&' | '<' | '>' | '^' => {
                out.push('^');
                out.push(ch);
            }
            _ => out.push(ch),
        }
    }
    out
}

/// The target properties as strings, ordered by name as CMake orders them.
fn sorted(props: &PropertyMap) -> Vec<(String, String)> {
    let mut entries = props
        .names()
        .into_iter()
        .filter_map(|key| {
            let value = props.get(&key)?;
            Some((string(&key), string(value)))
        })
        .collect::<Vec<_>>();
    entries.sort();
    entries
}

fn extension(path: &str) -> String {
    file_name(path)
        .rsplit_once('.')
        .map(|(_, extension)| extension.to_ascii_lowercase())
        .unwrap_or_default()
}

fn file_name(path: &str) -> &str {
    path.rsplit(['/', '\\']).next().unwrap_or(path)
}

/// Return the part of `path` below `dir`.
fn strip_dir<'p>(path: &'p str, dir: &str) -> Option<&'p str> {
    path.strip_prefix(dir.trim_end_matches('/'))?
        .strip_prefix('/')
        .filter(|rest| !rest.is_empty())
}

#[cfg(test)]
mod tests {
    use super::{echo_text, strip_dir};

    #[test]
    fn comments_are_encoded_for_echo() {
        assert_eq!(echo_text("a \"b\" & <c>\r\nd^"), "a ^\"b^\" ^& ^<c^>\td^^");
    }

    #[test]
    fn strip_dir_needs_a_path_below_the_directory() {
        assert_eq!(strip_dir("/src/a/b.cs", "/src"), Some("a/b.cs"));
        assert_eq!(strip_dir("/src/a/b.cs", "/src/"), Some("a/b.cs"));
        assert_eq!(strip_dir("/srcx/b.cs", "/src"), None);
        assert_eq!(strip_dir("/src", "/src"), None);
    }
}
