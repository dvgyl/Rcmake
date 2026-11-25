// SPDX-License-Identifier: MIT OR Apache-2.0

use rcmake_model::{ExportSet, InstallKind, InstallOptions, InstallRule, ProjectModel};
use serde::{Deserialize, Serialize};

use super::*;

#[derive(Clone, Serialize, Deserialize)]
pub(super) struct Installs {
    start: usize,
    target_start: usize,
    rules: Vec<Rule>,
}

#[derive(Clone, Serialize, Deserialize)]
struct Rule {
    targets: Vec<Vec<u8>>,
    export: Option<Vec<u8>>,
    origin: PropertyOrigin,
    options: Options,
    properties: Vec<(Vec<u8>, Vec<u8>)>,
}

#[derive(Clone, Serialize, Deserialize)]
struct Options {
    destination: Option<Vec<u8>>,
    component: Option<Vec<u8>>,
    permissions: Vec<Vec<u8>>,
    configurations: Vec<Vec<u8>>,
    optional: bool,
    exclude_from_all: bool,
    rename: Option<Vec<u8>>,
    includes_destination: Vec<Vec<u8>>,
    build_depends: Vec<Vec<u8>>,
    all_components: bool,
    export_package_dependencies: bool,
}

impl From<&InstallOptions> for Options {
    fn from(source: &InstallOptions) -> Self {
        Self {
            destination: source.destination.as_ref().map(|value| value.to_vec()),
            component: source.component.as_ref().map(|value| value.to_vec()),
            permissions: capture_strings(&source.permissions),
            configurations: capture_strings(&source.configurations),
            optional: source.optional,
            exclude_from_all: source.exclude_from_all,
            rename: source.rename.as_ref().map(|value| value.to_vec()),
            includes_destination: capture_strings(&source.includes_destination),
            build_depends: capture_strings(&source.build_depends),
            all_components: source.all_components,
            export_package_dependencies: source.export_package_dependencies,
        }
    }
}

impl From<&Options> for InstallOptions {
    fn from(source: &Options) -> Self {
        Self {
            destination: source.destination.as_deref().map(Into::into),
            component: source.component.as_deref().map(Into::into),
            permissions: restore_strings(&source.permissions),
            configurations: restore_strings(&source.configurations),
            optional: source.optional,
            exclude_from_all: source.exclude_from_all,
            rename: source.rename.as_deref().map(Into::into),
            includes_destination: restore_strings(&source.includes_destination),
            build_depends: restore_strings(&source.build_depends),
            all_components: source.all_components,
            export_package_dependencies: source.export_package_dependencies,
        }
    }
}

fn capture_strings(values: &[BString]) -> Vec<Vec<u8>> {
    values.iter().map(|value| value.to_vec()).collect()
}

fn restore_strings(values: &[Vec<u8>]) -> Vec<BString> {
    values.iter().map(|value| value.as_slice().into()).collect()
}

fn property_prefix(index: usize) -> String {
    format!("_RCMAKE_INSTALL_{index}_")
}

impl Installs {
    pub(super) fn capture(
        model: &ProjectModel,
        bin: &Path,
        dir: DirId,
        start: usize,
        target_start: usize,
    ) -> Option<Self> {
        let mut rules = Vec::new();
        for (index, rule) in model.installs.iter().enumerate().skip(start) {
            let InstallKind::Targets { targets, export } = &rule.kind else {
                return None;
            };
            if rule.dir != dir || targets.iter().any(|id| id.index() < target_start) {
                return None;
            }
            let targets = targets
                .iter()
                .map(|id| Some(model.target(*id)?.name.to_vec()))
                .collect::<Option<Vec<_>>>()?;
            let prefix = property_prefix(index);
            let properties = model
                .global
                .props
                .names()
                .into_iter()
                .filter_map(|name| {
                    name.starts_with(prefix.as_bytes())
                        .then(|| Some((name.to_vec(), model.global.props.get(&name)?.to_vec())))
                        .flatten()
                })
                .collect();
            rules.push(Rule {
                targets,
                export: export.as_ref().map(|value| value.to_vec()),
                origin: PropertyOrigin::new(bin.to_owned(), &rule.backtrace),
                options: (&rule.options).into(),
                properties,
            });
        }
        Some(Self {
            start,
            target_start,
            rules,
        })
    }

    pub(super) fn resolve(
        &self,
        model: &ProjectModel,
        targets: &[Target],
        bin: &Path,
        dir: DirId,
    ) -> Option<Vec<RestoredRule>> {
        if self.start != model.installs.len() || self.target_start != model.targets().count() {
            return None;
        }
        self.rules
            .iter()
            .enumerate()
            .map(|(offset, rule)| {
                let prefix = property_prefix(self.start.checked_add(offset)?);
                if !valid_origin(&rule.origin, bin)
                    || rule.properties.iter().any(|(name, _)| {
                        !name.starts_with(prefix.as_bytes())
                            || model.global.props.get(name).is_some()
                    })
                {
                    return None;
                }
                let targets = rule
                    .targets
                    .iter()
                    .map(|name| {
                        let relative = targets
                            .iter()
                            .position(|target| target.name.as_slice() == name)?;
                        model.replay_target_id(relative)
                    })
                    .collect::<Option<Vec<_>>>()?;
                Some(RestoredRule {
                    rule: InstallRule {
                        dir,
                        backtrace: rule.origin.backtrace(),
                        kind: InstallKind::Targets {
                            targets,
                            export: rule.export.as_deref().map(Into::into),
                        },
                        options: (&rule.options).into(),
                    },
                    properties: rule.properties.clone(),
                })
            })
            .collect()
    }
}

pub(super) struct RestoredRule {
    rule: InstallRule,
    properties: Vec<(Vec<u8>, Vec<u8>)>,
}

impl RestoredRule {
    pub(super) fn insert(self, model: &mut ProjectModel) {
        let members = match &self.rule.kind {
            InstallKind::Targets {
                targets,
                export: Some(name),
            } => Some((targets.clone(), name.clone())),
            _ => None,
        };
        let index = model.add_install(self.rule);
        if let Some((targets, name)) = members {
            let set = model
                .exports
                .entry(name.clone())
                .or_insert_with(|| ExportSet {
                    name,
                    ..ExportSet::default()
                });
            set.targets.extend(targets);
            set.install_rules.push(index);
        }
        for (name, value) in self.properties {
            model
                .global
                .props
                .set(name.as_slice().into(), value.as_slice().into());
        }
    }
}
