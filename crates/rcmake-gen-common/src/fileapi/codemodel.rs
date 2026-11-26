mod backtraces;
mod dependencies;
mod directories;
mod file_sets;
mod includes;
mod installers;
mod other_installers;
mod projects;
mod sources;
mod targets;

use std::collections::{HashMap, HashSet};

use rcmake_base::BString;
use rcmake_model::{CustomCommandKind, DirId, RuntimeDependencyInstall, TargetId};
use rcmake_resolve::{BuildGraph, ResolvedTarget, TargetInstaller};

use super::{FileApi, Members, UInt, Value, array, members, object, paths, relative};
use crate::GenError;

use directories::InstallRules;
use projects::{Project, project_tree};
use targets::{TargetIndexes, TargetReferences, abstract_targets};

pub(super) struct ProjectIndices<'a> {
    pub(super) runtime_installs_by_dir: HashMap<DirId, Vec<&'a RuntimeDependencyInstall>>,
    pub(super) utility_commands: HashSet<TargetId>,
    pub(super) first_config_deps: HashMap<TargetId, Vec<TargetId>>,
}

pub(super) struct ConfigIndices<'a> {
    pub(super) project: &'a ProjectIndices<'a>,
    pub(super) build_graph: Option<&'a BuildGraph>,
    pub(super) installers_by_target: HashMap<TargetId, Vec<&'a TargetInstaller>>,
    pub(super) installers_by_rule_target: HashMap<(usize, TargetId), Vec<&'a TargetInstaller>>,
    pub(super) custom_outputs_by_target: HashMap<TargetId, Vec<(usize, rcmake_model::CommandId)>>,
}

impl FileApi<'_, '_> {
    pub(super) fn codemodel(&mut self) -> Result<Members, GenError> {
        let mut runtime_installs_by_dir: HashMap<DirId, Vec<&RuntimeDependencyInstall>> =
            HashMap::new();
        for rule in &self.model.runtime_dependency_installs {
            runtime_installs_by_dir
                .entry(rule.dir)
                .or_default()
                .push(rule);
        }
        let utility_commands: HashSet<TargetId> = self
            .model
            .commands()
            .filter_map(|(_, command)| match command.kind {
                CustomCommandKind::Utility(id) => Some(id),
                _ => None,
            })
            .collect();
        let project_indices = ProjectIndices {
            runtime_installs_by_dir,
            utility_commands,
            first_config_deps: crate::ide::first_config_dependencies(self.result),
        };
        let mut configurations = Vec::new();
        for config in &self.result.configs {
            let targets = self
                .result
                .targets
                .iter()
                .filter(|target| target.config == *config)
                .collect();
            configurations.push(self.configuration(&config.0, targets, &project_indices)?);
        }
        if configurations.is_empty() {
            configurations.push(self.configuration(b"", Vec::new(), &project_indices)?);
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
        project_indices: &ProjectIndices<'_>,
    ) -> Result<Value, GenError> {
        let build_graph = self
            .result
            .graphs
            .iter()
            .find(|graph| graph.config.0.as_slice() == config);
        let mut installers_by_target: HashMap<TargetId, Vec<&TargetInstaller>> = HashMap::new();
        let mut installers_by_rule_target: HashMap<(usize, TargetId), Vec<&TargetInstaller>> =
            HashMap::new();
        for installer in &self.result.install.target_installers {
            if installer.config.matches(config) {
                installers_by_target
                    .entry(installer.target)
                    .or_default()
                    .push(installer);
                if !installer.paths.is_empty() {
                    installers_by_rule_target
                        .entry((installer.rule, installer.target))
                        .or_default()
                        .push(installer);
                }
            }
        }
        let mut custom_outputs_by_target: HashMap<TargetId, Vec<(usize, rcmake_model::CommandId)>> =
            HashMap::new();
        if let Some(graph) = build_graph {
            for (&step_index, owner) in &graph.custom_output_owners {
                for &target_id in &owner.targets {
                    custom_outputs_by_target
                        .entry(target_id)
                        .or_default()
                        .push((step_index, owner.command));
                }
            }
        }
        let config_indices = ConfigIndices {
            project: project_indices,
            build_graph,
            installers_by_target,
            installers_by_rule_target,
            custom_outputs_by_target,
        };
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
        let mut target_refs =
            self.target_references(config, &targets.build, &targets.identities, &config_indices)?;
        let abstract_refs = self.target_references(
            config,
            &targets.abstract_targets,
            &targets.identities,
            &config_indices,
        )?;
        self.add_utility_targets(
            config,
            &targets.identities,
            &project_by_dir,
            &mut directory_targets,
            &mut projects,
            &mut target_refs,
            project_indices,
        )?;
        let directory_refs = self.directory_references(
            config,
            &directories,
            &project_by_dir,
            &directory_targets,
            &install_rules.by_dir,
            &targets.identities,
            &config_indices,
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
