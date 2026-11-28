//! Write directory replies and resolve their install rules.

use std::collections::BTreeMap;

use rcmake_base::{
    BString, list,
    path_lex::{Lexical, cmake_style},
};
use rcmake_diag::Backtrace;
use rcmake_genex::{ConfigId, EvalContext, EvalSite, evaluate, parse};
use rcmake_model::{DirId, Directory, InstallKind, InstallRule, ProjectModel, TargetId};
use rcmake_resolve::usage::UsageResolver;

use crate::GenError;

use super::super::{
    Bool, FileApi, Kind, Object, Value, array, cache_value, members, object, relative,
};
use super::{
    ConfigIndices,
    backtraces::Backtraces,
    file_prefix, indices, installers, number, other_installers, relative_or_root,
    targets::{TargetIndexes, TargetRef},
};

type RulesByDir<'a> = BTreeMap<DirId, Vec<(usize, &'a InstallRule)>>;

pub(super) struct InstallRules<'a> {
    pub(super) by_dir: RulesByDir<'a>,
}

impl<'a> InstallRules<'a> {
    pub(super) fn collect(model: &'a ProjectModel) -> Self {
        let mut rules_by_dir: BTreeMap<_, Vec<_>> = BTreeMap::new();
        for (index, rule) in model.installs.iter().enumerate() {
            rules_by_dir
                .entry(rule.dir)
                .or_default()
                .push((index, rule));
        }
        Self {
            by_dir: rules_by_dir,
        }
    }
}

