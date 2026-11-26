use std::collections::{BTreeMap, BTreeSet};

use rcmake_base::{BString, ByteSlice as _, list};
use rcmake_cps::{CpsComponentType, CpsPackage, CpsUsage};
use rcmake_diag::MessageKind;
use rcmake_eval::{CmdError, Cx};
use rcmake_model::{
    BT, GeneratorKind, ImportedVisibility, LinkItem, Target, TargetKind, TargetOrigin, UsageField,
};

use super::{PackageDiagnostic, cat};

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
    let mut warnings = Vec::new();
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
        let mut target = new_target(cat(&[&package.name, b"::", component_name]), kind);
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
        apply_usage(
            &mut target,
            &package.name,
            &component.usage,
            b"",
            &mut warnings,
        );
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
                apply_usage(&mut target, &package.name, usage, name, &mut warnings);
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
            add_link(&mut target, cat(&[&package.name, b"::", component]));
        }
        targets.push(target);
    }

    for body in warnings {
        PackageDiagnostic {
            body,
            files: vec![source.clone()],
        }
        .emit_as(cx, MessageKind::Warning);
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
            let property = cat(&[b"MAP_IMPORTED_CONFIG_", &config.to_ascii_uppercase()]);
            let value = cx.var(&cat(&[b"CMAKE_", &property]))?;
            Some((property, value))
        })
        .collect()
}

/// Apply one CPS usage block to `target`. The property writes below keep the order that
/// CMake 4.5.1 uses.
fn apply_usage(
    target: &mut Target,
    package: &[u8],
    usage: &CpsUsage,
    config: &[u8],
    warnings: &mut Vec<BString>,
) {
    let suffix = if config.is_empty() {
        BString::default()
    } else {
        cat(&[b"_", &config.to_ascii_uppercase()])
    };
    for feature in &usage.compile_features {
        let standard = match feature.as_slice() {
            [b'c' | b'C', a, b] if a.is_ascii_digit() && b.is_ascii_digit() => {
                Some(cat(&[b"c_std_", &[*a, *b]]))
            }
            [b'c' | b'C', b'+', b'+', a, b] if a.is_ascii_digit() && b.is_ascii_digit() => {
                Some(cat(&[b"cxx_std_", &[*a, *b]]))
            }
            _ => None,
        };
        if let Some(standard) = standard {
            add_usage(
                target,
                UsageField::InterfaceCompileFeatures,
                standard,
                config,
            );
        } else if feature.eq_ignore_ascii_case(b"threads") {
            add_link(
                target,
                condition("Threads::Threads".into(), b"CONFIG", config),
            );
        }
    }
    for feature in &usage.link_features {
        if feature.eq_ignore_ascii_case(b"thread") {
            add_link(
                target,
                condition("Threads::Threads".into(), b"CONFIG", config),
            );
        }
    }
    add_language_lists(
        target,
        UsageField::InterfaceCompileOptions,
        b"compile_flags",
        &usage.compile_flags,
        config,
        warnings,
    );
    for entry in &usage.link_flags {
        add_usage(
            target,
            UsageField::InterfaceLinkOptions,
            entry.clone(),
            config,
        );
    }
    add_definitions(target, &usage.definitions, config, warnings);
    add_language_lists(
        target,
        UsageField::InterfaceIncludeDirectories,
        b"includes",
        &usage.includes,
        config,
        warnings,
    );
    for (prop, value) in [
        (b"IMPORTED_LOCATION".as_slice(), &usage.location),
        (b"IMPORTED_IMPLIB", &usage.link_location),
        (b"IMPORTED_SONAME", &usage.link_name),
    ] {
        if let Some(value) = value {
            target.props.set(cat(&[prop, &suffix]), value.clone());
        }
    }
    // Link languages drop `*` and unknown names without a warning.
    for name in &usage.link_languages {
        if let Some(lang) = language(name, false) {
            target.props.append(
                "INTERFACE_LINK_LANGUAGES".into(),
                &condition(lang.into(), b"CONFIG", config),
            );
        }
    }
    for (entries, scope) in [
        (&usage.requires, b"".as_slice()),
        (&usage.compile_requires, b"COMPILE_ONLY"),
        (&usage.link_requires, b"LINK_ONLY"),
    ] {
        for entry in entries {
            let name = reference(package, entry);
            let link = if scope.is_empty() {
                name
            } else {
                cat(&[b"$<", scope, b":", &name, b">"])
            };
            add_link(target, condition(link, b"CONFIG", config));
        }
    }
    if !usage.dyld_requires.is_empty() {
        target.props.set(
            cat(&[b"IMPORTED_LINK_DEPENDENT_LIBRARIES", &suffix]),
            list::join(
                usage
                    .dyld_requires
                    .iter()
                    .map(|name| reference(package, name)),
            ),
        );
    }
    for entry in &usage.link_libraries {
        add_link(target, condition(entry.clone(), b"CONFIG", config));
    }
}

