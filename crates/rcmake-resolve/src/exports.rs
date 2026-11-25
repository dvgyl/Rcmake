mod interface_paths;

use indexmap::{IndexMap, IndexSet, map::Entry};
use rcmake_base::{BString, list, path_lex::Lexical};
use rcmake_diag::{
    Diagnostic, Effect, MessageKind, PolicyId, PolicyStatus, RenderOptions,
    render as render_diagnostic,
};
use rcmake_genex::{ConfigId, EvalContext, GenexNode, PreprocessMode, TargetHandle, render};
use rcmake_model::{
    DirId, ExportDependencyMode, ExportSet, InstallKind, InstallRule, ProjectModel, Target,
    TargetId, TargetKind,
};

use crate::{
    ExportPlan, ExportedTarget, InstallActionKind, InstallManifest, ResolveError, ResolvedTarget,
    export_links::{self, ExportCatalog, ExportPlacement},
    sources::absolute_path,
    usage::UsageResolver,
};

pub(crate) mod source_sets;

pub(crate) fn resolve_exports(
    model: &ProjectModel,
    usage: &mut UsageResolver<'_>,
    targets: &[ResolvedTarget],
    install: &InstallManifest,
    configs: &[ConfigId],
) -> Result<Vec<ExportPlan>, ResolveError> {
    let mut install_catalog = ExportCatalog::new();
    for rule in &model.installs {
        let InstallKind::Export {
            set,
            namespace,
            file,
            ..
        } = &rule.kind
        else {
            continue;
        };
        if let Some(members) = model.exports.get(set) {
            let placement = ExportPlacement {
                set: set.clone(),
                namespace: namespace.clone(),
                file: file.clone().unwrap_or_else(|| export_filename(set)),
            };
            for &id in &members.targets {
                install_catalog
                    .entry(id)
                    .or_default()
                    .push(placement.clone());
            }
        }
    }
    let mut plans = Vec::new();
    for (index, rule) in model.installs.iter().enumerate() {
        let InstallKind::Export {
            set,
            namespace,
            file,
            cxx_modules_directory,
        } = &rule.kind
        else {
            continue;
        };
        let members = model.exports.get(set).ok_or_else(|| {
            let mut message = BString::default();
            if model.installs.iter().any(|candidate| {
                candidate.dir == rule.dir
                    && matches!(&candidate.kind, InstallKind::PackageInfo(spec) if spec.export == *set)
            }) {
                message.extend_from_slice(
                    format!("CMake Error: INSTALL(PACKAGE_INFO) given unknown export \"{set}\"\n").as_bytes()
                );
            }
            message.extend_from_slice(
                format!("CMake Error: INSTALL(EXPORT) given unknown export \"{set}\"\n").as_bytes(),
            );
            ResolveError::Message(message)
        })?;
        let legacy_interface = (2..)
            .map_while(|arg| {
                model
                    .global
                    .props
                    .get(format!("_RCMAKE_INSTALL_{index}_ARG{arg}").as_bytes())
            })
            .any(|arg| arg == b"EXPORT_LINK_INTERFACE_LIBRARIES");
        validate_members(model, &members.targets, Some(set))?;
        let mut include_rules = IndexMap::<TargetId, Vec<_>>::new();
        for &index in &members.install_rules {
            let Some(rule) = model.installs.get(index) else {
                continue;
            };
            if let InstallKind::Targets { targets, .. } = &rule.kind
                && !rule.options.includes_destination.is_empty()
            {
                for &id in targets {
                    include_rules.entry(id).or_default().push(rule);
                }
            }
        }
        let mut plan = ExportPlan {
            set: set.clone(),
            namespace: namespace.clone(),
            file: file.clone().unwrap_or_else(|| export_filename(set)),
            cxx_modules_directory: cxx_modules_directory.clone(),
            install: true,
            ..ExportPlan::default()
        };
        let mut packages = IndexSet::new();
        for &id in &members.targets {
            let mut target = export_target(
                model,
                usage,
                &plan.set,
                id,
                targets.iter().filter(|target| {
                    target.id == id
                        && (rule.options.configurations.is_empty()
                            || rule
                                .options
                                .configurations
                                .iter()
                                .any(|c| target.config.matches(c)))
                }),
                ExportOptions {
                    install: Some(install),
                    legacy_interface,
                },
            )?;
            if let Some(rules) = include_rules.get(&id) {
                add_include_destinations(usage, id, rules, &mut target)?;
            }
            packages.extend(export_links::qualify(
                model,
                id,
                &members.targets,
                &plan.namespace,
                PreprocessMode::Install,
                &install_catalog,
                &mut target,
            )?);
            target.source_sets =
                source_sets::export(model, usage, configs, id, Some(&members.install_rules))?;
            plan.targets.push(target);
        }
        if rule.options.export_package_dependencies {
            plan.package_dependencies = package_dependencies(model, &plan.set, packages);
        }
        // The parent install omits excluded directories. Their export rules
        // still take part in dependency resolution and validation.
        if !model.dir_excluded_from_all(rule.dir) {
            plans.push(plan);
        }
    }

    let build_plans = build_exports(model)?;
    let mut build_catalog = ExportCatalog::new();
    for BuildExport { plan, members, .. } in build_plans.values() {
        let placement = ExportPlacement {
            set: plan.set.clone(),
            namespace: plan.namespace.clone(),
            file: plan.file.clone(),
        };
        for &id in members {
            build_catalog.entry(id).or_default().push(placement.clone());
        }
    }
    for BuildExport {
        mut plan,
        members,
        export_packages,
        legacy_interface,
    } in build_plans.into_values()
    {
        let mut packages = IndexSet::new();
        validate_members(model, &members, None)?;
        for &id in &members {
            let mut target = export_target(
                model,
                usage,
                &plan.set,
                id,
                targets.iter().filter(|target| target.id == id),
                ExportOptions {
                    install: None,
                    legacy_interface,
                },
            )?;
            packages.extend(export_links::qualify(
                model,
                id,
                &members,
                &plan.namespace,
                PreprocessMode::Build,
                &build_catalog,
                &mut target,
            )?);
            target.source_sets = source_sets::export(model, usage, configs, id, None)?;
            plan.targets.push(target);
        }
        if export_packages {
            plan.package_dependencies = package_dependencies(model, &plan.set, packages);
        }
        plans.push(plan);
    }
    Ok(plans)
}

