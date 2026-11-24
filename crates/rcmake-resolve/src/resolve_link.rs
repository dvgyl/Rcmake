use std::sync::Arc;

use indexmap::{IndexMap, IndexSet};
use rcmake_base::{BString, list, path_lex::Lexical, shell::quote};
use rcmake_genex::{ConfigId, EvalContext};
use rcmake_host::path::to_native;
use rcmake_model::{BT, DirId, ProjectModel, Target, TargetId, UsageField};

use crate::{
    LinkFragment, LinkFragmentRole, LinkKind, LinkStep, ResolveError, ResolvedTarget, ide, lower,
    sources::{SourcePaths, TargetInputs},
    usage::{ResolvedLinkItem, ResolvedLinkKind, UsageResolver},
};

#[derive(Default)]
pub(crate) struct ResolvedLink {
    step: Option<LinkStep>,
    position_independent: Option<bool>,
    dependencies: Vec<TargetId>,
    module_links: Vec<TargetId>,
    objects: IndexSet<BString>,
    ide: Option<IdeLinkSettings>,
}

struct IdeLinkSettings {
    items: Vec<ide::IdeLinkItem>,
    options: Vec<BT<BString>>,
    directories: Vec<BString>,
    rpaths: Vec<BString>,
    install_name_dir: Option<BString>,
}

pub(crate) fn resolve_link(
    model: &ProjectModel,
    usage: &mut UsageResolver<'_>,
    paths: &SourcePaths<'_>,
    resolved: &IndexMap<TargetId, ResolvedTarget>,
    id: TargetId,
    target: &Target,
    ctx: &EvalContext,
) -> Result<ResolvedLink, ResolveError> {
    let facts = resolved
        .get(&id)
        .ok_or_else(|| ResolveError::Message("Missing resolved target.".into()))?;
    let languages = std::iter::once(facts)
        .chain(facts.dependencies.iter().filter_map(|id| resolved.get(id)))
        .flat_map(|facts| {
            facts
                .compile_groups
                .iter()
                .map(|group| group.language.as_slice())
        });
    let mut ctx = EvalContext {
        link_language: lower::linker_language(model, target, languages.clone()),
        ..ctx.clone()
    };
    let kind = LinkKind::for_target(target.kind);
    let mut items = Vec::new();
    if kind.is_some() {
        items = usage.ordered_links(id, &ctx)?;
        let language_for = |items: &[Arc<ResolvedLinkItem>]| {
            let linked_languages = link_items(items)
                .filter_map(|item| match item.kind {
                    ResolvedLinkKind::Objects(id) => resolved.get(&id),
                    _ => None,
                })
                .flat_map(|facts| {
                    facts
                        .compile_groups
                        .iter()
                        .map(|group| group.language.as_slice())
                });
            lower::linker_language(model, target, languages.clone().chain(linked_languages))
        };
        let language = language_for(&items);
        if language != ctx.link_language {
            ctx.link_language = language;
            items = usage.ordered_links(id, &ctx)?;
            if language_for(&items) != ctx.link_language {
                return Err(ResolveError::Message(
                    format!(
                        "The link language changed during link resolution for target {}.",
                        target.name
                    )
                    .into(),
                ));
            }
        }
    }
    let linked_targets = usage.link_targets(id, &ctx)?;
    let position_independent = usage.validate_compatible(id, &ctx, paths)?;
    let mut link = ResolvedLink {
        position_independent,
        dependencies: target
            .utility_deps
            .iter()
            .filter_map(|dep| model.find_target(target.dir, &dep.value))
            .chain(linked_targets.iter().copied())
            .chain(facts.dependencies.iter().copied())
            .chain(link_items(&items).filter_map(|item| paths.object_owner(&item.text.value)))
            .filter(|dep| *dep != id && resolved.contains_key(dep))
            .collect::<IndexSet<_>>()
            .into_iter()
            .collect(),
        module_links: if facts.cxx_modules.is_some() {
            linked_targets
                .into_iter()
                .chain(facts.dependencies.iter().copied())
                .collect()
        } else {
            Vec::new()
        },
        ..ResolvedLink::default()
    };
    let Some(kind) = kind else {
        return Ok(link);
    };
    let language = ctx.link_language.clone().ok_or_else(|| {
        ResolveError::Message(
            [
                b"Cannot determine link language for target ",
                target.name.as_slice(),
            ]
            .concat()
            .into(),
        )
    })?;
    let options = usage.collect(id, UsageField::LinkOptions, &ctx)?.entries;
    let directories = usage
        .collect(id, UsageField::LinkDirectories, &ctx)?
        .entries;
    let mut fragments = Vec::with_capacity(options.len() + directories.len() + items.len());
    for option in &options {
        let value = option.value.as_slice();
        let text = value
            .strip_prefix(b"SHELL:")
            .map_or_else(|| quote(value), BString::from);
        fragments.push(link_fragment(text, option, LinkFragmentRole::Flags));
    }
    for directory in &directories {
        let text = quote(&[b"-L", directory.value.as_slice()].concat());
        fragments.push(link_fragment(
            text,
            directory,
            LinkFragmentRole::LibraryPath,
        ));
    }
    for item in &items {
        append_link_item(
            model,
            resolved,
            target.dir,
            &ctx.config,
            item,
            &mut fragments,
        );
    }
    fragments.sort_by_key(|fragment| fragment.role == LinkFragmentRole::Libraries);
    for item in link_items(&items) {
        if let ResolvedLinkKind::Objects(id) = item.kind {
            if let Some(facts) = resolved.get(&id) {
                link.objects
                    .extend(facts.compile_units.iter().map(|unit| unit.object.clone()));
            } else if let Some(objects) = usage.target_objects(id, &ctx.config, None) {
                link.objects
                    .extend(list::split(&objects, list::Empty::Drop));
            }
        }
    }
    let output = facts
        .artifacts
        .file
        .as_ref()
        .map_or_else(BString::default, |path| build_path(model, path));
    let step = LinkStep {
        kind,
        language,
        fragments,
        output,
    };
    if let Some(flavor) = usage.ide() {
        let items = ide::collect_link_items(usage, model, &ctx.config, link_items(&items));
        let (rpaths, install_name_dir) = if flavor == ide::IdeFlavor::Xcode {
            let libraries = items
                .iter()
                .filter_map(|item| match item {
                    ide::IdeLinkItem::File(path) => Some(path.clone()),
                    ide::IdeLinkItem::Target(id) => resolved.get(id)?.artifacts.linker_file.clone(),
                    _ => None,
                })
                .collect::<Vec<_>>();
            let rpaths = lower::ide_rpaths(model, target, id, &ctx, usage, &step, &libraries)?;
            let install_name_dir = (kind == LinkKind::SharedLibrary)
                .then(|| lower::install_name_dir(model, target, &facts.artifacts))
                .map(|dir| usage.eval_ide_property(id, b"INSTALL_NAME_DIR", &dir, &ctx))
                .transpose()?;
            (rpaths, install_name_dir)
        } else {
            (Vec::new(), None)
        };
        link.ide = Some(IdeLinkSettings {
            items,
            options,
            directories: directories.into_iter().map(|entry| entry.value).collect(),
            rpaths,
            install_name_dir,
        });
    }
    link.step = Some(step);
    Ok(link)
}

