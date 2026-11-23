//! Common Package Specification files for package information exports.

use std::collections::{BTreeMap, BTreeSet};

use rcmake_base::{BString, ByteSlice, list, path_lex::Lexical};
use rcmake_cps::{
    CpsComponent, CpsComponentType, CpsConfigSupplement, CpsError, CpsFileSet, CpsPackage,
    CpsTargetFileSet, CpsUsage, naming,
};
use rcmake_genex::{ConfigId, EvalContext, PreprocessMode, TargetHandle, UsageTraversal};
use rcmake_model::{
    DirId, FileSetKind, InstallKind, InstallRule, PackageInfoSpec, ProjectModel, Target, TargetId,
    TargetKind, UsageScope,
};
use rcmake_resolve::{GenerationResult, InstallActionKind, ResolvedTarget, usage::UsageResolver};

use crate::{
    GenError, genex,
    install_script::file_sets::{options_for_set, set_files},
};

use super::provenance::{
    author_warning, component, exports, generation_error, imported_package, imported_version,
    package_variable, unknown_import,
};

const PREFIX: &[u8] = b"@prefix@";

/// Render a CPS file set. Use install paths if the model has a matching install rule.
///
/// # Errors
///
/// Return an error for an invalid package, export set, or usage requirement.
pub fn render_package_info(
    model: &ProjectModel,
    result: &GenerationResult,
    spec: &PackageInfoSpec,
) -> Result<CpsFileSet, GenError> {
    let rule = model.installs.iter().find(
        |rule| matches!(&rule.kind, InstallKind::PackageInfo(install) if install.as_ref() == spec),
    );
    render_for_tree(model, result, spec, rule, &mut BTreeSet::new())
}

pub(crate) fn install_destination(
    model: &ProjectModel,
    rule: &InstallRule,
    spec: &PackageInfoSpec,
) -> Result<BString, GenError> {
    if let Some(destination) = &rule.options.destination {
        return Ok(destination.clone());
    }
    let lib = model
        .definition(rule.dir, b"CMAKE_INSTALL_LIBDIR")
        .map_or(b"lib".as_slice(), |value| value.as_slice());
    let windows = model
        .definition(rule.dir, b"CMAKE_SYSTEM_NAME")
        .is_some_and(|name| name == b"Windows")
        || cfg!(windows);
    naming::install_directory(&spec.package, lib, windows, spec.lower_case_file).map_err(cps_error)
}

pub(crate) fn cps_error(error: CpsError) -> GenError {
    GenError::Invalid(error.to_string().into())
}

