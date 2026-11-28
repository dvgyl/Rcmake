//! Visual Studio 17 text solutions.

use std::{collections::BTreeMap, fmt::Write};

use crate::{Project, Settings, plan::Solution, project_type, windows};

pub(crate) fn render(solution: &Solution, settings: &Settings, projects: &[Project]) -> String {
    let mut out = String::with_capacity(projects.len().saturating_mul(256) + 512);
    out.push_str(
        "\u{feff}\r\nMicrosoft Visual Studio Solution File, Format Version 12.00\r\n# Visual Studio Version 17\r\n",
    );

    let mut members: Vec<_> = solution.members(projects).collect();
    members.sort_by_key(|project| {
        (
            solution.startup.as_ref() != Some(&project.name),
            &project.name,
        )
    });
    for (folder, guid) in &solution.folders {
        let name = field(folder.rsplit('/').next().unwrap_or(folder));
        let path = field(&windows(folder));
        let _ = write!(
            out,
            "Project(\"{{{}}}\") = \"{name}\", \"{path}\", \"{guid}\"\r\n",
            project_type::FOLDER
        );
        if let Some(items) = solution.items.get(folder) {
            out.push_str("\tProjectSection(SolutionItems) = preProject\r\n");
            for item in items {
                let _ = write!(out, "\t\t{item} = {item}\r\n");
            }
            out.push_str("\tEndProjectSection\r\n");
        }
        out.push_str("EndProject\r\n");
    }
    for project in &members {
        let name = field(&project.name);
        let path = field(&windows(&project.path_from(&solution.dir)));
        let _ = write!(
            out,
            "Project(\"{{{}}}\") = \"{name}\", \"{path}\", \"{}\"\r\n",
            field(&project.type_id),
            project.guid
        );
        out.push_str("\tProjectSection(ProjectDependencies) = postProject\r\n");
        for dep in solution.dependencies(projects, project) {
            let _ = write!(out, "\t\t{} = {}\r\n", dep.guid, dep.guid);
        }
        out.push_str("\tEndProjectSection\r\n");
        out.push_str("EndProject\r\n");
    }
    out.push_str("Global\r\n\tGlobalSection(SolutionConfigurationPlatforms) = preSolution\r\n");
    let platform = &settings.platform;
    for config in &settings.configs {
        let _ = write!(out, "\t\t{config}|{platform} = {config}|{platform}\r\n");
    }
    out.push_str(
        "\tEndGlobalSection\r\n\tGlobalSection(ProjectConfigurationPlatforms) = postSolution\r\n",
    );
    for project in members {
        let guid = &project.guid;
        for (index, config) in settings.configs.iter().enumerate() {
            let cfg = format!("{config}|{platform}");
            // An external project can map the configuration and the platform.
            let target = format!("{}|{}", project.config(index, config), project.platform);
            let _ = write!(out, "\t\t{guid}.{cfg}.ActiveCfg = {target}\r\n");
            if solution.builds(project, index) {
                let _ = write!(out, "\t\t{guid}.{cfg}.Build.0 = {target}\r\n");
            }
            if project.deploy.get(index) == Some(&true) {
                let _ = write!(out, "\t\t{guid}.{cfg}.Deploy.0 = {target}\r\n");
            }
        }
    }
    out.push_str("\tEndGlobalSection\r\n");
    if !solution.folders.is_empty() {
        out.push_str("\tGlobalSection(NestedProjects) = preSolution\r\n");
        let mut children = BTreeMap::<&str, Vec<&str>>::new();
        for (folder, guid) in &solution.folders {
            if let Some((parent, _)) = folder.rsplit_once('/') {
                children.entry(parent).or_default().push(guid);
            }
        }
        for project in solution
            .members(projects)
            .filter(|project| !project.folder.is_empty())
        {
            children
                .entry(&project.folder)
                .or_default()
                .push(&project.guid);
        }
        for (folder, entries) in children {
            if let Some(parent) = solution.folders.get(folder) {
                for entry in entries {
                    let _ = write!(out, "\t\t{entry} = {parent}\r\n");
                }
            }
        }
        out.push_str("\tEndGlobalSection\r\n");
    }
    for section in &solution.sections {
        let scope = if section.post { "post" } else { "pre" };
        let _ = write!(
            out,
            "\tGlobalSection({}) = {scope}Solution\r\n",
            section.name
        );
        for (key, value) in &section.entries {
            let _ = write!(out, "\t\t{key} = {value}\r\n");
        }
        // CMake adds the solution GUID to a user `ExtensibilityGlobals`
        // section that does not set one.
        if section.post
            && section.name == "ExtensibilityGlobals"
            && !section.entries.iter().any(|(key, _)| key == "SolutionGuid")
        {
            let _ = write!(out, "\t\tSolutionGuid = {}\r\n", solution.guid);
        }
        out.push_str("\tEndGlobalSection\r\n");
    }
    if !solution.overrides("ExtensibilityGlobals") {
        let _ = write!(
            out,
            "\tGlobalSection(ExtensibilityGlobals) = postSolution\r\n\t\tSolutionGuid = {}\r\n\tEndGlobalSection\r\n",
            solution.guid
        );
    }
    if !solution.overrides("ExtensibilityAddIns") {
        out.push_str(
            "\tGlobalSection(ExtensibilityAddIns) = postSolution\r\n\tEndGlobalSection\r\n",
        );
    }
    out.push_str("EndGlobal\r\n");
    out
}

fn field(value: &str) -> String {
    value.replace('"', "\"\"").replace(['\r', '\n'], " ")
}
