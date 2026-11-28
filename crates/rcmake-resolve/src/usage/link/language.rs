use std::collections::HashSet;

use indexmap::{IndexMap, IndexSet};
use rcmake_base::{BString, list, truth};
use rcmake_genex::{EvalContext, EvalFlags};
use rcmake_model::{Target, TargetId, TargetKind};

use super::{ResolvedLinkKind, UsageResolver, link_context};
use crate::{ResolveError, ResolvedTarget, lower};

#[derive(Clone, Copy, Eq, Hash, PartialEq)]
enum Contribution {
    Link,
    DirectObject,
    ArchiveObject,
}

impl UsageResolver<'_> {
    pub(crate) fn select_link_language(
        &mut self,
        id: TargetId,
        target: &Target,
        resolved: &IndexMap<TargetId, ResolvedTarget>,
        ctx: &EvalContext,
    ) -> Result<Option<BString>, ResolveError> {
        if let Some(language) = target
            .props
            .get(b"LINKER_LANGUAGE")
            .filter(|lang| !lang.is_empty())
        {
            return Ok(Some(language.clone()));
        }
        if target
            .props
            .get(b"HAS_CXX")
            .is_some_and(|flag| truth::is_on(flag))
        {
            return Ok(Some("CXX".into()));
        }
        let mut phase = link_context(id, ctx);
        phase.link_language = None;
        let (first, flags) = self.link_language_pass(id, target, resolved, &phase)?;
        let first = first.or_else(|| ctx.link_language.clone());
        if flags.link_language_sensitive && first.is_some() {
            phase.link_language.clone_from(&first);
            let (second, _) = self.link_language_pass(id, target, resolved, &phase)?;
            let second = second.or_else(|| ctx.link_language.clone());
            if second != first {
                let first = first.unwrap_or_default();
                let second = second.unwrap_or_default();
                return Err(ResolveError::Message(
                    format!("Evaluation of $<LINK_LANGUAGE:...> or $<LINK_LAND_AND_ID:...> changes\nthe linker language for target \"{}\" (from '{first}' to '{second}') which is invalid.", target.name).into(),
                ));
            }
        }
        Ok(first)
    }

    fn link_language_pass(
        &mut self,
        id: TargetId,
        target: &Target,
        resolved: &IndexMap<TargetId, ResolvedTarget>,
        ctx: &EvalContext,
    ) -> Result<(Option<BString>, EvalFlags), ResolveError> {
        let (_, preferred, flags) =
            self.language_closure(id, &|id| resolved.get(&id), ctx, true)?;
        Ok((
            lower::linker_language(
                self.model,
                target,
                preferred.iter().map(|language| language.as_slice()),
            ),
            flags,
        ))
    }

    pub(crate) fn language_closure<'facts>(
        &mut self,
        id: TargetId,
        facts: &impl Fn(TargetId) -> Option<&'facts ResolvedTarget>,
        ctx: &EvalContext,
        object_dependencies: bool,
    ) -> Result<(IndexSet<BString>, IndexSet<BString>, EvalFlags), ResolveError> {
        let ctx = &link_context(id, ctx);
        let target = self.target(id)?;
        let mut languages = IndexSet::new();
        let mut dependencies = Vec::new();
        if let Some(facts) = facts(id) {
            languages.extend(
                facts
                    .compile_groups
                    .iter()
                    .map(|group| group.language.clone()),
            );
            if object_dependencies {
                dependencies.extend(
                    facts
                        .dependencies
                        .iter()
                        .copied()
                        .map(|id| (id, Contribution::DirectObject)),
                );
            }
        }
        let mut preferred = languages.clone();
        let (direct, mut flags) = self.direct_links(id, ctx)?;
        self.language_dependencies(&direct, &mut dependencies);

        let mut visited = HashSet::new();
        let mut propagates_cache = std::collections::HashMap::<BString, bool>::new();
        let mut index = 0;
        while let Some(&(dependency, contribution)) = dependencies.get(index) {
            index += 1;
            if dependency == id || !visited.insert((dependency, contribution)) {
                continue;
            }
            let library = self.target(dependency)?;
            if library.kind == TargetKind::StaticLibrary
                || (library.kind == TargetKind::ObjectLibrary && contribution != Contribution::Link)
            {
                let inherited = if library.is_imported() {
                    imported_languages(library, ctx)
                } else {
                    facts(dependency)
                        .into_iter()
                        .flat_map(|facts| {
                            facts
                                .compile_groups
                                .iter()
                                .map(|group| group.language.clone())
                        })
                        .collect()
                };
                languages.extend(inherited.iter().cloned());
                preferred.extend(inherited.into_iter().filter(|language| {
                    contribution == Contribution::DirectObject
                        || *propagates_cache.entry(language.clone()).or_insert_with(|| {
                            lower::definition(
                                self.model,
                                target.dir,
                                &[
                                    b"CMAKE_",
                                    language.as_slice(),
                                    b"_LINKER_PREFERENCE_PROPAGATES",
                                ]
                                .concat(),
                            )
                            .map_or(language == "CXX", |value| truth::is_on(value))
                        })
                }));
            }
            if object_dependencies
                && matches!(
                    library.kind,
                    TargetKind::StaticLibrary | TargetKind::ObjectLibrary
                )
            {
                let objects = if contribution == Contribution::DirectObject {
                    Contribution::DirectObject
                } else {
                    Contribution::ArchiveObject
                };
                if let Some(facts) = facts(dependency) {
                    dependencies.extend(facts.dependencies.iter().copied().map(|id| (id, objects)));
                }
                if library.kind == TargetKind::StaticLibrary && !library.is_imported() {
                    let (own_direct, own_flags) =
                        self.direct_links(dependency, &link_context(dependency, ctx))?;
                    flags = flags.union(own_flags);
                    for item in own_direct.iter() {
                        let entries = match &item.kind {
                            ResolvedLinkKind::Group { members, .. } => members.as_slice(),
                            _ => std::slice::from_ref(item.as_ref()),
                        };
                        for entry in entries {
                            match entry.kind {
                                ResolvedLinkKind::Objects(obj_id) => {
                                    dependencies.push((obj_id, objects));
                                }
                                ResolvedLinkKind::ObjectFile => {
                                    if let Some(obj_id) = self
                                        .source_paths()
                                        .and_then(|paths| paths.object_owner(&entry.text.value))
                                    {
                                        dependencies.push((obj_id, objects));
                                    }
                                }
                                _ => {}
                            }
                        }
                    }
                }
            }
            let (links, deps) = self.usage_links(dependency, true, ctx)?;
            flags = flags.union(deps);
            self.language_dependencies(&links, &mut dependencies);
        }

        Ok((languages, preferred, flags))
    }

    fn language_dependencies(
        &self,
        items: &[std::sync::Arc<super::ResolvedLinkItem>],
        dependencies: &mut Vec<(TargetId, Contribution)>,
    ) {
        for item in items {
            let entries = match &item.kind {
                ResolvedLinkKind::Group { members, .. } => members.as_slice(),
                _ => std::slice::from_ref(item.as_ref()),
            };
            dependencies.extend(entries.iter().filter_map(|entry| match entry.kind {
                ResolvedLinkKind::Objects(id) => Some((id, Contribution::DirectObject)),
                ResolvedLinkKind::Library(id) | ResolvedLinkKind::Interface(id) => {
                    Some((id, Contribution::Link))
                }
                ResolvedLinkKind::ObjectFile => Some((
                    self.source_paths()?.object_owner(&entry.text.value)?,
                    Contribution::DirectObject,
                )),
                _ => None,
            }));
        }
    }
}

fn imported_languages(target: &Target, ctx: &EvalContext) -> Vec<BString> {
    let property = b"IMPORTED_LINK_INTERFACE_LANGUAGES";
    let configured = super::super::artifacts::selected_imported_config(target, &ctx.config)
        .filter(|config| !config.is_empty())
        .map(|config| [property.as_slice(), b"_", &config].concat());
    configured
        .as_ref()
        .and_then(|key| target.props.get(key))
        .or_else(|| target.props.get(property))
        .map_or_else(Vec::new, |value| list::split(value, list::Empty::Drop))
}
