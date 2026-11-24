//! Render the project plan as MSBuild XML.

use crate::{MSBUILD_NS, relative, xml::Writer};

use super::*;

const EVENTS: [&str; 3] = ["PreBuildEvent", "PreLinkEvent", "PostBuildEvent"];

pub(crate) fn render(ctx: &Context<'_>, plan: &Plan) -> String {
    let settings = ctx.settings;
    let project = ctx.project;
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
    xml.text("VCProjectUpgraderObjectName", &[], "NoUpgrade");
    if let Some(target) = project.target.and_then(|id| ctx.model.target(id)) {
        for key in target.props.names() {
            let name = string(&key);
            if let Some(property) = name.strip_prefix("VS_GLOBAL_").filter(|key| xml_name(key))
                && let Some(value) = target.props.get(&key)
            {
                xml.text(property, &[], &string(value));
            }
        }
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
        xml.text("ConfigurationType", &[], configuration_type);
        xml.text("CharacterSet", &[], "MultiByte");
        xml.text("PlatformToolset", &[], &settings.toolset);
        if let Some(config) = plan.configs.get(name) {
            for (property, value) in &config.properties {
                xml.text(property, &[], value);
            }
        }
        xml.end();
    }
    xml.empty(
        "Import",
        &[("Project", r"$(VCTargetsPath)\Microsoft.Cpp.props")],
    );
    xml.empty("ImportGroup", &[("Label", "ExtensionSettings")]);
    xml.start("ImportGroup", &[("Label", "PropertySheets")]);
    xml.empty(
        "Import",
        &[
            (
                "Project",
                r"$(UserRootDir)\Microsoft.Cpp.$(Platform).user.props",
            ),
            (
                "Condition",
                r"exists('$(UserRootDir)\Microsoft.Cpp.$(Platform).user.props')",
            ),
            ("Label", "LocalAppDataPlatform"),
        ],
    );
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
                &format!("{}\\", msbuild(&windows(&intermediate(project, name)))),
            );
            xml.text("TargetName", &attrs, &msbuild(&config.name));
            xml.text("TargetExt", &attrs, &msbuild(&config.extension));
            if let Some(value) = &config.link_incremental {
                xml.text("LinkIncremental", &attrs, value);
            }
            if let Some(value) = &config.generate_manifest {
                xml.text("GenerateManifest", &attrs, value);
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
        xml.end();
        xml.empty("ResourceCompile", &[]);
        xml.empty("Midl", &[]);
        if project.kind == TargetKind::StaticLibrary {
            xml.start("Lib", &[]);
            config.link.write(&mut xml);
            xml.end();
        } else if matches!(
            project.kind,
            TargetKind::Executable | TargetKind::SharedLibrary | TargetKind::ModuleLibrary
        ) {
            xml.start("Link", &[]);
            if project.kind == TargetKind::Executable && config.gui {
                xml.text("SubSystem", &[], "Windows");
            }
            config.link.write(&mut xml);
            xml.end();
        }
        for (event, rules) in EVENTS.iter().zip(&config.events) {
            if !rules.is_empty() {
                xml.start(event, &[]);
                let commands = rules
                    .iter()
                    .flat_map(|rule| rule.commands.iter().cloned())
                    .collect::<Vec<_>>();
                xml.text("Command", &[], &rules::script(&commands));
                if let Some(message) = rules.iter().find(|rule| !rule.message.is_empty()) {
                    xml.text("Message", &[], &msbuild(&message.message));
                }
                xml.end();
            }
        }
        xml.end();
    }
    write_items(&mut xml, plan, settings, true);
    write_items(&mut xml, plan, settings, false);
    if !project.dependencies.is_empty() {
        xml.start("ItemGroup", &[]);
        for dep in project
            .dependencies
            .iter()
            .filter_map(|&index| ctx.projects.get(index))
        {
            let path = msbuild(&windows(&relative(&project.dir, &dep.path())));
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
            xml.text("ReferenceOutputAssembly", &[], "false");
            xml.text("CopyToOutputDirectory", &[], "Never");
            xml.text("LinkLibraryDependencies", &[], "false");
            xml.end();
        }
        xml.end();
    }
    xml.empty(
        "Import",
        &[("Project", r"$(VCTargetsPath)\Microsoft.Cpp.targets")],
    );
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
    xml.empty("ImportGroup", &[("Label", "ExtensionTargets")]);
    xml.finish()
}

fn write_items(xml: &mut Writer, plan: &Plan, settings: &Settings, custom: bool) {
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
        let path = msbuild(&windows(&item.path));
        let attrs = [("Include", path.as_str())];
        if matches!(item.kind, Kind::Header | Kind::Other) {
            xml.empty(item.kind.tag(), &attrs);
            continue;
        }
        xml.start(item.kind.tag(), &attrs);
        for name in &settings.configs {
            let condition = settings.condition(name);
            let attrs = [("Condition", condition.as_str())];
            let Some(config) = item.configs.get(name).filter(|config| !config.excluded) else {
                xml.text("ExcludedFromBuild", &attrs, "true");
                continue;
            };
            if let Some(rule) = &config.rule {
                rules::metadata(xml, rule, &condition);
            }
            if !config.object.is_empty() {
                xml.text("ObjectFileName", &attrs, &msbuild(&windows(&config.object)));
            }
            if matches!(config.language.as_str(), "C" | "CXX")
                && config.language != source_language(&item.path)
            {
                let mode = if config.language == "C" {
                    "CompileAsC"
                } else {
                    "CompileAsCpp"
                };
                xml.text("CompileAs", &attrs, mode);
            }
            if let Some(compile) = &config.compile {
                compile.write_condition(xml, &attrs);
            }
        }
        xml.end();
    }
    xml.end();
}