impl FileApi<'_, '_> {
    #[allow(clippy::too_many_arguments)]
    pub(super) fn directory_references(
        &mut self,
        config: &[u8],
        directories: &[(DirId, &Directory)],
        project_by_dir: &[usize],
        directory_targets: &[TargetIndexes],
        rules_by_dir: &RulesByDir<'_>,
        targets: &BTreeMap<TargetId, TargetRef<'_>>,
        config_indices: &ConfigIndices<'_>,
    ) -> Result<Vec<Value>, GenError> {
        let mut has_install = vec![false; directories.len()];
        for (id, dir) in directories.iter().rev() {
            let Some(has_rule) = has_install.get_mut(id.index()) else {
                continue;
            };
            *has_rule |= rules_by_dir.contains_key(id)
                || config_indices
                    .project
                    .runtime_installs_by_dir
                    .contains_key(id);
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
            let directory_rules = rules_by_dir.get(id).map(Vec::as_slice).unwrap_or_default();
            let directory =
                self.directory_object(*id, dir, config, directory_rules, targets, config_indices)?;
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
            // Read `CMAKE_MINIMUM_REQUIRED_VERSION` at the end of the directory: final variable
            // value first, then the cache entry.
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

    fn directory_object(
        &self,
        id: DirId,
        dir: &Directory,
        config: &[u8],
        rules: &[(usize, &InstallRule)],
        targets: &BTreeMap<TargetId, TargetRef<'_>>,
        config_indices: &ConfigIndices<'_>,
    ) -> Result<Value, GenError> {
        let mut graph = Backtraces::new(self.source);
        let rows = other_installers::Rows {
            api: self,
            config: ConfigId(config.into()),
        };
        let mut named = config_indices
            .project
            .runtime_installs_by_dir
            .get(&id)
            .map(Vec::as_slice)
            .unwrap_or_default()
            .iter()
            .copied()
            .peekable();
        let mut installers = Vec::new();
        for &(index, rule) in rules {
            while let Some(set) = named.next_if(|set| set.position <= index) {
                other_installers::append(
                    &mut installers,
                    rows.dependencies(
                        set.dir,
                        &set.backtrace,
                        &set.name,
                        &set.library,
                        set.framework.as_ref(),
                    )?,
                    &set.backtrace,
                    &mut graph,
                );
            }
            if let InstallKind::Targets { targets: ids, .. } = &rule.kind {
                for id in ids {
                    let Some(reference) = targets.get(id) else {
                        continue;
                    };
                    if !reference.abstract_target {
                        let projected = config_indices
                            .installers_by_rule_target
                            .get(&(index, *id))
                            .map(Vec::as_slice)
                            .unwrap_or_default()
                            .iter()
                            .map(|installer| installers::target(installer, reference, self.build));
                        other_installers::append(
                            &mut installers,
                            projected,
                            &rule.backtrace,
                            &mut graph,
                        );
                    }
                    let file_rows = config_indices
                        .file_installers_by_rule_target
                        .get(&(index, *id))
                        .map(Vec::as_slice)
                        .unwrap_or_default()
                        .iter()
                        .map(|installer| installers::target_file(installer, self.source));
                    other_installers::append(
                        &mut installers,
                        file_rows,
                        &rule.backtrace,
                        &mut graph,
                    );
                    if let Some(raw) = self.model.target(*id) {
                        other_installers::append(
                            &mut installers,
                            rows.file_sets(index, rule, reference, raw)?,
                            &rule.backtrace,
                            &mut graph,
                        );
                        other_installers::append(
                            &mut installers,
                            rows.bmi(index, rule, reference)?,
                            &rule.backtrace,
                            &mut graph,
                        );
                    }
                }
                other_installers::append(
                    &mut installers,
                    rows.unnamed_dependencies(index, rule)?,
                    &rule.backtrace,
                    &mut graph,
                );
                continue;
            }
            if !applies(&rule.options.configurations, config) {
                continue;
            }
            let mut base = members([(
                "component",
                Value::string(install_component(self.model, rule)),
            )]);
            if rule.options.all_components {
                base.insert("isForAllComponents".into(), Bool(true));
            }
            if rule.options.optional {
                base.insert("isOptional".into(), Bool(true));
            }
            if rule.options.exclude_from_all {
                base.insert("isExcludeFromAll".into(), Bool(true));
            }
            let is_directory_rule = matches!(rule.kind, InstallKind::Directory(_));
            let evaluated_destination = rule
                .options
                .destination
                .as_ref()
                .map(|destination| {
                    eval_install_genex(self.model, rule.dir, config, &rule.backtrace, destination)
                        .map(|evaluated| {
                            if is_directory_rule {
                                cmake_style(&evaluated)
                            } else {
                                strip_trailing_slash(&evaluated)
                            }
                        })
                })
                .transpose()?;
            if let Some(destination) = &evaluated_destination {
                base.insert("destination".into(), Value::string(destination.clone()));
            }
            let kind = match &rule.kind {
                InstallKind::Targets { .. } => continue,
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
                    let mut expanded_files = Vec::new();
                    for file in files {
                        if file.value.windows(2).any(|pair| pair == b"$<") {
                            let evaluated = eval_install_genex(
                                self.model,
                                rule.dir,
                                config,
                                &file.backtrace,
                                &file.value,
                            )?;
                            expanded_files.extend(list::split(&evaluated, list::Empty::Drop));
                        } else if !file.value.is_empty() {
                            expanded_files.push(file.value.clone());
                        }
                    }
                    if expanded_files.is_empty() {
                        continue;
                    }
                    let directory = matches!(rule.kind, InstallKind::Directory(_));
                    let evaluated_rename = rule
                        .options
                        .rename
                        .as_ref()
                        .map(|name| {
                            eval_install_genex(self.model, rule.dir, config, &rule.backtrace, name)
                        })
                        .transpose()?;
                    let rename = evaluated_rename
                        .as_ref()
                        .filter(|name| !name.is_empty() && expanded_files.len() == 1);
                    let files = expanded_files.iter().map(|file| {
                        if directory {
                            if let Some(dir_prefix) = file.strip_suffix(b"/.") {
                                let rel = relative(dir_prefix, self.source);
                                if rel == b"." {
                                    Value::string(rel)
                                } else {
                                    let mut path = rel;
                                    path.extend_from_slice(b"/.");
                                    Value::string(path)
                                }
                            } else {
                                let (from, to) = file.strip_suffix(b"/").map_or_else(
                                    || (file.as_slice(), Lexical::new(file).filename()),
                                    |from| (from, b".".as_slice()),
                                );
                                installers::path(self.source, from, to)
                            }
                        } else if let Some(to) = rename {
                            installers::path(self.source, file, to)
                        } else {
                            Value::string(relative(file, self.source))
                        }
                    });
                    base.insert("paths".into(), array(files));
                    if directory { "directory" } else { "file" }
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
                    let destination = evaluated_destination
                        .as_deref()
                        .map_or(b"".as_slice(), Vec::as_slice);
                    let dir_rel = relative(&dir.binary_dir, self.build);
                    base.insert(
                        "paths".into(),
                        array([Value::string(installers::export_path(
                            &dir_rel,
                            destination,
                            &file,
                        ))]),
                    );
                    if let Some(export) = self.model.exports.get(set) {
                        base.insert(
                            "exportTargets".into(),
                            array(export.targets.iter().filter_map(|id| targets.get(id)).map(
                                |target| {
                                    object([
                                        ("id", Value::string(target.id.as_str())),
                                        ("index", number(target.index)),
                                    ])
                                },
                            )),
                        );
                    }
                    "export"
                }
            };
            graph.tag(&mut base, &rule.backtrace);
            base.insert("type".into(), Value::string(kind));
            installers.push(Object(base));
        }

        for set in named {
            other_installers::append(
                &mut installers,
                rows.dependencies(
                    set.dir,
                    &set.backtrace,
                    &set.name,
                    &set.library,
                    set.framework.as_ref(),
                )?,
                &set.backtrace,
                &mut graph,
            );
        }
        Ok(object([
            ("codemodelVersion", Kind::Codemodel.version_json()),
            (
                "paths",
                self.relative_paths(&dir.source_dir, &dir.binary_dir),
            ),
            ("installers", array(installers)),
            ("backtraceGraph", graph.into_json()),
        ]))
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

pub(super) fn applies(configurations: &[BString], config: &[u8]) -> bool {
    configurations.is_empty()
        || configurations
            .iter()
            .any(|value| value.eq_ignore_ascii_case(config))
}

fn eval_install_genex(
    model: &ProjectModel,
    dir: DirId,
    config: &[u8],
    backtrace: &Backtrace,
    value: &BString,
) -> Result<BString, GenError> {
    if !value.windows(2).any(|pair| pair == b"$<") {
        return Ok(value.clone());
    }
    let context = EvalContext {
        config: ConfigId(config.into()),
        site: EvalSite {
            backtrace: backtrace.clone(),
            property: None,
        },
        ..EvalContext::default()
    };
    let genex = parse(value).map_err(|err| GenError::Invalid(err.to_string().into()))?;
    let mut usage = UsageResolver::new(model);
    evaluate(&genex, &context, &mut usage.host(dir))
        .map(|(evaluated, _)| evaluated)
        .map_err(|err| GenError::Invalid(err.to_string().into()))
}

fn strip_trailing_slash(path: &[u8]) -> BString {
    let normalized = cmake_style(path);
    let mut slice = normalized.as_slice();
    while slice.len() > 1 && !slice.ends_with(b":/") {
        let Some(trimmed) = slice.strip_suffix(b"/") else {
            break;
        };
        slice = trimmed;
    }
    slice.into()
}
