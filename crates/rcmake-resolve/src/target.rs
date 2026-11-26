use indexmap::{IndexMap, IndexSet, map::Entry};
use rcmake_base::{BString, path_lex::Lexical, truth};
use rcmake_diag::{Backtrace, MessageKind};
use rcmake_genex::EvalContext;
use rcmake_model::{
    BT, DirId, FileSet, FileSetId, FileSetKind, ProjectModel, Target, TargetId, TargetKind,
    UsageField, UsageScope,
};

use crate::{
    ArtifactSet, CompileGroup, CompileUnit, ResolveError, ResolvedTarget, TargetMetadata,
    cxx_modules::{
        is_std_provider, resolve_module_info, validate_module_scanning, validate_module_source,
    },
    ide::{self, IdeCompile},
    lower,
    sources::{self, SourcePaths, TargetInputs, absolute_path, external_object},
    unity,
    usage::{self, UsageResolver},
};

mod rules;
pub(crate) use rules::instantiate_rules;

pub(crate) fn resolve_target<'a>(
    model: &'a ProjectModel,
    paths: &SourcePaths<'_>,
    usage: &mut UsageResolver<'a>,
    id: TargetId,
    target: &Target,
    ctx: &EvalContext,
) -> Result<(ResolvedTarget, TargetInputs), ResolveError> {
    let dir = model.dir(target.dir);
    let mut metadata = TargetMetadata {
        name: target.name.clone(),
        kind: target.kind,
        source_dir: dir.source_dir.clone(),
        binary_dir: dir.binary_dir.clone(),
        backtrace: target.backtrace.clone(),
        folder: target.props.get(b"FOLDER").cloned(),
        exclude_from_all: excluded_from_all(model, usage, id, target, ctx)?,
        generator_provided: target.kind == TargetKind::GlobalTarget || is_std_provider(target),
    };
    if let Some(strategy) = lower::install_object_name_strategy(model, target)
        && !matches!(strategy.as_slice(), b"FULL" | b"SHORT")
    {
        let body: BString = format!(
            "Property INSTALL_OBJECT_NAME_STRATEGY of target \"{}\" set to the unsupported strategy {strategy}", target.name
        ).into();
        paths.emit(
            model,
            MessageKind::Error,
            crate::cxx_modules::generation_location(model, target),
            body.clone(),
        );
        return Err(ResolveError::Reported(body));
    }
    let flavor = usage.ide();
    let (artifacts, ide) = if let Some(flavor) = flavor {
        let (artifacts, facts) = ide_artifacts(model, usage, id, target, ctx, flavor)?;
        metadata.folder = facts.attributes.get(b"FOLDER".as_slice()).cloned();
        (artifacts, Some(facts))
    } else {
        let artifacts = if lower::artifacts::is_dll_platform(model, target.dir) {
            usage.ide_artifacts(id, &ctx.config)?.0
        } else if has_configured_artifact_path(model, target) {
            let mut artifacts = lower::compute_artifacts(model, target, &ctx.config);
            let (configured, _) = usage.ide_artifacts(id, &ctx.config)?;
            artifacts.file = configured.file;
            artifacts.linker_file = configured.linker_file;
            artifacts.soname_file = configured.soname_file;
            artifacts.import_file = configured.import_file;
            artifacts
        } else {
            lower::compute_artifacts(model, target, &ctx.config)
        };
        (artifacts, None)
    };
    let mut resolved = ResolvedTarget {
        fortran: None,
        id,
        config: ctx.config.clone(),
        metadata,
        artifacts,
        compile_groups: Vec::new(),
        compile_units: Vec::new(),
        link: None,
        dependencies: Vec::new(),
        cxx_modules: None,
        ide,
    };

    let export_symbol = export_symbol(target);
    let mut dependencies = IndexSet::new();
    let mut inputs = TargetInputs::default();
    let mut groups = IndexMap::new();
    let mut ide_compile = flavor.map(IdeCompile::new);
    let sources = usage.collect_property(id, b"SOURCES", ctx)?.entries;
    let mut membership = crate::pipeline::file_set_membership::Membership::default();
    let (file_sets, members) = collect_file_sets(model, paths, usage, id, ctx, &mut membership)?;
    if !target.file_sets.is_empty()
        && target
            .policies
            .policies
            .get(rcmake_diag::PolicyId::new(154))
            == rcmake_diag::PolicyStatus::New
        && matches!(
            model.generator,
            Some(
                rcmake_model::GeneratorKind::Ninja | rcmake_model::GeneratorKind::NinjaMultiConfig
            )
        )
    {
        let mut order = crate::FileSetOrder::default();
        for (file, member) in &members {
            let path = || lower::build_relative(model, file);
            if member.set.kind == FileSetKind::Sources
                && member
                    .set
                    .props
                    .get(b"INDEPENDENT_FILES")
                    .is_some_and(|value| truth::is_on(value))
            {
                order.independent.insert(path());
            } else if member.set.scope != UsageScope::Private {
                order.public.insert(path());
            }
        }
        inputs.file_set_order = Some(order);
    }
    if !membership.is_empty() {
        for source in &sources {
            if let Some(value) = paths.resolve_at(model, dir, source)? {
                membership.source(&value);
            }
        }
        membership.validate(model, paths, target)?;
    }
    let unity = paths.unity.as_ref().and_then(|plan| plan.get(&id));
    let collect_unity = paths.unity.is_none() && unity::enabled(target);
    let unity_sources = unity.into_iter().flat_map(|plan| {
        plan.files
            .iter()
            .map(|(path, file)| (BT::new(path.clone(), target.backtrace.clone()), Some(file)))
    });
    let original_sources = sources
        .into_iter()
        .chain(members.values().map(|member| member.source.clone()))
        .map(|source| (source, None));
    let mut compile_ctx = ctx.clone();
    let mut entry_start = 0;
    let mut entry_backtrace = None::<rcmake_diag::Backtrace>;
    for (mut source, unity_file) in unity_sources.chain(original_sources) {
        if entry_backtrace
            .as_ref()
            .is_none_or(|trace| !trace.frames().eq(source.backtrace.frames()))
        {
            entry_start = inputs.sources.len();
            entry_backtrace = Some(source.backtrace.clone());
        }
        if unity_file.is_none() {
            let Some(value) = paths.resolve_at(model, dir, &source)? else {
                inputs.sources.truncate(entry_start);
                break;
            };
            source.value = value;
        }
        if !inputs.sources.insert(source.value.clone())
            || unity
                .and_then(|plan| plan.file_for_source(&source.value, &ctx.config))
                .is_some()
        {
            continue;
        }
        let owner = paths.object_owner(&source.value);
        dependencies.extend(owner);
        if matches!(
            target.kind,
            TargetKind::Utility | TargetKind::GlobalTarget | TargetKind::InterfaceLibrary
        ) && !is_std_provider(target)
        {
            continue;
        }
        if owner.is_some() || external_object(dir, &source.value) {
            inputs
                .objects
                .insert(lower::build_relative(model, &source.value));
            continue;
        }
        let member = members.get(&source.value);
        let source_props = sources::properties(dir, &source.value);
        if source_props.get(b"MACOSX_PACKAGE_LOCATION").is_some()
            && lower::is_darwin(model, target.dir)
            && ide::bundle_kind(target) != ide::BundleKind::None
        {
            continue;
        }
        if paths.is_directory(&source.value)
            || member.is_some_and(|member| member.set.kind == FileSetKind::Headers)
        {
            continue;
        }
        let module = member.filter(|member| member.set.kind == FileSetKind::CxxModules);
        let header_only = source_props
            .get(b"HEADER_FILE_ONLY")
            .is_some_and(|value| truth::is_on(value));
        let language = if header_only {
            None
        } else {
            unity_file
                .map(|file| file.language.clone())
                .or_else(|| {
                    member
                        .and_then(|member| member.set.props.get(b"LANGUAGE"))
                        .or_else(|| source_props.get(b"LANGUAGE"))
                        .filter(|language| !language.is_empty())
                        .cloned()
                })
                .or_else(|| lower::source_language(model, target, &source.value))
        };
        if module.is_some() {
            validate_module_source(model, paths, target, &source.value, language.as_ref())?;
        }
        let Some(language) = language else { continue };
        if member.is_some_and(|member| {
            member.set.kind == FileSetKind::Sources
                && member
                    .set
                    .props
                    .get(b"SKIP_PRECOMPILE_HEADERS")
                    .is_some_and(|value| truth::is_on(value))
        }) {
            inputs.sources_without_pch.insert(source.value.clone());
        }
        if collect_unity && let Some(group) = unity::source_group(id, source_props, member) {
            inputs.unity_groups.insert(source.value.clone(), group);
        }
        let group_key = (
            language.clone(),
            member.map(|member| member.id),
            sources::has_source_compile_properties(source_props, &ctx.config)
                .then(|| source.value.clone()),
            unity_file.map(|file| (file.relocatable, file.config_guard)),
        );
        let (group, facts) = match groups.entry(group_key) {
            Entry::Occupied(entry) => (entry.index(), entry.into_mut()),
            Entry::Vacant(entry) => {
                compile_ctx.language = Some(language.clone());
                let features =
                    usage.collect_for_result(id, UsageField::CompileFeatures, &compile_ctx)?;
                for feature in &features.entries {
                    if let Err(message) = model.validate_compile_feature(&feature.value, |key| {
                        lower::definition(model, target.dir, key)
                    }) {
                        let mut message = message;
                        if let Some(first) = message.first_mut() {
                            first.make_ascii_uppercase();
                        }
                        let body =
                            BString::from(format!("{message} for target \"{}\".", target.name));
                        paths.emit(
                            model,
                            MessageKind::Error,
                            crate::cxx_modules::generation_location(model, target),
                            body.clone(),
                        );
                        return Err(ResolveError::Reported(body));
                    }
                }
                if let Some(standard) = lower::required_standard(&language, &features.entries) {
                    inputs
                        .standard_requirements
                        .insert(language.clone(), standard);
                }
                let mut flags = Vec::new();
                if let Some(value) = target.props.get(b"COMPILE_FLAGS") {
                    if ide_compile.is_some() {
                        let value =
                            usage.eval_ide_property(id, b"COMPILE_FLAGS", value, &compile_ctx)?;
                        sources::append_compile_flags(&mut flags, &value, Backtrace::default());
                    } else {
                        sources::append_compile_flags(&mut flags, value, Backtrace::default());
                    }
                }
                flags.extend(
                    usage
                        .collect_for_result(id, UsageField::CompileOptions, &compile_ctx)?
                        .entries,
                );
                let mut defines = strip_define_flags(
                    usage
                        .collect_for_result(id, UsageField::CompileDefinitions, &compile_ctx)?
                        .entries,
                );
                if let Some(symbol) = &export_symbol
                    && !defines.iter().any(|define| define.value == *symbol)
                {
                    defines.insert(0, BT::new(symbol.clone(), Backtrace::default()));
                }
                let includes = usage.collect_includes_for_result(id, &compile_ctx)?;
                let group = entry.index();
                let mut facts = CompileGroup {
                    language,
                    flags,
                    defines,
                    source_define_start: None,
                    includes,
                    units: Vec::new(),
                };
                let base = ide_compile.as_ref().map(|ide| ide.settings(&facts));
                sources::apply_compile_properties(
                    usage,
                    id,
                    &compile_ctx,
                    &source,
                    member,
                    source_props,
                    &mut facts,
                )?;
                if let Some(file) = unity_file {
                    file.apply_compile_properties(
                        &model.dir(DirId::ROOT).source_dir,
                        &ctx.config,
                        &mut facts,
                    );
                }
                if let (Some(ide), Some(base)) = (&mut ide_compile, base) {
                    ide.record_group(group, base, &facts);
                }
                let facts = entry.insert(facts);
                (group, facts)
            }
        };
        if let Err(error) =
            sources::install_object_name(model, target.dir, &source.value, &ctx.config)
        {
            return Err(match error {
                ResolveError::Message(body) => {
                    paths.emit(
                        model,
                        MessageKind::Error,
                        crate::cxx_modules::generation_location(model, target),
                        body.clone(),
                    );
                    ResolveError::Reported(body)
                }
                error => error,
            });
        }
        facts.units.push(resolved.compile_units.len());
        if flavor.is_none() && !lower::short_object_paths(model) {
            crate::sources::custom_object_name(model, target.dir, &source.value).map_err(
                |error| match error {
                    ResolveError::Message(body) => {
                        paths.emit(
                            model,
                            MessageKind::Error,
                            crate::cxx_modules::generation_location(model, target),
                            body.clone(),
                        );
                        ResolveError::Reported(body)
                    }
                    error => error,
                },
            )?;
        }
        let object = lower::compile_object_path(
            model,
            target,
            &ctx.config,
            &source.value,
            unity_file,
            flavor,
        );
        resolved.compile_units.push(CompileUnit {
            object,
            source,
            group,
            scan: false,
        });
    }
    resolved.compile_groups = groups.into_values().collect();
    if let Some(ide) = ide_compile {
        ide.finish(&mut resolved, model, target, paths, &inputs, &members)?;
    }
    resolve_module_info(
        model,
        paths,
        target,
        &inputs,
        &mut resolved,
        file_sets,
        &members,
        unity,
    );
    if file_sets || resolved.compile_units.iter().any(|unit| unit.scan) {
        validate_module_scanning(model, paths, target, &inputs, file_sets)?;
    }
    resolved.dependencies = dependencies.into_iter().collect();
    Ok((resolved, inputs))
}