struct BuildExport {
    plan: ExportPlan,
    members: Vec<TargetId>,
    export_packages: bool,
    legacy_interface: bool,
}

fn build_exports(model: &ProjectModel) -> Result<IndexMap<BString, BuildExport>, ResolveError> {
    let mut build_plans = IndexMap::<BString, BuildExport>::new();
    let props = &model.global.props;
    let count = props
        .get(b"_RCMAKE_BUILD_EXPORT_COUNT")
        .and_then(|value| property_index(value))
        .unwrap_or_default();
    for index in 0..count {
        let section = format!("_RCMAKE_BUILD_EXPORT_{index}");
        let dir_index = props
            .get(format!("{section}_DIRECTORY").as_bytes())
            .and_then(|value| property_index(value))
            .ok_or_else(invalid_export_record)?;
        let (dir, directory) = model
            .dirs()
            .nth(dir_index)
            .ok_or_else(invalid_export_record)?;
        let args: Vec<_> = (0..)
            .map_while(|arg| props.get(format!("{section}_ARG{arg}").as_bytes()))
            .map(|value| value.as_slice())
            .collect();
        let (set, ids, mut options) = match args.as_slice() {
            [b"EXPORT", set, options @ ..] => (
                BString::from(*set),
                export_set(model, set)?.targets.clone(),
                options,
            ),
            [b"TARGETS", rest @ ..] => {
                let end = rest
                    .iter()
                    .position(|name| {
                        matches!(
                            *name,
                            b"FILE"
                                | b"NAMESPACE"
                                | b"APPEND"
                                | b"EXPORT_LINK_INTERFACE_LIBRARIES"
                                | b"CXX_MODULES_DIRECTORY"
                        )
                    })
                    .unwrap_or(rest.len());
                let (names, options) = rest.split_at(end);
                let ids = names
                    .iter()
                    .map(|name| {
                        model
                            .find_target(dir, name)
                            .ok_or_else(invalid_export_record)
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                (BString::default(), ids, options)
            }
            _ => return Err(invalid_export_record()),
        };
        let mut namespace = BString::default();
        let mut file = export_filename(&set);
        let mut export_packages = false;
        let mut legacy_interface = false;
        let mut append = false;
        let mut cxx_modules_directory = None;
        while let Some((key, rest)) = options.split_first() {
            options = rest;
            match *key {
                b"FILE" | b"NAMESPACE" | b"CXX_MODULES_DIRECTORY" => {
                    let (arg, rest) = options.split_first().ok_or_else(invalid_export_record)?;
                    if *key == b"FILE" {
                        file = (*arg).into();
                    } else if *key == b"NAMESPACE" {
                        namespace = (*arg).into();
                    } else {
                        cxx_modules_directory = Some((*arg).into());
                    }
                    options = rest;
                }
                b"EXPORT_PACKAGE_DEPENDENCIES" => export_packages = true,
                b"APPEND" => append = true,
                b"EXPORT_LINK_INTERFACE_LIBRARIES" => legacy_interface = true,
                _ => return Err(invalid_export_record()),
            }
        }
        let file = absolute_path(&directory.binary_dir, &file);
        if append && let Some(existing) = build_plans.get_mut(&file) {
            existing.members.extend(ids);
            existing.legacy_interface |= legacy_interface;
        } else {
            build_plans.insert(
                file.clone(),
                BuildExport {
                    plan: ExportPlan {
                        set,
                        namespace,
                        file,
                        cxx_modules_directory,
                        ..ExportPlan::default()
                    },
                    members: ids,
                    export_packages,
                    legacy_interface,
                },
            );
        }
    }
    Ok(build_plans)
}

fn package_dependencies(
    model: &ProjectModel,
    set: &[u8],
    mut inferred: IndexSet<BString>,
) -> IndexMap<BString, Vec<BString>> {
    let mut dependencies = IndexMap::new();
    if let Some(configured) = model.export_dependencies.get(set) {
        for (name, setup) in configured {
            let used = inferred.shift_remove(name);
            let enabled = match setup.mode {
                ExportDependencyMode::On => true,
                ExportDependencyMode::Off => false,
                ExportDependencyMode::Auto => used,
            };
            if enabled {
                dependencies.insert(name.clone(), setup.args.clone());
            }
        }
    }
    for name in &model.package_lookups {
        if inferred.shift_remove(name) {
            dependencies.insert(name.clone(), Vec::new());
        }
    }
    dependencies.extend(inferred.into_iter().map(|name| (name, Vec::new())));
    dependencies
}

fn add_include_destinations(
    usage: &mut UsageResolver<'_>,
    id: TargetId,
    rules: &[&InstallRule],
    target: &mut ExportedTarget,
) -> Result<(), ResolveError> {
    const KEY: &[u8] = b"INTERFACE_INCLUDE_DIRECTORIES";
    const PREFIX: &[u8] = b"${_IMPORT_PREFIX}";
    let handle = TargetHandle(id.index() as u32);
    let ctx = EvalContext {
        head: Some(handle),
        current: Some(handle),
        ..EvalContext::default()
    };
    let mut includes = BString::default();
    for rule in rules {
        let mut host = usage.host(rule.dir);
        for destination in &rule.options.includes_destination {
            let value = rcmake_genex::rewrite_install_prefix(destination, PREFIX);
            let ast = rcmake_genex::strip(&rcmake_genex::parse(&value)?, PreprocessMode::Install);
            let (paths, flags) = rcmake_genex::evaluate(&ast, &ctx, &mut host)?;
            if flags.context_sensitive || flags.head_sensitive || flags.link_language_sensitive {
                return Err(ResolveError::Message(
                    "INCLUDES DESTINATION requires paths that do not depend on the evaluation context."
                        .into(),
                ));
            }
            for path in list::split(&paths, list::Empty::Drop) {
                if !includes.is_empty() {
                    includes.push(b';');
                }
                let rooted = !Lexical::new(&path).root_directory().is_empty();
                if !rooted && !path.starts_with(PREFIX) {
                    includes.extend_from_slice(PREFIX);
                    includes.push(b'/');
                }
                includes.extend_from_slice(&path);
            }
        }
    }
    if !includes.is_empty() {
        match target.properties.entry(KEY.into()) {
            Entry::Occupied(mut entry) => {
                entry.get_mut().push(b';');
                entry.get_mut().extend_from_slice(&includes);
            }
            Entry::Vacant(entry) => {
                entry.insert(includes);
            }
        }
    }
    Ok(())
}

fn property_index(value: &[u8]) -> Option<usize> {
    std::str::from_utf8(value).ok()?.parse().ok()
}

fn invalid_export_record() -> ResolveError {
    ResolveError::Message("Invalid build export record.".into())
}

fn export_set<'a>(model: &'a ProjectModel, set: &[u8]) -> Result<&'a ExportSet, ResolveError> {
    model
        .exports
        .get(set)
        .ok_or_else(|| ResolveError::Message([b"Unknown export set: ", set].concat().into()))
}

