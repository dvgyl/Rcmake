//! Install actions and configure metadata.

use std::convert::Infallible;

use indexmap::{IndexMap, IndexSet};
use rcmake_base::{BString, list, path_lex::Lexical};
use rcmake_genex::{ConfigId, EvalContext, EvalSite, evaluate, parse};
use rcmake_model::{BT, DirId, InstallKind, InstallOptions, InstallRule, ProjectModel, TargetKind};

use super::{compute_artifacts, definition, join};
use crate::{
    ConfigureMetadata, InstallAction, InstallActionKind, InstallManifest, ResolveError,
    ResolvedTarget, Toolchain, usage::UsageResolver,
};

/// Collect listfiles and compiler data from the model.
///
/// Listfiles keep the order of their first appearance. Read compiler values
/// from directory variable snapshots and the cache.
#[must_use]
pub fn collect_metadata(model: &ProjectModel) -> ConfigureMetadata {
    let mut listfiles = IndexSet::new();
    let mut languages = IndexMap::new();
    for (dir, directory) in model.dirs() {
        listfiles.extend(directory.listfiles.iter().cloned());
        for key in directory.vars.keys().chain(directory.props.names().iter()) {
            if let Some(language) = compiler_language(key) {
                languages.entry(language).or_insert(dir);
            }
        }
    }
    for key in model
        .global
        .props
        .names()
        .iter()
        .chain(model.cache.iter().map(|(key, _)| key))
    {
        if let Some(language) = compiler_language(key) {
            languages.entry(language).or_insert(DirId::ROOT);
        }
    }
    let toolchains = languages
        .into_iter()
        .filter_map(|(language, dir)| {
            let read = |suffix: &[u8]| {
                definition(
                    model,
                    dir,
                    &[b"CMAKE_", language.as_slice(), b"_", suffix].concat(),
                )
            };
            let compiler = read(b"COMPILER")?;
            if rcmake_base::truth::is_off(compiler) {
                return None;
            }
            let value = |suffix| read(suffix).cloned().unwrap_or_default();
            let optional = |suffix| read(suffix).filter(|v| !v.is_empty()).cloned();
            let paths =
                |suffix| read(suffix).map_or_else(Vec::new, |v| list::split(v, list::Empty::Drop));
            let toolchain = Toolchain {
                compiler_path: compiler.clone(),
                compiler_id: value(b"COMPILER_ID"),
                compiler_version: value(b"COMPILER_VERSION"),
                implicit_includes: paths(b"IMPLICIT_INCLUDE_DIRECTORIES"),
                implicit_link_dirs: paths(b"IMPLICIT_LINK_DIRECTORIES"),
                source_extensions: paths(b"SOURCE_FILE_EXTENSIONS"),
                scandep_rule: optional(b"SCANDEP_SOURCE"),
                module_map_format: optional(b"MODULE_MAP_FORMAT"),
                module_map_flag: optional(b"MODULE_MAP_FLAG"),
                compile_bmi_rule: optional(b"MODULE_BMI_ONLY_FLAG"),
            };
            Some((language, toolchain))
        })
        .collect();
    ConfigureMetadata {
        listfiles: listfiles.into_iter().collect(),
        toolchains,
        configure_log: definition(model, DirId::ROOT, b"CMAKE_CONFIGURE_LOG").cloned(),
        cmake_command: definition(model, DirId::ROOT, b"CMAKE_COMMAND").cloned(),
        ctest_command: definition(model, DirId::ROOT, b"CMAKE_CTEST_COMMAND").cloned(),
    }
}

fn compiler_language(key: &[u8]) -> Option<BString> {
    let lang = key.strip_prefix(b"CMAKE_")?.strip_suffix(b"_COMPILER")?;
    (!lang.is_empty()).then(|| lang.into())
}

/// Lower install rules in model order.
///
/// Export action indices refer to the plans for the install export rules in that
/// same order.
#[must_use]
pub fn lower_install_manifest(model: &ProjectModel, targets: &[ResolvedTarget]) -> InstallManifest {
    match lower_install_rules(model, targets, |rule, files| {
        Ok::<_, Infallible>(vec![(
            rule.options.clone(),
            literal_files(model, rule, files),
        )])
    }) {
        Ok(manifest) => manifest,
        Err(never) => match never {},
    }
}

