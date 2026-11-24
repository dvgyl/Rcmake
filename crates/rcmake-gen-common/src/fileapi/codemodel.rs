mod backtraces;
mod directories;
mod includes;
mod projects;
mod targets;

use rcmake_base::BString;
use rcmake_resolve::ResolvedTarget;

use super::{FileApi, Members, UInt, Value, array, members, object, paths, relative};
use crate::GenError;

use directories::InstallRules;
use projects::{Project, project_tree};
use targets::{TargetIndexes, TargetReferences, abstract_targets};

impl FileApi<'_, '_> {
    pub(super) fn codemodel(&mut self) -> Result<Members, GenError> {
        let mut configurations = Vec::new();
        for config in &self.result.configs {
            let targets = self
                .result
                .targets
                .iter()
                .filter(|target| target.config == *config)
                .collect();
            configurations.push(self.configuration(&config.0, targets)?);
        }
        if configurations.is_empty() {
            configurations.push(self.configuration(b"", Vec::new())?);
        }
        Ok(members([
            ("paths", paths(self.source, self.build)),
            ("configurations", array(configurations)),
        ]))
    }

    fn configuration(
        &mut self,
        config: &[u8],
        targets: Vec<&ResolvedTarget>,
    ) -> Result<Value, GenError> {
        let abstract_targets = abstract_targets(self.model, config, &targets);
        let targets = targets.into_iter().chain(&abstract_targets).collect();
        let directories: Vec<_> = self.model.dirs().collect();
        let (mut projects, project_by_dir) = project_tree(self.model, &directories);
        let mut directory_targets = vec![TargetIndexes::default(); directories.len()];
        let targets = TargetReferences::collect(
            self.model,
            self.build,
            targets,
            &directories,
            &project_by_dir,
            &mut directory_targets,
            &mut projects,
        );
        let install_rules = InstallRules::collect(self.model);
        let mut target_refs = self.target_references(
            config,
            &targets.build,
            &targets.identities,
            &install_rules.by_target,
        )?;
        let abstract_refs = self.target_references(
            config,
            &targets.abstract_targets,
            &targets.identities,
            &install_rules.by_target,
        )?;
        self.add_utility_targets(
            config,
            &targets.identities,
            &project_by_dir,
            &mut directory_targets,
            &mut projects,
            &mut target_refs,
        )?;
        let directory_refs = self.directory_references(
            config,
            &directories,
            &project_by_dir,
            &directory_targets,
            &install_rules.by_dir,
            &targets.identities,
        )?;
        Ok(object([
            ("name", Value::string(config)),
            ("directories", array(directory_refs)),
            (
                "projects",
                array(projects.into_iter().map(Project::into_json)),
            ),
            ("targets", array(target_refs)),
            ("abstractTargets", array(abstract_refs)),
        ]))
    }

    fn relative_paths(&self, source: &[u8], build: &[u8]) -> Value {
        paths(
            &relative_or_root(source, self.source),
            &relative_or_root(build, self.build),
        )
    }
}

fn number(index: usize) -> Value {
    UInt(index as u64)
}

fn indices(body: &mut Members, field: &str, indices: impl IntoIterator<Item = usize>) {
    optional_array(body, field, indices.into_iter().map(number));
}

fn optional_array(body: &mut Members, field: &str, items: impl IntoIterator<Item = Value>) {
    let items: Vec<_> = items.into_iter().collect();
    if !items.is_empty() {
        body.insert(field.into(), Value::Array(items));
    }
}

fn file_prefix(kind: &str, name: &[u8], config: &[u8]) -> String {
    fn safe(bytes: &[u8]) -> String {
        bytes
            .iter()
            .take(96)
            .map(|&byte| {
                if byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_') {
                    char::from(byte)
                } else {
                    '_'
                }
            })
            .collect()
    }
    let name = safe(name);
    if config.is_empty() {
        format!("{kind}-{name}")
    } else {
        format!("{kind}-{name}-{}", safe(config))
    }
}

fn relative_or_root(path: &[u8], root: &[u8]) -> BString {
    if path.is_empty() {
        ".".into()
    } else {
        relative(path, root)
    }
}