fn has_configured_artifact_path(model: &ProjectModel, target: &Target) -> bool {
    (lower::is_darwin(model, target.dir)
        && (ide::bundle_kind(target) != ide::BundleKind::None
            || (target.kind == TargetKind::SharedLibrary
                && target
                    .props
                    .get(b"ENABLE_EXPORTS")
                    .is_some_and(|value| truth::is_on(value)))))
        || target.props.names().iter().any(|key| {
            (key.windows(b"OUTPUT_NAME".len())
                .any(|part| part == b"OUTPUT_NAME")
                || key
                    .windows(b"OUTPUT_DIRECTORY".len())
                    .any(|part| part == b"OUTPUT_DIRECTORY"))
                && target
                    .props
                    .get(key)
                    .is_some_and(|value| value.windows(2).any(|part| part == b"$<"))
        })
}

fn ide_artifacts(
    model: &ProjectModel,
    usage: &mut UsageResolver<'_>,
    id: TargetId,
    target: &Target,
    ctx: &EvalContext,
    flavor: ide::IdeFlavor,
) -> Result<(ArtifactSet, Box<ide::IdeFacts>), ResolveError> {
    let dir = model.dir(target.dir);
    let (artifacts, naming) = usage.ide_artifacts(id, &ctx.config)?;
    let bundle = if flavor == ide::IdeFlavor::Xcode {
        ide::bundle_kind(target)
    } else {
        ide::BundleKind::None
    };
    let info_plist = (bundle != ide::BundleKind::None).then(|| {
        absolute_path(
            &dir.binary_dir,
            &[b"CMakeFiles/", target.name.as_slice(), b".dir/Info.plist"].concat(),
        )
    });
    let mut facts = Box::new(ide::IdeFacts {
        naming,
        bundle,
        info_plist,
        ..Default::default()
    });
    facts.collect_properties(usage, target, id, ctx, flavor)?;
    Ok((artifacts, facts))
}

