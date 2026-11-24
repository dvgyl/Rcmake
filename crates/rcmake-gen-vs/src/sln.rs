//! Visual Studio 17 text solutions.

use std::{collections::BTreeMap, fmt::Write};

use crate::{CPP_TYPE, Solution, relative, windows};

const FOLDER_TYPE: &str = "{2150E333-8FDC-42A3-9474-1A3956D46DE8}";

pub(crate) fn render(solution: &Solution<'_>) -> String {
    let mut out = String::from(
        "\u{feff}\nMicrosoft Visual Studio Solution File, Format Version 12.00\n# Visual Studio Version 17\n",
    );

    let mut projects: Vec<_> = solution.members().collect();
    projects.sort_by_key(|project| {
        (
            solution.startup.as_ref() != Some(&project.name),
            &project.name,
        )
    });
    for (path, guid) in &solution.folders {
        let name = field(path.rsplit('/').next().unwrap_or(path));
        let path = field(&windows(path));
        let _ = writeln!(
            out,
            "Project(\"{FOLDER_TYPE}\") = \"{name}\", \"{path}\", \"{guid}\"\nEndProject"
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
        out.push_str("\tProjectSection(ProjectDependencies) = postProject\n");
        for dep in solution.dependencies(project) {
            let _ = writeln!(out, "\t\t{} = {}", dep.guid, dep.guid);
        }
        out.push_str("\tEndProjectSection\n");
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
        let mut children = BTreeMap::<&str, Vec<&str>>::new();
        for (folder, guid) in &solution.folders {
            if let Some((parent, _)) = folder.rsplit_once('/') {
                children.entry(parent).or_default().push(guid);
            }
        }
        for project in solution
            .members()
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
                    let _ = writeln!(out, "\t\t{entry} = {parent}");
                }
            }
        }
        out.push_str("\tEndGlobalSection\n");
    }
    let _ = writeln!(
        out,
        "\tGlobalSection(ExtensibilityGlobals) = postSolution\n\t\tSolutionGuid = {}\n\tEndGlobalSection\n\tGlobalSection(ExtensibilityAddIns) = postSolution\n\tEndGlobalSection\nEndGlobal",
        solution.guid
    );
    out.replace('\n', "\r\n")
}

fn field(value: &str) -> String {
    value.replace('"', "\"\"").replace(['\r', '\n'], " ")
}
