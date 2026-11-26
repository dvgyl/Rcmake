//! Visual Studio 18 XML solutions.

use std::collections::BTreeMap;

use crate::{CPP_TYPE, Project, Settings, guid, plan::Solution, relative, windows, xml::Writer};

pub(crate) fn render(solution: &Solution, settings: &Settings, projects: &[Project]) -> String {
    let mut xml = Writer::new(false, "UTF-8");
    xml.start("Solution", &[]);
    xml.start("Configurations", &[]);
    for config in &settings.configs {
        xml.empty("BuildType", &[("Name", config)]);
    }
    xml.empty("Platform", &[("Name", &settings.platform)]);
    xml.end();
    let mut members: Vec<_> = solution.members(projects).collect();
    members.sort_by(|a, b| a.name.cmp(&b.name));
    let mut by_folder: BTreeMap<&str, Vec<&Project>> = BTreeMap::new();
    for project in members {
        by_folder
            .entry(project.folder.as_str())
            .or_default()
            .push(project);
    }
    if let Some(root_projects) = by_folder.get("") {
        for project in root_projects {
            write_project(&mut xml, solution, settings, projects, project);
        }
    }
    for folder in solution.folders.keys() {
        let name = format!("/{folder}/");
        xml.start("Folder", &[("Name", &name)]);
        if let Some(folder_projects) = by_folder.get(folder.as_str()) {
            for project in folder_projects {
                write_project(&mut xml, solution, settings, projects, project);
            }
        }
        xml.end();
    }
    xml.finish()
}

fn write_project(
    xml: &mut Writer,
    solution: &Solution,
    settings: &Settings,
    projects: &[Project],
    project: &Project,
) {
    let path = windows(&relative(&solution.dir, &project.path()));
    let kind = guid::bare(CPP_TYPE);
    let id = guid::bare(&project.guid);
    let mut attrs = vec![("Path", path.as_str()), ("Type", &kind), ("Id", &id)];
    if solution.startup.as_ref() == Some(&project.name) {
        attrs.push(("DefaultStartup", "true"));
    }
    let mut dependencies = solution.dependencies(projects, project).peekable();
    if dependencies.peek().is_none()
        && project.build.iter().all(|&build| build)
        && !project.deploy.iter().any(|&deploy| deploy)
    {
        xml.empty("Project", &attrs);
        return;
    }
    xml.start("Project", &attrs);
    for dep in dependencies {
        xml.empty(
            "BuildDependency",
            &[("Project", &windows(&relative(&solution.dir, &dep.path())))],
        );
    }
    for (index, config) in settings.configs.iter().enumerate() {
        let selection = format!("{config}|*");
        if project.build.get(index) != Some(&true) {
            xml.empty("Build", &[("Solution", &selection), ("Project", "false")]);
        }
        if project.deploy.get(index) == Some(&true) {
            xml.empty("Deploy", &[("Solution", &selection)]);
        }
    }
    xml.end();
}