fn export_filename(set: &[u8]) -> BString {
    [set, b".cmake"].concat().into()
}

#[derive(Clone, Copy)]
struct ExportOptions<'a> {
    install: Option<&'a InstallManifest>,
    legacy_interface: bool,
}

impl ResolveError {
    /// Add the export diagnostic when an exported target has no linker language.
    pub fn with_export_linker_language(self, model: &ProjectModel) -> Self {
        let Self::Message(message) = &self else {
            return self;
        };
        let Some(name) = message.strip_prefix(b"Cannot determine link language for target ") else {
            return self;
        };
        let Some((id, target)) = model.targets().find(|(_, target)| target.name == name) else {
            return self;
        };
        let contains = |set: &BString| {
            model
                .exports
                .get(set)
                .is_some_and(|set| set.targets.contains(&id))
        };
        let installed = model.installs.iter().any(|rule| {
            !model.dir_excluded_from_all(rule.dir)
                && match &rule.kind {
                    InstallKind::Export { set, .. } => contains(set),
                    InstallKind::PackageInfo(spec) => contains(&spec.export),
                    _ => false,
                }
        });
        let exported = build_exports(model).is_ok_and(|plans| {
            plans.values().any(|plan| plan.members.contains(&id))
        }) || model.build_exports.iter().any(|rule| {
            matches!(&rule.kind, rcmake_model::ExportKind::PackageInfo(spec) if contains(&spec.export))
        });
        if !installed && !exported {
            return self;
        }
        let error = export_diagnostic(
            model,
            target,
            format!(
                "Exporting the target \"{}\" is not allowed since its linker language cannot be determined",
                target.name,
            ),
        );
        Self::Message(
            format!(
                "CMake Error: CMake can not determine linker language for target: {}\n{error}",
                target.name
            )
            .into(),
        )
    }
}