pub(crate) fn render_for_tree(
    model: &ProjectModel,
    result: &GenerationResult,
    spec: &PackageInfoSpec,
    install: Option<&InstallRule>,
    warnings: &mut BTreeSet<BString>,
) -> Result<CpsFileSet, GenError> {
    let set = model.exports.get(&spec.export).ok_or_else(|| {
        GenError::Invalid(format!("Unknown export set \"{}\".", spec.export).into())
    })?;
    let appendix = spec.appendix.as_deref().map(Vec::as_slice);
    let file_name = naming::package_file_name(&spec.package, appendix, spec.lower_case_file)
        .map_err(cps_error)?;
    let mut package = CpsPackage {
        name: spec.package.clone(),
        cps_version: rcmake_cps::CPS_VERSION.into(),
        version: spec.version.clone(),
        compat_version: spec.compat_version.clone(),
        version_schema: spec.version_schema.clone(),
        license: spec.license.clone(),
        default_license: spec.default_license.clone(),
        description: spec.description.clone(),
        website: spec.homepage_url.clone(),
        default_components: spec.default_targets.clone(),
        configurations: spec.default_configurations.clone(),
        ..CpsPackage::default()
    };
    let prefix = install
        .and_then(|rule| model.definition(rule.dir, b"CMAKE_INSTALL_PREFIX"))
        .map_or(b"/usr/local".as_slice(), |value| value.as_slice());
    if let Some(rule) = install {
        let destination = install_destination(model, rule, spec)?;
        let path = install_path(&destination, prefix);
        if !path.starts_with(PREFIX) || path.starts_with(b"@prefix@/../") || path == b"@prefix@/.."
        {
            return Err(GenError::Invalid(
                format!(r#"The CPS destination "{destination}" is outside the install prefix."#)
                    .into(),
            ));
        }
        package.cps_path = Some(path);
    }
    let mut usage = UsageResolver::new(model);
    let mut supplements = BTreeMap::<BString, CpsConfigSupplement>::new();
    let configs: Vec<_> = configurations(result)
        .filter(|config| {
            install.is_none_or(|rule| matches_configuration(config, &rule.options.configurations))
        })
        .collect();
    let mode = if install.is_some() {
        PreprocessMode::Install
    } else {
        PreprocessMode::Build
    };
    let dependencies = Dependencies {
        model,
        result,
        spec,
        mode,
    };
    for &id in &set.targets {
        let target = model
            .target(id)
            .ok_or_else(|| GenError::Invalid("Unknown export target.".into()))?;
        let component_name = export_name(target);
        let ty = match target.kind {
            TargetKind::StaticLibrary => CpsComponentType::Archive,
            TargetKind::SharedLibrary => CpsComponentType::Dylib,
            TargetKind::ModuleLibrary => CpsComponentType::Module,
            TargetKind::Executable => CpsComponentType::Executable,
            TargetKind::ObjectLibrary if install.is_none() => {
                CpsComponentType::Unknown("unknown".into())
            }
            TargetKind::InterfaceLibrary | TargetKind::ObjectLibrary => {
                if target
                    .props
                    .get(b"SYMBOLIC")
                    .is_some_and(|p| rcmake_base::truth::is_on(p))
                {
                    CpsComponentType::Symbolic
                } else {
                    CpsComponentType::Interface
                }
            }
            _ => {
                return Err(GenError::Invalid(
                    format!("Cannot export target \"{}\" as CPS.", target.name).into(),
                ));
            }
        };
        let mut common = requirements(&dependencies, &mut usage, id, target, &mut package)?;
        let mut by_config = BTreeMap::new();
        let mut sets_by_config = Vec::new();
        for config in &configs {
            let facts = result
                .targets
                .iter()
                .find(|facts| facts.id == id && facts.config == *config);
            let mut config_usage = CpsUsage::default();
            if let Some(facts) = facts {
                config_usage = artifact_usage(model, result, facts, install.is_some(), prefix);
                if target.kind == TargetKind::SharedLibrary {
                    runtime_dependencies(
                        &dependencies,
                        &mut usage,
                        facts,
                        target,
                        &mut package,
                        &common,
                        &mut config_usage,
                    )?;
                }
            }
            let sets = component_file_sets(
                model,
                &mut usage,
                target,
                &set.install_rules,
                config,
                install,
                prefix,
                warnings,
                &mut config_usage,
            )?;
            sets_by_config.push((config.clone(), sets));
            if config_usage != CpsUsage::default() {
                by_config.insert(config_name(&config.0), std::mem::take(&mut config_usage));
            }
        }
        if let Some((_, first)) = sets_by_config.first()
            && sets_by_config.iter().all(|(_, sets)| sets == first)
        {
            common.file_sets = first.clone();
            add_set_includes(&mut common);
        } else {
            for (config, file_sets) in sets_by_config {
                let usage = by_config.entry(config_name(&config.0)).or_default();
                usage.file_sets = file_sets;
                add_set_includes(usage);
            }
        }
        if install.is_some() {
            for (config, usage) in std::mem::take(&mut by_config) {
                supplements
                    .entry(config.clone())
                    .or_insert_with(|| CpsConfigSupplement {
                        name: spec.package.clone(),
                        configuration: config,
                        ..CpsConfigSupplement::default()
                    })
                    .components
                    .insert(component_name.clone(), usage);
            }
        }
        package.components.insert(
            component_name,
            CpsComponent {
                ty,
                usage: common,
                license: target.props.get(b"SPDX_LICENSE").cloned(),
                configurations: by_config,
            },
        );
    }
    let invalid_defaults = package.default_components.iter()
        .filter(|default| !package.components.contains_key(*default))
        .map(|default| format!(
            r#"Package "{}" specifies DEFAULT_TARGETS "{default}", which is not a target in the export set "{}"."#,
            spec.package, spec.export,
        ).into()).collect::<Vec<_>>();
    if !invalid_defaults.is_empty() {
        return Err(generation_error(invalid_defaults));
    }
    let supplements = supplements
        .into_iter()
        .map(|(config, contents)| {
            naming::supplement_file_name(&spec.package, appendix, &config, spec.lower_case_file)
                .map(|name| (name, contents))
                .map_err(cps_error)
        })
        .collect::<Result<_, _>>()?;
    Ok(CpsFileSet {
        file_name,
        package,
        supplements,
    })
}

pub(crate) fn configurations(result: &GenerationResult) -> impl Iterator<Item = ConfigId> + '_ {
    result
        .configs
        .iter()
        .cloned()
        .chain(result.configs.is_empty().then(ConfigId::default))
}

