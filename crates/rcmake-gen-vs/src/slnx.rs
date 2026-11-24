//! Visual Studio 18 XML solutions.

use crate::{CPP_TYPE, Project, Solution, guid, relative, windows, xml::Writer};

pub(crate) fn render(solution: &Solution<'_>) -> String {
    let mut xml = Writer::new(false, "UTF-8");
    xml.start("Solution", &[]);
    xml.start("Configurations", &[]);
    for config in &solution.settings.configs {
        xml.empty("BuildType", &[("Name", config)]);
    }
    xml.empty("Platform", &[("Name", &solution.settings.platform)]);
    xml.end();
    let mut projects: Vec<_> = solution.members().collect();
    projects.sort_by(|a, b| a.name.cmp(&b.name));
    for project in projects.iter().filter(|project| project.folder.is_empty()) {
        write_project(&mut xml, solution, project);
    }
    for folder in solution.folders.keys() {
        let name = format!("/{folder}/");
        xml.start("Folder", &[("Name", &name)]);
        for project in projects.iter().filter(|project| project.folder == *folder) {
            write_project(&mut xml, solution, project);
        }
        xml.end();
    }
    xml.finish()
}

fn write_project(xml: &mut Writer, solution: &Solution<'_>, project: &Project) {
    let path = windows(&relative(solution.dir, &project.path()));
    let kind = guid::bare(CPP_TYPE);
    let id = guid::bare(&project.guid);
    let mut attrs = vec![("Path", path.as_str()), ("Type", &kind), ("Id", &id)];
    if solution.startup.as_ref() == Some(&project.name) {
        attrs.push(("DefaultStartup", "true"));
    }
    let mut dependencies = solution.dependencies(project).peekable();
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
            &[("Project", &windows(&relative(solution.dir, &dep.path())))],
        );
    }
    for (index, config) in solution.settings.configs.iter().enumerate() {
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
