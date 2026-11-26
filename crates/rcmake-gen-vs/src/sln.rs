//! Visual Studio 17 text solutions.

use std::{collections::BTreeMap, fmt::Write};

use crate::{CPP_TYPE, Project, Settings, plan::Solution, relative, windows};

const FOLDER_TYPE: &str = "{2150E333-8FDC-42A3-9474-1A3956D46DE8}";

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
    for (path, guid) in &solution.folders {
        let name = field(path.rsplit('/').next().unwrap_or(path));
        let path = field(&windows(path));
        let _ = write!(
            out,
            "Project(\"{FOLDER_TYPE}\") = \"{name}\", \"{path}\", \"{guid}\"\r\nEndProject\r\n"
        );
    }
    for project in &members {
        let name = field(&project.name);
        let path = field(&windows(&relative(&solution.dir, &project.path())));
        let _ = write!(
            out,
            "Project(\"{CPP_TYPE}\") = \"{name}\", \"{path}\", \"{}\"\r\n",
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
        for (index, config) in settings.configs.iter().enumerate() {
            let cfg = format!("{config}|{platform}");
            let _ = write!(out, "\t\t{}.{cfg}.ActiveCfg = {cfg}\r\n", project.guid);
            if project.build.get(index) == Some(&true) {
                let _ = write!(out, "\t\t{}.{cfg}.Build.0 = {cfg}\r\n", project.guid);
            }
            if project.deploy.get(index) == Some(&true) {
                let _ = write!(out, "\t\t{}.{cfg}.Deploy.0 = {cfg}\r\n", project.guid);
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
    let _ = write!(
        out,
        "\tGlobalSection(ExtensibilityGlobals) = postSolution\r\n\t\tSolutionGuid = {}\r\n\tEndGlobalSection\r\n\tGlobalSection(ExtensibilityAddIns) = postSolution\r\n\tEndGlobalSection\r\nEndGlobal\r\n",
        solution.guid
    );
    out
}

fn field(value: &str) -> String {
    value.replace('"', "\"\"").replace(['\r', '\n'], " ")
}