pub(crate) fn matches_configuration(config: &ConfigId, filter: &[BString]) -> bool {
    filter.is_empty() || filter.iter().any(|name| config.matches(name))
}

pub(crate) fn config_name(config: &[u8]) -> BString {
    if config.is_empty() {
        "noconfig".into()
    } else {
        config.into()
    }
}

pub(crate) fn export_name(target: &Target) -> BString {
    target
        .props
        .get(b"EXPORT_NAME")
        .cloned()
        .unwrap_or_else(|| target.name.clone())
}

fn install_path(path: &[u8], prefix: &[u8]) -> BString {
    let path = Lexical::new(path).normalize();
    let prefix = Lexical::new(prefix).normalize();
    if path == prefix {
        return PREFIX.into();
    }
    let relative = if Lexical::new(&path).absolute() {
        path.strip_prefix(prefix.trim_end_with(|c| c == '/'))
            .and_then(|suffix| suffix.strip_prefix(b"/"))
    } else {
        Some(path.as_slice())
    };
    relative.map_or_else(
        || path.clone(),
        |relative| {
            if relative == b"." {
                PREFIX.into()
            } else {
                Lexical::new(PREFIX).append(relative)
            }
        },
    )
}

fn artifact_usage(
    model: &ProjectModel,
    result: &GenerationResult,
    facts: &ResolvedTarget,
    install: bool,
    prefix: &[u8],
) -> CpsUsage {
    let build = Lexical::new(&model.dir(DirId::ROOT).binary_dir);
    let location = |artifact: &Option<BString>| {
        let artifact = build.append(artifact.as_ref()?);
        if !install {
            return Some(artifact);
        }
        result.install.actions.iter().find_map(|action| {
            let InstallActionKind::Files {
                files,
                file_type,
                rename,
                ..
            } = &action.kind
            else {
                return None;
            };
            if file_type == b"FILE"
                || file_type == b"PROGRAM"
                || !files.contains(&artifact)
                || !matches_configuration(&facts.config, &action.configurations)
            {
                return None;
            }
            let name = rename
                .as_ref()
                .map_or(Lexical::new(&artifact).filename(), |value| value.as_slice());
            Some(install_path(
                &Lexical::new(&action.destination).append(name),
                prefix,
            ))
        })
    };
    let file = location(&facts.artifacts.file);
    let link = location(&facts.artifacts.import_file);
    let mut languages = Vec::new();
    if file.is_some() && facts.metadata.kind == TargetKind::StaticLibrary {
        for group in &facts.compile_groups {
            push_unique(&mut languages, language(&group.language));
        }
    }
    CpsUsage {
        location: file,
        link_location: link,
        link_languages: languages,
        ..CpsUsage::default()
    }
}

