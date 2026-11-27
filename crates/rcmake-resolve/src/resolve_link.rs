use std::sync::Arc;

use indexmap::{IndexMap, IndexSet};
use rcmake_base::{BString, list, path_lex::Lexical, shell::quote, truth};
use rcmake_genex::{ArtifactKind, ArtifactPart, ConfigId, EvalContext, UsageTraversal};
use rcmake_host::path::to_native;
use rcmake_model::{BT, DirId, ProjectModel, Target, TargetId, UsageField};

use crate::{
    InterfaceInclude, LinkFragment, LinkFragmentRole, LinkKind, LinkStep, ResolveError,
    ResolvedTarget, ide, lower,
    sources::{SourcePaths, TargetInputs},
    usage::{ResolvedLinkItem, ResolvedLinkKind, UsageResolver},
};

pub(crate) mod directories;
mod features;
use features::Features;

#[derive(Default)]
pub(crate) struct ResolvedLink {
    step: Option<LinkStep>,
    languages: IndexSet<BString>,
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

#[expect(
    clippy::too_many_arguments,
    reason = "Link resolution consumes target, usage, and precompiled-header plans."
)]
pub(crate) fn resolve_link(
    model: &ProjectModel,
    usage: &mut UsageResolver<'_>,
    paths: &SourcePaths<'_>,
    resolved: &IndexMap<TargetId, ResolvedTarget>,
    id: TargetId,
    target: &Target,
    ctx: &EvalContext,
    precompiled: &lower::PchPlans,
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
        usage.ordered_links_for_result(id, &ctx)?
    } else {
        Vec::new()
    };
    let linked_targets = usage.link_targets(id, &ctx)?;
    let compile_targets = usage.compile_targets(id, &ctx)?;
    for dep_id in linked_targets.iter().chain(&compile_targets) {
        if let Some(dep_target) = model.target(*dep_id).filter(|t| t.is_imported()) {
            crate::usage::artifacts::warn_cmp0200(model, paths, dep_target, &ctx.config);
        }
    }
    let position_independent = usage.validate_compatible(id, &ctx, paths)?;
    let mut link = ResolvedLink {
        position_independent,
        dependencies: target
            .utility_deps
            .iter()
            .filter_map(|dep| model.find_target(target.dir, &dep.value))
            .chain(linked_targets.iter().copied())
            .chain(compile_targets.iter().copied())
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
    let options = usage
        .collect_for_result(id, UsageField::LinkOptions, &ctx)?
        .entries;
    let directories = usage
        .collect_for_result(id, UsageField::LinkDirectories, &ctx)?
        .entries;
    link.languages = usage
        .language_closure(id, &|id| resolved.get(&id), &ctx, true)?
        .0;
    let naming = LinkLibraryFlags::new(model, target, &language);
    let mut fragments = Vec::with_capacity(options.len() + directories.len() + items.len());
    let windows_host = lower::windows_host(model, target.dir);
    for option in &options {
        let value = option.value.as_slice();
        let text = if value.starts_with(b"LINKER:") {
            value.into()
        } else {
            value.strip_prefix(b"SHELL:").map_or_else(
                || {
                    if windows_host {
                        quote(value)
                    } else {
                        lower::cmake_posix_quote(value)
                    }
                },
                BString::from,
            )
        };
        fragments.push(link_fragment(text, option, LinkFragmentRole::Flags));
    }
    if language == b"Swift" && kind != LinkKind::StaticArchive {
        // Mirrors `cmLinkLineComputer::ComputeLinkPath` (`cmLinkLineComputer.cxx:132-159`):
        // Swift links emit `<LIBRARY_PATH_FLAG><dir>` for every linked library target.
        let mut swift_dirs = BString::default();
        for item in link_items(&items) {
            if let ResolvedLinkKind::Library(dep_id) = item.kind
                && let Some(dep) = model.target(dep_id)
                && matches!(
                    dep.kind,
                    rcmake_model::TargetKind::StaticLibrary
                        | rcmake_model::TargetKind::SharedLibrary
                )
            {
                let dep_file = resolved
                    .get(&dep_id)
                    .and_then(|facts| {
                        facts
                            .artifacts
                            .import_file
                            .as_ref()
                            .or(facts.artifacts.file.as_ref())
                    })
                    .cloned()
                    .or_else(|| {
                        usage.artifact(
                            dep_id,
                            ArtifactKind::ImportFile,
                            ArtifactPart::Full,
                            &ctx.config,
                        )
                    })
                    .or_else(|| {
                        usage.artifact(dep_id, ArtifactKind::File, ArtifactPart::Full, &ctx.config)
                    });
                if let Some(file) = dep_file {
                    let dir = Lexical::new(&file).parent();
                    if !dir.is_empty() {
                        if !swift_dirs.is_empty() {
                            swift_dirs.push(b' ');
                        }
                        if naming.path.ends_with(b" ") {
                            swift_dirs.extend_from_slice(naming.path);
                            swift_dirs.extend_from_slice(&quote(dir));
                            swift_dirs.extend_from_slice(naming.path_terminator);
                        } else {
                            swift_dirs.extend_from_slice(&quote(
                                &[naming.path, dir, naming.path_terminator].concat(),
                            ));
                        }
                    }
                }
            }
        }
        if !swift_dirs.is_empty() {
            fragments.push(LinkFragment {
                text: BT::new(swift_dirs, Default::default()),
                role: LinkFragmentRole::LibraryPath,
            });
        }
    }
    for directory in &directories {
        let text = quote(&[naming.path, directory.value.as_slice()].concat());
        fragments.push(link_fragment(
            text,
            directory,
            LinkFragmentRole::LibraryPath,
        ));
    }
    if kind != LinkKind::StaticArchive
        && let Some(standard) = lower::definition(
            model,
            target.dir,
            &[
                b"CMAKE_",
                language.as_slice(),
                b"_STANDARD_LINK_DIRECTORIES",
            ]
            .concat(),
        )
    {
        for directory in list::split(standard, list::Empty::Drop) {
            let text = quote(&[naming.path, directory.as_slice(), naming.path_terminator].concat());
            let origin = BT::new(directory, target.backtrace.clone());
            fragments.push(link_fragment(text, &origin, LinkFragmentRole::LibraryPath));
        }
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
                    (target.dir, &naming),
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
                link.objects
                    .extend(precompiled.object_library_companions(model, id).cloned());
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
    let vs_template = (usage.ide() == Some(ide::IdeFlavor::VisualStudio)
        && kind != LinkKind::StaticArchive)
        .then(|| features.visual_studio_whole_archive())
        .flatten();
    let step = LinkStep {
        kind,
        language,
        fragments,
        output,
    };
    if let Some(flavor) = usage.ide() {
        let mut items = ide::collect_link_items(
            usage,
            model,
            &ctx.config,
            link_items(&items),
            vs_template.as_ref(),
        );
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
            let rpaths = lower::ide_rpaths(
                model,
                target,
                &ctx.config,
                &step,
                &libraries,
                &link.languages,
                usage,
            )?;
            let install_name_dir = (kind == LinkKind::SharedLibrary)
                .then(|| {
                    lower::install_name_dir(model, target, &facts.artifacts, &ctx.config, None)
                })
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
    links: Vec<(TargetId, ResolvedLink)>,
    providers: Option<&IndexMap<TargetId, TargetId>>,
) {
    let std_objects: IndexMap<_, Vec<_>> = providers
        .into_iter()
        .flat_map(|providers| providers.values().copied())
        .collect::<IndexSet<_>>()
        .into_iter()
        .filter_map(|id| {
            let target = resolved.get(&id)?;
            let info = target.cxx_modules.as_ref()?;
            let objects = info
                .units
                .iter()
                .filter(|unit| !unit.bmi_only)
                .filter_map(|unit| target.compile_units.get(unit.unit))
                .map(|unit| unit.object.clone())
                .collect();
            Some((id, objects))
        })
        .collect();
    let module_dirs: IndexMap<_, _> = resolved
        .values()
        .filter_map(|target| {
            let info = target.cxx_modules.as_ref()?;
            (!info.units.is_empty()).then(|| (target.id, info.module_dir.clone()))
        })
        .collect();
    for (id, link) in links {
        let (Some(target), Some(inputs)) = (resolved.get_mut(&id), inputs.get_mut(&id)) else {
            continue;
        };
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
        inputs.link_languages = link.languages;
        inputs.xcframework_inputs = link.xcframework_inputs;
        target.link = link.step;
        target.dependencies = link.dependencies;
        inputs.position_independent = link.position_independent;
        inputs.objects.extend(link.objects);
        if let Some(info) = &mut target.cxx_modules {
            let mut linked = link.module_links.into_iter().collect::<IndexSet<_>>();
            if let Some(&id) = providers.and_then(|providers| providers.get(&target.id)) {
                linked.insert(id);
                target.dependencies.push(id);
                inputs
                    .objects
                    .extend(std_objects.get(&id).into_iter().flatten().cloned());
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
    (id, target): (TargetId, &Target),
    dependencies: &[TargetId],
    artifact: &impl Fn(TargetId) -> Option<&'a BString>,
    config: &ConfigId,
    link_language: Option<&BString>,
    usage: &mut UsageResolver<'_>,
) -> Result<Vec<(BString, bool)>, ResolveError> {
    let mut directories: IndexMap<BString, bool> = IndexMap::new();
    for (root, system) in linked_framework_roots(model, target, dependencies, artifact, config) {
        directories
            .entry(Lexical::new(&root).parent().into())
            .and_modify(|previous| *previous &= system)
            .or_insert(system);
    }
    for root in direct_raw_framework_roots(model, target, id, config, link_language, usage)? {
        let parent: BString = Lexical::new(&root).parent().into();
        if !parent.is_empty() {
            directories
                .entry(parent)
                .and_modify(|previous| *previous = false)
                .or_insert(false);
        }
    }
    Ok(directories.into_iter().collect())
}

fn direct_raw_framework_roots(
    model: &ProjectModel,
    target: &Target,
    id: TargetId,
    config: &ConfigId,
    link_language: Option<&BString>,
    usage: &mut UsageResolver<'_>,
) -> Result<Vec<BString>, ResolveError> {
    if !lower::is_darwin(model, target.dir) {
        return Ok(Vec::new());
    }
    let mut roots = IndexSet::new();
    for traversal in [UsageTraversal::Link, UsageTraversal::Compile] {
        let context = EvalContext {
            config: config.clone(),
            head: Some(crate::usage::handle(id)),
            traversal,
            link_language: link_language.cloned(),
            ..EvalContext::default()
        };
        let items = usage.direct_link_items(id, false, &context)?;
        for item in items.iter() {
            if matches!(item.kind, ResolvedLinkKind::Raw)
                && let Some(root) = lower::framework::wrapper_root(&item.text.value, b".framework")
            {
                roots.insert(Lexical::new(root).normalize());
            }
        }
    }
    Ok(roots.into_iter().collect())
}

pub(crate) fn transitive_raw_framework_dirs(
    model: &ProjectModel,
    target: &Target,
    id: TargetId,
    config: &ConfigId,
    usage: &mut UsageResolver<'_>,
) -> Result<Vec<BString>, ResolveError> {
    if !lower::is_darwin(model, target.dir) {
        return Ok(Vec::new());
    }
    let head = Some(crate::usage::handle(id));
    let link_language = if let Some(language) = target
        .props
        .get(b"LINKER_LANGUAGE")
        .filter(|lang| !lang.is_empty())
    {
        Some(language.clone())
    } else if target
        .props
        .get(b"HAS_CXX")
        .is_some_and(|flag| truth::is_on(flag))
    {
        Some("CXX".into())
    } else {
        let source_languages: Vec<BString> = target
            .sources
            .iter()
            .flat_map(|source| list::split(&source.value, list::Empty::Drop))
            .filter_map(|source| lower::source_language(model, target, &source))
            .collect();
        let base_ctx = EvalContext {
            config: config.clone(),
            head,
            traversal: UsageTraversal::Link,
            ..EvalContext::default()
        };
        lower::linker_language(
            model,
            target,
            source_languages.iter().map(|language| language.as_slice()),
        )
        .or_else(|| {
            usage
                .select_link_language(id, target, &IndexMap::new(), &base_ctx)
                .ok()
                .flatten()
        })
    };
    let mut excluded: IndexSet<BString> = lower::definition(
        model,
        target.dir,
        b"CMAKE_PLATFORM_IMPLICIT_LINK_FRAMEWORK_DIRECTORIES",
    )
    .into_iter()
    .flat_map(|value| list::split(value, list::Empty::Drop))
    .map(|value| Lexical::new(&value).normalize())
    .collect();
    for root in
        direct_raw_framework_roots(model, target, id, config, link_language.as_ref(), usage)?
    {
        excluded.insert(Lexical::new(Lexical::new(&root).parent()).normalize());
    }
    let context = EvalContext {
        config: config.clone(),
        head,
        traversal: UsageTraversal::Link,
        link_language,
        ..EvalContext::default()
    };
    let deps = usage.link_targets(id, &context)?;
    let mut dirs = IndexSet::new();
    for dep in deps {
        let items = usage.direct_link_items(dep, true, &context)?;
        for item in items.iter() {
            if matches!(item.kind, ResolvedLinkKind::Raw)
                && let Some(root) = lower::framework::wrapper_root(&item.text.value, b".framework")
            {
                let parent = Lexical::new(Lexical::new(root).parent()).normalize();
                if !parent.is_empty() && !excluded.contains(&parent) {
                    dirs.insert(parent);
                }
            }
        }
    }
    Ok(dirs.into_iter().collect())
}

fn linked_framework_roots<'a>(
    model: &ProjectModel,
    target: &Target,
    dependencies: &[TargetId],
    artifact: &impl Fn(TargetId) -> Option<&'a BString>,
    config: &ConfigId,
) -> Vec<(BString, bool)> {
    let no_imported_system = target
        .props
        .get(b"NO_SYSTEM_FROM_IMPORTED")
        .is_some_and(|value| truth::is_on(value));
    dependencies
        .iter()
        .filter_map(|&id| {
            let dependency = model.target(id)?;
            let imported = dependency.is_imported();
            let layout = ide::bundle_kind(dependency);
            if !imported && !matches!(layout, ide::BundleKind::Framework { .. }) {
                return None;
            }
            let fallback;
            let path = if let Some(path) = artifact(id) {
                path
            } else {
                fallback = lower::compute_artifacts(model, dependency, config);
                fallback.file.as_ref()?
            };
            let root = framework_root(&layout, imported, path)?;
            let system = !(imported && no_imported_system)
                && dependency
                    .props
                    .get(b"SYSTEM")
                    .map_or(imported, |value| truth::is_on(value));
            Some((root.into(), system))
        })
        .collect()
}

/// Return the framework bundle that contains the artifact `path`.
///
/// Only a framework layout supplies a framework directory. An imported
/// target without a layout supplies a directory only when its artifact lies in
/// a `.framework` wrapper. An application bundle never supplies one, also when
/// it lies inside a framework.
fn framework_root<'p>(
    layout: &ide::BundleKind,
    imported: bool,
    path: &'p [u8],
) -> Option<&'p [u8]> {
    if matches!(layout, ide::BundleKind::App { .. }) {
        return None;
    }
    match layout {
        ide::BundleKind::Framework { .. } => layout.root(path),
        _ => None,
    }
    .or_else(|| {
        imported
            .then(|| lower::framework::wrapper_root(path, b".framework"))
            .flatten()
    })
}

/// Collect the framework bundles that supply includes to a target's compile groups.
pub fn codemodel_frameworks<'a>(
    model: &ProjectModel,
    target: &Target,
    resolved: &ResolvedTarget,
    artifact: impl Fn(TargetId) -> Option<&'a BString>,
    usage: &mut UsageResolver<'_>,
) -> Result<Vec<InterfaceInclude>, ResolveError> {
    if !lower::is_darwin(model, target.dir) || resolved.compile_units.is_empty() {
        return Ok(Vec::new());
    }
    let context = EvalContext {
        config: resolved.config.clone(),
        head: Some(crate::usage::handle(resolved.id)),
        link_language: resolved.link.as_ref().map(|link| link.language.clone()),
        ..EvalContext::default()
    };
    let dependencies = usage.compile_framework_targets(resolved.id, &context)?;
    let mut seen = IndexSet::new();
    let mut frameworks = Vec::new();
    for (path, system) in
        linked_framework_roots(model, target, &dependencies, &artifact, &resolved.config)
    {
        let normalized = Lexical::new(&path).normalize();
        if seen.insert(normalized) {
            frameworks.push(InterfaceInclude {
                path: BT::new(path, Default::default()),
                system,
            });
        }
    }
    for path in direct_raw_framework_roots(
        model,
        target,
        resolved.id,
        &resolved.config,
        context.link_language.as_ref(),
        usage,
    )? {
        if seen.insert(path.clone()) {
            frameworks.push(InterfaceInclude {
                path: BT::new(path, Default::default()),
                system: false,
            });
        }
    }
    Ok(frameworks)
}

fn append_link_item(
    model: &ProjectModel,
    usage: &UsageResolver<'_>,
    resolved: &IndexMap<TargetId, ResolvedTarget>,
    (dir, naming): (DirId, &LinkLibraryFlags<'_>),
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
                && let Some(path) = resolved.get(id).and_then(|target| {
                    target
                        .artifacts
                        .import_file
                        .as_ref()
                        .or(target.artifacts.file.as_ref())
                })
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
            let path = if let Some(imported) =
                model.target(*id).filter(|target| target.is_imported())
            {
                lower::compute_artifacts(model, imported, config).linker_file
            } else if let Some(target) = resolved.get(id) {
                target
                    .artifacts
                    .import_file
                    .as_ref()
                    .or_else(|| {
                        if matches!(
                            model.generator,
                            Some(
                                rcmake_model::GeneratorKind::Ninja
                                    | rcmake_model::GeneratorKind::NinjaMultiConfig
                            )
                        ) {
                            target
                                .artifacts
                                .file
                                .as_ref()
                                .or(target.artifacts.linker_file.as_ref())
                        } else {
                            target
                                .artifacts
                                .linker_file
                                .as_ref()
                                .or(target.artifacts.file.as_ref())
                        }
                    })
                    .cloned()
            } else {
                usage
                    .artifact(*id, ArtifactKind::ImportFile, ArtifactPart::Full, config)
                    .or_else(|| {
                        usage.artifact(*id, ArtifactKind::LinkerFile, ArtifactPart::Full, config)
                    })
                    .or_else(|| usage.artifact(*id, ArtifactKind::File, ArtifactPart::Full, config))
            };
            if let Some(path) = path {
                if lower::is_darwin(model, dir)
                    && model.target(*id).is_some_and(|target| {
                        target.is_imported()
                            && target
                                .props
                                .get(b"FRAMEWORK")
                                .is_some_and(|value| truth::is_on(value))
                    })
                    && let Some(directory) = imported_framework_parent(&path)
                {
                    append_framework_dir(model, dir, directory, &item.text, fragments);
                }
                append_library_path(model, dir, &path, &item.text, fragments);
                return;
            }
            (naming.library(value), Libraries)
        }
        ResolvedLinkKind::Objects(_) | ResolvedLinkKind::Interface(_) => return,
        ResolvedLinkKind::Group { members, .. } => {
            for member in members {
                append_link_item(
                    model,
                    usage,
                    resolved,
                    (dir, naming),
                    config,
                    member,
                    fragments,
                );
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
        ResolvedLinkKind::Raw => (naming.library(value), Libraries),
    };
    fragments.push(link_fragment(text, &item.text, role));
}

/// Return the directory that contains `<Name>.framework` for a binary inside
/// that bundle, for example `/sdk/lib` for `/sdk/lib/Qt.framework/Versions/A/Qt`.
fn imported_framework_parent(path: &[u8]) -> Option<&[u8]> {
    let marker = b".framework/";
    let end = path.windows(marker.len()).position(|part| part == marker)?;
    let bundle = path.get(..end + marker.len() - 1)?;
    let parent = Lexical::new(bundle).parent();
    (!parent.is_empty()).then_some(parent)
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
        let windows = lower::artifacts::is_dll_platform(model, dir);
        let ninja = model
            .generator
            .is_some_and(rcmake_model::GeneratorKind::is_ninja);
        let mut path = if windows || ninja {
            lower::build_relative(model, path)
        } else {
            path.into()
        };
        if windows && !path.contains(&b'/') && !path.contains(&b'\\') {
            path = [b"./".as_slice(), &path].concat().into();
        }
        fragments.push(link_fragment(
            quote(&path),
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

struct LinkLibraryFlags<'a> {
    path: &'a [u8],
    path_terminator: &'a [u8],
    prefix: &'a [u8],
    suffix: &'a [u8],
    msvc: bool,
}

impl<'a> LinkLibraryFlags<'a> {
    fn new(model: &'a ProjectModel, target: &Target, language: &[u8]) -> Self {
        let msvc = lower::artifacts::is_msvc(model, target.dir, language);
        let flag = |key: &[u8], fallback: &'static [u8]| {
            lower::definition(
                model,
                target.dir,
                &[b"CMAKE_", language, b"_", key].concat(),
            )
            .or_else(|| lower::definition(model, target.dir, &[b"CMAKE_", key].concat()))
            .map_or(fallback, |value| value.as_slice())
        };
        Self {
            path: flag(
                b"LIBRARY_PATH_FLAG",
                if msvc { b"-LIBPATH:" } else { b"-L" },
            ),
            path_terminator: flag(b"LIBRARY_PATH_TERMINATOR", b""),
            prefix: flag(b"LINK_LIBRARY_FLAG", if msvc { b"" } else { b"-l" }),
            suffix: flag(b"LINK_LIBRARY_SUFFIX", if msvc { b".lib" } else { b"" }),
            msvc,
        }
    }

    fn library(&self, value: &[u8]) -> BString {
        if value.starts_with(b"-") {
            return value.into();
        }
        let (_, extension) = Lexical::new(value).stem_extension(true);
        if value.contains(&b'/')
            || value.contains(&b'\\')
            || (self.msvc && !extension.is_empty())
            || extension.eq_ignore_ascii_case(b".lib")
        {
            quote(value)
        } else {
            quote(&[self.prefix, value, self.suffix].concat())
        }
    }
}

fn link_fragment(text: BString, origin: &BT<BString>, role: LinkFragmentRole) -> LinkFragment {
    LinkFragment {
        text: BT::new(text, origin.backtrace.clone()),
        role,
    }
}

fn build_path(model: &ProjectModel, path: &[u8]) -> BString {
    // Xcode runs the linker outside the build tree, so it needs full paths.
    if model.generator == Some(rcmake_model::GeneratorKind::Xcode) {
        return path.into();
    }
    let root = to_native(&model.dir(DirId::ROOT).binary_dir);
    let native = to_native(path);
    native
        .strip_prefix(root)
        .map_or_else(|_| path.into(), rcmake_host::path::from_native)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn framework_root_accepts_only_framework_layouts() {
        let library: &[u8] = b"/opt/Foo.framework/Versions/A/Foo";
        let framework = ide::BundleKind::Framework {
            extension: BString::from("framework"),
            version: BString::from("A"),
        };
        assert_eq!(
            framework_root(&framework, false, library),
            Some(b"/opt/Foo.framework".as_slice())
        );
        assert_eq!(
            framework_root(&ide::BundleKind::None, true, library),
            Some(b"/opt/Foo.framework".as_slice())
        );
        assert_eq!(framework_root(&ide::BundleKind::None, false, library), None);

        let app = ide::BundleKind::App {
            extension: BString::from("app"),
        };
        let executable: &[u8] = b"/opt/tool.app/Contents/MacOS/tool";
        assert_eq!(framework_root(&app, true, executable), None);
        assert_eq!(framework_root(&app, false, executable), None);

        let helper: &[u8] =
            b"/opt/Foo.framework/Versions/A/Helpers/helper.app/Contents/MacOS/helper";
        assert_eq!(framework_root(&app, true, helper), None);
        assert_eq!(framework_root(&app, false, helper), None);
    }

    #[test]
    fn link_library_flags_preserves_raw_dash_flags_verbatim() {
        use rcmake_model::{TargetKind, TargetOrigin};

        let mut model = ProjectModel::default();
        model.set_root_dirs("/src".into(), "/out".into());
        let target = Target::new(
            "app".into(),
            TargetKind::Executable,
            TargetOrigin::Normal,
            DirId::ROOT,
            Default::default(),
            Default::default(),
        );
        let flags = LinkLibraryFlags::new(&model, &target, b"CXX");
        let raw = br#"-Wl,--no-as-needed,"/build/lib/libtorch.so" -Wl,--as-needed"#;
        assert_eq!(flags.library(raw).as_slice(), raw);
    }

    #[test]
    fn transitive_raw_framework_dirs_evaluates_against_consuming_target_head_and_link_language()
    -> Result<(), Box<dyn std::error::Error>> {
        use rcmake_model::{LinkItem, TargetKind, TargetOrigin, UsageScope};

        let mut model = ProjectModel::default();
        model.set_root_dirs("/src".into(), "/out".into());
        let root = model.dir_mut(DirId::ROOT);
        root.vars
            .insert("CMAKE_SYSTEM_NAME".into(), "Darwin".into());
        root.vars
            .insert("CMAKE_C_SOURCE_FILE_EXTENSIONS".into(), "c".into());
        root.vars
            .insert("CMAKE_CXX_SOURCE_FILE_EXTENSIONS".into(), "cpp".into());

        let link_item = |text: &str, scope: UsageScope| LinkItem {
            text: BT::new(text.into(), Default::default()),
            lookup_dir: DirId::ROOT,
            opens_lookup_scope: false,
            closes_lookup_scope: false,
            scope: Some(scope),
        };

        let mut iface = Target::new(
            "iface".into(),
            TargetKind::InterfaceLibrary,
            TargetOrigin::Normal,
            DirId::ROOT,
            Default::default(),
            Default::default(),
        );
        // Set USE_FOO on the dependency itself so evaluating against the dependency
        // instead of the consuming head target would falsely enable Foo.framework for app_off.
        iface.props.set("USE_FOO".into(), "ON".into());
        iface.usage.interface_link_libraries = Some(vec![
            link_item(
                "$<$<BOOL:$<TARGET_PROPERTY:USE_FOO>>:/opt/Foo/Foo.framework>",
                UsageScope::Interface,
            ),
            link_item(
                "$<$<LINK_LANGUAGE:CXX>:/opt/Cxx/Bar.framework>",
                UsageScope::Interface,
            ),
        ]);
        model.add_target(iface)?;

        let mut app_on = Target::new(
            "app_on".into(),
            TargetKind::Executable,
            TargetOrigin::Normal,
            DirId::ROOT,
            Default::default(),
            Default::default(),
        );
        app_on.props.set("USE_FOO".into(), "ON".into());
        app_on
            .sources
            .push(BT::new("main.cpp".into(), Default::default()));
        app_on.usage.link_libraries = Some(vec![link_item("iface", UsageScope::Private)]);
        let on_id = model.add_target(app_on)?;

        let mut app_off = Target::new(
            "app_off".into(),
            TargetKind::Executable,
            TargetOrigin::Normal,
            DirId::ROOT,
            Default::default(),
            Default::default(),
        );
        app_off.props.set("USE_FOO".into(), "OFF".into());
        app_off
            .sources
            .push(BT::new("main.c".into(), Default::default()));
        app_off.usage.link_libraries = Some(vec![link_item("iface", UsageScope::Private)]);
        let off_id = model.add_target(app_off)?;

        let mut usage = UsageResolver::new(&model);
        let config = ConfigId::default();
        let on_dirs = transitive_raw_framework_dirs(
            &model,
            model.target(on_id).ok_or("missing app_on")?,
            on_id,
            &config,
            &mut usage,
        )?;
        assert_eq!(on_dirs, vec![BString::from("/opt/Foo"), "/opt/Cxx".into()]);

        let off_dirs = transitive_raw_framework_dirs(
            &model,
            model.target(off_id).ok_or("missing app_off")?,
            off_id,
            &config,
            &mut usage,
        )?;
        assert!(
            off_dirs.is_empty(),
            "app_off must evaluate TARGET_PROPERTY and LINK_LANGUAGE against itself, not iface: {off_dirs:?}"
        );
        Ok(())
    }
}
