//! Target property defaults captured at creation.

use rcmake_base::{BString, list};
use rcmake_eval::Cx;
use rcmake_model::{BT, GeneratorKind, LinkItem, Target, TargetKind, UsageField};

const BUILD_PROPERTIES: &[&str] = &[
    "C_STANDARD",
    "C_STANDARD_REQUIRED",
    "C_EXTENSIONS",
    "CXX_STANDARD",
    "CXX_STANDARD_REQUIRED",
    "CXX_EXTENSIONS",
    "POSITION_INDEPENDENT_CODE",
    "ARCHIVE_OUTPUT_DIRECTORY",
    "LIBRARY_OUTPUT_DIRECTORY",
    "RUNTIME_OUTPUT_DIRECTORY",
    "PDB_OUTPUT_DIRECTORY",
    "COMPILE_PDB_OUTPUT_DIRECTORY",
    "MACOSX_RPATH",
    "BUILD_RPATH",
    "INSTALL_RPATH",
    "INSTALL_RPATH_USE_LINK_PATH",
    "SKIP_BUILD_RPATH",
    "BUILD_WITH_INSTALL_RPATH",
    "BUILD_RPATH_USE_ORIGIN",
    "INSTALL_NAME_DIR",
    "BUILD_WITH_INSTALL_NAME_DIR",
    "C_VISIBILITY_PRESET",
    "CXX_VISIBILITY_PRESET",
    "VISIBILITY_INLINES_HIDDEN",
    "INTERPROCEDURAL_OPTIMIZATION",
    "EXPORT_COMPILE_COMMANDS",
    "OSX_ARCHITECTURES",
    "OSX_DEPLOYMENT_TARGET",
    "OSX_SYSROOT",
    "UNITY_BUILD",
    "UNITY_BUILD_BATCH_SIZE",
    "UNITY_BUILD_UNIQUE_ID",
    "CXX_SCAN_FOR_MODULES",
    "LINK_LIBRARIES_STRATEGY",
];

const BUILD_DEFAULTS: &[(&str, &str)] = &[
    ("SKIP_BUILD_RPATH", "OFF"),
    ("BUILD_WITH_INSTALL_RPATH", "OFF"),
    ("INSTALL_RPATH", ""),
    ("INSTALL_RPATH_USE_LINK_PATH", "OFF"),
    ("EXPORT_COMPILE_COMMANDS", ""),
    ("OSX_ARCHITECTURES", ""),
];

const PER_CONFIG_PROPERTIES: &[&str] = &[
    "ARCHIVE_OUTPUT_DIRECTORY",
    "LIBRARY_OUTPUT_DIRECTORY",
    "RUNTIME_OUTPUT_DIRECTORY",
    "PDB_OUTPUT_DIRECTORY",
    "COMPILE_PDB_OUTPUT_DIRECTORY",
    "INTERPROCEDURAL_OPTIMIZATION",
    "OSX_ARCHITECTURES",
];

pub(super) fn initialize(cx: &mut Cx<'_>, target: &mut Target) {
    let builds = !target.is_imported() && target.kind != TargetKind::InterfaceLibrary;
    if builds {
        for &(property, value) in BUILD_DEFAULTS {
            target.props.set(property.into(), value.into());
        }
        if matches!(
            target.kind,
            TargetKind::SharedLibrary | TargetKind::ModuleLibrary
        ) {
            target
                .props
                .set("POSITION_INDEPENDENT_CODE".into(), "True".into());
        }
        for property in BUILD_PROPERTIES {
            from_cmake(cx, target, property);
        }
        if target.kind == TargetKind::Executable {
            for property in ["WIN32_EXECUTABLE", "MACOSX_BUNDLE"] {
                from_cmake(cx, target, property);
            }
        }
        inherit_directory_usage(cx, target);
    }
    if target.kind != TargetKind::InterfaceLibrary {
        initialize_configurations(cx, target);
    }
    for property in ["FOLDER", "VERIFY_INTERFACE_HEADER_SETS"] {
        from_cmake(cx, target, property);
    }
    if target.is_imported() || cx.project().dir(target.dir).system {
        target.props.set("SYSTEM".into(), "ON".into());
    }
    let definitions: Vec<_> = cx
        .global()
        .defined_props
        .iter()
        .filter(|entry| entry.scope == b"TARGET")
        .filter_map(|entry| Some((entry.name.clone(), entry.initialize_from_variable.clone()?)))
        .collect();
    for (property, variable) in definitions {
        if let Some(value) = cx.var(&variable) {
            target.props.set(property, value);
        }
    }
}

fn from_cmake(cx: &mut Cx<'_>, target: &mut Target, property: &str) {
    if let Some(value) = cx.var(format!("CMAKE_{property}").as_bytes()) {
        target.props.set(property.into(), value);
    }
}

fn initialize_configurations(cx: &mut Cx<'_>, target: &mut Target) {
    let generator = cx
        .project()
        .generator
        .or_else(|| GeneratorKind::from_name(&cx.var_or_empty(b"CMAKE_GENERATOR")));
    let configs = if generator.is_some_and(GeneratorKind::multi_config) {
        list::split(
            &cx.var_or_empty(b"CMAKE_CONFIGURATION_TYPES"),
            list::Empty::Drop,
        )
    } else {
        let config = cx.var_or_empty(b"CMAKE_BUILD_TYPE");
        if config.is_empty() {
            Vec::new()
        } else {
            vec![config]
        }
    };
    for mut config in configs {
        config.make_ascii_uppercase();
        let postfix: BString = [config.as_slice(), b"_POSTFIX"].concat().into();
        if target.kind != TargetKind::Executable
            && let Some(value) = cx.var(&[b"CMAKE_", postfix.as_slice()].concat())
        {
            target.props.set(postfix, value);
        }
        for property in PER_CONFIG_PROPERTIES {
            let name: BString = [property.as_bytes(), b"_", config.as_slice()]
                .concat()
                .into();
            if let Some(value) = cx.var(&[b"CMAKE_", name.as_slice()].concat()) {
                target.props.set(name, value);
            }
        }
    }
}

fn inherit_directory_usage(cx: &mut Cx<'_>, target: &mut Target) {
    let dir = cx.project().dir(target.dir);
    for field in UsageField::ALL {
        if field.on_directory()
            && let Some(entries) = dir.usage.fields.get(&field)
        {
            target.usage.fields.insert(field, entries.clone());
        }
    }
    if let Some(libraries) = dir.props.get(b"LINK_LIBRARIES") {
        let items = list::split(libraries, list::Empty::Drop)
            .into_iter()
            .map(|value| LinkItem {
                text: BT::new(value, target.backtrace.clone()),
                lookup_dir: target.dir,
            });
        target
            .usage
            .link_libraries
            .get_or_insert_default()
            .extend(items);
    }
}