fn language(value: &[u8]) -> BString {
    match value {
        b"CXX" => "cpp".into(),
        b"OBJC" => "objc".into(),
        b"OBJCXX" => "objcpp".into(),
        _ => value.to_ascii_lowercase().into(),
    }
}

fn push_unique(items: &mut Vec<BString>, value: BString) {
    if !items.contains(&value) {
        items.push(value);
    }
}

#[allow(clippy::too_many_arguments)]
fn component_file_sets(
    model: &ProjectModel,
    resolver: &mut UsageResolver<'_>,
    target: &Target,
    install_rules: &[usize],
    config: &ConfigId,
    install: Option<&InstallRule>,
    prefix: &[u8],
    warnings: &mut BTreeSet<BString>,
    config_usage: &mut CpsUsage,
) -> Result<Vec<CpsTargetFileSet>, GenError> {
    let mut file_sets = Vec::new();
    for set in target
        .file_sets
        .iter()
        .filter_map(|&id| model.file_set(id))
        .filter(|set| set.scope != UsageScope::Private && set.kind == FileSetKind::Headers)
    {
        let paths = set_files(model, resolver, target, set, config)?;
        let mut roots = BTreeMap::<_, Vec<_>>::new();
        if let Some(install) = install {
            let Some((rule, options)) = install_rules.iter().find_map(|&index| {
                let rule = model.installs.get(index)?;
                let InstallKind::Targets { targets, .. } = &rule.kind else {
                    return None;
                };
                let options = options_for_set(model, index, &set.name)?;
                (targets.contains(&set.target)
                    && matches_configuration(config, &options.configurations))
                .then_some((rule, options))
            }) else {
                continue;
            };
            let default = model
                .definition(rule.dir, b"CMAKE_INSTALL_INCLUDEDIR")
                .map_or(b"include".as_slice(), |value| value.as_slice());
            let context = EvalContext {
                config: config.clone(),
                ..EvalContext::default()
            };
            let destination = options
                .destination
                .as_deref()
                .map_or(default, Vec::as_slice);
            let ast = rcmake_genex::parse(destination)
                .map_err(|error| GenError::Invalid(error.to_string().into()))?;
            let (destination, flags) =
                rcmake_genex::evaluate(&ast, &context, &mut resolver.host(rule.dir))
                    .map_err(|error| GenError::Invalid(error.to_string().into()))?;
            if flags.context_sensitive || flags.head_sensitive {
                warnings.extend(author_warning(model, &install.backtrace, format!(
                    r#"The "{}" target's interface file set "{}" of type "HEADERS" has a context-sensitive destination, which is not supported.  The file set will not be exported."#,
                    target.name, set.name,
                )));
                push_unique(
                    config_usage.includes.entry("*".into()).or_default(),
                    install_path(&destination, prefix),
                );
                continue;
            }
            let files = roots.entry(install_path(&destination, prefix)).or_default();
            for (_, relative) in paths.into_values().flatten() {
                push_unique(files, relative);
            }
        } else {
            roots.extend(paths.into_iter().map(|(root, paths)| {
                (
                    root,
                    paths.into_iter().map(|(_, relative)| relative).collect(),
                )
            }));
        }
        file_sets.extend(roots.into_iter().map(|(root, files)| CpsTargetFileSet {
            name: Some(set.name.clone()),
            ty: "includes".into(),
            root,
            files,
        }));
    }
    Ok(file_sets)
}

fn add_set_includes(usage: &mut CpsUsage) {
    for set in usage.file_sets.iter().filter(|set| set.ty == b"includes") {
        push_unique(
            usage.includes.entry("*".into()).or_default(),
            set.root.clone(),
        );
    }
}

