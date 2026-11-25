//! Generate metadata for external packaging tools.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::{Path, PathBuf};

use rcmake_base::{BString, truth};
use rcmake_eval::Interp;
use rcmake_host::{Host, WriteMode, path::to_native};
use rcmake_json::{Value, write_compact};

use crate::config::{CpackConfig, absolute, script};
use crate::stage::{clear_output_file, project_components};
use crate::{CpackError, PackageContext, PackageGenerator, error, safe_name};

type Object = BTreeMap<BString, Value>;

pub(super) struct ExternalGenerator;

impl PackageGenerator for ExternalGenerator {
    fn name(&self) -> &'static str {
        "External"
    }

    fn extension(&self) -> &'static str {
        ".json"
    }

    fn package(&self, cx: &PackageContext<'_>) -> Result<Vec<PathBuf>, CpackError> {
        let cfg = CpackConfig {
            vars: cx.vars.clone(),
        };
        let file = safe_name(&[cx.file_name, b".json"].concat())?;
        let file = cx.output_dir.join(file);
        let mut data = BString::default();
        write_metadata(&metadata(&cfg), 0, &mut data);
        clear_output_file(cx.host, &file)?;
        cx.host.write(&file, &data, WriteMode::CreateNew)?;
        Ok(vec![file])
    }
}

pub(crate) fn initialize(interp: &mut Interp) -> Result<(), CpackError> {
    let mut cx = interp.cx();
    cx.set_var(b"CPACK_EXTERNAL_KNOWN_VERSIONS", "1.0".into());
    if !cx.var_defined(b"CPACK_PACKAGING_INSTALL_PREFIX") {
        cx.set_var(b"CPACK_PACKAGING_INSTALL_PREFIX", "/".into());
    }
    let root = cx.var_or_empty(b"CMAKE_ROOT");
    let module = to_native(&root).join("Modules/Internal/CPack/CPackExternal.cmake");
    if script(interp, &module).is_err() {
        return Err(error(
            "Error while executing CPackExternal.cmake\nCPack Error: Cannot initialize the generator External",
        ));
    }
    Ok(())
}

pub(crate) fn run_script(
    host: &dyn Host,
    cwd: &Path,
    interp: &mut Interp,
    packages: &mut Vec<PathBuf>,
) -> Result<(), CpackError> {
    let cfg = crate::config::snapshot(interp);
    let Some(file) = cfg
        .get(b"CPACK_EXTERNAL_PACKAGE_SCRIPT")
        .filter(|file| !file.is_empty())
    else {
        return Ok(());
    };
    script(interp, &absolute(cwd, file))?;
    let cfg = crate::config::snapshot(interp);
    packages.extend(
        cfg.list(b"CPACK_EXTERNAL_BUILT_PACKAGES")
            .iter()
            .map(|file| absolute(cwd, file)),
    );
    for package in packages.iter() {
        if host.metadata(package, rcmake_host::Follow::Yes)?.kind != rcmake_host::FileKind::File {
            return Err(error(format!(
                "Expected a package file at {}",
                package.display()
            )));
        }
    }
    Ok(())
}

