use indexmap::{IndexMap, IndexSet, map::Entry};
use rcmake_base::{BString, ByteSlice, list, path_lex::Lexical, truth};
use rcmake_diag::{Backtrace, Location, MessageKind};
use rcmake_genex::EvalContext;
use rcmake_model::{
    BT, CustomCommand, CustomCommandKind, DirId, FileSet, FileSetId, FileSetKind, ProjectModel,
    Target, TargetId, TargetKind, UsageField,
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
            let Some(value) = paths.resolve_at(model, dir, &source)? else {
                inputs.sources.clear();
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
                let compile_ctx = EvalContext {
                    language: Some(language.clone()),
                    ..ctx.clone()
                };
                let features = usage.collect(id, UsageField::CompileFeatures, &compile_ctx)?;
                for feature in &features.entries {
                    if let Err(message) = model.validate_compile_feature(&feature.value, |key| {
                        lower::definition(model, target.dir, key)
                    }) {
                        if lower::definition(model, target.dir, b"CMAKE_DEBUG_TARGET_PROPERTIES")
                            .is_some_and(|properties| {
                                list::split(properties, list::Empty::Drop)
                                    .iter()
                                    .any(|property| property == "COMPILE_FEATURES")
                            })
                        {
                            paths.emit(
                                model,
                                MessageKind::Log,
                                Location::Backtrace(feature.backtrace.clone()),
                                format!(
                                    "Used compile features for target {}:\n\n * {}",
                                    target.name, feature.value
                                )
                                .into(),
                            );
                        }
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
            let Some(value) = paths.resolve_at(model, dir, &file)? else {
                continue;
            };
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

pub(crate) fn instantiate_rules(
    model: &ProjectModel,
    host: &dyn rcmake_host::Host,
) -> Result<Option<ProjectModel>, ResolveError> {
    let selected = model
        .targets()
        .flat_map(|(_, target)| {
            target.file_sets.iter().filter_map(move |id| {
                let set = model.file_set(*id)?;
                let name = set.props.get(b"_RCMAKE_RULE")?;
                let dir = model.file_set_rule_dir(*id).unwrap_or(target.dir);
                Some((*id, target, set, name, dir, model.custom_rule(dir, name)?))
            })
        })
        .collect::<Vec<_>>();
    if selected.is_empty() {
        return Ok(None);
    }
    let mut prepared = model.clone();
    let diagnostics = SourcePaths::new(host);
    for (id, target, set, name, dir, rule) in selected {
        let directory = model.dir(dir);
        let mut replacements = IndexMap::from_iter([
            ("RULE".into(), name.clone()),
            ("TARGET".into(), target.name.clone()),
            ("FILE_SET".into(), set.name.clone()),
            (
                "SOURCE_DIR".into(),
                model.dir(DirId::ROOT).source_dir.clone(),
            ),
            (
                "BINARY_DIR".into(),
                model.dir(DirId::ROOT).binary_dir.clone(),
            ),
            (
                "OUTPUT_CURRENT_SOURCE_DIR".into(),
                model.dir(target.dir).source_dir.clone(),
            ),
        ]);
        let output_set = rule
            .get(b"OUTPUT_FILE_SET")
            .map_or_else(Vec::new, |value| list::split(value, list::Empty::Keep));
        let [output_name, output_type] = output_set.as_slice() else {
            continue;
        };
        let output_name = expand_rule_patterns(output_name, &replacements, true);
        let output_kind =
            FileSetKind::from_name(output_type).unwrap_or(if output_type == b"SYCL_HEADERS" {
                FileSetKind::Headers
            } else {
                FileSetKind::Sources
            });
        let existing = target.file_sets.iter().copied().find(|&id| {
            model
                .file_set(id)
                .is_some_and(|set| set.name == output_name)
        });
        let invalid = if output_name.contains(&b'<') || output_name.contains(&b'>') {
            Some(format!(
                "Output File Set name, for rule \"{name}\", did not expand correctly:\n\n  \"{output_name}\""
            ))
        } else {
            existing.and_then(|id| model.file_set(id)).filter(|set| set.kind != output_kind).map(|set| {
                format!("The output file set \"{output_name}\", for the target \"{}\", has the type \"{}\" instead of \"{output_type}\", as specified by the rule \"{name}\".", target.name, set.kind.as_str())
            })
        };
        if let Some(body) = invalid {
            let body = BString::from(body);
            diagnostics.emit(
                model,
                MessageKind::Error,
                Location::FileLine {
                    file: lower::join(&directory.source_dir, b"CMakeLists.txt")
                        .as_slice()
                        .into(),
                    line: 0,
                },
                body.clone(),
            );
            return Err(ResolveError::Reported(body));
        }
        replacements.insert("OUTPUT_FILE_SET".into(), output_name.clone());
        replacements.insert("CURRENT_BINARY_DIR".into(), directory.binary_dir.clone());
        replacements.insert("CURRENT_SOURCE_DIR".into(), directory.source_dir.clone());
        let output_dir = &model.dir(target.dir).binary_dir;
        replacements.insert("OUTPUT_CURRENT_BINARY_DIR".into(), output_dir.clone());
        let mut generated = Vec::new();
        for source in &set.files {
            let absolute = absolute_path(&directory.source_dir, &source.value);
            let lexical = Lexical::new(&absolute);
            let filename = lexical.filename();
            let stem = filename
                .rsplit_once_str(b".")
                .map_or(filename, |(base, _)| base);
            let mut replacements = replacements.clone();
            replacements.extend([
                ("SOURCE".into(), absolute.clone()),
                ("INPUT_DIR".into(), lexical.parent().into()),
                ("FILE_NAME".into(), filename.into()),
                ("BASE_NAME".into(), stem.into()),
            ]);
            let source_props = directory.source_props.get(&absolute);
            for key in [
                b"INCLUDE_DIRECTORIES".as_slice(),
                b"COMPILE_OPTIONS",
                b"COMPILE_DEFINITIONS",
            ] {
                let mut values = [
                    rule.get(key),
                    source_props.and_then(|props| props.get(key)),
                    set.props.get(key),
                ];
                if key == b"INCLUDE_DIRECTORIES" {
                    values.swap(0, 2);
                }
                let values = values
                    .into_iter()
                    .flatten()
                    .filter(|value| !value.is_empty());
                replacements.insert(key.into(), list::join(values));
            }
            let source_key = [name.as_slice(), b"_PATTERNS"].concat();
            let reserved = replacements
                .keys()
                .cloned()
                .collect::<std::collections::BTreeSet<_>>();
            let patterns = set
                .props
                .get(b"RULE_PATTERNS")
                .into_iter()
                .chain(
                    directory
                        .source_props
                        .get(&absolute)
                        .and_then(|props| props.get(&source_key)),
                )
                .chain(set.props.get(b"_RCMAKE_RULE_FILE_SET_PATTERNS"))
                .chain(
                    set.props
                        .get(&[b"_RCMAKE_RULE_PATTERNS_".as_slice(), &absolute].concat()),
                );
            for pattern in patterns.flat_map(|value| list::split(value, list::Empty::Drop)) {
                if let Some((key, value)) = pattern.split_once_str(b"=")
                    && key.len() >= 2
                    && key.first().is_some_and(u8::is_ascii_uppercase)
                    && key
                        .iter()
                        .all(|key| key.is_ascii_uppercase() || key.is_ascii_digit() || *key == b'_')
                    && !reserved.contains(key)
                {
                    replacements.insert(key.into(), value.into());
                }
            }
            let expand = |value: &[u8]| expand_rule_patterns(value, &replacements, false);
            let words = |key: &[u8]| {
                rule.get(key)
                    .map_or_else(Vec::new, |value| list::split(value, list::Empty::Drop))
            };
            let paths = |key: &[u8]| {
                words(key)
                    .iter()
                    .map(|path| {
                        BT::new(
                            crate::sources::absolute_path(&directory.binary_dir, &expand(path)),
                            source.backtrace.clone(),
                        )
                    })
                    .collect::<Vec<_>>()
            };
            let mut command = CustomCommand::new(
                target.dir,
                CustomCommandKind::Output,
                source.backtrace.clone(),
                target.policies.policies,
            );
            command.outputs = paths(b"OUTPUT");
            command.byproducts = paths(b"BYPRODUCTS");
            command.depends = words(b"DEPENDS")
                .iter()
                .map(|value| BT::new(expand(value), source.backtrace.clone()))
                .collect();
            command
                .depends
                .push(BT::new(absolute, source.backtrace.clone()));
            command.depfile = rule
                .get(b"DEPFILE")
                .filter(|value| !value.is_empty())
                .map(|value| expand(value));
            command.comment = rule.get(b"COMMENT").map(|value| expand(value));
            command.verbatim = rule
                .get(b"VERBATIM")
                .is_some_and(|value| truth::is_on(value));
            command.command_expand_lists = rule
                .get(b"COMMAND_EXPAND_LISTS")
                .is_some_and(|value| truth::is_on(value));
            command.command_lines = (0..)
                .map_while(|index| {
                    rule.get(format!("COMMAND_{index}").as_bytes())
                        .map(|value| {
                            list::split(value, list::Empty::Keep)
                                .iter()
                                .map(|arg| expand(arg))
                                .collect()
                        })
                })
                .collect();
            generated.extend(command.outputs.iter().cloned());
            prepared
                .add_command(command)
                .map_err(|error| ResolveError::Message(error.to_string().into()))?;
        }
        if let Some(input) = prepared.file_set_mut(id) {
            input.files.clear();
            if existing.is_none() {
                input.name = output_name;
                input.kind = output_kind;
                input.base_dirs.clear();
            }
        }
        if let Some(output) = prepared.file_set_mut(existing.unwrap_or(id)) {
            output.files.extend(generated);
            if output.base_dirs.is_empty() {
                output.base_dirs.push(BT::new(
                    directory.binary_dir.clone(),
                    target.backtrace.clone(),
                ));
            }
        }
    }
    Ok(Some(prepared))
}

fn expand_rule_patterns(
    value: &[u8],
    patterns: &IndexMap<BString, BString>,
    preserve_unknown: bool,
) -> BString {
    let mut value = BString::from(value);
    for _ in 0..patterns.len().min(32) {
        let mut next = value.clone();
        for (key, replacement) in patterns {
            let pattern = [b"<", key.as_slice(), b">"].concat();
            let extra = replacement.len().saturating_sub(pattern.len());
            let growth = next.find_iter(&pattern).count().saturating_mul(extra);
            if next.len().saturating_add(growth) > 65_536 {
                return value;
            }
            next = next.replace(&pattern, replacement).into();
        }
        if value == next {
            break;
        }
        value = next;
    }
    if preserve_unknown {
        return value;
    }
    let mut expanded = BString::default();
    let mut remaining = value.as_slice();
    while let Some((before, after)) = remaining.split_once_str(b"<")
        && let Some((name, tail)) = after.split_once_str(b">")
    {
        expanded.extend_from_slice(before);
        let placeholder = !before.ends_with(b"$")
            && !name.is_empty()
            && name
                .iter()
                .all(|byte| byte.is_ascii_alphanumeric() || *byte == b'_');
        let (open, close) = if placeholder {
            (b'{', b'}')
        } else {
            (b'<', b'>')
        };
        expanded.push(open);
        expanded.extend_from_slice(name);
        expanded.push(close);
        remaining = tail;
    }
    expanded.extend_from_slice(remaining);
    expanded
}