pub(crate) fn lower_install_manifest_with_configs(
    model: &ProjectModel,
    targets: &[ResolvedTarget],
    configs: &[ConfigId],
) -> Result<InstallManifest, ResolveError> {
    let mut usage = UsageResolver::new(model);
    lower_install_rules(model, targets, |rule, files| {
        if !files
            .iter()
            .any(|path| path.value.windows(2).any(|pair| pair == b"$<"))
        {
            return Ok(vec![(
                rule.options.clone(),
                literal_files(model, rule, files),
            )]);
        }
        let source = &model.dir(rule.dir).source_dir;
        let mut host = usage.host(rule.dir);
        let mut groups = Vec::new();
        for config in configs {
            let mut options = rule.options.clone();
            if !options.configurations.is_empty()
                && !options
                    .configurations
                    .iter()
                    .any(|name| config.matches(name))
            {
                continue;
            }
            if configs.len() > 1 {
                options.configurations = vec![config.0.clone()];
            }
            let mut paths = Vec::new();
            for file in files {
                let context = EvalContext {
                    config: config.clone(),
                    site: EvalSite {
                        backtrace: file.backtrace.clone(),
                        property: None,
                    },
                    ..EvalContext::default()
                };
                let (value, _) = evaluate(&parse(&file.value)?, &context, &mut host)?;
                paths.extend(
                    list::split(&value, list::Empty::Drop)
                        .into_iter()
                        .map(|path| join(source, &path)),
                );
            }
            if !paths.is_empty() {
                groups.push((options, paths));
            }
        }
        Ok(groups)
    })
}

fn literal_files(model: &ProjectModel, rule: &InstallRule, files: &[BT<BString>]) -> Vec<BString> {
    let source = &model.dir(rule.dir).source_dir;
    files.iter().map(|file| join(source, &file.value)).collect()
}

fn lower_install_rules<E>(
    model: &ProjectModel,
    targets: &[ResolvedTarget],
    mut files_for_configs: impl FnMut(
        &InstallRule,
        &[BT<BString>],
    ) -> Result<Vec<(InstallOptions, Vec<BString>)>, E>,
) -> Result<InstallManifest, E> {
    let mut manifest = InstallManifest::default();
    let mut export_plan = 0;
    for (index, rule) in model.installs.iter().enumerate() {
        let options = &rule.options;
        let recorded = recorded_args(model, index);
        let source = &model.dir(rule.dir).source_dir;
        let kind = match &rule.kind {
            InstallKind::Targets { targets: ids, .. } => {
                let groups = target_options(&recorded);
                for id in ids {
                    let Some(raw) = model.target(*id).filter(|raw| !raw.is_imported()) else {
                        continue;
                    };
                    let matching: Vec<_> = targets.iter().filter(|t| t.id == *id).collect();
                    for target in &matching {
                        let Some((class, file_type, fallback)) =
                            target_install_kind(target.metadata.kind)
                        else {
                            continue;
                        };
                        let extra = groups.get(class);
                        let mut options =
                            extra.map_or_else(|| options.clone(), |extra| extra.merge(options));
                        if !options.configurations.is_empty()
                            && !options
                                .configurations
                                .iter()
                                .any(|config| target.config.matches(config))
                        {
                            continue;
                        }
                        if matching.len() > 1 {
                            options.configurations = vec![target.config.0.clone()];
                        }
                        let artifacts = if target.artifacts.file.is_some() {
                            target.artifacts.clone()
                        } else {
                            compute_artifacts(model, raw, &target.config)
                        };
                        let bin = &model.dir(DirId::ROOT).binary_dir;
                        let mut files = artifact_paths(&artifacts, extra);
                        let (key, default) = fallback;
                        let default =
                            definition(model, rule.dir, key).map_or(default, |v| v.as_slice());
                        let namelink = extra.and_then(|group| group.namelink_component).and_then(
                            |component| {
                                let linker = artifacts.linker_file.as_ref()?;
                                (artifacts
                                    .soname_file
                                    .as_ref()
                                    .is_some_and(|soname| soname != linker)
                                    && files.shift_remove(linker))
                                .then(|| (component, join(bin, linker)))
                            },
                        );
                        if !files.is_empty() {
                            let files = files.into_iter().map(|path| join(bin, path)).collect();
                            manifest.actions.push(action(
                                model,
                                rule,
                                &options,
                                default,
                                file_action(file_type, files, &options),
                            ));
                        }
                        if let Some((component, file)) = namelink {
                            options.component = Some(component.into());
                            let kind = file_action(file_type, vec![file], &options);
                            manifest
                                .actions
                                .push(action(model, rule, &options, default, kind));
                        }
                    }
                    if let Some(headers) = raw.props.get(b"PUBLIC_HEADER") {
                        let files: Vec<_> = list::split(headers, list::Empty::Drop)
                            .iter()
                            .map(|path| join(source, path))
                            .collect();
                        if !files.is_empty() {
                            let options = groups
                                .get(b"PUBLIC_HEADER".as_slice())
                                .map_or_else(|| options.clone(), |extra| extra.merge(options));
                            let default = definition(model, rule.dir, b"CMAKE_INSTALL_INCLUDEDIR")
                                .map_or(b"include".as_slice(), |value| value.as_slice());
                            let kind = file_action(b"FILE", files, &options);
                            manifest
                                .actions
                                .push(action(model, rule, &options, default, kind));
                        }
                    }
                }
                continue;
            }
            InstallKind::Files(files) | InstallKind::Programs(files) => {
                let file_type = if matches!(rule.kind, InstallKind::Programs(_)) {
                    b"PROGRAM".as_slice()
                } else {
                    b"FILE"
                };
                for (options, files) in files_for_configs(rule, files)? {
                    let kind = file_action(file_type, files, &options);
                    manifest
                        .actions
                        .push(action(model, rule, &options, b"", kind));
                }
                continue;
            }
            InstallKind::Directory(dirs) => InstallActionKind::Directory {
                dirs: dirs.iter().map(|d| join(source, &d.value)).collect(),
                options: directory_options(&recorded, &options.permissions),
            },
            InstallKind::Script(path) => InstallActionKind::Script(join(source, path)),
            InstallKind::Code(code) => InstallActionKind::Code(code.clone()),
            InstallKind::Export { .. } => {
                let plan = export_plan;
                export_plan += 1;
                InstallActionKind::Export { plan }
            }
            // CPS and SBOM files do not have install actions yet.
            InstallKind::PackageInfo(_) | InstallKind::Sbom(_) => continue,
        };
        manifest
            .actions
            .push(action(model, rule, options, b"", kind));
    }
    Ok(manifest)
}