fn requirements(
    dependencies: &Dependencies<'_>,
    resolver: &mut UsageResolver<'_>,
    id: TargetId,
    target: &Target,
    package: &mut CpsPackage,
) -> Result<CpsUsage, GenError> {
    let Dependencies {
        model, spec, mode, ..
    } = *dependencies;
    let handle = TargetHandle(id.index() as u32);
    let context = EvalContext {
        head: Some(handle),
        current: Some(handle),
        ..EvalContext::default()
    };
    let values = |key: &[u8]| -> Result<Vec<BString>, GenError> {
        let property = rcmake_model::UsageField::from_name(key)
            .and_then(|field| target.usage.fields.get(&field))
            .map(|entries| list::join(entries.iter().map(|entry| &entry.value)));
        let raw = property.as_ref().or_else(|| target.props.get(key));
        let Some(raw) = raw else {
            return Ok(Vec::new());
        };
        let value = interface_value(raw, mode, key, &target.name, None)?;
        Ok(list::split(&value, list::Empty::Drop))
    };
    let mut usage = CpsUsage::default();
    for (key, map) in [
        (
            b"INTERFACE_INCLUDE_DIRECTORIES".as_slice(),
            &mut usage.includes,
        ),
        (b"INTERFACE_COMPILE_OPTIONS", &mut usage.compile_flags),
    ] {
        let mut entries = values(key)?;
        if key == b"INTERFACE_INCLUDE_DIRECTORIES" && mode == PreprocessMode::Install {
            for entry in &mut entries {
                if !entry.starts_with(PREFIX) && !Lexical::new(entry).absolute() {
                    *entry = Lexical::new(PREFIX).append(entry);
                }
            }
        }
        if key == b"INTERFACE_INCLUDE_DIRECTORIES" {
            let mut seen = BTreeSet::new();
            entries.retain(|entry| seen.insert(entry.clone()));
        }
        if !entries.is_empty() {
            map.insert("*".into(), entries);
        }
    }
    for definition in values(b"INTERFACE_COMPILE_DEFINITIONS")? {
        let (name, value) = definition
            .split_once_str(b"=")
            .map_or((definition.as_slice(), None), |(key, value)| {
                (key, Some(value.into()))
            });
        usage
            .definitions
            .entry("*".into())
            .or_default()
            .insert(name.strip_prefix(b"-D").unwrap_or(name).into(), value);
    }
    for feature in values(b"INTERFACE_COMPILE_FEATURES")? {
        for (cmake, cps) in [
            (b"cxx_std_".as_slice(), b"c++".as_slice()),
            (b"c_std_", b"c"),
        ] {
            if let Some(version) = feature.strip_prefix(cmake) {
                push_unique(&mut usage.compile_features, [cps, version].concat().into());
            }
        }
    }
    usage.compile_features.sort_unstable();
    usage.link_directories = values(b"INTERFACE_LINK_DIRECTORIES")?;
    usage.link_flags = values(b"INTERFACE_LINK_OPTIONS")?;
    let raw = target
        .usage
        .interface_link_libraries
        .as_ref()
        .map(|items| {
            items
                .iter()
                .map(|item| (item.lookup_dir, item.text.value.clone()))
                .collect::<Vec<_>>()
        })
        .unwrap_or_else(|| {
            target
                .props
                .get(b"INTERFACE_LINK_LIBRARIES")
                .into_iter()
                .map(|value| (target.dir, value.clone()))
                .collect()
        });
    let mut errors = Vec::new();
    for (dir, value) in raw {
        collect_links(
            dependencies,
            target,
            package,
            &mut usage,
            dir,
            &value,
            &mut errors,
        )?;
    }
    if !errors.is_empty() {
        return Err(generation_error(errors));
    }
    if mode == PreprocessMode::Install
        && let Some(set) = model.exports.get(&spec.export)
    {
        for &index in &set.install_rules {
            let Some(rule) = model.installs.get(index) else {
                continue;
            };
            if !matches!(&rule.kind, InstallKind::Targets { targets, .. } if targets.contains(&id))
            {
                continue;
            }
            for path in &rule.options.includes_destination {
                let path = rcmake_genex::rewrite_install_prefix(path, PREFIX);
                for entry in list::split(
                    &genex(resolver, rule.dir, &context, &path, Some(mode))?,
                    list::Empty::Drop,
                ) {
                    let entry = if entry.starts_with(PREFIX) || Lexical::new(&entry).absolute() {
                        entry
                    } else {
                        Lexical::new(PREFIX).append(&entry)
                    };
                    push_unique(usage.includes.entry("*".into()).or_default(), entry);
                }
            }
        }
    }
    Ok(usage)
}

