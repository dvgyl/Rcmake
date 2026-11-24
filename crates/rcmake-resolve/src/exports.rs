use indexmap::{IndexMap, IndexSet, map::Entry};
use rcmake_base::{BString, list, path_lex::Lexical};
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
        if model.dir_excluded_from_all(rule.dir) {
            continue;
        }
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
    for rule in &model.installs {
        if model.dir_excluded_from_all(rule.dir) {
            continue;
        }
        let InstallKind::Export {
            set,
            namespace,
            file,
            cxx_modules_directory,
        } = &rule.kind
        else {
            continue;
        };
        let members = export_set(model, set)?;
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
                Some(install),
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
        plans.push(plan);
    }

    let build_plans = build_exports(model)?;
    let mut build_catalog = ExportCatalog::new();
    for (plan, members, _) in build_plans.values() {
        let placement = ExportPlacement {
            set: plan.set.clone(),
            namespace: plan.namespace.clone(),
            file: plan.file.clone(),
        };
        for &id in members {
            build_catalog.entry(id).or_default().push(placement.clone());
        }
    }
    for (mut plan, members, export_packages) in build_plans.into_values() {
        let mut packages = IndexSet::new();
        let members: Vec<_> = members.into_iter().collect();
        for &id in &members {
            let mut target = export_target(
                model,
                usage,
                &plan.set,
                id,
                targets.iter().filter(|target| target.id == id),
                None,
            )?;
            if !plan
                .targets
                .iter()
                .any(|existing| existing.name == target.name)
            {
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
        }
        if export_packages {
            plan.package_dependencies = package_dependencies(model, &plan.set, packages);
        }
        plans.push(plan);
    }
    Ok(plans)
}

type BuildExports = IndexMap<BString, (ExportPlan, IndexSet<TargetId>, bool)>;

fn build_exports(model: &ProjectModel) -> Result<BuildExports, ResolveError> {
    let mut build_plans = BuildExports::new();
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
                b"EXPORT_LINK_INTERFACE_LIBRARIES" => {}
                _ => return Err(invalid_export_record()),
            }
        }
        let file = absolute_path(&directory.binary_dir, &file);
        if append && let Some((_, members, _)) = build_plans.get_mut(&file) {
            members.extend(ids);
        } else {
            build_plans.insert(
                file.clone(),
                (
                    ExportPlan {
                        set,
                        namespace,
                        file,
                        cxx_modules_directory,
                        ..ExportPlan::default()
                    },
                    ids.into_iter().collect(),
                    export_packages,
                ),
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
                if !Lexical::new(&path).absolute() && !path.starts_with(PREFIX) {
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

fn export_target<'a>(
    model: &ProjectModel,
    usage: &mut UsageResolver<'_>,
    set: &[u8],
    id: TargetId,
    resolved: impl Iterator<Item = &'a ResolvedTarget>,
    install: Option<&InstallManifest>,
) -> Result<ExportedTarget, ResolveError> {
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
            add_export_property(&mut exported, key, value, mode)?;
        }
    }
    for (field, entries) in &target.usage.fields {
        let key = field.name();
        if key.starts_with("INTERFACE_") {
            let value = list::join(entries.iter().map(|entry| &entry.value));
            add_export_property(&mut exported, key.into(), &value, mode)?;
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
        if properties.contains_key(b"IMPORTED_LOCATION".as_slice()) {
            if let Some(soname) = &facts.artifacts.soname_file {
                properties.insert(
                    "IMPORTED_SONAME".into(),
                    Lexical::new(soname).filename().into(),
                );
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
                    let object = absolute_path(root, &unit.object);
                    manifest
                        .actions
                        .iter()
                        .filter(|action| {
                            action
                                .configurations
                                .iter()
                                .any(|config| facts.config.matches(config))
                        })
                        .find_map(|action| match &action.kind {
                            InstallActionKind::Files {
                                files,
                                rename: Some(name),
                                file_type,
                                ..
                            } if file_type == b"FILE" && files.contains(&object) => {
                                Some(absolute_path(&action.destination, name))
                            }
                            _ => None,
                        })
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
    target: &mut ExportedTarget,
    key: BString,
    value: &[u8],
    mode: PreprocessMode,
) -> Result<(), ResolveError> {
    let mut items = export_property_items(value, mode)?;
    if mode == PreprocessMode::Install {
        for item in &mut items {
            *item = rcmake_genex::rewrite_install_prefix(item, b"${_IMPORT_PREFIX}");
            if matches!(
                key.as_slice(),
                b"INTERFACE_INCLUDE_DIRECTORIES" | b"INTERFACE_SYSTEM_INCLUDE_DIRECTORIES"
            ) && !Lexical::new(item).absolute()
                && !item.starts_with(b"$")
            {
                *item = [b"${_IMPORT_PREFIX}/", item.as_slice()].concat().into();
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