fn action(
    model: &ProjectModel,
    rule: &InstallRule,
    options: &InstallOptions,
    destination: &[u8],
    kind: InstallActionKind,
) -> InstallAction {
    InstallAction {
        component: options
            .component
            .clone()
            .or_else(|| {
                definition(model, rule.dir, b"CMAKE_INSTALL_DEFAULT_COMPONENT_NAME").cloned()
            })
            .unwrap_or_else(|| "Unspecified".into()),
        destination: options
            .destination
            .clone()
            .unwrap_or_else(|| destination.into()),
        configurations: options.configurations.clone(),
        kind,
        optional: options.optional,
    }
}

fn file_action(
    file_type: &[u8],
    files: Vec<BString>,
    options: &InstallOptions,
) -> InstallActionKind {
    InstallActionKind::Files {
        file_type: file_type.into(),
        files,
        rename: options.rename.clone(),
        permissions: options.permissions.clone(),
    }
}

type TargetInstallKind = (&'static [u8], &'static [u8], (&'static [u8], &'static [u8]));

fn target_install_kind(kind: TargetKind) -> Option<TargetInstallKind> {
    match kind {
        TargetKind::Executable => {
            Some((b"RUNTIME", b"EXECUTABLE", (b"CMAKE_INSTALL_BINDIR", b"bin")))
        }
        TargetKind::StaticLibrary => Some((
            b"ARCHIVE",
            b"STATIC_LIBRARY",
            (b"CMAKE_INSTALL_LIBDIR", b"lib"),
        )),
        TargetKind::SharedLibrary => Some((
            b"LIBRARY",
            b"SHARED_LIBRARY",
            (b"CMAKE_INSTALL_LIBDIR", b"lib"),
        )),
        TargetKind::ModuleLibrary => Some((
            b"LIBRARY",
            b"MODULE_LIBRARY",
            (b"CMAKE_INSTALL_LIBDIR", b"lib"),
        )),
        _ => None,
    }
}

fn artifact_paths<'a>(
    artifacts: &'a crate::ArtifactSet,
    group: Option<&Group<'_>>,
) -> IndexSet<&'a BString> {
    let mut paths = IndexSet::new();
    let has_namelink = artifacts
        .soname_file
        .as_ref()
        .zip(artifacts.linker_file.as_ref())
        .is_some_and(|(soname, linker)| soname != linker);
    if !group.is_some_and(|group| group.namelink_only) {
        paths.extend(artifacts.file.iter().chain(&artifacts.soname_file));
    }
    if group.is_none_or(|group| !group.namelink_skip && (!group.namelink_only || has_namelink)) {
        paths.extend(&artifacts.linker_file);
    }
    paths
}

fn recorded_args(model: &ProjectModel, rule: usize) -> Vec<&[u8]> {
    let section = format!("_RCMAKE_INSTALL_{rule}_ARG");
    (0..)
        .map_while(|index| {
            model
                .global
                .props
                .get(format!("{section}{index}").as_bytes())
                .map(|v| v.as_slice())
        })
        .collect()
}

