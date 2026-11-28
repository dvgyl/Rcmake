use indexmap::{IndexMap, IndexSet};
use rcmake_base::{BString, truth};
use rcmake_diag::Backtrace;
use rcmake_genex::{ConfigId, EvalContext};
use rcmake_model::{
    BT, FileSet, FileSetKind, GeneratorKind, ProjectModel, PropertyMap, Target, TargetId,
    TargetKind, TargetOrigin, UsageField, UsageScope,
};

use super::{is_std_provider, stdlib};
use crate::{
    ResolveError, ResolvedTarget, lower,
    sources::absolute_path,
    usage::{UsageResolver, handle},
};

const SYNTHETIC_MARKER: &[u8] = b"@synth_";
const SYNTHETIC_PROP: &[u8] = b"_RCMAKE_CXX_MODULE_SYNTHETIC";

/// Consumer-to-synthetic-provider redirections: for each consumer target,
/// pairs of `(original_provider_id, synthetic_provider_id)`.
pub(crate) type SyntheticBindings = IndexMap<TargetId, Vec<(TargetId, TargetId)>>;

pub(crate) fn is_synthetic_provider(target: &Target) -> bool {
    target.props.get(SYNTHETIC_PROP).is_some()
        || target
            .name
            .windows(SYNTHETIC_MARKER.len())
            .any(|window| window == SYNTHETIC_MARKER)
}

type ModuleFileKey = Vec<(BString, Vec<BString>)>;

#[derive(PartialEq, Eq, Hash)]
struct SynthKey {
    provider: TargetId,
    config: BString,
    compiler: Vec<Option<BString>>,
    standard: BString,
    extensions: bool,
    required: bool,
    runtime: Option<BString>,
    options: Vec<BString>,
    definitions: Vec<BString>,
    module_files: ModuleFileKey,
    dep_synths: Vec<(TargetId, TargetId)>,
}

struct CompileSettings {
    compiler: Vec<Option<BString>>,
    standard: BString,
    extensions: bool,
    required: bool,
    runtime: Option<BString>,
    options: Vec<BString>,
    definitions: Vec<BString>,
    raw_options: Vec<BT<BString>>,
    raw_definitions: Vec<BT<BString>>,
    raw_features: Vec<BT<BString>>,
}

struct EvaluatedModuleSet {
    name: BString,
    bases: Vec<BT<BString>>,
    files: Vec<BT<BString>>,
    props: PropertyMap,
}

fn target_compile_settings(
    model: &ProjectModel,
    usage: &mut UsageResolver<'_>,
    id: TargetId,
    target: &Target,
    config: &ConfigId,
) -> Result<CompileSettings, ResolveError> {
    let context = EvalContext {
        config: config.clone(),
        language: Some("CXX".into()),
        head: Some(handle(id)),
        current: Some(handle(id)),
        ..EvalContext::default()
    };
    let raw_options = usage
        .collect_for_result(id, UsageField::CompileOptions, &context)?
        .entries;
    let raw_definitions = usage
        .collect_for_result(id, UsageField::CompileDefinitions, &context)?
        .entries;
    let raw_features = usage
        .collect_for_result(id, UsageField::CompileFeatures, &context)?
        .entries;
    let (standard, extensions, required) = lower::cxx_standard_mode(model, target, &raw_features);
    let get = |name: &[u8]| lower::definition(model, target.dir, name);
    let runtime = get(b"CMAKE_MSVC_RUNTIME_LIBRARY_DEFAULT")
        .filter(|default| !default.is_empty())
        .map(|default| {
            let raw = target.props.get(b"MSVC_RUNTIME_LIBRARY").unwrap_or(default);
            usage.eval_ide_property(id, b"MSVC_RUNTIME_LIBRARY", raw, &context)
        })
        .transpose()?;
    let compiler = [
        b"CMAKE_CXX_COMPILER".as_slice(),
        b"CMAKE_CXX_COMPILER_ID",
        b"CMAKE_CXX_COMPILER_VERSION",
        b"CMAKE_CXX_COMPILER_FRONTEND_VARIANT",
        b"CMAKE_CXX_COMPILER_TARGET",
        b"CMAKE_CXX_COMPILER_EXTERNAL_TOOLCHAIN",
    ]
    .iter()
    .map(|name| get(name).cloned())
    .collect();
    let options =
        stdlib::compatibility_options(&raw_options, get(b"CMAKE_CXX_COMPILER_FRONTEND_VARIANT"));
    let definitions = raw_definitions
        .iter()
        .map(|entry| entry.value.clone())
        .collect();
    Ok(CompileSettings {
        compiler,
        standard,
        extensions,
        required,
        runtime,
        options,
        definitions,
        raw_options,
        raw_definitions,
        raw_features,
    })
}