fn interface_value(
    value: &[u8],
    mode: PreprocessMode,
    property: &[u8],
    target: &[u8],
    wrapper_name: Option<&str>,
) -> Result<BString, GenError> {
    let value = rcmake_genex::rewrite_install_prefix(value, PREFIX);
    let ast =
        rcmake_genex::parse(&value).map_err(|error| GenError::Invalid(error.to_string().into()))?;
    let ast = rcmake_genex::strip(&ast, mode);
    let mut result = BString::default();
    for node in ast.nodes {
        match node {
            rcmake_genex::GenexNode::Text(text) => result.extend_from_slice(&text),
            _ => {
                let property = BString::from(property);
                let target = BString::from(target);
                let subject = wrapper_name.map_or_else(
                    || format!(r#"Property "{property}" of target "{target}""#),
                    |name| {
                        format!(r#"$<{name}:...> expression in "{property}" of target "{target}""#)
                    },
                );
                let mut error = generation_error([format!(
                    "{subject} contains a generator expression.  This is not allowed."
                )
                .into()]);
                if let GenError::Invalid(message) = &mut error {
                    message.extend_from_slice(b"CMake Generate step failed.  Build files cannot be regenerated correctly.");
                }
                return Err(error);
            }
        }
    }
    Ok(result)
}

fn collect_links(
    dependencies: &Dependencies<'_>,
    target: &Target,
    package: &mut CpsPackage,
    usage: &mut CpsUsage,
    dir: DirId,
    value: &[u8],
    errors: &mut Vec<BString>,
) -> Result<(), GenError> {
    let Dependencies { model, mode, .. } = *dependencies;
    for item in split_link_items(value) {
        let (item, requires, wrapper_name) = if let Some(inner) = wrapper(item, b"$<LINK_ONLY:") {
            (inner, &mut usage.link_requires, Some("LINK_ONLY"))
        } else if let Some(inner) = wrapper(item, b"$<COMPILE_ONLY:") {
            (inner, &mut usage.compile_requires, Some("COMPILE_ONLY"))
        } else {
            (item, &mut usage.requires, None)
        };
        let items = interface_value(
            item,
            mode,
            b"INTERFACE_LINK_LIBRARIES",
            &target.name,
            wrapper_name,
        )?;
        for name in list::split(&items, list::Empty::Drop) {
            if let Some(id) = model.find_target(dir, &name) {
                match dependencies.name(id, target) {
                    Ok(dependency) => push_unique(requires, dependency.record(model, package)?),
                    Err(messages) => errors.extend(messages),
                }
            } else {
                push_unique(&mut usage.link_libraries, name);
            }
        }
    }
    Ok(())
}

fn wrapper<'a>(value: &'a [u8], prefix: &[u8]) -> Option<&'a [u8]> {
    value.strip_prefix(prefix)?.strip_suffix(b">")
}

fn split_link_items(value: &[u8]) -> Vec<&[u8]> {
    let mut depth = 0usize;
    let mut start = 0;
    let mut previous = 0;
    let mut escaped = false;
    let mut items = Vec::new();
    for (index, &byte) in value.iter().enumerate() {
        match byte {
            b'<' if previous == b'$' => depth += 1,
            b'>' => depth = depth.saturating_sub(1),
            b';' if depth == 0 && !escaped => {
                items.extend(value.get(start..index));
                start = index + 1;
            }
            _ => {}
        }
        escaped = byte == b'\\' && !escaped;
        previous = byte;
    }
    items.extend(value.get(start..));
    items
}