fn metadata(cfg: &CpackConfig) -> Value {
    let mut root = Object::from([
        ("formatVersionMajor".into(), Value::Int(1)),
        ("formatVersionMinor".into(), Value::Int(0)),
    ]);
    for (field, key) in [
        ("buildConfig", b"CPACK_BUILD_CONFIG".as_slice()),
        (
            "defaultDirectoryPermissions",
            b"CPACK_INSTALL_DEFAULT_DIRECTORY_PERMISSIONS",
        ),
        ("packageDescriptionFile", b"CPACK_PACKAGE_DESCRIPTION_FILE"),
        (
            "packageDescriptionSummary",
            b"CPACK_PACKAGE_DESCRIPTION_SUMMARY",
        ),
        ("packageName", b"CPACK_PACKAGE_NAME"),
        ("packageVersion", b"CPACK_PACKAGE_VERSION"),
    ] {
        if let Some(value) = cfg.get(key) {
            root.insert(field.into(), Value::string(value));
        }
    }
    for (field, key) in [
        (
            "warnOnAbsoluteInstallDestination",
            b"CPACK_WARN_ON_ABSOLUTE_INSTALL_DESTINATION".as_slice(),
        ),
        (
            "errorOnAbsoluteInstallDestination",
            b"CPACK_ERROR_ON_ABSOLUTE_INSTALL_DESTINATION",
        ),
    ] {
        root.insert(field.into(), Value::Bool(cfg.on(key)));
    }
    let strip = cfg
        .get(b"CPACK_STRIP_FILES")
        .is_some_and(|value| !truth::is_off(value));
    root.insert("stripFiles".into(), Value::Bool(strip));
    let internal_destdir = cfg.get(b"CPACK_SET_DESTDIR") == Some(b"I_ON");
    root.insert("setDestdir".into(), Value::Bool(internal_destdir));
    if internal_destdir && let Some(prefix) = cfg.get(b"CPACK_PACKAGING_INSTALL_PREFIX") {
        root.insert("packagingInstallPrefix".into(), Value::string(prefix));
    }

    let mut components = Object::new();
    let mut groups = Object::new();
    let mut types = Object::new();
    let mut projects = Vec::new();
    for [directory, project, selection, subdir] in
        cfg.list(b"CPACK_INSTALL_CMAKE_PROJECTS").as_chunks::<4>().0
    {
        let names = project_components(cfg, selection);
        let install_types = if cfg.on(b"CPACK_MONOLITHIC_INSTALL") {
            Vec::new()
        } else {
            cfg.list(
                format!(
                    "CPACK_{}_INSTALL_TYPES",
                    BString::from(selection.to_ascii_uppercase())
                )
                .as_bytes(),
            )
        };
        add_install_types(cfg, &install_types, &mut types);
        add_components(cfg, &names, &mut components, &mut groups, &mut types);
        projects.push(Value::Object(Object::from([
            ("component".into(), Value::string(selection.clone())),
            ("components".into(), strings(names)),
            ("directory".into(), Value::string(directory.clone())),
            ("installationTypes".into(), strings(install_types)),
            ("projectName".into(), Value::string(project.clone())),
            ("subDirectory".into(), Value::string(subdir.clone())),
        ])));
    }
    root.insert("projects".into(), Value::Array(projects));
    root.insert("components".into(), Value::Object(components));
    root.insert("componentGroups".into(), Value::Object(groups));
    root.insert("installationTypes".into(), Value::Object(types));
    Value::Object(root)
}

fn add_install_types(cfg: &CpackConfig, names: &[BString], types: &mut Object) {
    for name in names {
        let index = types.len() as u64 + 1;
        let options = MemberOptions::new(cfg, "INSTALL_TYPE", name);
        types.entry(name.clone()).or_insert_with(|| {
            Value::Object(Object::from([
                ("name".into(), Value::string(name.clone())),
                (
                    "displayName".into(),
                    Value::string(options.get("DISPLAY_NAME").unwrap_or(name)),
                ),
                ("index".into(), Value::UInt(index)),
            ]))
        });
    }
}

fn add_components(
    cfg: &CpackConfig,
    names: &[BString],
    components: &mut Object,
    groups: &mut Object,
    types: &mut Object,
) {
    let mut pending = VecDeque::from(names.to_vec());
    let mut queued: BTreeSet<_> = names.iter().cloned().collect();
    while let Some(name) = pending.pop_front() {
        if components.contains_key(&name) {
            continue;
        }
        let options = MemberOptions::new(cfg, "COMPONENT", &name);
        let mut component = options.fields(&name);
        let dependencies = options.list("DEPENDS");
        for dep in &dependencies {
            if !components.contains_key(dep) && queued.insert(dep.clone()) {
                pending.push_back(dep.clone());
            }
        }
        component.insert("dependencies".into(), strings(dependencies));
        let install_types = options.list("INSTALL_TYPES");
        add_install_types(cfg, &install_types, types);
        component.insert("installationTypes".into(), strings(install_types));
        component.insert(
            "archiveFile".into(),
            Value::string(options.get("ARCHIVE_FILE").unwrap_or_default()),
        );
        for (field, key) in [
            ("isHidden", "HIDDEN"),
            ("isRequired", "REQUIRED"),
            ("isDisabledByDefault", "DISABLED"),
            ("isDownloaded", "DOWNLOADED"),
        ] {
            component.insert(field.into(), Value::Bool(options.on(key)));
        }
        if let Some(group) = options.get("GROUP").filter(|group| !group.is_empty()) {
            component.insert("group".into(), Value::string(group));
            add_group(cfg, group, groups);
            append_member(groups, group, b"components", &name);
        }
        components.insert(name, Value::Object(component));
    }
}

fn add_group<'a>(cfg: &'a CpackConfig, mut group: &'a [u8], groups: &mut Object) {
    let mut links = Vec::new();
    while !groups.contains_key(group) {
        let options = MemberOptions::new(cfg, "COMPONENT_GROUP", group);
        let mut fields = options.fields(group);
        fields.insert("components".into(), Value::Array(Vec::new()));
        fields.insert("subgroups".into(), Value::Array(Vec::new()));
        fields.insert(
            "isExpandedByDefault".into(),
            Value::Bool(options.on("EXPANDED")),
        );
        fields.insert("isBold".into(), Value::Bool(options.on("BOLD_TITLE")));
        let parent = options
            .get("PARENT_GROUP")
            .filter(|parent| !parent.is_empty());
        if let Some(parent) = parent {
            fields.insert("parentGroup".into(), Value::string(parent));
        }
        groups.insert(group.into(), Value::Object(fields));
        let Some(parent) = parent else { break };
        links.push((parent, group));
        group = parent;
    }
    // Add links after their parent objects exist.
    for (parent, child) in links.into_iter().rev() {
        append_member(groups, parent, b"subgroups", child);
    }
}

