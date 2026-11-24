use std::sync::Arc;

use indexmap::{IndexMap, IndexSet};
use rcmake_base::{BString, list, path_lex::Lexical, shell::quote};
use rcmake_genex::{ArtifactKind, ArtifactPart, ConfigId, EvalContext};
use rcmake_host::path::to_native;
use rcmake_model::{BT, DirId, ProjectModel, Target, TargetId, UsageField};

use crate::{
    InterfaceInclude, LinkFragment, LinkFragmentRole, LinkKind, LinkStep, ResolveError,
    ResolvedTarget, ide, lower,
    sources::{SourcePaths, TargetInputs},
    usage::{ResolvedLinkItem, ResolvedLinkKind, UsageResolver},
};

mod features;
use features::Features;

#[derive(Default)]
pub(crate) struct ResolvedLink {
    step: Option<LinkStep>,
    position_independent: Option<bool>,
    dependencies: Vec<TargetId>,
    module_links: Vec<TargetId>,
    objects: IndexSet<BString>,
    xcframework_inputs: IndexSet<BString>,
    xcframework_includes: Vec<InterfaceInclude>,
    ide: Option<IdeLinkSettings>,
}

struct IdeLinkSettings {
    items: Vec<ide::IdeLinkItem>,
    xcframeworks: IndexMap<BString, BString>,
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
    crate::usage::link_options::warning_as_error_options(model, target).map_err(|body| {
        let body_text = rcmake_diag::format_paragraphs(&body, 2, 77);
        paths.host.write_out(
            rcmake_host::Stream::Stderr,
            &([b"CMake Error:\n".as_slice(), &body_text, b"\n\n"].concat()),
        );
        ResolveError::Reported(body)
    })?;
    let mut ctx = ctx.clone();
    ctx.link_language = usage.select_link_language(id, target, resolved, &ctx)?;
    let kind = LinkKind::for_target(target.kind);
    let items = if kind.is_some() {
        usage.ordered_links(id, &ctx)?
    } else {
        Vec::new()
    };
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
        let text = if value.starts_with(b"LINKER:") {
            value.into()
        } else {
            value
                .strip_prefix(b"SHELL:")
                .map_or_else(|| quote(value), BString::from)
        };
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
    let mut slices = IndexMap::new();
    if lower::is_darwin(model, target.dir) {
        for item in link_items(&items) {
            if let Some(root) = xcframework_root(usage, model, &ctx.config, item)
                && let indexmap::map::Entry::Vacant(entry) = slices.entry(root.clone())
            {
                let slice = paths
                    .xcframeworks
                    .select(model, paths, target, &root, &item.text)?;
                let wrapper = Lexical::new(&slice.library);
                let framework = wrapper.filename().strip_suffix(b".framework");
                let include = match framework {
                    Some(_) => Some(&slice.library),
                    None => slice.headers.as_ref(),
                };
                if let Some(include) = include {
                    link.xcframework_includes.push(InterfaceInclude {
                        path: BT::new(include.clone(), item.text.backtrace.clone()),
                        system: false,
                    });
                }
                if kind != LinkKind::StaticArchive {
                    let dependency = if let Some(name) = framework {
                        lower::join(&slice.library, name)
                    } else {
                        slice.library.clone()
                    };
                    link.xcframework_inputs.insert(dependency);
                }
                entry.insert(slice);
            }
        }
    }
    let mut features = Features::new(model, target, paths, &language);
    features.append(
        items.iter().map(Arc::as_ref),
        &mut fragments,
        &mut |item, fragments| {
            if let Some(slice) =
                xcframework_root(usage, model, &ctx.config, item).and_then(|root| slices.get(&root))
            {
                append_library_path(model, target.dir, &slice.library, &item.text, fragments);
            } else {
                append_link_item(
                    model,
                    usage,
                    resolved,
                    target.dir,
                    &ctx.config,
                    item,
                    fragments,
                );
            }
        },
    )?;
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
        let mut items = ide::collect_link_items(usage, model, &ctx.config, link_items(&items));
        for item in &mut items {
            if let ide::IdeLinkItem::File(path) = item
                && let Some(slice) = slices.get(&Lexical::new(path).normalize())
            {
                *path = slice.library.clone();
            }
        }
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
            xcframeworks: slices
                .into_iter()
                .map(|(root, slice)| (root, slice.library))
                .collect(),
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
            facts.xcframeworks = settings.xcframeworks;
            for group in &mut facts.compile {
                for entry in &link.xcframework_includes {
                    let include = ide::include(entry, ide::IdeFlavor::Xcode);
                    if !group.includes.contains(&include) {
                        group.includes.push(include);
                    }
                }
            }
            facts.link_options = settings.options;
            facts.link_directories = settings.directories;
            facts.build_rpaths = settings.rpaths;
            facts.install_name_dir = settings.install_name_dir;
        }
        for group in &mut target.compile_groups {
            for include in &link.xcframework_includes {
                if !group
                    .includes
                    .iter()
                    .any(|entry| entry.path.value == include.path.value)
                {
                    group.includes.push(include.clone());
                }
            }
        }
        inputs.xcframework_inputs = link.xcframework_inputs;
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

