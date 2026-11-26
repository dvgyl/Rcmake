use indexmap::{IndexMap, IndexSet};
use rcmake_base::truth;
use rcmake_diag::{
    DiagAction, DiagCategory, Diagnostic, Effect, Location, MessageKind, RenderOptions,
};
use rcmake_host::{Host, Stream};
use rcmake_model::{InstallKind, Target, TargetId, TargetKind};

use super::super::compute_artifacts;
use super::projection::{Projection, file_path};
use super::{
    BString, Backtrace, ConfigId, DirId, EvaluateInstall, GeneratorKind, Group, InstallAction,
    InstallActionKind, InstallManifest, InstallObjectName, InstallOptions, InstallRule, Lexical,
    ProjectModel, ResolveError, ResolvedTarget, RuntimeEdits, action_configs, add_bmi_include,
    definition, eval_destination, file_action, join, list, matches_config, target_options,
};
use crate::NamelinkMode;

pub(super) fn warn_framework_change(
    model: &ProjectModel,
    index: usize,
    rule: &InstallRule,
    host: &dyn Host,
) -> Result<(), crate::ResolveError> {
    let InstallKind::Targets { targets, .. } = &rule.kind else {
        return Ok(());
    };
    let category = DiagCategory::Author;
    let action = model
        .global
        .props
        .get(format!("_RCMAKE_INSTALL_{index}_AUTHOR_ACTION").as_bytes())
        .and_then(|value| DiagAction::parse(value))
        .unwrap_or_else(|| category.default_action());
    if action == DiagAction::Ignore {
        return Ok(());
    }
    for target in targets
        .iter()
        .filter_map(|&id| model.target(id).map(|target| (id, target)))
    {
        let (id, target) = target;
        if !super::super::is_darwin(model, target.dir)
            || !framework_added_after_install(model, index, id, target)
        {
            continue;
        }
        let error = action > DiagAction::Warn;
        let body: BString = format!(
            "Target '{}' was changed to a FRAMEWORK sometime after install().  This may result in the wrong install DESTINATION.  Set the FRAMEWORK property earlier.",
            target.name,
        ).into();
        let diagnostic = Diagnostic {
            annotations: None,
            kind: if error {
                MessageKind::AuthorError
            } else {
                MessageKind::AuthorWarning
            },
            effect: Effect::None,
            category: Some(category),
            policy: None,
            location: Location::Backtrace(
                rule.backtrace
                    .frames()
                    .filter(|frame| frame.name.is_some())
                    .collect::<Vec<_>>()
                    .into_iter()
                    .rev()
                    .fold(Backtrace::default(), |trace, frame| {
                        trace.push(frame.clone())
                    }),
            ),
            body: body.clone(),
        };
        host.write_out(
            Stream::Stderr,
            &rcmake_diag::render(
                &diagnostic,
                &RenderOptions {
                    relative_base: Some(model.dir(DirId::ROOT).source_dir.clone()),
                    ..RenderOptions::default()
                },
            ),
        );
        if error {
            return Err(crate::ResolveError::Reported(body));
        }
    }
    Ok(())
}

fn framework_added_after_install(
    model: &ProjectModel,
    index: usize,
    id: TargetId,
    target: &Target,
) -> bool {
    matches!(
        crate::ide::bundle_kind(target),
        crate::ide::BundleKind::Framework { .. }
    ) && model
        .global
        .props
        .get(format!("_RCMAKE_INSTALL_{index}_FRAMEWORK_{}", id.index()).as_bytes())
        .is_some_and(|value| !truth::is_on(value))
}

type TargetSelection<'a> = (
    &'a [TargetId],
    Option<&'a IndexMap<TargetId, IndexSet<ConfigId>>>,
);