pub(crate) struct FileSetMember<'a> {
    pub(crate) id: FileSetId,
    pub(crate) set: &'a FileSet,
    pub(crate) source: BT<BString>,
    pub(crate) relative_directory: BString,
}

pub(crate) fn collect_file_sets<'a>(
    model: &'a ProjectModel,
    paths: &SourcePaths<'_>,
    usage: &mut UsageResolver<'a>,
    id: TargetId,
    ctx: &EvalContext,
    membership: &mut crate::pipeline::file_set_membership::Membership,
) -> Result<(bool, IndexMap<BString, FileSetMember<'a>>), ResolveError> {
    let mut has_modules = false;
    let mut members = IndexMap::new();
    let mut ctx = ctx.clone();
    ctx.head = Some(usage::handle(id));
    for (set_id, set) in usage.source_file_sets(id, &ctx)? {
        let is_module = set.kind == FileSetKind::CxxModules;
        has_modules |= is_module;
        let owner = model
            .target(set.target)
            .ok_or_else(|| ResolveError::Message("Unknown file set target.".into()))?;
        let dir = model.dir(owner.dir);
        let mut bases = usage
            .file_set_paths(set.target, b"BASE_DIRS", &set.base_dirs, &ctx)?
            .into_iter()
            .map(|base| absolute_path(&dir.source_dir, &base.value))
            .collect::<Vec<_>>();
        if bases.is_empty() {
            bases.push(dir.source_dir.clone());
        }
        for file in usage.file_set_paths(set.target, b"SOURCES", &set.files, &ctx)? {
            let Some(value) = resolved_file_set_member(model, paths, dir, &file)? else {
                continue;
            };
            if set.kind != FileSetKind::Headers {
                membership.member(&value, set_id, &set.name);
            }
            let parent = Lexical::new(&value).parent();
            let relative_directory = bases.iter().find_map(|base| {
                let base = Lexical::new(base);
                let parent = Lexical::new(parent);
                base.prefix(parent).then(|| parent.relative(base)).flatten()
            });
            let relative_directory =
                match relative_directory {
                    Some(path) if path == "." => BString::default(),
                    Some(path) => path,
                    None if !is_module => BString::default(),
                    None => return Err(ResolveError::Message(
                        format!(
                            "File {value} must be in one of the base directories of file set {}.",
                            set.name
                        )
                        .into(),
                    )),
                };
            let member = FileSetMember {
                id: set_id,
                set,
                source: BT::new(value.clone(), file.backtrace.clone()),
                relative_directory,
            };
            match members.entry(value) {
                Entry::Occupied(mut entry) if is_module => {
                    entry.insert(member);
                }
                Entry::Vacant(entry) => {
                    entry.insert(member);
                }
                _ => {}
            }
        }
    }
    Ok((has_modules, members))
}