struct Dependency<'a> {
    package: Option<BString>,
    component: BString,
    imported: Option<&'a Target>,
}

impl Dependency<'_> {
    fn record(self, model: &ProjectModel, package: &mut CpsPackage) -> Result<BString, GenError> {
        let Some(name) = self.package else {
            return Ok([b":", self.component.as_slice()].concat().into());
        };
        let pkg_config = self.imported.is_some_and(|target| {
            target
                .props
                .get(b"_RCMAKE_PKG_CONFIG_PACKAGE")
                .is_some_and(|name| !name.is_empty())
        });
        let requirement = match package.requires.entry(name.clone()) {
            std::collections::btree_map::Entry::Occupied(entry) => {
                let existing_pkg_config = !entry.get().domains.is_empty();
                if existing_pkg_config != pkg_config {
                    return Err(generation_error([format!(
                        r#"Package "{}" requires package "{name}" in multiple domain sets.  This is not supported."#,
                        package.name,
                    ).into()]));
                }
                entry.into_mut()
            }
            std::collections::btree_map::Entry::Vacant(entry) => {
                let requirement = entry.insert(Default::default());
                if pkg_config {
                    requirement.domains.push("pkg-config".into());
                } else if let Some(target) = self.imported {
                    requirement.version = imported_version(model, target, &name).cloned();
                    if let Some(dir) = package_variable(model, target.dir, &name, b"_DIR") {
                        requirement.hints.push(dir.clone());
                        requirement.version.get_or_insert_with(BString::default);
                    }
                }
                requirement
            }
        };
        push_unique(&mut requirement.components, self.component.clone());
        Ok([name.as_slice(), b":", &self.component].concat().into())
    }
}

struct Dependencies<'a> {
    model: &'a ProjectModel,
    result: &'a GenerationResult,
    spec: &'a PackageInfoSpec,
    mode: PreprocessMode,
}

