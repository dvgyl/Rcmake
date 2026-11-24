//! Build the project hierarchy and its directory indexes.

use std::collections::BTreeMap;

use rcmake_base::BString;
use rcmake_model::{DirId, Directory, ProjectModel};

use super::super::{Object, Value, array, cache_value, members};
use super::{indices, number, targets::TargetIndexes};

#[derive(Default)]
pub(super) struct Project {
    name: BString,
    parent: Option<usize>,
    children: Vec<usize>,
    directories: Vec<usize>,
    pub(super) targets: TargetIndexes,
}

impl Project {
    pub(super) fn into_json(self) -> Value {
        let mut body = members([
            ("name", Value::string(self.name)),
            (
                "directoryIndexes",
                array(self.directories.into_iter().map(number)),
            ),
        ]);
        if let Some(parent) = self.parent {
            body.insert("parentIndex".into(), number(parent));
        }
        indices(&mut body, "childIndexes", self.children);
        self.targets.insert(&mut body);
        Object(body)
    }
}

pub(super) fn project_tree(
    model: &ProjectModel,
    directories: &[(DirId, &Directory)],
) -> (Vec<Project>, Vec<usize>) {
    let mut project_by_dir = Vec::<usize>::new();
    let mut projects = Vec::<Project>::new();
    let mut project_names = BTreeMap::new();
    for (key, entry) in model.cache.iter() {
        if let Some(name) = key.strip_suffix(b"_SOURCE_DIR")
            && let Some(binary) = cache_value(model, &[name, b"_BINARY_DIR"].concat())
        {
            project_names.insert((entry.value.as_slice(), binary), name);
        }
    }
    for (index, (_, dir)) in directories.iter().enumerate() {
        let parent = dir
            .parent
            .and_then(|id| project_by_dir.get(id.index()))
            .copied();
        let name = dir
            .props
            .get(b"PROJECT_NAME")
            .map(|value| value.as_slice())
            .or_else(|| {
                if index == 0 {
                    cache_value(model, b"CMAKE_PROJECT_NAME")
                } else {
                    project_names
                        .get(&(dir.source_dir.as_slice(), dir.binary_dir.as_slice()))
                        .copied()
                }
            });
        let project_index = match parent {
            Some(parent)
                if projects.get(parent).is_some_and(|project| {
                    name.is_none_or(|name| name == project.name.as_slice())
                }) =>
            {
                parent
            }
            _ => {
                let index = projects.len();
                if let Some(parent) = parent.and_then(|parent| projects.get_mut(parent)) {
                    parent.children.push(index);
                }
                projects.push(Project {
                    name: name.unwrap_or(b"Project").into(),
                    parent,
                    ..Project::default()
                });
                index
            }
        };
        if let Some(project) = projects.get_mut(project_index) {
            project.directories.push(index);
        }
        project_by_dir.push(project_index);
    }
    (projects, project_by_dir)
}