pub(crate) fn attach_links(
    resolved: &mut IndexMap<TargetId, ResolvedTarget>,
    inputs: &mut IndexMap<TargetId, TargetInputs>,
    links: Vec<ResolvedLink>,
    std_id: Option<TargetId>,
) {
    let std_objects = std_id
        .and_then(|id| resolved.get(&id))
        .and_then(|target| Some((target, target.cxx_modules.as_ref()?)))
        .into_iter()
        .flat_map(|(target, info)| {
            info.units
                .iter()
                .filter(|unit| !unit.bmi_only)
                .filter_map(|unit| target.compile_units.get(unit.unit))
                .map(|unit| unit.object.clone())
        })
        .collect::<Vec<_>>();
    let module_dirs: IndexMap<_, _> = resolved
        .values()
        .filter_map(|target| {
            let info = target.cxx_modules.as_ref()?;
            (!info.units.is_empty()).then(|| (target.id, info.module_dir.clone()))
        })
        .collect();
    for ((target, inputs), link) in resolved.values_mut().zip(inputs.values_mut()).zip(links) {
        if let (Some(facts), Some(settings)) = (&mut target.ide, link.ide) {
            facts.link_items = settings.items;
            facts.link_options = settings.options;
            facts.link_directories = settings.directories;
            facts.build_rpaths = settings.rpaths;
            facts.install_name_dir = settings.install_name_dir;
        }
        target.link = link.step;
        target.dependencies = link.dependencies;
        inputs.position_independent = link.position_independent;
        inputs.objects.extend(link.objects);
        if let Some(info) = &mut target.cxx_modules {
            let mut linked = link.module_links.into_iter().collect::<IndexSet<_>>();
            if let Some(id) = std_id
                .filter(|id| *id != target.id && target.compile_units.iter().any(|unit| unit.scan))
            {
                linked.insert(id);
                target.dependencies.push(id);
                inputs.objects.extend(std_objects.iter().cloned());
            }
            info.linked_target_dirs.extend(
                linked
                    .iter()
                    .filter(|id| **id != target.id)
                    .filter_map(|id| module_dirs.get(id).cloned()),
            );
        }
    }
}

