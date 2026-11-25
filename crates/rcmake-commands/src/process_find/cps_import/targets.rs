use std::collections::BTreeSet;

use rcmake_base::{BString, ByteSlice as _, list};
use rcmake_cps::{CpsComponentType, CpsPackage, CpsUsage};
use rcmake_eval::{CmdError, Cx};
use rcmake_model::{
    BT, GeneratorKind, ImportedVisibility, LinkItem, Target, TargetKind, TargetOrigin, UsageField,
};

use super::text;

const SOURCE: &[u8] = b"_RCMAKE_CPS_SOURCE";

pub(in crate::process_find) fn target_kind(ty: &CpsComponentType) -> Option<TargetKind> {
    Some(match ty {
        CpsComponentType::Archive => TargetKind::StaticLibrary,
        CpsComponentType::Dylib => TargetKind::SharedLibrary,
        CpsComponentType::Module => TargetKind::ModuleLibrary,
        CpsComponentType::Executable => TargetKind::Executable,
        CpsComponentType::Interface | CpsComponentType::Symbolic => TargetKind::InterfaceLibrary,
        CpsComponentType::Jar | CpsComponentType::Unknown(_) => return None,
    })
}

pub(in crate::process_find) fn import(
    cx: &mut Cx<'_>,
    package: &CpsPackage,
    source: &BString,
) -> Result<(), CmdError> {
    let dir = cx.current_dir_id();
    let backtrace = cx.backtrace();
    let policies = *cx.policies_mut();
    let configuration_maps = configuration_maps(cx);
    let mut targets = Vec::new();
    let new_target = |name, kind| {
        let mut target = Target::new(
            name,
            kind,
            TargetOrigin::Imported(ImportedVisibility::Local),
            dir,
            backtrace.clone(),
            policies,
        );
        target.props.set("SYSTEM".into(), "ON".into());
        target.props.set(SOURCE.into(), source.clone());
        for (property, value) in &configuration_maps {
            target.props.set(property.clone(), value.clone());
        }
        target
    };
    for (component_name, component) in &package.components {
        let Some(kind) = target_kind(&component.ty) else {
            continue;
        };
        let mut target = new_target(text(&[&package.name, b"::", component_name]), kind);
        if let Some(license) = component
            .license
            .as_ref()
            .or(package.default_license.as_ref())
            .or(package.license.as_ref())
            .filter(|license| !license.is_empty())
        {
            target.props.set("SPDX_LICENSE".into(), license.clone());
        }
        if component.ty == CpsComponentType::Symbolic {
            target.props.set("SYMBOLIC".into(), "TRUE".into());
        }
        apply_usage(&mut target, &package.name, &component.usage, b"");
        let mut seen = BTreeSet::new();
        let configurations: Vec<_> = package
            .configurations
            .iter()
            .filter_map(|default| {
                component
                    .configurations
                    .iter()
                    .find(|(name, _)| name.eq_ignore_ascii_case(default))
            })
            .chain(&component.configurations)
            .filter(|(name, _)| seen.insert(name.to_ascii_uppercase()))
            .collect();
        if !configurations.is_empty() {
            target.props.set(
                "IMPORTED_CONFIGURATIONS".into(),
                list::join(
                    configurations
                        .iter()
                        .map(|(name, _)| name.to_ascii_uppercase()),
                ),
            );
            for (name, usage) in configurations {
                apply_usage(&mut target, &package.name, usage, name);
            }
        }
        targets.push(target);
    }
    if !package.default_components.is_empty() {
        let mut target = new_target(package.name.clone(), TargetKind::InterfaceLibrary);
        if let Some(license) = &package.license {
            target.props.set("SPDX_LICENSE".into(), license.clone());
        }
        for component in &package.default_components {
            add_link(&mut target, text(&[&package.name, b"::", component]));
        }
        targets.push(target);
    }

    let project = cx.project();
    let mut new_targets = Vec::new();
    for target in targets {
        if let Some(existing) = project
            .find_target(dir, &target.name)
            .and_then(|id| project.target(id))
        {
            if existing.props.get(SOURCE) != Some(source) {
                return Err(CmdError::Status(format!(
                    "cannot create target {:?} because a target with the same name already exists.",
                    target.name
                ).into()));
            }
        } else {
            new_targets.push(target);
        }
    }
    for target in new_targets {
        project
            .add_target(target)
            .map_err(|error| CmdError::Status(error.to_string().into()))?;
    }
    Ok(())
}

fn configuration_maps(cx: &mut Cx<'_>) -> Vec<(BString, BString)> {
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
        vec![cx.var_or_empty(b"CMAKE_BUILD_TYPE")]
    };
    configs
        .into_iter()
        .filter(|config| !config.is_empty())
        .filter_map(|config| {
            let property = text(&[b"MAP_IMPORTED_CONFIG_", &config.to_ascii_uppercase()]);
            let value = cx.var(&text(&[b"CMAKE_", &property]))?;
            Some((property, value))
        })
        .collect()
}