#[derive(Default)]
struct Group<'a> {
    options: InstallOptions,
    namelink_only: bool,
    namelink_skip: bool,
    namelink_component: Option<&'a [u8]>,
}

impl Group<'_> {
    fn merge(&self, base: &InstallOptions) -> InstallOptions {
        let mut options = base.clone();
        let group = &self.options;
        options.destination = group.destination.clone().or(options.destination);
        options.component = group.component.clone().or(options.component);
        options.rename = group.rename.clone().or(options.rename);
        if !group.permissions.is_empty() {
            options.permissions.clone_from(&group.permissions);
        }
        if !group.configurations.is_empty() {
            options.configurations.clone_from(&group.configurations);
        }
        options.optional |= group.optional;
        options
    }
}

fn group_key(key: &[u8]) -> bool {
    matches!(
        key,
        b"ARCHIVE"
            | b"LIBRARY"
            | b"RUNTIME"
            | b"OBJECTS"
            | b"FRAMEWORK"
            | b"BUNDLE"
            | b"PRIVATE_HEADER"
            | b"PUBLIC_HEADER"
            | b"RESOURCE"
            | b"FILE_SET"
            | b"CXX_MODULES_BMI"
    )
}

fn target_option(key: &[u8]) -> bool {
    group_key(key)
        || matches!(
            key,
            b"EXPORT"
                | b"INCLUDES"
                | b"DESTINATION"
                | b"COMPONENT"
                | b"PERMISSIONS"
                | b"CONFIGURATIONS"
                | b"OPTIONAL"
                | b"EXCLUDE_FROM_ALL"
                | b"NAMELINK_ONLY"
                | b"NAMELINK_SKIP"
                | b"NAMELINK_COMPONENT"
        )
}

fn target_options<'a>(recorded: &[&'a [u8]]) -> IndexMap<&'a [u8], Group<'a>> {
    let mut groups = IndexMap::new();
    let mut group = None;
    let mut args = recorded.iter().copied().skip(1).peekable();
    while let Some(key) = args.next() {
        if group_key(key) {
            group = Some(key);
            if key == b"FILE_SET" {
                args.next();
            }
            continue;
        }
        if key == b"INCLUDES" {
            args.next();
            while args.next_if(|arg| !target_option(arg)).is_some() {}
            continue;
        }
        let mut read = || args.next().map(BString::from);
        let entry: &mut Group<'_> = match group {
            Some(group) => groups.entry(group).or_default(),
            None => {
                if matches!(key, b"EXPORT" | b"DESTINATION" | b"COMPONENT") {
                    read();
                }
                continue;
            }
        };
        match key {
            b"DESTINATION" => {
                entry.options.destination = read().map(|v| Lexical::new(&v).normalize())
            }
            b"COMPONENT" => entry.options.component = read(),
            b"OPTIONAL" => entry.options.optional = true,
            b"NAMELINK_ONLY" => entry.namelink_only = true,
            b"NAMELINK_SKIP" => entry.namelink_skip = true,
            b"NAMELINK_COMPONENT" => entry.namelink_component = args.next(),
            b"PERMISSIONS" | b"CONFIGURATIONS" => {
                let values = std::iter::from_fn(|| {
                    args.next_if(|arg| !target_option(arg)).map(BString::from)
                })
                .collect();
                if key == b"PERMISSIONS" {
                    entry.options.permissions = values;
                } else {
                    entry.options.configurations = values;
                }
            }
            _ => {}
        }
    }
    groups
}

fn directory_options(recorded: &[&[u8]], permissions: &[BString]) -> Vec<BString> {
    let mut options = Vec::new();
    if !permissions.is_empty() {
        options.push("FILE_PERMISSIONS".into());
        options.extend_from_slice(permissions);
    }
    let mut args = recorded.iter().copied().skip(1).peekable();
    while let Some(key) = args.next() {
        match key {
            b"DESTINATION" | b"TYPE" | b"COMPONENT" | b"RENAME" => {
                args.next();
            }
            b"PATTERN" | b"REGEX" => {
                options.push(key.into());
                options.extend(args.map(BString::from));
                break;
            }
            b"FILES_MATCHING" | b"USE_SOURCE_PERMISSIONS" | b"MESSAGE_NEVER" => {
                options.push(key.into())
            }
            b"DIRECTORY_PERMISSIONS" => {
                options.push(key.into());
                while let Some(permission) = args.next_if(|arg| permission_keyword(arg)) {
                    options.push(permission.into());
                }
            }
            _ => {}
        }
    }
    options
}

fn permission_keyword(value: &[u8]) -> bool {
    value.starts_with(b"OWNER_")
        || value.starts_with(b"GROUP_")
        || value.starts_with(b"WORLD_")
        || matches!(value, b"SETUID" | b"SETGID")
}
