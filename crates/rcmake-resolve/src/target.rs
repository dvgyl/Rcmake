use indexmap::{IndexMap, IndexSet, map::Entry};
use rcmake_base::{BString, path_lex::Lexical, truth};
use rcmake_diag::Backtrace;
use rcmake_genex::EvalContext;
use rcmake_model::{
    BT, FileSet, FileSetId, FileSetKind, ProjectModel, Target, TargetId, TargetKind, UsageField,
};

use crate::{
    ArtifactSet, CompileGroup, CompileUnit, ResolveError, ResolvedTarget, TargetMetadata,
    cxx_modules::{
        STD_MODULE_TARGET, resolve_module_info, validate_module_scanning, validate_module_source,
    },
    ide::{self, IdeCompile},
    lower,
    sources::{self, SourcePaths, TargetInputs, absolute_path, external_object},
    unity,
    usage::{self, UsageResolver},
};

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
        exclude_from_all: model.excluded_from_all(target),
        generator_provided: target.kind == TargetKind::GlobalTarget
            || target.name == STD_MODULE_TARGET,
    };
    let flavor = usage.ide();
    let (artifacts, ide) = if let Some(flavor) = flavor {
        let (artifacts, facts) = ide_artifacts(model, usage, id, target, ctx, flavor)?;
        metadata.folder = facts.attributes.get(b"FOLDER".as_slice()).cloned();
        (artifacts, Some(facts))
    } else {
        (lower::compute_artifacts(model, target, &ctx.config), None)
    };
    let mut resolved = ResolvedTarget {
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
    let (file_sets, members) = collect_file_sets(model, paths, usage, id, ctx)?;
    let sources = usage.collect_property(id, b"SOURCES", ctx)?.entries;
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
    for (mut source, unity_file) in unity_sources.chain(original_sources) {
        if unity_file.is_none() {
            source.value = paths.resolve_at(model, dir, &source)?;
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
        ) && target.name != STD_MODULE_TARGET
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
        if member.is_some_and(|member| member.set.kind == FileSetKind::Headers) {
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
        if collect_unity && let Some(group) = unity::source_group(id, source_props, member) {
            inputs.unity_groups.insert(source.value.clone(), group);
        }
        let group_key = (
            language.clone(),
            member.map(|member| member.id),
            sources::has_source_compile_properties(source_props).then(|| source.value.clone()),
            unity_file.map(|file| (file.relocatable, file.config_guard)),
        );
        let (group, facts) = match groups.entry(group_key) {
            Entry::Occupied(entry) => (entry.index(), entry.into_mut()),
            Entry::Vacant(entry) => {
                let compile_ctx = EvalContext {
                    language: Some(language.clone()),
                    ..ctx.clone()
                };
                let features = usage.collect(id, UsageField::CompileFeatures, &compile_ctx)?;
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
                        .collect(id, UsageField::CompileOptions, &compile_ctx)?
                        .entries,
                );
                let mut defines = usage
                    .collect(id, UsageField::CompileDefinitions, &compile_ctx)?
                    .entries;
                if let Some(symbol) = &export_symbol
                    && !defines.iter().any(|define| define.value == *symbol)
                {
                    defines.insert(0, BT::new(symbol.clone(), target.backtrace.clone()));
                }
                let includes = usage.collect_includes(id, &compile_ctx)?;
                let group = entry.index();
                let mut facts = CompileGroup {
                    language,
                    flags,
                    defines,
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
                    file.apply_compile_properties(model, &ctx.config, &mut facts);
                }
                if let (Some(ide), Some(base)) = (&mut ide_compile, base) {
                    ide.record_group(group, base, &facts, || {
                        lower::language_flags(model, target, &ctx.config, &facts.language)
                    });
                }
                let facts = entry.insert(facts);
                (group, facts)
            }
        };
        facts.units.push(resolved.compile_units.len());
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
        ide.finish(&mut resolved, dir, paths, &inputs, &members);
    }
    resolve_module_info(
        model,
        target,
        &inputs,
        &mut resolved,
        file_sets,
        &members,
        unity,
    );
    if file_sets || resolved.compile_units.iter().any(|unit| unit.scan) {
        validate_module_scanning(model, paths, target, &inputs, file_sets)?;
        let standard = inputs.standard_requirements.get(b"CXX".as_slice()).copied();
        let responses = lower::scan_responses(model, target, &resolved, standard)?;
        if let Some(modules) = &mut resolved.cxx_modules {
            modules.scan_responses = responses;
        }
    }
    resolved.dependencies = dependencies.into_iter().collect();
    Ok((resolved, inputs))
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
    let (artifacts, naming) = usage.ide_artifacts(id, &ctx.config);
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

fn collect_file_sets<'a>(
    model: &'a ProjectModel,
    paths: &SourcePaths<'_>,
    usage: &mut UsageResolver<'a>,
    id: TargetId,
    ctx: &EvalContext,
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
            let value = paths.resolve_at(model, dir, &file)?;
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
    if !matches!(
        target.kind,
        TargetKind::SharedLibrary | TargetKind::ModuleLibrary
    ) {
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