fn export_diagnostic(model: &ProjectModel, target: &Target, body: impl Into<BString>) -> BString {
    render_diagnostic(
        &Diagnostic {
            annotations: None,
            kind: MessageKind::Error,
            effect: Effect::Fatal,
            category: None,
            policy: None,
            location: crate::cxx_modules::generation_location(model, target),
            body: body.into(),
        },
        &RenderOptions {
            relative_base: Some(model.dir(DirId::ROOT).source_dir.clone()),
            ..RenderOptions::default()
        },
    )
}

fn validate_members(
    model: &ProjectModel,
    members: &[TargetId],
    install_set: Option<&BString>,
) -> Result<(), ResolveError> {
    let mut seen = IndexSet::new();
    for &id in members {
        if seen.insert(id) {
            continue;
        }
        let target = model.target(id).ok_or_else(invalid_export_record)?;
        let body = if let Some(set) = install_set {
            format!(
                "install(EXPORT \"{set}\" ...) includes target \"{}\" more than once in the export set.",
                target.name,
            )
        } else {
            format!("given target \"{}\" more than once.", target.name)
        };
        return Err(ResolveError::Message(export_diagnostic(
            model, target, body,
        )));
    }
    Ok(())
}

fn export_target<'a>(
    model: &ProjectModel,
    usage: &mut UsageResolver<'_>,
    set: &[u8],
    id: TargetId,
    resolved: impl Iterator<Item = &'a ResolvedTarget>,
    options: ExportOptions<'_>,
) -> Result<ExportedTarget, ResolveError> {
    let ExportOptions {
        install,
        legacy_interface,
    } = options;
    let target = model
        .target(id)
        .filter(|target| !target.is_imported())
        .ok_or_else(invalid_export_record)?;
    let kind = match target.kind {
        TargetKind::Executable => "EXECUTABLE",
        TargetKind::StaticLibrary => "STATIC",
        TargetKind::SharedLibrary => "SHARED",
        TargetKind::ModuleLibrary => "MODULE",
        TargetKind::ObjectLibrary if install.is_none() => "OBJECT",
        TargetKind::ObjectLibrary | TargetKind::InterfaceLibrary => "INTERFACE",
        _ => return Err(invalid_export_record()),
    };
    let mut exported = ExportedTarget {
        name: export_name(target).clone(),
        kind: kind.into(),
        ..ExportedTarget::default()
    };
    export_properties(model, target, &mut exported)?;
    if target
        .props
        .get(b"ENABLE_EXPORTS")
        .is_some_and(|value| rcmake_base::truth::is_on(value))
    {
        exported
            .properties
            .insert("ENABLE_EXPORTS".into(), "1".into());
    }
    let mode = if install.is_some() {
        PreprocessMode::Install
    } else {
        PreprocessMode::Build
    };
    for key in target
        .props
        .names()
        .into_iter()
        .filter(|key| key.starts_with(b"INTERFACE_"))
    {
        if let Some(value) = target.props.get(&key) {
            add_export_property(model, target, &mut exported, key, value, mode)?;
        }
    }
    for (field, entries) in &target.usage.fields {
        let key = field.name();
        if key.starts_with("INTERFACE_") {
            let value = list::join(entries.iter().map(|entry| &entry.value));
            add_export_property(model, target, &mut exported, key.into(), &value, mode)?;
        }
    }
    let alternative = model
        .export_xcframeworks
        .get(set)
        .and_then(|entries| entries.get(&target.name))
        .map(|expression| {
            let ast = rcmake_genex::strip(&rcmake_genex::parse(expression)?, mode);
            let value = render(&ast.nodes);
            Ok::<_, ResolveError>(if install.is_some() {
                rcmake_genex::rewrite_install_prefix(&value, b"${_IMPORT_PREFIX}")
            } else {
                value
            })
        })
        .transpose()?;
    let layout = crate::ide::bundle_kind(target);
    let darwin = crate::lower::is_darwin(model, target.dir);
    if darwin && matches!(layout, crate::ide::BundleKind::Framework { .. }) {
        exported.properties.insert("FRAMEWORK".into(), "1".into());
    }
    for facts in resolved {
        let mut properties = IndexMap::new();
        for (key, path) in [
            ("IMPORTED_LOCATION", &facts.artifacts.file),
            ("IMPORTED_IMPLIB", &facts.artifacts.import_file),
        ] {
            if let Some(path) = path
                && let Some(path) = exported_location(install, facts, path, &layout)
            {
                properties.insert(key.into(), path);
            }
        }
        if let Some(expression) = &alternative {
            let context = EvalContext {
                config: facts.config.clone(),
                head: Some(crate::usage::handle(id)),
                ..EvalContext::default()
            };
            let value =
                usage.eval_ide_property(id, b"XCFRAMEWORK_LOCATION", expression, &context)?;
            if !value.is_empty() {
                let path = if install.is_some() {
                    value
                } else {
                    absolute_path(&model.dir(target.dir).binary_dir, &value)
                };
                exported
                    .xcframework_locations
                    .insert(facts.config.0.clone(), path);
            }
        }
        let legacy_key = [
            b"LINK_INTERFACE_LIBRARIES_",
            facts.config.0.to_ascii_uppercase().as_slice(),
        ]
        .concat();
        if target.kind != TargetKind::InterfaceLibrary
            && let Some(value) = target
                .props
                .get(&legacy_key)
                .or_else(|| target.props.get(b"LINK_INTERFACE_LIBRARIES"))
        {
            if legacy_interface {
                properties.insert("IMPORTED_LINK_INTERFACE_LIBRARIES".into(), value.clone());
            } else if target.policies.policies.get(PolicyId::new(22)) == PolicyStatus::New {
                return Err(ResolveError::Message(export_diagnostic(
                    model,
                    target,
                    format!(
                        "Target \"{}\" has policy CMP0022 enabled, but also has old-style LINK_INTERFACE_LIBRARIES properties populated, but it was exported without the EXPORT_LINK_INTERFACE_LIBRARIES to export the old-style properties",
                        target.name,
                    ),
                )));
            }
        }
        if properties.contains_key(b"IMPORTED_LOCATION".as_slice()) {
            if let Some(soname) = &facts.artifacts.soname_file {
                let mut value = if darwin {
                    crate::lower::install_name_prefix(model, target, &facts.artifacts)
                } else {
                    BString::default()
                };
                value.extend_from_slice(Lexical::new(soname).filename());
                properties.insert("IMPORTED_SONAME".into(), value);
            }
            if kind == "STATIC" {
                let languages: IndexSet<_> = facts
                    .compile_groups
                    .iter()
                    .map(|group| &group.language)
                    .collect();
                properties.insert(
                    "IMPORTED_LINK_INTERFACE_LANGUAGES".into(),
                    list::join(languages),
                );
            }
        } else if kind == "OBJECT" && !facts.compile_units.is_empty() {
            let base = &model.dir(DirId::ROOT).binary_dir;
            let paths = facts
                .compile_units
                .iter()
                .map(|unit| absolute_path(base, &unit.object));
            properties.insert("IMPORTED_OBJECTS".into(), list::join(paths));
        }
        if target.kind == TargetKind::ObjectLibrary
            && let Some(manifest) = install
        {
            let root = &model.dir(DirId::ROOT).binary_dir;
            let paths = facts
                .compile_units
                .iter()
                .filter_map(|unit| {
                    let built = absolute_path(root, &unit.object);
                    crate::lower::exported_object_path(manifest, &facts.config, &built)
                })
                .collect::<Vec<_>>();
            if !paths.is_empty() {
                exported.kind = "OBJECT".into();
                properties.insert("IMPORTED_OBJECTS".into(), list::join(paths));
            }
        }
        if !properties.is_empty() || exported.xcframework_locations.contains_key(&facts.config.0) {
            exported
                .config_properties
                .insert(facts.config.0.clone(), properties);
        }
    }
    Ok(exported)
}