fn link_items(items: &[Arc<ResolvedLinkItem>]) -> impl Iterator<Item = &ResolvedLinkItem> {
    items.iter().flat_map(|item| match &item.kind {
        ResolvedLinkKind::Group { members, .. } => members.as_slice(),
        _ => std::slice::from_ref(item.as_ref()),
    })
}

fn append_link_item(
    model: &ProjectModel,
    resolved: &IndexMap<TargetId, ResolvedTarget>,
    dir: DirId,
    config: &ConfigId,
    item: &ResolvedLinkItem,
    fragments: &mut Vec<LinkFragment>,
) {
    use LinkFragmentRole::Libraries;
    let value = item.text.value.as_slice();
    let (text, role) = match &item.kind {
        ResolvedLinkKind::Library(id) => {
            let Some(raw) = model.target(*id) else { return };
            let imported;
            let artifacts = if let Some(target) = resolved.get(id) {
                &target.artifacts
            } else {
                imported = lower::compute_artifacts(model, raw, config);
                &imported
            };
            let path = artifacts.linker_file.as_ref().or(artifacts.file.as_ref());
            if let Some(path) = path {
                append_library_path(model, dir, path, &item.text, fragments);
                return;
            }
            (raw_library(value), Libraries)
        }
        ResolvedLinkKind::Objects(_) | ResolvedLinkKind::Interface(_) => return,
        ResolvedLinkKind::Group { members, .. } => {
            for member in members {
                append_link_item(model, resolved, dir, config, member, fragments);
            }
            return;
        }
        ResolvedLinkKind::ObjectFile => (quote(value), Libraries),
        ResolvedLinkKind::Raw if value.contains(&b'/') && !value.starts_with(b"-") => {
            append_library_path(model, dir, value, &item.text, fragments);
            return;
        }
        ResolvedLinkKind::Raw if is_framework_option(value) && lower::is_darwin(model, dir) => {
            (value.into(), Libraries)
        }
        ResolvedLinkKind::Raw => (raw_library(value), Libraries),
    };
    fragments.push(link_fragment(text, &item.text, role));
}

fn is_framework_option(value: &[u8]) -> bool {
    matches!(
        value.split(|byte| byte.is_ascii_whitespace()).next(),
        Some(
            b"-framework"
                | b"-weak_framework"
                | b"-needed_framework"
                | b"-reexport_framework"
                | b"-lazy_framework"
        )
    )
}

fn append_library_path(
    model: &ProjectModel,
    dir: DirId,
    path: &[u8],
    origin: &BT<BString>,
    fragments: &mut Vec<LinkFragment>,
) {
    let path_view = Lexical::new(path);
    if let Some(name) = path_view.filename().strip_suffix(b".framework")
        && !name.is_empty()
    {
        let parent = Lexical::new(path_view.parent()).normalize();
        let implicit = lower::definition(
            model,
            dir,
            b"CMAKE_PLATFORM_IMPLICIT_LINK_FRAMEWORK_DIRECTORIES",
        )
        .is_some_and(|dirs| {
            list::split(dirs, list::Empty::Drop)
                .iter()
                .any(|path| Lexical::new(path).normalize() == parent)
        });
        let directory = quote(&[b"-F", path_view.parent()].concat());
        let role = LinkFragmentRole::FrameworkPath;
        if !implicit
            && !fragments
                .iter()
                .any(|fragment| fragment.role == role && fragment.text.value == directory)
        {
            fragments.push(link_fragment(directory, origin, role));
        }
        let framework = [b"-framework ", quote(name).as_slice()].concat().into();
        fragments.push(link_fragment(
            framework,
            origin,
            LinkFragmentRole::Libraries,
        ));
    } else {
        fragments.push(link_fragment(
            quote(path),
            origin,
            LinkFragmentRole::Libraries,
        ));
    }
}

fn raw_library(value: &[u8]) -> BString {
    if value.starts_with(b"-") || value.contains(&b'/') || value.ends_with(b".lib") {
        quote(value)
    } else {
        quote(&[b"-l", value].concat())
    }
}

fn link_fragment(text: BString, origin: &BT<BString>, role: LinkFragmentRole) -> LinkFragment {
    LinkFragment {
        text: BT::new(text, origin.backtrace.clone()),
        role,
    }
}

fn build_path(model: &ProjectModel, path: &[u8]) -> BString {
    let root = to_native(&model.dir(DirId::ROOT).binary_dir);
    let native = to_native(path);
    native
        .strip_prefix(root)
        .map_or_else(|_| path.into(), rcmake_host::path::from_native)
}
