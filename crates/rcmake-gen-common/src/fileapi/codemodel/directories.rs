//! Write directory replies and resolve their install rules.

use std::collections::BTreeMap;

use rcmake_base::BString;
use rcmake_model::{
    DirId, Directory, InstallKind, InstallRule, ProjectModel, TargetId, TargetKind,
};
use rcmake_resolve::{InstallActionKind, ResolvedTarget};

use crate::GenError;

use super::super::{
    Bool, FileApi, Kind, Object, Value, array, cache_value, members, object, relative,
};
use super::{
    backtraces::Backtraces,
    file_prefix, indices, number, relative_or_root,
    targets::{TargetIndexes, TargetRef},
};

type RulesByDir<'a> = BTreeMap<DirId, Vec<(usize, &'a InstallRule)>>;

pub(super) struct InstallRules<'a> {
    pub(super) by_target: BTreeMap<TargetId, Vec<&'a InstallRule>>,
    pub(super) by_dir: RulesByDir<'a>,
}

impl<'a> InstallRules<'a> {
    pub(super) fn collect(model: &'a ProjectModel) -> Self {
        let mut rules_by_target: BTreeMap<_, Vec<_>> = BTreeMap::new();
        let mut rules_by_dir: BTreeMap<_, Vec<_>> = BTreeMap::new();
        for (index, rule) in model.installs.iter().enumerate() {
            rules_by_dir
                .entry(rule.dir)
                .or_default()
                .push((index, rule));
            if let InstallKind::Targets { targets, .. } = &rule.kind {
                for id in targets {
                    rules_by_target.entry(*id).or_default().push(rule);
                }
            }
        }
        Self {
            by_target: rules_by_target,
            by_dir: rules_by_dir,
        }
    }
}

impl FileApi<'_, '_> {
    pub(super) fn directory_references(
        &mut self,
        config: &[u8],
        directories: &[(DirId, &Directory)],
        project_by_dir: &[usize],
        directory_targets: &[TargetIndexes],
        rules_by_dir: &RulesByDir<'_>,
        targets: &BTreeMap<TargetId, TargetRef<'_>>,
    ) -> Result<Vec<Value>, GenError> {
        let mut has_install = vec![false; directories.len()];
        for (id, dir) in directories.iter().rev() {
            let Some(has_rule) = has_install.get_mut(id.index()) else {
                continue;
            };
            *has_rule |= rules_by_dir.contains_key(id);
            if *has_rule
                && let Some(parent) = dir.parent.and_then(|id| has_install.get_mut(id.index()))
            {
                *parent = true;
            }
        }
        let mut directory_refs = Vec::new();
        for (((id, dir), &project_index), target_indexes) in directories
            .iter()
            .zip(project_by_dir)
            .zip(directory_targets)
        {
            let source = relative_or_root(&dir.source_dir, self.source);
            let build = relative_or_root(&dir.binary_dir, self.build);
            let directory_rules = rules_by_dir.get(id).map_or(&[][..], Vec::as_slice);
            let directory = self.directory_object(dir, config, directory_rules, targets);
            let prefix = file_prefix("directory", &build, config);
            let file = self.writer.hashed(&prefix, &directory)?;
            let mut reference = members([
                ("source", Value::string(source)),
                ("build", Value::string(build)),
                ("projectIndex", number(project_index)),
                ("jsonFile", file),
            ]);
            if let Some(parent) = dir.parent {
                reference.insert("parentIndex".into(), number(parent.index()));
            }
            indices(
                &mut reference,
                "childIndexes",
                dir.children.iter().map(|id| id.index()),
            );
            target_indexes.insert(&mut reference);
            if has_install.get(id.index()) == Some(&true) {
                reference.insert("hasInstallRule".into(), Bool(true));
            }
            // `cmMakefile::GetDefinition("CMAKE_MINIMUM_REQUIRED_VERSION")` at the end of
            // the directory: its final variable value, else the cache entry.
            if let Some(minimum) = dir
                .vars
                .get(b"CMAKE_MINIMUM_REQUIRED_VERSION".as_slice())
                .or_else(|| dir.props.get(b"CMAKE_MINIMUM_REQUIRED_VERSION"))
                .map(|value| value.as_slice())
                .or_else(|| cache_value(self.model, b"CMAKE_MINIMUM_REQUIRED_VERSION"))
            {
                reference.insert(
                    "minimumCMakeVersion".into(),
                    object([("string", Value::string(minimum))]),
                );
            }
            directory_refs.push(Object(reference));
        }
        Ok(directory_refs)
    }

    pub(super) fn target_destination(
        &self,
        target: &ResolvedTarget,
        rule: &InstallRule,
        config: &[u8],
    ) -> Option<BString> {
        if !applies(&rule.options.configurations, config) {
            return None;
        }
        if let Some(destination) = &rule.options.destination {
            return Some(destination.clone());
        }
        if let Some(file) = &target.artifacts.file {
            let file = relative(file, self.build);
            let component = install_component(self.model, rule);
            let matching = self.result.install.actions.iter().find(|action| {
                let InstallActionKind::Files { files, .. } = &action.kind else {
                    return false;
                };
                applies(&action.configurations, config)
                    && action.component == component
                    && files
                        .iter()
                        .any(|source| relative(source, self.build) == file)
            });
            if let Some(action) = matching {
                return Some(action.destination.clone());
            }
        }
        let (var, default) = match target.metadata.kind {
            TargetKind::Executable => (b"CMAKE_INSTALL_BINDIR".as_slice(), b"bin".as_slice()),
            TargetKind::StaticLibrary | TargetKind::SharedLibrary | TargetKind::ModuleLibrary => {
                (b"CMAKE_INSTALL_LIBDIR".as_slice(), b"lib".as_slice())
            }
            _ => return None,
        };
        Some(cache_value(self.model, var).unwrap_or(default).into())
    }

