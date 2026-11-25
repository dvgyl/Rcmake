//! Target property defaults captured at creation.

use rcmake_base::{BString, ByteSlice, list};
use rcmake_eval::Cx;
use rcmake_model::{BT, GeneratorKind, LinkItem, Target, TargetKind, UsageField};

#[path = "generated_init.rs"]
mod generated_init;

use generated_init::TARGET_PROP_INITS;

const EXTRA_TARGET_PROP_INITS: &[(&str, &str)] = &[
    (
        "OSX_ARCHITECTURES_<CONFIG>",
        "CMAKE_OSX_ARCHITECTURES_<CONFIG>",
    ),
    ("LINKER_TYPE", "CMAKE_LINKER_TYPE"),
    ("OSX_DEPLOYMENT_TARGET", "CMAKE_OSX_DEPLOYMENT_TARGET"),
    ("OSX_SYSROOT", "CMAKE_OSX_SYSROOT"),
    ("UNITY_BUILD_UNIQUE_ID", "CMAKE_UNITY_BUILD_UNIQUE_ID"),
];

const BUILD_DEFAULTS: &[(&str, &str)] = &[
    ("SKIP_BUILD_RPATH", "OFF"),
    ("BUILD_WITH_INSTALL_RPATH", "OFF"),
    ("INSTALL_RPATH", ""),
    ("INSTALL_RPATH_USE_LINK_PATH", "OFF"),
    ("EXPORT_COMPILE_COMMANDS", ""),
    ("OSX_ARCHITECTURES", ""),
];

pub(super) fn initialize(cx: &mut Cx<'_>, target: &mut Target) {
    super::properties::initialize_file_set_properties(target);
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
        inherit_directory_usage(cx, target);
    }
    initialize_from_variables(cx, target, builds);
    if target.is_imported()
        || cx
            .project()
            .dir(target.dir)
            .props
            .get(b"SYSTEM")
            .is_some_and(|value| rcmake_base::truth::is_on(value))
    {
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

fn initialize_from_variables(cx: &mut Cx<'_>, target: &mut Target, builds: bool) {
    let generator = cx
        .project()
        .generator
        .or_else(|| GeneratorKind::from_name(&cx.var_or_empty(b"CMAKE_GENERATOR")));
    if builds && matches!(generator, Some(GeneratorKind::VisualStudio(_))) {
        for entry in list::split(&cx.var_or_empty(b"CMAKE_VS_GLOBALS"), list::Empty::Drop) {
            if let Some((name, value)) = entry.split_once_str(b"=")
                && !name.is_empty()
            {
                let key = [b"VS_GLOBAL_", name].concat();
                target.props.set(key.into(), value.into());
            }
        }
    }
    let xcode = generator == Some(GeneratorKind::Xcode);
    let multi_config = generator.is_some_and(GeneratorKind::multi_config);
    let mut configs = active_configurations(cx, multi_config);
    for config in &mut configs {
        config.make_ascii_uppercase();
    }
    for &(property, variable) in TARGET_PROP_INITS.iter().chain(EXTRA_TARGET_PROP_INITS) {
        // The compiler flag resolver uses these defaults when the property is unset.
        if variable.ends_with("_EXTENSIONS_DEFAULT") {
            continue;
        }
        if !applies_to_target(target, property, variable, builds, xcode) {
            continue;
        }
        if property.contains("<CONFIG>") {
            for config in &configs {
                let variable = substitute_config(variable, config);
                if let Some(value) = cx.var(&variable) {
                    target.props.set(substitute_config(property, config), value);
                }
            }
        } else if let Some(value) = cx.var(variable.as_bytes()) {
            target.props.set(property.into(), value);
        }
    }
}

fn applies_to_target(
    target: &Target,
    property: &str,
    variable: &str,
    builds: bool,
    xcode: bool,
) -> bool {
    use TargetKind::{Executable, InterfaceLibrary, SharedLibrary, StaticLibrary};

    let kind = target.kind;
    match property {
        "FOLDER"
        | "VERIFY_INTERFACE_HEADER_SETS"
        | "VERIFY_PRIVATE_HEADER_SETS"
        | "EXPORT_FIND_PACKAGE_NAME"
        | "MAP_IMPORTED_CONFIG_<CONFIG>" => true,
        "ANDROID_GUI"
        | "CROSSCOMPILING_EMULATOR"
        | "DEBUGGER_WORKING_DIRECTORY"
        | "TEST_LAUNCHER"
        | "VS_DEBUGGER_COMMAND"
        | "VS_DEBUGGER_COMMAND_ARGUMENTS"
        | "VS_DEBUGGER_ENVIRONMENT"
        | "VS_DEBUGGER_WORKING_DIRECTORY" => kind == Executable,
        "MACOSX_BUNDLE" | "WIN32_EXECUTABLE" => builds && kind == Executable,
        "AIX_EXPORT_ALL_SYMBOLS" | "WINDOWS_EXPORT_ALL_SYMBOLS" => {
            matches!(kind, Executable | SharedLibrary)
        }
        "AIX_SHARED_LIBRARY_ARCHIVE" => builds && kind == SharedLibrary,
        "ENABLE_EXPORTS" if variable == "CMAKE_SHARED_LIBRARY_ENABLE_EXPORTS" => {
            kind == SharedLibrary
        }
        "ENABLE_EXPORTS" => kind == Executable,
        "DOTNET_SDK"
        | "LINK_LIBRARIES_ONLY_TARGETS"
        | "LINK_LIBRARIES_STRATEGY"
        | "VS_USE_DEBUG_LIBRARIES" => !target.is_imported(),
        "DOTNET_TARGET_FRAMEWORK" | "DOTNET_TARGET_FRAMEWORK_VERSION" => kind != InterfaceLibrary,
        "<CONFIG>_POSTFIX" => !matches!(kind, Executable | InterfaceLibrary),
        "FRAMEWORK_MULTI_CONFIG_POSTFIX_<CONFIG>" => matches!(kind, SharedLibrary | StaticLibrary),
        "XCODE_GENERATE_SCHEME" => xcode,
        _ if property.starts_with("XCODE_") => xcode && builds,
        _ if property.ends_with("_<CONFIG>") => kind != InterfaceLibrary,
        _ => builds,
    }
}

fn active_configurations(cx: &mut Cx<'_>, multi_config: bool) -> Vec<BString> {
    if multi_config {
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
    }
}

fn substitute_config(template: &str, config: &[u8]) -> BString {
    match template.split_once("<CONFIG>") {
        Some((prefix, suffix)) => [prefix.as_bytes(), config, suffix.as_bytes()]
            .concat()
            .into(),
        None => template.into(),
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
                scope: None,
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