fn provider_module_sets(model: &ProjectModel, target: &Target) -> Vec<rcmake_model::FileSetId> {
    target
        .file_sets
        .iter()
        .copied()
        .filter(|&id| {
            model.file_set(id).is_some_and(|set| {
                set.kind == FileSetKind::CxxModules && set.scope != UsageScope::Private
            })
        })
        .collect()
}

fn evaluate_module_sets(
    model: &ProjectModel,
    usage: &mut UsageResolver<'_>,
    provider_id: TargetId,
    head_id: TargetId,
    config: &ConfigId,
) -> Result<Vec<EvaluatedModuleSet>, ResolveError> {
    let Some(provider) = model.target(provider_id) else {
        return Ok(Vec::new());
    };
    let module_sets = provider_module_sets(model, provider);
    if module_sets.is_empty() {
        return Ok(Vec::new());
    }
    let provider_dir = model.dir(provider.dir);
    let eval_ctx = EvalContext {
        config: config.clone(),
        language: Some("CXX".into()),
        head: Some(handle(head_id)),
        current: Some(handle(provider_id)),
        ..EvalContext::default()
    };
    let mut evaluated = Vec::with_capacity(module_sets.len());
    for set_id in module_sets {
        let Some(set) = model.file_set(set_id) else {
            continue;
        };
        let bases: Vec<_> = usage
            .file_set_paths(provider_id, b"BASE_DIRS", &set.base_dirs, &eval_ctx)?
            .into_iter()
            .map(|entry| {
                BT::new(
                    absolute_path(&provider_dir.source_dir, &entry.value),
                    entry.backtrace,
                )
            })
            .collect();
        let bases = if bases.is_empty() {
            vec![BT::new(
                provider_dir.source_dir.clone(),
                Backtrace::default(),
            )]
        } else {
            bases
        };
        let files: Vec<_> = usage
            .file_set_paths(provider_id, b"SOURCES", &set.files, &eval_ctx)?
            .into_iter()
            .map(|entry| {
                BT::new(
                    absolute_path(&provider_dir.source_dir, &entry.value),
                    entry.backtrace,
                )
            })
            .collect();
        if files.is_empty() {
            continue;
        }
        evaluated.push(EvaluatedModuleSet {
            name: set.name.clone(),
            bases,
            files,
            props: set.props.clone(),
        });
    }
    Ok(evaluated)
}

fn module_set_key(sets: &[EvaluatedModuleSet]) -> ModuleFileKey {
    sets.iter()
        .map(|set| {
            (
                set.name.clone(),
                set.files.iter().map(|entry| entry.value.clone()).collect(),
            )
        })
        .collect()
}

struct SynthState<'a> {
    model: &'a ProjectModel,
    config: &'a ConfigId,
    usage: UsageResolver<'a>,
    overlay: Option<ProjectModel>,
    cache: IndexMap<SynthKey, TargetId>,
    per_provider_count: IndexMap<TargetId, usize>,
    bindings: SyntheticBindings,
    provider_cache: IndexMap<TargetId, (CompileSettings, ModuleFileKey)>,
}

struct ConsumerContext<'a> {
    id: TargetId,
    target: &'a Target,
    settings: &'a CompileSettings,
}

fn record_binding(bindings: &mut SyntheticBindings, owner: TargetId, pair: (TargetId, TargetId)) {
    let entry = bindings.entry(owner).or_default();
    if !entry.contains(&pair) {
        entry.push(pair);
    }
}