fn export_properties(
    model: &ProjectModel,
    target: &Target,
    exported: &mut ExportedTarget,
) -> Result<(), ResolveError> {
    for key in [b"SPDX_LICENSE".as_slice(), b"SYMBOLIC"] {
        if let Some(value) = target.props.get(key) {
            exported.properties.insert(key.into(), value.clone());
        }
    }
    let Some(keys) = target.props.get(b"EXPORT_PROPERTIES") else {
        return Ok(());
    };
    for key in list::split(keys, list::Empty::Drop) {
        let reason = if key.starts_with(b"IMPORTED_") || key.starts_with(b"INTERFACE_") {
            "IMPORTED_* and INTERFACE_* properties are reserved."
        } else {
            let Some(value) = target.props.get(&key) else {
                continue;
            };
            if !value.windows(2).any(|bytes| bytes == b"$<") {
                exported.properties.insert(key, value.clone());
                continue;
            }
            "this property contains a generator expression.  This is not allowed."
        };
        return Err(ResolveError::Message(export_diagnostic(
            model,
            target,
            format!(
                "Target \"{}\" contains property \"{key}\" in EXPORT_PROPERTIES but {reason}",
                target.name,
            ),
        )));
    }
    Ok(())
}

pub(crate) fn export_name(target: &Target) -> &BString {
    target
        .props
        .get(b"EXPORT_NAME")
        .filter(|name| !name.is_empty())
        .unwrap_or(&target.name)
}