    fn directory_object(
        &self,
        dir: &Directory,
        config: &[u8],
        rules: &[(usize, &InstallRule)],
        targets: &BTreeMap<TargetId, TargetRef<'_>>,
    ) -> Value {
        let mut graph = Backtraces::new(self.source);
        let mut installers = Vec::new();
        for &(index, rule) in rules
            .iter()
            .filter(|(_, rule)| applies(&rule.options.configurations, config))
        {
            let mut base = members([(
                "component",
                Value::string(install_component(self.model, rule)),
            )]);
            graph.tag(&mut base, &rule.backtrace);
            if rule.options.optional {
                base.insert("isOptional".into(), Bool(true));
            }
            if rule.options.exclude_from_all {
                base.insert("isExcludeFromAll".into(), Bool(true));
            }
            if let Some(destination) = &rule.options.destination {
                base.insert("destination".into(), Value::string(destination.clone()));
            }
            let kind = match &rule.kind {
                InstallKind::Targets { targets: ids, .. } => {
                    for target in ids.iter().filter_map(|id| targets.get(id)) {
                        let Some(destination) =
                            self.target_destination(target.target, rule, config)
                        else {
                            continue;
                        };
                        let mut item = base.clone();
                        item.insert("type".into(), Value::string("target"));
                        item.insert("targetId".into(), Value::string(target.id.as_str()));
                        if !target.abstract_target {
                            item.insert("targetIndex".into(), number(target.index));
                        }
                        item.insert("destination".into(), Value::string(destination));
                        if let Some(file) = &target.target.artifacts.file {
                            item.insert(
                                "paths".into(),
                                array([Value::string(relative(file, self.build))]),
                            );
                        }
                        installers.push(Object(item));
                    }
                    continue;
                }
                InstallKind::Files(_) | InstallKind::Programs(_)
                    if self
                        .model
                        .global
                        .props
                        .get(format!("_RCMAKE_INSTALL_{index}_ARG0").as_bytes())
                        .is_some_and(|signature| signature == b"IMPORTED_RUNTIME_ARTIFACTS") =>
                {
                    "importedRuntimeArtifacts"
                }
                InstallKind::Files(files)
                | InstallKind::Programs(files)
                | InstallKind::Directory(files) => {
                    let files = files.iter().map(|file| {
                        let from = relative(&file.value, self.source);
                        if let Some(to) = &rule.options.rename {
                            object([
                                ("from", Value::string(from)),
                                ("to", Value::string(to.clone())),
                            ])
                        } else {
                            Value::string(from)
                        }
                    });
                    base.insert("paths".into(), array(files));
                    if matches!(rule.kind, InstallKind::Directory(_)) {
                        "directory"
                    } else {
                        "file"
                    }
                }
                InstallKind::Script(script) => {
                    base.insert(
                        "scriptFile".into(),
                        Value::string(relative(script, self.source)),
                    );
                    "script"
                }
                InstallKind::Code(_) => "code",
                // The codemodel has no installer type for CPS and SBOM files.
                InstallKind::PackageInfo(_) | InstallKind::Sbom(_) => continue,
                InstallKind::Export { set, file, .. } => {
                    base.insert("exportName".into(), Value::string(set.clone()));
                    let file = file
                        .clone()
                        .unwrap_or_else(|| format!("{set}.cmake").into());
                    base.insert("paths".into(), array([Value::string(file)]));
                    if let Some(export) = self.model.exports.get(set) {
                        base.insert(
                            "exportTargets".into(),
                            array(export.targets.iter().filter_map(|id| targets.get(id)).map(
                                |target| {
                                    let mut item =
                                        members([("id", Value::string(target.id.as_str()))]);
                                    if !target.abstract_target {
                                        item.insert("index".into(), number(target.index));
                                    }
                                    Object(item)
                                },
                            )),
                        );
                    }
                    "export"
                }
            };
            base.insert("type".into(), Value::string(kind));
            installers.push(Object(base));
        }

        object([
            ("codemodelVersion", Kind::Codemodel.version_json()),
            (
                "paths",
                self.relative_paths(&dir.source_dir, &dir.binary_dir),
            ),
            ("installers", array(installers)),
            ("backtraceGraph", graph.into_json()),
        ])
    }
}

fn install_component<'a>(model: &'a ProjectModel, rule: &'a InstallRule) -> &'a [u8] {
    rule.options
        .component
        .as_ref()
        .map(|value| value.as_slice())
        .or_else(|| cache_value(model, b"CMAKE_INSTALL_DEFAULT_COMPONENT_NAME"))
        .unwrap_or(b"Unspecified")
}

fn applies(configurations: &[BString], config: &[u8]) -> bool {
    configurations.is_empty()
        || configurations
            .iter()
            .any(|value| value.eq_ignore_ascii_case(config))
}