fn ensure_provider_synth<'a>(
    state: &mut SynthState<'a>,
    consumer: &ConsumerContext<'_>,
    provider_id: TargetId,
    visiting: &mut IndexSet<TargetId>,
) -> Result<Option<TargetId>, ResolveError> {
    if provider_id == consumer.id || !visiting.insert(provider_id) {
        return Ok(None);
    }
    let model = state.model;
    let config = state.config;
    let Some(provider) = model
        .target(provider_id)
        .filter(|target| !is_std_provider(target) && !is_synthetic_provider(target))
    else {
        visiting.shift_remove(&provider_id);
        return Ok(None);
    };
    if provider_module_sets(model, provider).is_empty() {
        visiting.shift_remove(&provider_id);
        return Ok(None);
    }
    let evaluated_sets =
        evaluate_module_sets(model, &mut state.usage, provider_id, consumer.id, config)?;
    let c_module_key = module_set_key(&evaluated_sets);

    // Resolve transitive module providers bottom-up so imported module A
    // importing imported module B wires synth_A -> synth_B.
    let provider_link_ctx = EvalContext {
        config: config.clone(),
        language: Some("CXX".into()),
        link_language: Some("CXX".into()),
        head: Some(handle(consumer.id)),
        current: Some(handle(provider_id)),
        ..EvalContext::default()
    };
    let dep_targets = state
        .usage
        .interface_link_targets(provider_id, &provider_link_ctx)?;
    let mut dep_synths = Vec::new();
    for dep_id in dep_targets {
        if let Some(dep_synth_id) = ensure_provider_synth(state, consumer, dep_id, visiting)? {
            let pair = (dep_id, dep_synth_id);
            if !dep_synths.contains(&pair) {
                dep_synths.push(pair);
            }
        }
    }

    let needs_synth = if provider.is_imported()
        || provider.kind == TargetKind::InterfaceLibrary
        || !dep_synths.is_empty()
    {
        true
    } else {
        if !state.provider_cache.contains_key(&provider_id) {
            let p_settings =
                target_compile_settings(model, &mut state.usage, provider_id, provider, config)?;
            let p_sets =
                evaluate_module_sets(model, &mut state.usage, provider_id, provider_id, config)?;
            let p_module_key = module_set_key(&p_sets);
            state
                .provider_cache
                .insert(provider_id, (p_settings, p_module_key));
        }
        state
            .provider_cache
            .get(&provider_id)
            .is_some_and(|(p, p_module_key)| {
                p.compiler != consumer.settings.compiler
                    || p.standard != consumer.settings.standard
                    || p.extensions != consumer.settings.extensions
                    || p.required != consumer.settings.required
                    || p.runtime != consumer.settings.runtime
                    || p.options != consumer.settings.options
                    || p.definitions != consumer.settings.definitions
                    || *p_module_key != c_module_key
            })
    };
    if !needs_synth {
        visiting.shift_remove(&provider_id);
        return Ok(None);
    }

    let key = SynthKey {
        provider: provider_id,
        config: config.0.clone(),
        compiler: consumer.settings.compiler.clone(),
        standard: consumer.settings.standard.clone(),
        extensions: consumer.settings.extensions,
        required: consumer.settings.required,
        runtime: consumer.settings.runtime.clone(),
        options: consumer.settings.options.clone(),
        definitions: consumer.settings.definitions.clone(),
        module_files: c_module_key,
        dep_synths: dep_synths.clone(),
    };
    let synth_id = if let Some(&existing) = state.cache.get(&key) {
        existing
    } else {
        let model_mut = state.overlay.get_or_insert_with(|| model.clone());
        let count = state.per_provider_count.entry(provider_id).or_insert(0);
        let seq = *count;
        *count += 1;
        let synth_name: BString = format!("{}@synth_{seq}", provider.name).into();
        let mut synth = Target::new(
            synth_name,
            TargetKind::ObjectLibrary,
            TargetOrigin::Normal,
            consumer.target.dir,
            provider.backtrace.clone(),
            Default::default(),
        );
        synth.policies = consumer.target.policies;
        synth
            .props
            .set(SYNTHETIC_PROP.into(), provider.name.clone());
        synth
            .props
            .set("CXX_STANDARD".into(), consumer.settings.standard.clone());
        synth.props.set(
            "CXX_EXTENSIONS".into(),
            on_off(consumer.settings.extensions),
        );
        synth.props.set(
            "CXX_STANDARD_REQUIRED".into(),
            on_off(consumer.settings.required),
        );
        synth.props.set("CXX_SCAN_FOR_MODULES".into(), "ON".into());
        if let Some(runtime) = &consumer.settings.runtime {
            synth
                .props
                .set("MSVC_RUNTIME_LIBRARY".into(), runtime.clone());
        }
        // Copy provider interface usage requirements onto the synthetic provider.
        // Definitions come from the consumer-evaluated set so $<TARGET_PROPERTY:...>
        // expressions reflect the consuming target.
        for (iface, own) in [
            (
                UsageField::InterfaceCompileOptions,
                UsageField::CompileOptions,
            ),
            (
                UsageField::InterfaceIncludeDirectories,
                UsageField::IncludeDirectories,
            ),
            (
                UsageField::InterfaceSystemIncludeDirectories,
                UsageField::IncludeDirectories,
            ),
            (
                UsageField::InterfaceCompileFeatures,
                UsageField::CompileFeatures,
            ),
        ] {
            for entry in provider.usage.entries(iface) {
                synth.usage.push(own, entry.clone());
            }
        }
        let mut seen_definitions = IndexSet::new();
        for entry in &consumer.settings.raw_definitions {
            if seen_definitions.insert(entry.value.clone()) {
                synth
                    .usage
                    .push(UsageField::CompileDefinitions, entry.clone());
            }
        }
        if let Some(links) = &provider.usage.interface_link_libraries {
            let target_links = synth.usage.link_libraries.get_or_insert_with(Vec::new);
            for link in links {
                target_links.push(rcmake_model::LinkItem {
                    scope: Some(UsageScope::Private),
                    ..link.clone()
                });
            }
        }
        let mut seen_options = IndexSet::new();
        for entry in synth.usage.entries(UsageField::CompileOptions) {
            seen_options.insert(entry.value.clone());
        }
        for entry in &consumer.settings.raw_options {
            if consumer.settings.options.contains(&entry.value)
                && seen_options.insert(entry.value.clone())
            {
                synth.usage.push(UsageField::CompileOptions, entry.clone());
            }
        }
        for entry in &consumer.settings.raw_features {
            synth.usage.push(UsageField::CompileFeatures, entry.clone());
        }
        if !consumer.settings.standard.is_empty()
            && truth::is_off(
                synth
                    .props
                    .get(b"CXX_STANDARD")
                    .map_or(b"".as_slice(), |value| value.as_slice()),
            )
        {
            synth.usage.push(
                UsageField::CompileFeatures,
                BT::new("cxx_std_20".into(), Backtrace::default()),
            );
        }
        let synth_id = model_mut
            .add_target(synth)
            .map_err(|error| ResolveError::Message(error.to_string().into()))?;
        for set in evaluated_sets {
            model_mut
                .add_file_set(FileSet {
                    target: synth_id,
                    name: set.name,
                    kind: FileSetKind::CxxModules,
                    scope: UsageScope::Public,
                    base_dirs: set.bases,
                    files: set.files,
                    props: set.props,
                })
                .map_err(|error| ResolveError::Message(error.to_string().into()))?;
        }
        for &pair in &dep_synths {
            record_binding(&mut state.bindings, synth_id, pair);
        }
        state.cache.insert(key, synth_id);
        synth_id
    };
    visiting.shift_remove(&provider_id);
    Ok(Some(synth_id))
}

