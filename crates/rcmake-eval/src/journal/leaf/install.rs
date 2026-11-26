// SPDX-License-Identifier: MIT OR Apache-2.0

use rcmake_model::{ExportSet, InstallKind, InstallOptions, InstallRule, ProjectModel};

use super::*;

#[derive(Clone)]
pub(super) struct Installs {
    rules: Vec<Rule>,
}

#[derive(Clone)]
struct Rule {
    targets: Vec<Vec<u8>>,
    export: Option<Vec<u8>>,
    origin: PropertyOrigin,
    options: Options,
    properties: Vec<Property>,
}

#[derive(Clone)]
struct Property {
    field: Vec<u8>,
    target: Option<Vec<u8>>,
    value: Vec<u8>,
}

#[derive(Clone)]
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

impl ToJson for Installs {
    fn to_json(&self) -> Json {
        Json::object_from([("rules", self.rules.to_json())])
    }
}

impl FromJson for Installs {
    fn from_json(value: &Json) -> Result<Self, FieldError> {
        let obj = codec::fields(value, &["rules"])?;
        Ok(Self {
            rules: req(obj, "rules")?,
        })
    }
}

impl ToJson for Rule {
    fn to_json(&self) -> Json {
        Json::object_from([
            ("targets", codec::bytes_list(&self.targets)),
            ("export", codec::opt_bytes(self.export.as_deref())),
            ("origin", self.origin.to_json()),
            ("options", self.options.to_json()),
            ("properties", self.properties.to_json()),
        ])
    }
}

impl FromJson for Rule {
    fn from_json(value: &Json) -> Result<Self, FieldError> {
        let obj = codec::fields(
            value,
            &["targets", "export", "origin", "options", "properties"],
        )?;
        Ok(Self {
            targets: codec::field(obj, "targets", codec::from_bytes_list)?,
            export: codec::field(obj, "export", codec::from_opt_bytes)?,
            origin: req(obj, "origin")?,
            options: req(obj, "options")?,
            properties: req(obj, "properties")?,
        })
    }
}

impl ToJson for Property {
    fn to_json(&self) -> Json {
        Json::object_from([
            ("field", codec::bytes(&self.field)),
            ("target", codec::opt_bytes(self.target.as_deref())),
            ("value", codec::bytes(&self.value)),
        ])
    }
}

impl FromJson for Property {
    fn from_json(value: &Json) -> Result<Self, FieldError> {
        let obj = codec::fields(value, &["field", "target", "value"])?;
        Ok(Self {
            field: codec::field(obj, "field", codec::from_bytes)?,
            target: codec::field(obj, "target", codec::from_opt_bytes)?,
            value: codec::field(obj, "value", codec::from_bytes)?,
        })
    }
}

const OPTION_FIELDS: &[&str] = &[
    "destination",
    "component",
    "permissions",
    "configurations",
    "optional",
    "exclude_from_all",
    "rename",
    "includes_destination",
    "build_depends",
    "all_components",
    "export_package_dependencies",
];

impl ToJson for Options {
    fn to_json(&self) -> Json {
        Json::object_from([
            ("destination", codec::opt_bytes(self.destination.as_deref())),
            ("component", codec::opt_bytes(self.component.as_deref())),
            ("permissions", codec::bytes_list(&self.permissions)),
            ("configurations", codec::bytes_list(&self.configurations)),
            ("optional", self.optional.to_json()),
            ("exclude_from_all", self.exclude_from_all.to_json()),
            ("rename", codec::opt_bytes(self.rename.as_deref())),
            (
                "includes_destination",
                codec::bytes_list(&self.includes_destination),
            ),
            ("build_depends", codec::bytes_list(&self.build_depends)),
            ("all_components", self.all_components.to_json()),
            (
                "export_package_dependencies",
                self.export_package_dependencies.to_json(),
            ),
        ])
    }
}

impl FromJson for Options {
    fn from_json(value: &Json) -> Result<Self, FieldError> {
        let obj = codec::fields(value, OPTION_FIELDS)?;
        let list = |key: &str| codec::field(obj, key, codec::from_bytes_list);
        let text = |key: &str| codec::field(obj, key, codec::from_opt_bytes);
        Ok(Self {
            destination: text("destination")?,
            component: text("component")?,
            permissions: list("permissions")?,
            configurations: list("configurations")?,
            optional: req(obj, "optional")?,
            exclude_from_all: req(obj, "exclude_from_all")?,
            rename: text("rename")?,
            includes_destination: list("includes_destination")?,
            build_depends: list("build_depends")?,
            all_components: req(obj, "all_components")?,
            export_package_dependencies: req(obj, "export_package_dependencies")?,
        })
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

#[cfg(test)]
mod tests {
    use rcmake_diag::{Backtrace, Frame, FrameKind};

    use super::*;
    use crate::journal::codec::round_trip;

    fn options(destination: Option<Vec<u8>>) -> Options {
        Options {
            destination,
            component: Some(b"Runtime".to_vec()),
            permissions: vec![b"OWNER_READ".to_vec(), b"OWNER_WRITE".to_vec()],
            configurations: vec![b"Release".to_vec()],
            optional: true,
            exclude_from_all: false,
            rename: None,
            includes_destination: vec![b"include".to_vec()],
            build_depends: Vec::new(),
            all_components: false,
            export_package_dependencies: true,
        }
    }

    #[test]
    fn install_records_round_trip() -> Result<(), Box<dyn std::error::Error>> {
        let bin = if cfg!(windows) {
            "C:/work/build"
        } else {
            "/work/build"
        };
        let backtrace = Backtrace::default().push(Frame {
            file: b"/work/CMakeLists.txt".as_slice().into(),
            line: 12,
            name: Some(b"install".as_slice().into()),
            kind: FrameKind::Command,
        });
        let rule = |export, destination| Rule {
            targets: vec![b"lib".to_vec(), b"\xff".to_vec()],
            export,
            origin: PropertyOrigin::new(bin.into(), &backtrace),
            options: options(destination),
            properties: vec![
                Property {
                    field: b"FILE".to_vec(),
                    target: Some(b"lib".to_vec()),
                    value: b"lib.a".to_vec(),
                },
                Property {
                    field: b"\xfeRAW".to_vec(),
                    target: None,
                    value: Vec::new(),
                },
            ],
        };
        round_trip(&options(None))?;
        round_trip(&rule(None, None))?;
        round_trip(&Installs {
            rules: vec![
                rule(Some(b"Targets".to_vec()), Some(b"lib".to_vec())),
                rule(None, None),
            ],
        })?;
        round_trip(&Installs { rules: Vec::new() })?;
        Ok(())
    }
}
