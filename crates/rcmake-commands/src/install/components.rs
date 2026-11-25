// SPDX-License-Identifier: MIT OR Apache-2.0

use std::collections::BTreeSet;

use rcmake_base::{BString, list, truth};
use rcmake_eval::Cx;
use rcmake_model::{FileSetKind, InstallKind, InstallOptions, ProjectModel, TargetId, TargetKind};

pub(super) struct Group<'a> {
    pub(super) key: &'a [u8],
    pub(super) file_set: Option<&'a [u8]>,
    pub(super) component: Option<BString>,
    pub(super) namelink: Option<&'a [u8]>,
    pub(super) namelink_component: Option<&'a [u8]>,
}

pub(super) fn for_targets(
    cx: &mut Cx<'_>,
    targets: &[TargetId],
    options: &InstallOptions,
    groups: &[Group<'_>],
    anonymous_dependencies: bool,
) -> BTreeSet<BString> {
    let default = options
        .component
        .as_ref()
        .filter(|name| !name.is_empty())
        .cloned()
        .unwrap_or_else(|| super::default_component(cx));
    let dll = cx.var_is_on(b"WIN32") || cx.var_is_on(b"CYGWIN");
    let dependency_group = if super::runtime::uses_runtime_group(cx) {
        b"RUNTIME".as_slice()
    } else {
        b"LIBRARY"
    };
    let darwin_host = cx.var_or_empty(b"CMAKE_HOST_SYSTEM_NAME") == b"Darwin";
    let model = cx.project();
    let mut components = BTreeSet::new();
    if anonymous_dependencies
        && targets
            .iter()
            .filter_map(|&id| model.target(id))
            .any(|target| {
                matches!(
                    target.kind,
                    TargetKind::Executable | TargetKind::SharedLibrary | TargetKind::ModuleLibrary
                )
            })
    {
        for key in
            std::iter::once(dependency_group).chain(darwin_host.then_some(b"FRAMEWORK".as_slice()))
        {
            let component = groups
                .iter()
                .rev()
                .find(|group| group.key == key)
                .and_then(|group| group.component.as_ref())
                .filter(|name| !name.is_empty())
                .unwrap_or(&default);
            components.insert(component.clone());
        }
    }
    for target in targets.iter().filter_map(|&id| model.target(id)) {
        let on = |key: &[u8]| {
            target
                .props
                .get(key)
                .is_some_and(|value| truth::is_on(value))
        };
        let artifact_groups: &[&[u8]] = match target.kind {
            TargetKind::Executable if on(b"MACOSX_BUNDLE") => &[b"BUNDLE"],
            TargetKind::Executable if on(b"ENABLE_EXPORTS") && dll => &[b"RUNTIME", b"ARCHIVE"],
            TargetKind::Executable => &[b"RUNTIME"],
            TargetKind::StaticLibrary | TargetKind::SharedLibrary if on(b"FRAMEWORK") => {
                &[b"FRAMEWORK"]
            }
            TargetKind::StaticLibrary => &[b"ARCHIVE"],
            TargetKind::SharedLibrary if dll => &[b"RUNTIME", b"ARCHIVE"],
            TargetKind::SharedLibrary | TargetKind::ModuleLibrary => &[b"LIBRARY"],
            TargetKind::ObjectLibrary => &[b"OBJECTS"],
            TargetKind::InterfaceLibrary if options.destination.is_some() => &[b""],
            _ => &[],
        };
        for &key in artifact_groups {
            let group = groups.iter().rev().find(|group| group.key == key);
            if key == b"OBJECTS" && options.destination.is_none() && group.is_none() {
                continue;
            }
            let component = group
                .and_then(|group| group.component.as_ref())
                .unwrap_or(&default);
            if !group.is_some_and(|group| group.namelink == Some(b"NAMELINK_ONLY")) {
                components.insert(component.clone());
            }
            if let Some(group) = group
                && group.namelink != Some(b"NAMELINK_SKIP")
                && (group.namelink == Some(b"NAMELINK_ONLY") || group.namelink_component.is_some())
            {
                components.insert(
                    group
                        .namelink_component
                        .map_or_else(|| component.clone(), BString::from),
                );
            }
        }
        for key in [b"PUBLIC_HEADER".as_slice(), b"PRIVATE_HEADER", b"RESOURCE"] {
            if target.props.get(key).is_some_and(|value| !value.is_empty()) {
                let group = groups.iter().rev().find(|group| group.key == key);
                if group.is_some() || options.destination.is_some() {
                    components.insert(
                        group
                            .and_then(|group| group.component.clone())
                            .unwrap_or_else(|| default.clone()),
                    );
                }
            }
        }
        for group in groups {
            let has_set = target
                .file_sets
                .iter()
                .filter_map(|&id| model.file_set(id))
                .any(|set| match group.key {
                    b"FILE_SET" => group.file_set == Some(set.name.as_slice()),
                    b"CXX_MODULES_BMI" => set.kind == FileSetKind::CxxModules,
                    _ => false,
                });
            if has_set {
                components.insert(group.component.as_ref().unwrap_or(&default).clone());
            }
        }
    }
    components
}

pub(crate) fn registered(model: &ProjectModel) -> BTreeSet<BString> {
    let mut components = BTreeSet::new();
    for (index, rule) in model.installs.iter().enumerate() {
        if let Some(record) = model
            .global
            .props
            .get(format!("_RCMAKE_INSTALL_{index}_COMPONENTS").as_bytes())
        {
            components.extend(list::split(record, list::Empty::Drop));
        } else if !matches!(rule.kind, InstallKind::Targets { .. }) {
            components.insert(rule.options.component.as_ref().cloned().unwrap_or_else(|| {
                model
                    .definition(rule.dir, b"CMAKE_INSTALL_DEFAULT_COMPONENT_NAME")
                    .filter(|name| !name.is_empty())
                    .cloned()
                    .unwrap_or_else(|| "Unspecified".into())
            }));
        }
    }
    for install in &model.runtime_dependency_installs {
        let options = std::iter::once(&install.library).chain(install.framework.as_ref());
        components.extend(options.filter_map(|options| options.component.clone()));
    }
    components
}