/// Discover consumers of interface C++ module file sets that require synthetic
/// BMI provider targets (imported module providers, interface module targets,
/// or local providers whose effective module compile settings differ from the
/// consumer's).
pub(crate) fn providers(
    model: &ProjectModel,
    consumers: &IndexMap<TargetId, ResolvedTarget>,
    config: &ConfigId,
) -> Result<Option<(ProjectModel, SyntheticBindings)>, ResolveError> {
    if !matches!(
        model.generator,
        Some(GeneratorKind::Ninja | GeneratorKind::NinjaMultiConfig)
    ) {
        return Ok(None);
    }
    let has_interface_modules = model.targets().any(|(_, target)| {
        !is_std_provider(target)
            && !is_synthetic_provider(target)
            && !provider_module_sets(model, target).is_empty()
    });
    if !has_interface_modules {
        return Ok(None);
    }
    let mut state = SynthState {
        model,
        config,
        usage: UsageResolver::new(model),
        overlay: None,
        cache: IndexMap::new(),
        per_provider_count: IndexMap::new(),
        bindings: SyntheticBindings::new(),
        provider_cache: IndexMap::new(),
    };

    for (&consumer_id, resolved) in consumers {
        if !resolved.compile_units.iter().any(|unit| unit.scan) {
            continue;
        }
        let Some(consumer_target) = model
            .target(consumer_id)
            .filter(|target| !is_std_provider(target) && !is_synthetic_provider(target))
        else {
            continue;
        };
        let link_ctx = EvalContext {
            config: config.clone(),
            language: Some("CXX".into()),
            link_language: Some("CXX".into()),
            ..EvalContext::default()
        };
        let linked_targets = state.usage.link_targets(consumer_id, &link_ctx)?;
        let mut consumer_settings: Option<CompileSettings> = None;
        for provider_id in linked_targets {
            if provider_id == consumer_id {
                continue;
            }
            let Some(provider) = model
                .target(provider_id)
                .filter(|target| !is_std_provider(target) && !is_synthetic_provider(target))
            else {
                continue;
            };
            if provider_module_sets(model, provider).is_empty() {
                continue;
            }
            if consumer_settings.is_none() {
                consumer_settings = Some(target_compile_settings(
                    model,
                    &mut state.usage,
                    consumer_id,
                    consumer_target,
                    config,
                )?);
            }
            let Some(c_settings) = consumer_settings.as_ref() else {
                continue;
            };
            let consumer_ctx = ConsumerContext {
                id: consumer_id,
                target: consumer_target,
                settings: c_settings,
            };
            let mut visiting = IndexSet::new();
            if let Some(synth_id) =
                ensure_provider_synth(&mut state, &consumer_ctx, provider_id, &mut visiting)?
            {
                record_binding(&mut state.bindings, consumer_id, (provider_id, synth_id));
            }
        }
    }
    Ok(state.overlay.map(|model| (model, state.bindings)))
}

fn on_off(value: bool) -> BString {
    if value { "ON" } else { "OFF" }.into()
}