/// Add each list. A list for one language applies only to that language.
fn add_language_lists(
    target: &mut Target,
    field: UsageField,
    key: &[u8],
    by_language: &BTreeMap<BString, Vec<BString>>,
    config: &[u8],
    warnings: &mut Vec<BString>,
) {
    for (name, entries) in by_language {
        let Some(lang) = language(name, true) else {
            warnings.push(unknown_language(name, key, &target.name));
            continue;
        };
        for entry in entries {
            let value = if lang == b"*" {
                entry.clone()
            } else {
                condition(entry.clone(), b"COMPILE_LANGUAGE", lang)
            };
            add_usage(target, field, value, config);
        }
    }
}

/// Definition values of one name by CMake language. `*` sorts before every language name.
type LanguageValues<'a> = BTreeMap<&'static [u8], Option<&'a BString>>;

/// Add the definitions in name order. When a language overrides a `*` definition,
/// the `*` definition applies only to the other languages.
fn add_definitions(
    target: &mut Target,
    definitions: &BTreeMap<BString, BTreeMap<BString, Option<BString>>>,
    config: &[u8],
    warnings: &mut Vec<BString>,
) {
    let mut by_name = BTreeMap::<&[u8], LanguageValues<'_>>::new();
    for (name, values) in definitions {
        let Some(lang) = language(name, true) else {
            warnings.push(unknown_language(name, b"definitions", &target.name));
            continue;
        };
        for (key, value) in values {
            by_name
                .entry(key.as_slice())
                .or_default()
                .entry(lang)
                .or_insert(value.as_ref());
        }
    }
    let field = UsageField::InterfaceCompileDefinitions;
    for (key, languages) in by_name {
        if let Some(&value) = languages.get(b"*".as_slice()) {
            let all = definition(key, value);
            if languages.len() == 1 {
                add_usage(target, field, all, config);
                continue;
            }
            let excluded: Vec<&[u8]> = languages
                .keys()
                .copied()
                .filter(|lang| *lang != b"*".as_slice())
                .collect();
            let excluded = excluded.join(b",".as_slice());
            let value = cat(&[
                b"$<$<NOT:$<COMPILE_LANGUAGE:",
                &excluded,
                b">>:",
                &all,
                b">",
            ]);
            add_usage(target, field, value, config);
        }
        for (lang, value) in languages {
            if lang != b"*".as_slice() {
                let value = condition(definition(key, value), b"COMPILE_LANGUAGE", lang);
                add_usage(target, field, value, config);
            }
        }
    }
}

/// Format one definition as `NAME=value`, or as `NAME` without a value.
fn definition(key: &[u8], value: Option<&BString>) -> BString {
    value.map_or_else(|| key.into(), |value| cat(&[key, b"=", value]))
}

fn unknown_language(name: &[u8], key: &[u8], target: &[u8]) -> BString {
    cat(&[
        b"ignoring unknown language \"",
        name,
        b"\" in ",
        key,
        b" for ",
        target,
    ])
}

fn reference(package: &[u8], entry: &[u8]) -> BString {
    if let Some((dependency, component)) = entry.split_once_str(b":") {
        let dependency = if dependency.is_empty() {
            package
        } else {
            dependency
        };
        cat(&[dependency, b"::", component])
    } else {
        entry.into()
    }
}

/// Map a CPS language name to a CMake language name. Case is ignored.
/// CMake 4.5.1 maps `swift` to the lowercase name `swift`.
fn language(name: &[u8], glob: bool) -> Option<&'static [u8]> {
    if glob && name == b"*" {
        return Some(b"*");
    }
    Some(match name.to_ascii_lowercase().as_slice() {
        b"c" => b"C",
        b"c++" | b"cpp" | b"cxx" => b"CXX",
        b"objc" => b"OBJC",
        b"objc++" | b"objcpp" | b"objcxx" => b"OBJCXX",
        b"swift" => b"swift",
        b"hip" => b"HIP",
        b"cuda" => b"CUDA",
        b"ispc" => b"ISPC",
        b"c#" | b"csharp" => b"CSharp",
        b"fortran" => b"Fortran",
        _ => return None,
    })
}

fn condition(entry: BString, key: &[u8], value: &[u8]) -> BString {
    if value.is_empty() {
        entry
    } else {
        cat(&[b"$<$<", key, b":", value, b">:", &entry, b">"])
    }
}

fn add_usage(target: &mut Target, field: UsageField, entry: BString, config: &[u8]) {
    let value = condition(entry, b"CONFIG", config);
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
            opens_lookup_scope: false,
            closes_lookup_scope: false,
            scope: None,
            text: BT::new(value, target.backtrace.clone()),
            lookup_dir: target.dir,
        });
}