impl<'a> Dependencies<'a> {
    fn name(&self, id: TargetId, consumer: &Target) -> Result<Dependency<'a>, Vec<BString>> {
        let Self {
            model,
            result,
            spec,
            mode,
        } = *self;
        let target = model
            .target(id)
            .ok_or_else(|| vec!["Unknown dependency target.".into()])?;
        let contains = |set: &[u8]| {
            model
                .exports
                .get(set)
                .is_some_and(|set| set.targets.contains(&id))
        };
        if contains(&spec.export) {
            return Ok(Dependency {
                package: None,
                component: export_name(target),
                imported: None,
            });
        }
        if target.is_imported() {
            if let Some(name) = target
                .props
                .get(b"_RCMAKE_PKG_CONFIG_PACKAGE")
                .filter(|name| !name.is_empty())
            {
                return Ok(Dependency {
                    package: Some(name.clone()),
                    component: name.clone(),
                    imported: Some(target),
                });
            }
            let package = imported_package(model, target)
                .ok_or_else(|| vec![unknown_import(consumer, target)])?;
            let namespace = [package, b"::"].concat();
            if !target.name.starts_with(&namespace) {
                return Err(vec![format!(
                r#"Target "{}" references target "{}", which comes from the "{}" package, but does not belong to the package's canonical namespace ("{}").  This is not allowed."#,
                consumer.name, target.name, BString::from(package), BString::from(namespace),
            ).into()]);
            }
            return Ok(Dependency {
                package: Some(package.into()),
                component: component(target, &namespace),
                imported: Some(target),
            });
        }
        let candidates =
            exports(model, result, id, mode).map_err(|error| vec![error.to_string().into()])?;
        let export_message = |kind: &str, name: &[u8]| {
            if mode == PreprocessMode::Install {
                format!(
                    r#"install({kind} "{}" ...) includes target "{}" which requires target "{}""#,
                    BString::from(name),
                    consumer.name,
                    target.name,
                )
            } else {
                format!(
                    r#"export called with target "{}" which requires target "{}""#,
                    consumer.name, target.name
                )
            }
        };
        let subject = export_message("PACKAGE_INFO", &spec.package);
        let Some(first) = candidates.first() else {
            return Err(vec![
                format!("{subject} that is not in any export set.").into(),
            ]);
        };
        let multiple_sets = candidates.iter().any(|other| other.set != first.set);
        if multiple_sets
            || candidates
                .iter()
                .any(|other| other.namespace != first.namespace)
        {
            let files = candidates
                .iter()
                .map(|entry| entry.file.as_slice())
                .collect::<Vec<_>>()
                .join(b", ".as_slice());
            let reason = if multiple_sets {
                "in multiple other export sets"
            } else {
                "in another export set which is exported multiple times with different namespaces"
            };
            let details = format!(
                "that is not in this export set, but {reason}: {}.\n\nAn exported target cannot depend upon another target which is exported in more than one export set or with more than one namespace.  Consider consolidating the exports of the \"{}\" target to a single export.",
                BString::from(files),
                target.name,
            );
            let mut messages = Vec::new();
            for export in model
                .find_target(consumer.dir, &consumer.name)
                .map(|id| exports(model, result, id, mode))
                .transpose()
                .map_err(|error| vec![error.to_string().into()])?
                .into_iter()
                .flatten()
            {
                if let Some(set) = export.set.filter(|_| export.file.ends_with(b".cmake")) {
                    messages.push(format!("{} {details}", export_message("EXPORT", &set)).into());
                }
            }
            messages.push(format!("{subject} {details}").into());
            return Err(messages);
        }
        let Some(package) = first.namespace.strip_suffix(b"::") else {
            return Err(vec![format!(
            r#"Target "{}" references target "{}{}", which does not use the standard namespace separator.  This is not allowed."#,
            consumer.name, first.namespace, export_name(target),
        ).into()]);
        };
        Ok(Dependency {
            package: (package != spec.package).then(|| package.into()),
            component: export_name(target),
            imported: None,
        })
    }
}

fn runtime_dependencies(
    dependencies: &Dependencies<'_>,
    resolver: &mut UsageResolver<'_>,
    facts: &ResolvedTarget,
    target: &Target,
    package: &mut CpsPackage,
    common: &CpsUsage,
    config: &mut CpsUsage,
) -> Result<(), GenError> {
    let Dependencies { model, mode, .. } = *dependencies;
    let handle = TargetHandle(facts.id.index() as u32);
    let context = EvalContext {
        head: Some(handle),
        current: Some(handle),
        config: facts.config.clone(),
        traversal: UsageTraversal::Link,
        link_language: facts.link.as_ref().map(|link| link.language.clone()),
        ..Default::default()
    };
    let entries = target.usage.link_libraries.as_ref().map_or_else(
        || {
            target
                .props
                .get(b"LINK_LIBRARIES")
                .into_iter()
                .map(|value| (target.dir, value))
                .collect::<Vec<_>>()
        },
        |items| {
            items
                .iter()
                .map(|item| (item.lookup_dir, &item.text.value))
                .collect()
        },
    );
    for (dir, value) in entries {
        for name in list::split(
            &genex(resolver, dir, &context, value, Some(mode))?,
            list::Empty::Drop,
        ) {
            let Some(id) = model.find_target(dir, &name) else {
                continue;
            };
            if !model
                .target(id)
                .is_some_and(|target| target.kind == TargetKind::SharedLibrary)
            {
                continue;
            }
            let dependency = dependencies.name(id, target).map_err(generation_error)?;
            let reference = dependency.record(model, package)?;
            if !common.requires.contains(&reference) && !common.link_requires.contains(&reference) {
                push_unique(&mut config.dyld_requires, reference);
            }
        }
    }
    Ok(())
}