fn append_member(groups: &mut Object, group: &[u8], field: &[u8], name: &[u8]) {
    if let Some(Value::Object(fields)) = groups.get_mut(group)
        && let Some(Value::Array(members)) = fields.get_mut(field)
    {
        members.push(Value::string(name));
    }
}

struct MemberOptions<'a> {
    cfg: &'a CpackConfig,
    prefix: String,
}

impl<'a> MemberOptions<'a> {
    fn new(cfg: &'a CpackConfig, kind: &str, name: &[u8]) -> Self {
        let name = BString::from(name.to_ascii_uppercase());
        Self {
            cfg,
            prefix: format!("CPACK_{kind}_{name}_"),
        }
    }

    fn get(&self, key: &str) -> Option<&'a [u8]> {
        self.cfg.get(format!("{}{key}", self.prefix).as_bytes())
    }

    fn on(&self, key: &str) -> bool {
        self.get(key).is_some_and(truth::is_on)
    }

    fn list(&self, key: &str) -> Vec<BString> {
        self.cfg.list(format!("{}{key}", self.prefix).as_bytes())
    }

    fn fields(&self, name: &[u8]) -> Object {
        Object::from([
            ("name".into(), Value::string(name)),
            (
                "displayName".into(),
                Value::string(self.get("DISPLAY_NAME").unwrap_or(name)),
            ),
            (
                "description".into(),
                Value::string(self.get("DESCRIPTION").unwrap_or_default()),
            ),
        ])
    }
}

fn strings(values: Vec<BString>) -> Value {
    Value::Array(values.into_iter().map(Value::String).collect())
}

// External uses multiline arrays, including arrays of one string.
fn write_metadata(value: &Value, depth: usize, data: &mut BString) {
    let newline = |data: &mut BString, depth: usize| {
        data.push(b'\n');
        for _ in 0..depth {
            data.extend_from_slice(b"  ");
        }
    };
    match value {
        Value::Object(fields) if !fields.is_empty() => {
            data.push(b'{');
            for (index, (key, value)) in fields.iter().enumerate() {
                if index > 0 {
                    data.push(b',');
                }
                newline(data, depth + 1);
                data.extend_from_slice(&write_compact(&Value::string(key.clone())));
                data.extend_from_slice(b" : ");
                if matches!(value, Value::Object(fields) if !fields.is_empty())
                    || matches!(value, Value::Array(items) if !items.is_empty())
                {
                    newline(data, depth + 1);
                }
                write_metadata(value, depth + 1, data);
            }
            newline(data, depth);
            data.push(b'}');
        }
        Value::Array(items) if !items.is_empty() => {
            data.push(b'[');
            for (index, value) in items.iter().enumerate() {
                if index > 0 {
                    data.push(b',');
                }
                newline(data, depth + 1);
                write_metadata(value, depth + 1, data);
            }
            newline(data, depth);
            data.push(b']');
        }
        value => data.extend_from_slice(&write_compact(value)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lists_installation_types_used_by_components() -> Result<(), Box<dyn std::error::Error>> {
        let cfg = CpackConfig {
            vars: [
                ("CPACK_INSTALL_CMAKE_PROJECTS", "build;package;ALL;/"),
                ("CPACK_COMPONENTS_ALL", "core;documentation"),
                ("CPACK_COMPONENT_CORE_INSTALL_TYPES", "full"),
                ("CPACK_COMPONENT_DOCUMENTATION_INSTALL_TYPES", "full;docs"),
                ("CPACK_INSTALL_TYPE_DOCS_DISPLAY_NAME", "Documentation"),
            ]
            .into_iter()
            .map(|(name, value)| (name.into(), value.into()))
            .collect(),
        };
        let Value::Object(root) = metadata(&cfg) else {
            return Err("Expected an external package object.".into());
        };
        let Some(Value::Object(types)) = root.get(b"installationTypes".as_slice()) else {
            return Err("The package has no installation types.".into());
        };
        let Some(Value::Object(docs)) = types.get(b"docs".as_slice()) else {
            return Err("The documentation installation type is missing.".into());
        };
        assert_eq!(docs.get(b"index".as_slice()), Some(&Value::UInt(2)));
        assert_eq!(
            docs.get(b"displayName".as_slice()),
            Some(&Value::string("Documentation"))
        );
        Ok(())
    }
}