fn apply_usage(target: &mut Target, package: &[u8], usage: &CpsUsage, config: &[u8]) {
    let suffix = if config.is_empty() {
        BString::default()
    } else {
        text(&[b"_", &config.to_ascii_uppercase()])
    };
    for (prop, value) in [
        (b"IMPORTED_LOCATION".as_slice(), &usage.location),
        (b"IMPORTED_IMPLIB", &usage.link_location),
        (b"IMPORTED_SONAME", &usage.link_name),
    ] {
        if let Some(value) = value {
            target.props.set(text(&[prop, &suffix]), value.clone());
        }
    }
    if !usage.link_languages.is_empty() {
        target.props.set(
            text(&[b"IMPORTED_LINK_INTERFACE_LANGUAGES", &suffix]),
            list::join(
                usage
                    .link_languages
                    .iter()
                    .filter_map(|name| language(name)),
            ),
        );
    }
    if !usage.dyld_requires.is_empty() {
        target.props.set(
            text(&[b"IMPORTED_LINK_DEPENDENT_LIBRARIES", &suffix]),
            list::join(
                usage
                    .dyld_requires
                    .iter()
                    .map(|name| reference(package, name)),
            ),
        );
    }

    for (field, entries) in [
        (UsageField::InterfaceLinkOptions, &usage.link_flags),
        (
            UsageField::InterfaceLinkDirectories,
            &usage.link_directories,
        ),
    ] {
        for entry in entries {
            add_usage(target, field, entry.clone(), config, b"*");
        }
    }
    for (field, by_language) in [
        (UsageField::InterfaceIncludeDirectories, &usage.includes),
        (
            UsageField::InterfaceSystemIncludeDirectories,
            &usage.includes,
        ),
        (UsageField::InterfaceCompileOptions, &usage.compile_flags),
    ] {
        for (lang, entries) in by_language {
            for entry in entries {
                add_usage(target, field, entry.clone(), config, lang);
            }
        }
    }
    for (lang, definitions) in &usage.definitions {
        for (key, value) in definitions {
            let definition = value
                .as_ref()
                .map_or_else(|| key.clone(), |value| text(&[key, b"=", value]));
            add_usage(
                target,
                UsageField::InterfaceCompileDefinitions,
                definition,
                config,
                lang,
            );
        }
    }
    for entry in &usage.link_libraries {
        add_link(target, condition(entry.clone(), b"CONFIG", config));
    }
    for (entries, scope) in [
        (&usage.requires, b"".as_slice()),
        (&usage.link_requires, b"LINK_ONLY"),
        (&usage.compile_requires, b"COMPILE_ONLY"),
    ] {
        for entry in entries {
            let name = reference(package, entry);
            let link = if scope.is_empty() {
                name
            } else {
                text(&[b"$<", scope, b":", &name, b">"])
            };
            add_link(target, condition(link, b"CONFIG", config));
        }
    }
}

fn reference(package: &[u8], entry: &[u8]) -> BString {
    if let Some((dependency, component)) = entry.split_once_str(b":") {
        let dependency = if dependency.is_empty() {
            package
        } else {
            dependency
        };
        text(&[dependency, b"::", component])
    } else {
        entry.into()
    }
}

fn language(name: &[u8]) -> Option<&'static [u8]> {
    Some(match name {
        b"c" => b"C",
        b"cpp" => b"CXX",
        b"fortran" => b"Fortran",
        b"objc" => b"OBJC",
        b"objcxx" => b"OBJCXX",
        b"cuda" => b"CUDA",
        b"hip" => b"HIP",
        b"swift" => b"Swift",
        b"asm" => b"ASM",
        _ => return None,
    })
}

fn condition(entry: BString, key: &[u8], value: &[u8]) -> BString {
    if value.is_empty() {
        entry
    } else {
        text(&[b"$<$<", key, b":", value, b">:", &entry, b">"])
    }
}

fn add_usage(target: &mut Target, field: UsageField, entry: BString, config: &[u8], lang: &[u8]) {
    let value = if lang == b"*" {
        entry
    } else if let Some(lang) = language(lang) {
        condition(entry, b"COMPILE_LANGUAGE", lang)
    } else {
        return;
    };
    let value = condition(value, b"CONFIG", config);
    target
        .usage
        .push(field, BT::new(value, target.backtrace.clone()));
}

fn add_link(target: &mut Target, value: BString) {
    target
        .usage
        .interface_link_libraries
        .get_or_insert_with(Vec::new)
        .push(LinkItem {
            scope: None,
            text: BT::new(value, target.backtrace.clone()),
            lookup_dir: target.dir,
        });
}
