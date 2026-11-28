//! Visual Studio 18 XML solutions.

use std::collections::BTreeMap;

use crate::{Project, Settings, guid, plan::Solution, project_type, xml::Writer};

pub(crate) fn render(solution: &Solution, settings: &Settings, projects: &[Project]) -> String {
    let mut xml = Writer::new(false, "UTF-8").compact();
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
        for item in solution.items.get(folder).into_iter().flatten() {
            xml.empty("File", &[("Path", item)]);
        }
        xml.end();
    }
    // Unlike the `.sln` writer, this format has no default sections.
    for section in &solution.sections {
        let mut attrs = vec![("Name", section.name.as_str())];
        if section.post {
            attrs.push(("Scope", "PostLoad"));
        }
        if section.entries.is_empty() {
            xml.empty("Properties", &attrs);
            continue;
        }
        xml.start("Properties", &attrs);
        for (key, value) in &section.entries {
            xml.empty("Properties", &[("Name", key), ("Value", value)]);
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
    let path = project_path(solution, project);
    // CMake 4.5.1 writes the type and the GUID in lower case, also a cached
    // external GUID such as `AAA-bbb` (`.sln` keeps the case as given).
    let kind = project.type_id.to_ascii_lowercase();
    let id = guid::bare(&project.guid);
    let mut attrs = vec![("Path", path.as_str()), ("Type", &kind), ("Id", &id)];
    if solution.startup.as_ref() == Some(&project.name) {
        attrs.push(("DefaultStartup", "true"));
    }
    let platform = (project.platform != settings.platform
        || project_type::explicit_platform(&project.type_id))
    .then_some(project.platform.as_str());
    let mapped = settings
        .configs
        .iter()
        .enumerate()
        .any(|(index, config)| project.config(index, config) != config);
    let mut dependencies = solution.dependencies(projects, project).peekable();
    if dependencies.peek().is_none()
        && (0..settings.configs.len()).all(|index| solution.builds(project, index))
        && !project.deploy.iter().any(|&deploy| deploy)
        && platform.is_none()
        && !mapped
    {
        xml.empty("Project", &attrs);
        return;
    }
    xml.start("Project", &attrs);
    for dep in dependencies {
        xml.empty(
            "BuildDependency",
            &[("Project", &project_path(solution, dep))],
        );
    }
    for (index, config) in settings.configs.iter().enumerate() {
        let selection = format!("{config}|*");
        let target = project.config(index, config);
        if target != config {
            xml.empty(
                "BuildType",
                &[("Solution", &selection), ("Project", target)],
            );
        }
        if !solution.builds(project, index) {
            xml.empty("Build", &[("Solution", &selection), ("Project", "false")]);
        }
        if project.deploy.get(index) == Some(&true) {
            xml.empty("Deploy", &[("Solution", &selection)]);
        }
    }
    if let Some(platform) = platform {
        xml.empty("Platform", &[("Project", platform)]);
    }
    xml.end();
}

/// The project path relative to the solution. As in CMake, this format keeps
/// `/` separators, where the `.sln` format uses `\`.
fn project_path(solution: &Solution, project: &Project) -> String {
    project.path_from(&solution.dir)
}