fn export_symbol(target: &Target) -> Option<BString> {
    let exports = matches!(
        target.kind,
        TargetKind::SharedLibrary | TargetKind::ModuleLibrary
    ) || (target.kind == TargetKind::Executable
        && target
            .props
            .get(b"ENABLE_EXPORTS")
            .is_some_and(|value| truth::is_on(value)));
    if !exports {
        return None;
    }
    if let Some(symbol) = target.props.get(b"DEFINE_SYMBOL") {
        return (!symbol.is_empty()).then(|| symbol.clone());
    }
    let mut symbol = BString::default();
    if target.name.first().is_some_and(u8::is_ascii_digit) {
        symbol.push(b'_');
    }
    symbol.extend(target.name.iter().map(|&byte| {
        if byte.is_ascii_alphanumeric() {
            byte
        } else {
            b'_'
        }
    }));
    symbol.extend_from_slice(b"_EXPORTS");
    Some(symbol)
}

fn resolved_file_set_member(
    model: &ProjectModel,
    paths: &SourcePaths<'_>,
    dir: &rcmake_model::Directory,
    file: &BT<BString>,
) -> Result<Option<BString>, ResolveError> {
    if file.value.windows(2).any(|pair| pair == b"\\;") {
        if let Some(path) = paths.find(dir, &file.value) {
            return Ok(Some(path));
        }
        // File set entries can carry escaping from more than one list expansion.
        let mut name = BString::default();
        for byte in file.value.iter().copied() {
            if byte == b';' {
                while name.last() == Some(&b'\\') {
                    name.pop();
                }
            }
            name.push(byte);
        }
        if let Some(path) = paths.find(dir, &name) {
            return Ok(Some(path));
        }
    }
    paths.resolve_at(model, dir, file)
}

