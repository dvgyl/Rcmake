//! Visual Studio 17 text solutions.

use std::fmt::Write;

use crate::{CPP_TYPE, Solution, relative, windows};

const FOLDER_TYPE: &str = "{66A26720-8FB5-11D2-AA7E-00C04F688DDE}";

pub(crate) fn render(solution: &Solution<'_>) -> String {
    let mut out = String::from(
        "\u{feff}\nMicrosoft Visual Studio Solution File, Format Version 12.00\n# Visual Studio Version 17\n",
    );

    let mut projects: Vec<_> = solution.members().collect();
    if let Some(startup) = &solution.startup {
        projects.sort_by_key(|project| project.name != *startup);
    }
    for (path, guid) in &solution.folders {
        let name = field(path.rsplit('/').next().unwrap_or(path));
        let _ = writeln!(
            out,
            "Project(\"{FOLDER_TYPE}\") = \"{name}\", \"{name}\", \"{guid}\"\nEndProject"
        );
    }
    for project in &projects {
        let name = field(&project.name);
        let path = field(&windows(&relative(solution.dir, &project.path())));
        let _ = writeln!(
            out,
            "Project(\"{CPP_TYPE}\") = \"{name}\", \"{path}\", \"{}\"",
            project.guid
        );
        let mut dependencies = solution.dependencies(project).peekable();
        if dependencies.peek().is_some() {
            out.push_str("\tProjectSection(ProjectDependencies) = postProject\n");
            for dep in dependencies {
                let _ = writeln!(out, "\t\t{} = {}", dep.guid, dep.guid);
            }
            out.push_str("\tEndProjectSection\n");
        }
        out.push_str("EndProject\n");
    }
    out.push_str("Global\n\tGlobalSection(SolutionConfigurationPlatforms) = preSolution\n");
    let platform = &solution.settings.platform;
    for config in &solution.settings.configs {
        let _ = writeln!(out, "\t\t{config}|{platform} = {config}|{platform}");
    }
    out.push_str(
        "\tEndGlobalSection\n\tGlobalSection(ProjectConfigurationPlatforms) = postSolution\n",
    );
    for project in projects {
        for (index, config) in solution.settings.configs.iter().enumerate() {
            let cfg = format!("{config}|{platform}");
            let _ = writeln!(out, "\t\t{}.{cfg}.ActiveCfg = {cfg}", project.guid);
            if project.build.get(index) == Some(&true) {
                let _ = writeln!(out, "\t\t{}.{cfg}.Build.0 = {cfg}", project.guid);
            }
            if project.deploy.get(index) == Some(&true) {
                let _ = writeln!(out, "\t\t{}.{cfg}.Deploy.0 = {cfg}", project.guid);
            }
        }
    }
    out.push_str("\tEndGlobalSection\n");
    if !solution.folders.is_empty() {
        out.push_str("\tGlobalSection(NestedProjects) = preSolution\n");
        for project in solution.members() {
            if let Some(folder) = solution.folders.get(&project.folder) {
                let _ = writeln!(out, "\t\t{} = {folder}", project.guid);
            }
        }
        for (folder, guid) in &solution.folders {
            if let Some((parent, _)) = folder.rsplit_once('/')
                && let Some(parent) = solution.folders.get(parent)
            {
                let _ = writeln!(out, "\t\t{guid} = {parent}");
            }
        }
        out.push_str("\tEndGlobalSection\n");
    }
    let _ = writeln!(
        out,
        "\tGlobalSection(ExtensibilityGlobals) = postSolution\n\t\tSolutionGuid = {}\n\tEndGlobalSection\n\tGlobalSection(ExtensibilityAddIns) = postSolution\n\tEndGlobalSection\nEndGlobal",
        solution.guid
    );
    if cfg!(windows) {
        out.replace('\n', "\r\n")
    } else {
        out
    }
}

fn field(value: &str) -> String {
    value.replace('"', "\"\"").replace(['\r', '\n'], " ")
}