fn exported_location(
    install: Option<&InstallManifest>,
    target: &ResolvedTarget,
    path: &BString,
    layout: &crate::ide::BundleKind,
) -> Option<BString> {
    let Some(install) = install else {
        return Some(path.clone());
    };
    install.actions.iter().find_map(|action| {
        if !action.configurations.is_empty()
            && !action
                .configurations
                .iter()
                .any(|name| target.config.matches(name))
        {
            return None;
        }
        let installed = match &action.kind {
            InstallActionKind::Files {
                files, file_type, ..
            } if !matches!(file_type.as_slice(), b"FILE" | b"PROGRAM") && files.contains(path) => {
                Lexical::new(path).filename()
            }
            InstallActionKind::Directory { dirs, .. } => {
                let root = layout.root(path)?;
                if !dirs.iter().any(|dir| dir == root) {
                    return None;
                }
                path.get(Lexical::new(root).parent().len()..)?
                    .strip_prefix(b"/")?
            }
            _ => return None,
        };
        Some(absolute_path(&action.destination, installed))
    })
}

fn add_export_property(
    model: &ProjectModel,
    raw: &rcmake_model::Target,
    target: &mut ExportedTarget,
    key: BString,
    value: &[u8],
    mode: PreprocessMode,
) -> Result<(), ResolveError> {
    let mut items = export_property_items(value, mode)?;
    if mode == PreprocessMode::Install {
        let path_property = matches!(
            key.as_slice(),
            b"INTERFACE_INCLUDE_DIRECTORIES"
                | b"INTERFACE_SYSTEM_INCLUDE_DIRECTORIES"
                | b"INTERFACE_SOURCES"
        );
        let wrapped = rcmake_genex::parse(value)?;
        let mut install_paths = Vec::new();
        for node in &wrapped.nodes {
            let raw = render(std::slice::from_ref(node));
            if raw.starts_with(b"$<INSTALL_INTERFACE:") {
                install_paths.extend(export_property_items(&raw, mode)?);
            }
        }
        for item in &mut items {
            if path_property
                && install_paths.contains(item)
                && !Lexical::new(item).absolute()
                && !item.starts_with(b"$")
            {
                *item = [b"${_IMPORT_PREFIX}/", item.as_slice()].concat().into();
            }
            *item = rcmake_genex::rewrite_install_prefix(item, b"${_IMPORT_PREFIX}");
            if path_property {
                interface_paths::validate(model, raw, &key, item)?;
            }
        }
    }
    if !items.is_empty() {
        target.properties.insert(key, list::join(items));
    }
    Ok(())
}

pub(crate) fn export_property_items(
    value: &[u8],
    mode: PreprocessMode,
) -> Result<Vec<BString>, ResolveError> {
    let ast = rcmake_genex::strip(&rcmake_genex::parse(value)?, mode);
    let mut items = Vec::new();
    let mut item = BString::default();
    for node in &ast.nodes {
        match node {
            GenexNode::Text(text) => {
                let mut escaped = false;
                for &byte in text.iter() {
                    if byte == b';' && !escaped {
                        if !item.is_empty() {
                            items.push(std::mem::take(&mut item));
                        }
                    } else {
                        item.push(byte);
                    }
                    escaped = byte == b'\\' && !escaped;
                }
            }
            GenexNode::Expr(_) => item.extend_from_slice(&render(std::slice::from_ref(node))),
        }
    }
    if !item.is_empty() {
        items.push(item);
    }
    Ok(items)
}