/// Remove a leading `-D` from each definition, as CMake does when it adds definitions
/// to a compile line. A generator expression can produce such a value after the
/// `target_compile_definitions()` command has removed the literal ones.
fn strip_define_flags(defines: Vec<BT<BString>>) -> Vec<BT<BString>> {
    let mut seen = IndexMap::new();
    for mut define in defines {
        if define.value.starts_with(b"-D") {
            define.value.drain(..2);
        }
        seen.entry(define.value).or_insert(define.backtrace);
    }
    seen.into_iter()
        .map(|(value, backtrace)| BT::new(value, backtrace))
        .collect()
}

/// Evaluate `EXCLUDE_FROM_ALL` for one configuration. The property can hold a
/// generator expression, for example `$<NOT:$<CONFIG:Release>>`.
fn excluded_from_all(
    model: &ProjectModel,
    usage: &mut UsageResolver<'_>,
    id: TargetId,
    target: &Target,
    ctx: &EvalContext,
) -> Result<bool, ResolveError> {
    let Some(value) = target
        .props
        .get(b"EXCLUDE_FROM_ALL")
        .filter(|value| value.windows(2).any(|pair| pair == b"$<"))
    else {
        return Ok(model.excluded_from_all(target));
    };
    let ctx = EvalContext {
        head: Some(usage::handle(id)),
        current: Some(usage::handle(id)),
        ..ctx.clone()
    };
    usage
        .eval_ide_property(id, b"EXCLUDE_FROM_ALL", value, &ctx)
        .map(|value| truth::is_on(&value))
}
