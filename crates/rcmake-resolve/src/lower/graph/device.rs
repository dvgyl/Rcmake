use indexmap::{IndexMap, IndexSet};
use rcmake_base::BString;
use rcmake_genex::{EvalContext, UsageTraversal};
use rcmake_model::{GeneratorKind, ProjectModel, Target, TargetId, UsageField};

use crate::{
    LinkStep, ResolveError, ResolvedTarget,
    lower::{
        cuda::{self, arch, device_link},
        definition, language_flags,
    },
    usage::{UsageResolver, link_options::shell_arguments},
};

pub(super) fn link_languages(
    resolved: &ResolvedTarget,
    targets: &IndexMap<TargetId, &ResolvedTarget>,
) -> IndexSet<BString> {
    let mut languages = IndexSet::new();
    let mut seen = IndexSet::new();
    let mut pending = vec![resolved];
    while let Some(target) = pending.pop() {
        if seen.insert(target.id) {
            languages.extend(
                target
                    .compile_groups
                    .iter()
                    .map(|group| group.language.clone()),
            );
            pending.extend(
                target
                    .dependencies
                    .iter()
                    .filter_map(|id| targets.get(id).copied()),
            );
        }
    }
    languages
}

pub(super) fn plan(
    model: &ProjectModel,
    target: &Target,
    resolved: &ResolvedTarget,
    link: &LinkStep,
    languages: &IndexSet<BString>,
    objects: &[BString],
    usage: &mut UsageResolver<'_>,
) -> Result<Option<device_link::DeviceLinkPlan>, ResolveError> {
    if !matches!(
        model.generator,
        Some(GeneratorKind::Ninja | GeneratorKind::NinjaMultiConfig | GeneratorKind::UnixMakefiles)
    ) || !languages
        .iter()
        .any(|lang| matches!(lang.as_slice(), b"CUDA" | b"SYCL"))
    {
        return Ok(None);
    }
    let config = &resolved.config;
    let context = EvalContext {
        config: config.clone(),
        link_language: Some(link.language.clone()),
        traversal: UsageTraversal::Link,
        device_link: true,
        ..Default::default()
    };
    let items = usage.collect_link_items(resolved.id, &context)?;
    let languages: Vec<_> = languages.iter().cloned().collect();
    let Some(language) = device_link::device_link_language(model, target, &languages, &items)
    else {
        return Ok(None);
    };
    let libraries = device_link::device_libraries(model, target, config, &items, language);
    let options = usage
        .collect(resolved.id, UsageField::LinkOptions, &context)?
        .entries;
    let options = options
        .into_iter()
        .map(|entry| entry.value)
        .collect::<Vec<_>>();
    let mut flags = device_link::link_options(model, target, language, &options, true)?;
    if language == b"CUDA" {
        flags.extend(
            arch::architecture_flags(
                model,
                target,
                arch::ArchPhase::Link,
                config,
                cuda::in_try_compile(model),
            )
            .0,
        );
    }
    let directories = usage
        .collect(resolved.id, UsageField::LinkDirectories, &context)?
        .entries
        .into_iter()
        .map(|entry| [b"-L", entry.value.as_slice()].concat().into())
        .collect::<Vec<_>>();
    let compile_flags = language_flags(model, target, config, language)
        .iter()
        .flat_map(|flag| shell_arguments(flag))
        .collect::<Vec<_>>();
    let mut libraries = libraries;
    if language == b"CUDA" {
        let runtime =
            cuda::runtime::selected_runtime(model, target, config, &link.language, usage)?;
        let key = [
            b"CMAKE_CUDA_RUNTIME_LIBRARY_LINK_OPTIONS_",
            runtime.as_slice(),
        ]
        .concat();
        if let Some(options) = definition(model, target.dir, &key) {
            libraries.args.extend(
                rcmake_base::list::split(options, rcmake_base::list::Empty::Drop)
                    .into_iter()
                    .filter(|library| {
                        matches!(
                            library.as_slice(),
                            b"cudadevrt" | b"cudart" | b"cudart_static"
                        )
                    })
                    .map(|library| [b"-l", library.as_slice()].concat().into()),
            );
        }
    }
    device_link::plan(
        model,
        target,
        config,
        language,
        &device_link::DeviceLinkInputs {
            objects,
            libraries: &libraries,
            directories: &directories,
            link_flags: &flags,
            compile_flags: &compile_flags,
        },
    )
    .map(Some)
}