fn xcframework_root(
    usage: &UsageResolver<'_>,
    model: &ProjectModel,
    config: &ConfigId,
    item: &ResolvedLinkItem,
) -> Option<BString> {
    let path = match item.kind {
        ResolvedLinkKind::Library(id) if model.target(id)?.is_imported() => {
            usage.artifact(id, ArtifactKind::File, ArtifactPart::Full, config)?
        }
        ResolvedLinkKind::Raw if Lexical::new(&item.text.value).absolute() => {
            item.text.value.clone()
        }
        _ => return None,
    };
    Lexical::new(&path)
        .filename()
        .ends_with(b".xcframework")
        .then(|| Lexical::new(&path).normalize())
}

pub(crate) fn framework_dirs<'a>(
    model: &ProjectModel,
    target: &Target,
    dependencies: &[TargetId],
    artifact: &impl Fn(TargetId) -> Option<&'a BString>,
) -> Vec<BString> {
    if !lower::is_darwin(model, target.dir) {
        return Vec::new();
    }
    dependencies
        .iter()
        .filter_map(|&id| {
            let dependency = model.target(id)?;
            let layout = ide::bundle_kind(dependency);
            if !matches!(layout, ide::BundleKind::Framework { .. }) {
                return None;
            }
            let path = artifact(id)?;
            let root = layout.root(path)?;
            Some(Lexical::new(root).parent().into())
        })
        .collect::<IndexSet<_>>()
        .into_iter()
        .collect()
}

fn append_link_item(
    model: &ProjectModel,
    usage: &UsageResolver<'_>,
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
            if lower::is_darwin(model, dir)
                && let Some(dependency) = model.target(*id).filter(|target| !target.is_imported())
                && let layout @ ide::BundleKind::Framework { .. } = ide::bundle_kind(dependency)
                && let Some(path) = resolved
                    .get(id)
                    .and_then(|target| target.artifacts.file.as_ref())
                && let Some(root) = layout.root(path)
            {
                append_framework_dir(
                    model,
                    dir,
                    Lexical::new(root).parent(),
                    &item.text,
                    fragments,
                );
                fragments.push(link_fragment(
                    quote(&build_path(model, path)),
                    &item.text,
                    Libraries,
                ));
                return;
            }
            let path = if let Some(target) = resolved.get(id) {
                target
                    .artifacts
                    .linker_file
                    .as_ref()
                    .or(target.artifacts.file.as_ref())
                    .cloned()
            } else {
                usage
                    .artifact(*id, ArtifactKind::LinkerFile, ArtifactPart::Full, config)
                    .or_else(|| usage.artifact(*id, ArtifactKind::File, ArtifactPart::Full, config))
            };
            if let Some(path) = path {
                append_library_path(model, dir, &path, &item.text, fragments);
                return;
            }
            (raw_library(value), Libraries)
        }
        ResolvedLinkKind::Objects(_) | ResolvedLinkKind::Interface(_) => return,
        ResolvedLinkKind::Group { members, .. } => {
            for member in members {
                append_link_item(model, usage, resolved, dir, config, member, fragments);
            }
            return;
        }
        ResolvedLinkKind::ObjectFile => (quote(value), Libraries),
        ResolvedLinkKind::Raw if value.starts_with(b"LINKER:") || value.starts_with(b"SHELL:") => {
            (value.into(), Libraries)
        }
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
        append_framework_dir(model, dir, path_view.parent(), origin, fragments);
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

fn append_framework_dir(
    model: &ProjectModel,
    dir: DirId,
    directory: &[u8],
    origin: &BT<BString>,
    fragments: &mut Vec<LinkFragment>,
) {
    let parent = Lexical::new(directory).normalize();
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
    let directory = quote(&[b"-F", directory].concat());
    let role = LinkFragmentRole::FrameworkPath;
    if !implicit
        && !fragments
            .iter()
            .any(|fragment| fragment.role == role && fragment.text.value == directory)
    {
        fragments.push(link_fragment(directory, origin, role));
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