pub(super) fn lower(
    model: &ProjectModel,
    (index, rule): (usize, &InstallRule),
    (targets, configs, (ids, dependencies), edits, strip): (
        &[ResolvedTarget],
        &[ConfigId],
        TargetSelection<'_>,
        &RuntimeEdits,
        &mut super::strip::Tools<'_>,
    ),
    recorded: &[&[u8]],
    manifest: &mut InstallManifest,
    make_action: &impl Fn(&InstallOptions, &[u8], InstallActionKind) -> InstallAction,
    (eval, object_name): (&mut EvaluateInstall<'_>, &mut InstallObjectName<'_>),
) -> Result<(), ResolveError> {
    let options = &rule.options;
    let multi = configs.len() > 1 || model.generator.is_some_and(GeneratorKind::multi_config);
    let source = &model.dir(rule.dir).source_dir;
    let groups = target_options(recorded);
    let roots = ids.iter().map(|id| (id, None));
    let dependencies = dependencies
        .into_iter()
        .flatten()
        .map(|(id, configs)| (id, Some(configs)));
    for (id, dependency_configs) in roots.chain(dependencies) {
        let runtime_only = dependency_configs.is_some();
        let Some(raw) = model.target(*id).filter(|raw| !raw.is_imported()) else {
            continue;
        };
        let matching: Vec<_> = targets
            .iter()
            .filter(|t| {
                t.id == *id && dependency_configs.is_none_or(|configs| configs.contains(&t.config))
            })
            .collect();
        if raw.kind == TargetKind::ObjectLibrary {
            let object_options = groups
                .get(b"OBJECTS".as_slice())
                .map_or_else(|| options.clone(), |group| group.merge(options));
            if object_options
                .destination
                .as_ref()
                .is_none_or(|value| value.is_empty())
            {
                continue;
            }
            for target in &matching {
                if !matches_config(&object_options, &target.config) {
                    continue;
                }
                let mut options = object_options.clone();
                eval_destination(&mut options, raw.dir, &target.config, rule, eval)?;
                let destination = options
                    .destination
                    .as_deref()
                    .map_or(b"".as_slice(), Vec::as_slice);
                let projection = Projection {
                    rule: index,
                    target: *id,
                    config: &target.config,
                };
                let mut paths = Vec::new();
                let object_directory = super::super::artifacts::object_install_directory(
                    model,
                    raw,
                    &target.config,
                    destination,
                );
                for unit in &target.compile_units {
                    let custom_name = object_name(raw.dir, &target.config, &unit.source.value)?;
                    let object = super::objects::ObjectPaths::new(
                        model,
                        raw,
                        unit,
                        &object_directory,
                        &custom_name,
                    );
                    paths.push(object.installer_path(destination));
                    let mut action = object.action(&options, make_action);
                    action.configurations = vec![target.config.0.clone()];
                    manifest.actions.push(action);
                }
                let action =
                    make_action(&options, b"", InstallActionKind::Code(BString::default()));
                projection.add(manifest, &action, false, NamelinkMode::None, paths);
            }
            continue;
        }
        for target in &matching {
            let per_config = multi || matching.len() > 1;
            let projection = Projection {
                rule: index,
                target: *id,
                config: &target.config,
            };
            if raw.kind == TargetKind::SharedLibrary
                && super::super::artifacts::is_dll_platform(model, raw.dir)
            {
                if !runtime_only
                    && groups
                        .get(b"LIBRARY".as_slice())
                        .is_some_and(|group| group.namelink_only)
                {
                    continue;
                }
                let artifacts = &target.artifacts;
                let runtime = groups
                    .get(b"RUNTIME".as_slice())
                    .map_or_else(|| options.clone(), |group| group.merge(options));
                let archive = groups
                    .get(b"ARCHIVE".as_slice())
                    .map_or_else(|| options.clone(), |group| group.merge(options));
                let has_destination = |options: &InstallOptions| {
                    options
                        .destination
                        .as_ref()
                        .is_some_and(|path| !path.is_empty())
                };
                let explicit = has_destination(&runtime) || has_destination(&archive);
                for (file, mut options, file_type, variable, fallback) in [
                    (
                        &artifacts.import_file,
                        archive,
                        b"STATIC_LIBRARY".as_slice(),
                        b"CMAKE_INSTALL_LIBDIR".as_slice(),
                        b"lib".as_slice(),
                    ),
                    (
                        &artifacts.file,
                        runtime,
                        b"SHARED_LIBRARY".as_slice(),
                        b"CMAKE_INSTALL_BINDIR".as_slice(),
                        b"bin".as_slice(),
                    ),
                ] {
                    let Some(file) = file else { continue };
                    if (runtime_only && file_type == b"STATIC_LIBRARY")
                        || (!runtime_only && explicit && !has_destination(&options))
                        || ((configs.is_empty() || per_config)
                            && !matches_config(&options, &target.config))
                    {
                        continue;
                    }
                    if per_config {
                        options.configurations = vec![target.config.0.clone()];
                    }
                    let default = definition(model, rule.dir, variable)
                        .map_or(fallback, |value| value.as_slice());
                    if !has_destination(&options) {
                        options.destination = Some(default.into());
                    }
                    eval_destination(&mut options, raw.dir, &target.config, rule, eval)?;
                    let path = file_path(&model.dir(DirId::ROOT).binary_dir, file);
                    let kind = file_action(file_type, vec![path.0.clone()], &options);
                    let action = make_action(&options, default, kind);
                    projection.add(
                        manifest,
                        &action,
                        file_type == b"STATIC_LIBRARY",
                        NamelinkMode::None,
                        vec![path],
                    );
                    let strip_action = (file_type == b"SHARED_LIBRARY")
                        .then(|| {
                            strip.action(
                                model,
                                raw,
                                &action,
                                options
                                    .rename
                                    .unwrap_or_else(|| Lexical::new(file).filename().into()),
                            )
                        })
                        .flatten();
                    manifest.actions.push(action);
                    manifest.actions.extend(strip_action);
                }
                continue;
            }
            let layout = if super::super::is_darwin(model, raw.dir) {
                crate::ide::bundle_kind(raw)
            } else {
                crate::ide::BundleKind::None
            };
            let Some((mut class, file_type, fallback)) =
                target_install_kind(target.metadata.kind, &layout)
            else {
                continue;
            };
            if class == b"FRAMEWORK" && framework_added_after_install(model, index, *id, raw) {
                class = if raw.kind == TargetKind::StaticLibrary {
                    b"ARCHIVE"
                } else {
                    b"LIBRARY"
                };
            }
            let extra = groups.get(class);
            let mut options = extra.map_or_else(|| options.clone(), |extra| extra.merge(options));
            let runtime_group = Group {
                namelink_skip: true,
                ..Group::default()
            };
            let extra = if runtime_only {
                Some(&runtime_group)
            } else {
                extra
            };
            if (configs.is_empty() || per_config) && !matches_config(&options, &target.config) {
                continue;
            }
            if per_config {
                options.configurations = vec![target.config.0.clone()];
            }
            let artifacts = if target.artifacts.file.is_some() {
                target.artifacts.clone()
            } else {
                compute_artifacts(model, raw, &target.config)
            };
            let bin = &model.dir(DirId::ROOT).binary_dir;
            let bundle_root = artifacts
                .file
                .as_ref()
                .and_then(|file| layout.root(file))
                .map(BString::from);
            let mut files = if layout == crate::ide::BundleKind::None {
                artifact_paths(&artifacts, extra)
            } else {
                bundle_root.iter().collect()
            };
            let (key, default) = fallback;
            let default = definition(model, rule.dir, key).map_or(default, |v| v.as_slice());
            if !configs.is_empty() {
                options.destination.get_or_insert_with(|| default.into());
                eval_destination(&mut options, raw.dir, &target.config, rule, eval)?;
            }
            if !runtime_only
                && matches!(layout, crate::ide::BundleKind::Framework { .. })
                && groups
                    .get(b"LIBRARY".as_slice())
                    .is_some_and(|group| group.namelink_only)
            {
                continue;
            }
            let projection_action = make_action(
                &options,
                default,
                InstallActionKind::Code(BString::default()),
            );
            if raw.kind == TargetKind::SharedLibrary && layout == crate::ide::BundleKind::None {
                projection.library(
                    manifest,
                    &projection_action,
                    bin,
                    (&artifacts, extra, false),
                );
            } else {
                let paths = if let Some(root) = &bundle_root {
                    let installed = artifacts
                        .file
                        .as_ref()
                        .and_then(|file| {
                            Lexical::new(file).relative(Lexical::new(Lexical::new(root).parent()))
                        })
                        .unwrap_or_else(|| Lexical::new(root).filename().into());
                    vec![(join(bin, root), installed)]
                } else {
                    files
                        .iter()
                        .map(|file| {
                            let (from, to) = file_path(bin, file);
                            (from, options.rename.clone().unwrap_or(to))
                        })
                        .collect()
                };
                projection.add(
                    manifest,
                    &projection_action,
                    false,
                    NamelinkMode::None,
                    paths,
                );
            }
            let namelink = take_namelink(&artifacts, extra, &mut files);
            if !files.is_empty() {
                let installed = artifacts.file.as_ref().and_then(|file| {
                    if files.contains(file) {
                        let name = options
                            .rename
                            .clone()
                            .unwrap_or_else(|| Lexical::new(file).filename().into());
                        Some(name)
                    } else if let Some(root) =
                        bundle_root.as_ref().filter(|root| files.contains(root))
                    {
                        let relative = Lexical::new(file).relative(Lexical::new(root))?;
                        let name = options
                            .rename
                            .as_deref()
                            .map_or_else(|| Lexical::new(root).filename(), Vec::as_slice);
                        Some(join(name, &relative))
                    } else {
                        None
                    }
                });
                let edit = edits
                    .get(&(*id, target.config.clone()))
                    .zip(installed.as_ref());
                let files = files.into_iter().map(|path| join(bin, path)).collect();
                let action = make_action(
                    &options,
                    default,
                    if layout == crate::ide::BundleKind::None {
                        file_action(file_type, files, &options)
                    } else {
                        InstallActionKind::Directory {
                            dirs: files,
                            options: vec!["USE_SOURCE_PERMISSIONS".into()],
                        }
                    },
                );
                let configured = !action.configurations.is_empty();
                let as_code = |code| InstallAction {
                    kind: InstallActionKind::Code(code),
                    ..action.clone()
                };
                let (check, change) = edit.map_or((None, None), |(edit, name)| {
                    let check = edit
                        .check(&action.destination, name, configured)
                        .map(as_code);
                    let change = as_code(edit.render(&action.destination, name, configured));
                    (check, Some(change))
                });
                let strip_action = if matches!(layout, crate::ide::BundleKind::App { .. }) {
                    None
                } else {
                    installed.and_then(|name| strip.action(model, raw, &action, name))
                };
                manifest.actions.extend(check);
                manifest.actions.push(action);
                manifest.actions.extend(change);
                manifest.actions.extend(strip_action);
            }
            if !runtime_only
                && layout == crate::ide::BundleKind::None
                && let Some(stub) = &artifacts.import_file
                && stub.ends_with(b".tbd")
            {
                let group = groups.get(b"ARCHIVE".as_slice());
                let mut stub_options =
                    group.map_or_else(|| rule.options.clone(), |group| group.merge(&rule.options));
                if matches_config(&stub_options, &target.config) {
                    stub_options.optional = true;
                    if per_config {
                        stub_options.configurations = vec![target.config.0.clone()];
                    }
                    eval_destination(&mut stub_options, raw.dir, &target.config, rule, eval)?;
                    let usage = crate::usage::UsageResolver::new(model);
                    let path = |kind| {
                        usage.artifact(*id, kind, rcmake_genex::ArtifactPart::Full, &target.config)
                    };
                    let stubs = crate::ArtifactSet {
                        file: Some(stub.clone()),
                        soname_file: path(rcmake_genex::ArtifactKind::SonameImportFile),
                        linker_file: path(rcmake_genex::ArtifactKind::LinkerImportFile),
                        ..Default::default()
                    };
                    let stub_action = make_action(
                        &stub_options,
                        default,
                        InstallActionKind::Code(BString::default()),
                    );
                    projection.library(manifest, &stub_action, bin, (&stubs, group, true));
                    let mut files = artifact_paths(&stubs, group);
                    let namelink = take_namelink(&stubs, group, &mut files);
                    if !files.is_empty() {
                        let kind = file_action(
                            b"STATIC_LIBRARY",
                            files.into_iter().map(|path| join(bin, path)).collect(),
                            &stub_options,
                        );
                        manifest
                            .actions
                            .push(make_action(&stub_options, default, kind));
                    }
                    if let Some((component, linker)) = namelink {
                        stub_options.component = Some(component.into());
                        let kind =
                            file_action(b"STATIC_LIBRARY", vec![join(bin, linker)], &stub_options);
                        manifest
                            .actions
                            .push(make_action(&stub_options, default, kind));
                    }
                }
            }
            if let Some((component, file)) = namelink {
                options.component = Some(component.into());
                let kind = file_action(file_type, vec![join(bin, file)], &options);
                manifest.actions.push(make_action(&options, default, kind));
            }
        }
        if runtime_only {
            continue;
        }
        if matches!(
            model.generator,
            Some(GeneratorKind::Ninja | GeneratorKind::NinjaMultiConfig)
        ) {
            let bmi = groups
                .get(b"CXX_MODULES_BMI".as_slice())
                .map_or_else(|| options.clone(), |group| group.merge(options));
            if bmi
                .destination
                .as_ref()
                .is_some_and(|destination| !destination.is_empty())
            {
                for target in &matching {
                    if target
                        .compile_groups
                        .iter()
                        .any(|group| group.language == b"CXX")
                    {
                        let mut bmi = bmi.clone();
                        eval_destination(&mut bmi, raw.dir, &target.config, rule, eval)?;
                        add_bmi_include(
                            manifest,
                            model,
                            rule,
                            &bmi,
                            target,
                            multi || matching.len() > 1,
                        );
                    }
                }
            }
        }
        if matches!(
            crate::ide::bundle_kind(raw),
            crate::ide::BundleKind::Framework { .. }
        ) {
            continue;
        }
        for key in [b"PUBLIC_HEADER".as_slice(), b"PRIVATE_HEADER", b"RESOURCE"] {
            let property = String::from_utf8_lossy(key);
            let snapshot = format!("_RCMAKE_INSTALL_{index}_{property}_{}", id.index());
            let Some(headers) = model
                .global
                .props
                .get(snapshot.as_bytes())
                .or_else(|| raw.props.get(key))
            else {
                continue;
            };
            let files: Vec<_> = list::split(headers, list::Empty::Drop)
                .iter()
                .map(|path| join(source, path))
                .collect();
            if !files.is_empty() {
                let options = groups
                    .get(key)
                    .map_or_else(|| options.clone(), |extra| extra.merge(options));
                if key != b"PUBLIC_HEADER" && options.destination.is_none() {
                    continue;
                }
                let default = definition(model, rule.dir, b"CMAKE_INSTALL_INCLUDEDIR")
                    .map_or(b"include".as_slice(), |value| value.as_slice());
                let mut options = options;
                if !configs.is_empty() {
                    options.destination.get_or_insert_with(|| default.into());
                }
                for config in action_configs(configs, &options, false, multi) {
                    let mut options = options.clone();
                    if let Some(config) = config {
                        eval_destination(&mut options, rule.dir, config, rule, eval)?;
                        if multi {
                            options.configurations = vec![config.0.clone()];
                        }
                    }
                    let kind = file_action(b"FILE", files.clone(), &options);
                    manifest.actions.push(make_action(&options, default, kind));
                }
            }
        }
    }
    Ok(())
}

type TargetInstallKind = (&'static [u8], &'static [u8], (&'static [u8], &'static [u8]));

fn target_install_kind(
    kind: TargetKind,
    layout: &crate::ide::BundleKind,
) -> Option<TargetInstallKind> {
    use crate::ide::BundleKind;
    match layout {
        BundleKind::Framework { .. } => {
            return Some((
                b"FRAMEWORK",
                b"DIRECTORY",
                (b"CMAKE_INSTALL_LIBDIR", b"lib"),
            ));
        }
        BundleKind::App { .. } => {
            return Some((b"BUNDLE", b"DIRECTORY", (b"CMAKE_INSTALL_BINDIR", b"bin")));
        }
        _ => {}
    }
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

fn take_namelink<'a, 'g>(
    artifacts: &'a crate::ArtifactSet,
    group: Option<&Group<'g>>,
    files: &mut IndexSet<&'a BString>,
) -> Option<(&'g [u8], &'a BString)> {
    let component = group?.namelink_component?;
    let linker = artifacts.linker_file.as_ref()?;
    let soname = artifacts.soname_file.as_ref()?;
    (soname != linker && files.shift_remove(linker)).then_some((component, linker))
}
