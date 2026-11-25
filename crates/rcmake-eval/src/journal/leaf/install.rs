// SPDX-License-Identifier: MIT OR Apache-2.0

use rcmake_model::{ExportSet, InstallKind, InstallOptions, InstallRule, ProjectModel};
use serde::{Deserialize, Serialize};

use super::*;

#[derive(Clone, Serialize, Deserialize)]
pub(super) struct Installs {
    rules: Vec<Rule>,
}

#[derive(Clone, Serialize, Deserialize)]
struct Rule {
    targets: Vec<Vec<u8>>,
    export: Option<Vec<u8>>,
    origin: PropertyOrigin,
    options: Options,
    properties: Vec<Property>,
}

#[derive(Clone, Serialize, Deserialize)]
struct Property {
    field: Vec<u8>,
    target: Option<Vec<u8>>,
    value: Vec<u8>,
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

const INSTALL_PREFIX: &[u8] = b"_RCMAKE_INSTALL_";
const TARGET_FIELDS: &[&[u8]] = &[
    b"PUBLIC_HEADER",
    b"PRIVATE_HEADER",
    b"RESOURCE",
    b"FRAMEWORK",
];

fn index(value: &[u8]) -> Option<usize> {
    let digits = std::str::from_utf8(value).ok()?;
    let index = digits.parse::<usize>().ok()?;
    (index.to_string() == digits).then_some(index)
}

fn simple_field(field: &[u8]) -> bool {
    matches!(
        field,
        b"ARGC" | b"DIRECTORY" | b"ABSOLUTE_ACTION" | b"AUTHOR_ACTION" | b"MESSAGE" | b"COMPONENTS"
    ) || field.strip_prefix(b"ARG").and_then(index).is_some()
}

fn target_field(field: &[u8]) -> Option<(&[u8], usize)> {
    TARGET_FIELDS.iter().find_map(|&key| {
        let id = field.strip_prefix(key)?.strip_prefix(b"_")?;
        Some((key, index(id)?))
    })
}

pub(in crate::journal) fn earlier_install_property(name: &[u8], count: usize) -> bool {
    let Some(suffix) = name.strip_prefix(INSTALL_PREFIX) else {
        return false;
    };
    let mut parts = suffix.splitn(2, |byte| *byte == b'_');
    let (Some(rule), Some(field)) = (parts.next(), parts.next()) else {
        return false;
    };
    index(rule).is_some_and(|rule| rule < count)
        && (simple_field(field) || target_field(field).is_some())
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
            if let Some(export) = export
                && !model.exports.get(export).is_some_and(|set| {
                    set.install_rules.contains(&index)
                        && targets.iter().all(|target| set.targets.contains(target))
                })
            {
                return None;
            }
            let names = targets
                .iter()
                .map(|id| Some((id.index(), model.target(*id)?.name.to_vec())))
                .collect::<Option<BTreeMap<_, _>>>()?;
            let prefix = property_prefix(index);
            let properties = model
                .global
                .props
                .names()
                .into_iter()
                .filter(|name| name.starts_with(prefix.as_bytes()))
                .map(|name| {
                    let field = name.strip_prefix(prefix.as_bytes())?;
                    let (field, target) = if simple_field(field) {
                        (field, None)
                    } else {
                        let (field, id) = target_field(field)?;
                        (field, Some(names.get(&id)?.clone()))
                    };
                    let value = model.global.props.get(&name)?;
                    if field == b"DIRECTORY"
                        && value.as_slice() != dir.index().to_string().as_bytes()
                    {
                        return None;
                    }
                    Some(Property {
                        field: field.to_vec(),
                        target,
                        value: value.to_vec(),
                    })
                })
                .collect::<Option<Vec<_>>>()?;
            let targets = targets
                .iter()
                .map(|id| names.get(&id.index()).cloned())
                .collect::<Option<Vec<_>>>()?;
            rules.push(Rule {
                targets,
                export: export.as_ref().map(|value| value.to_vec()),
                origin: PropertyOrigin::new(bin.to_owned(), &rule.backtrace),
                options: (&rule.options).into(),
                properties,
            });
        }
        Some(Self { rules })
    }

    pub(super) fn resolve(
        &self,
        model: &ProjectModel,
        targets: &[Target],
        bin: &Path,
        dir: DirId,
    ) -> Option<Vec<RestoredRule>> {
        let properties = model.global.props.names();
        let target_id = |name: &[u8]| {
            let offset = targets
                .iter()
                .position(|target| target.name.as_slice() == name)?;
            model.replay_target_id(offset)
        };
        self.rules
            .iter()
            .enumerate()
            .map(|(offset, rule)| {
                let prefix = property_prefix(model.installs.len().checked_add(offset)?);
                if !valid_origin(&rule.origin, bin)
                    || properties
                        .iter()
                        .any(|name| name.starts_with(prefix.as_bytes()))
                {
                    return None;
                }
                let targets = rule
                    .targets
                    .iter()
                    .map(|name| target_id(name))
                    .collect::<Option<Vec<_>>>()?;
                let mut properties = BTreeMap::new();
                for property in &rule.properties {
                    let field = property.field.as_slice();
                    let (key, value) = match &property.target {
                        None if simple_field(field) => (
                            [prefix.as_bytes(), field].concat(),
                            if field == b"DIRECTORY" {
                                dir.index().to_string().into_bytes()
                            } else {
                                property.value.clone()
                            },
                        ),
                        Some(name) if TARGET_FIELDS.contains(&field) => {
                            let id = target_id(name)?;
                            if !targets.contains(&id) {
                                return None;
                            }
                            (
                                [
                                    prefix.as_bytes(),
                                    field,
                                    b"_",
                                    id.index().to_string().as_bytes(),
                                ]
                                .concat(),
                                property.value.clone(),
                            )
                        }
                        _ => return None,
                    };
                    if properties.insert(key, value).is_some() {
                        return None;
                    }
                }
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
                    properties,
                })
            })
            .collect()
    }
}

pub(super) struct RestoredRule {
    rule: InstallRule,
    properties: BTreeMap<Vec<u8>, Vec<u8>>,
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
