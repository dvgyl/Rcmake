use indexmap::{IndexMap, IndexSet};
use rcmake_base::{BString, list, path_lex::Lexical};
use rcmake_diag::{Diagnostic, Effect, MessageKind, RenderOptions, render};
use rcmake_genex::{ConfigId, EvalContext, PreprocessMode, evaluate, parse};
use rcmake_model::{DirId, FileSet, FileSetKind, InstallKind, ProjectModel, TargetId, UsageScope};

use crate::{
    ExportedSourceSet, ResolveError,
    cxx_modules::generation_location,
    file_sets::{options_for_set, set_files},
    usage::UsageResolver,
};

use super::export_property_items;

pub(crate) fn validate(model: &ProjectModel) -> Result<(), ResolveError> {
    for rule in &model.installs {
        let InstallKind::Export { set, .. } = &rule.kind else {
            continue;
        };
        let Some(export) = model.exports.get(set) else {
            continue;
        };
        for &id in &export.targets {
            let Some(target) = model.target(id) else {
                continue;
            };
            for source_set in target.file_sets.iter().filter_map(|id| model.file_set(*id)) {
                if source_set.kind != FileSetKind::Sources
                    || source_set.scope == UsageScope::Private
                    || export
                        .install_rules
                        .iter()
                        .any(|&index| install_options(model, index, source_set).is_some())
                {
                    continue;
                }
                let diagnostic = Diagnostic { annotations: None,
                    kind: MessageKind::Error,
                    effect: Effect::SetError,
                    category: None,
                    policy: None,
                    location: generation_location(model, target),
                    body: format!(
                        "File set \"{}\" is listed in interface file sets of {} but has not been exported",
                        source_set.name, target.name
                    ).into(),
                };
                return Err(ResolveError::Message(render(
                    &diagnostic,
                    &RenderOptions {
                        relative_base: Some(model.dir(DirId::ROOT).source_dir.clone()),
                        ..Default::default()
                    },
                )));
            }
        }
    }
    Ok(())
}

pub(super) fn export(
    model: &ProjectModel,
    usage: &mut UsageResolver<'_>,
    configs: &[ConfigId],
    id: TargetId,
    install: Option<&[usize]>,
) -> Result<Vec<ExportedSourceSet>, ResolveError> {
    let Some(target) = model.target(id) else {
        return Ok(Vec::new());
    };
    target
        .file_sets
        .iter()
        .filter_map(|id| model.file_set(*id))
        .filter(|set| {
            matches!(set.kind, FileSetKind::Sources | FileSetKind::Headers)
                && set.scope != UsageScope::Private
        })
        .map(|set| {
            let mut entries = Vec::new();
            for config in configs {
                let destination = if let Some(indices) = install {
                    let Some(destination) =
                        resolve_destination(model, usage, set, config, indices)?
                    else {
                        continue;
                    };
                    Some(destination)
                } else {
                    None
                };
                let mut bases = IndexSet::new();
                let mut files = IndexSet::new();
                for (base, members) in set_files(model, usage, target, set, config)? {
                    bases.insert(destination.clone().unwrap_or(base));
                    files.extend(members.into_iter().map(|(source, relative)| {
                        destination
                            .as_ref()
                            .map_or(source, |base| Lexical::new(base).append(&relative))
                    }));
                }
                entries.push((config, bases, files));
            }
            let base_paths = collect_paths(&entries, |(_, bases, _)| bases, install.is_some());
            let files = collect_paths(&entries, |(_, _, files)| files, install.is_some());
            Ok(ExportedSourceSet {
                kind: set.kind,
                name: set.name.clone(),
                bases: base_paths,
                files,
                properties: properties(set, install.is_some())?,
            })
        })
        .collect()
}

type FileSetEntries<'a> = (&'a ConfigId, IndexSet<BString>, IndexSet<BString>);

fn collect_paths(
    entries: &[FileSetEntries<'_>],
    select: impl for<'a> Fn(&'a FileSetEntries<'_>) -> &'a IndexSet<BString>,
    install: bool,
) -> Vec<BString> {
    let Some(first) = entries.first().map(&select) else {
        return Vec::new();
    };
    let as_path = |path: &BString| {
        if install && !Lexical::new(path).absolute() {
            BString::from([b"${_IMPORT_PREFIX}/".as_slice(), path.as_slice()].concat())
        } else {
            path.clone()
        }
    };
    if entries.iter().all(|entry| select(entry) == first) {
        return first.iter().map(as_path).collect();
    }
    entries
        .iter()
        .flat_map(|entry| {
            select(entry).iter().map(|path| {
                let config = &entry.0.0;
                let path = as_path(path);
                format!("$<$<CONFIG:{config}>:{path}>").into()
            })
        })
        .collect()
}

fn properties(set: &FileSet, install: bool) -> Result<IndexMap<BString, BString>, ResolveError> {
    let mode = if install {
        PreprocessMode::Install
    } else {
        PreprocessMode::Build
    };
    let mut properties = IndexMap::new();
    for key in set
        .props
        .names()
        .into_iter()
        .filter(|key| key == b"LANGUAGE" || key.starts_with(b"INTERFACE_"))
    {
        let Some(value) = set.props.get(&key) else {
            continue;
        };
        let mut items = export_property_items(value, mode)?;
        if install
            && matches!(
                key.as_slice(),
                b"INTERFACE_INCLUDE_DIRECTORIES" | b"INTERFACE_SYSTEM_INCLUDE_DIRECTORIES"
            )
        {
            for item in &mut items {
                if !Lexical::new(item).absolute() && !item.starts_with(b"$") {
                    *item = [b"${_IMPORT_PREFIX}/".as_slice(), item.as_slice()]
                        .concat()
                        .into();
                }
            }
        }
        if !items.is_empty() {
            properties.insert(key, list::join(items));
        }
    }
    Ok(properties)
}

fn install_options(
    model: &ProjectModel,
    index: usize,
    set: &FileSet,
) -> Option<(DirId, rcmake_model::InstallOptions)> {
    let rule = model.installs.get(index)?;
    let InstallKind::Targets { targets, .. } = &rule.kind else {
        return None;
    };
    if !targets.contains(&set.target) {
        return None;
    }
    Some((rule.dir, options_for_set(model, index, &set.name)?))
}

fn resolve_destination(
    model: &ProjectModel,
    usage: &mut UsageResolver<'_>,
    set: &FileSet,
    config: &ConfigId,
    indices: &[usize],
) -> Result<Option<BString>, ResolveError> {
    for &index in indices {
        let Some((dir, options)) = install_options(model, index, set) else {
            continue;
        };
        if !options.configurations.is_empty()
            && !options
                .configurations
                .iter()
                .any(|value| config.matches(value))
        {
            continue;
        }
        let destination = options.destination.or_else(|| {
            (set.kind == FileSetKind::Headers).then(|| {
                model
                    .definition(dir, b"CMAKE_INSTALL_INCLUDEDIR")
                    .cloned()
                    .unwrap_or_else(|| "include".into())
            })
        });
        if let Some(destination) = destination {
            let ctx = EvalContext {
                config: config.clone(),
                ..EvalContext::default()
            };
            return Ok(Some(
                evaluate(&parse(&destination)?, &ctx, &mut usage.host(dir))?.0,
            ));
        }
    }
    Ok(None)
}
