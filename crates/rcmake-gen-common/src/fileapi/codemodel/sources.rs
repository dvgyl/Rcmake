//! Source and compile group entries in a target reply.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use indexmap::IndexMap;
use rcmake_base::{BString, Limits, list, path_lex::Lexical, truth::is_on};
use rcmake_diag::{Backtrace, Frame, FrameKind};
use rcmake_genex::EvalContext;
use rcmake_model::{BT, PropertyMap, Target, TargetId, TargetKind, UsageField};
use rcmake_resolve::{
    BuildStep, CompileGroup, InterfaceInclude, ResolvedTarget,
    lower::{
        LanguageStandard, codemodel_compile_info, codemodel_frameworks, uses_apple_frameworks,
    },
    usage::UsageResolver,
};

use crate::GenError;

use super::super::{Bool, FileApi, Members, Object, Value, array, json_bytes, members, relative};
use super::{
    ConfigIndices,
    backtraces::Backtraces,
    file_sets::{FileSets, Membership, interface_sources, normalize_source_group_name},
    number, optional_array,
    targets::TargetRef,
};

struct Source {
    path: BString,
    trace: Backtrace,
    generated: bool,
    source_group: BString,
    compile: Option<(usize, BString)>,
}

impl FileApi<'_, '_> {
    #[allow(clippy::too_many_arguments)]
    pub(super) fn write_target_sources(
        &self,
        target: &ResolvedTarget,
        raw: Option<&Target>,
        targets: &BTreeMap<TargetId, TargetRef<'_>>,
        body: &mut Members,
        graph: &mut Backtraces<'_>,
        usage: &mut UsageResolver<'_>,
        config_indices: &ConfigIndices<'_>,
    ) -> Result<(), GenError> {
        let build_graph = config_indices.build_graph;
        let file_sets = FileSets::collect(self.model, target, raw, self.source, usage)?;
        optional_array(body, "fileSets", file_sets.entries.iter().cloned());
        let pch = build_graph.and_then(|graph| graph.pch_details.get(&target.id));
        let mut sources = Vec::new();
        let file_trace = Backtrace::default().push(Frame {
            file: Lexical::new(&target.metadata.source_dir)
                .append(b"CMakeLists.txt")
                .as_slice()
                .into(),
            line: 0,
            name: None,
            kind: FrameKind::Command,
        });
        if let Some(pch) = pch {
            sources.extend(pch.create.iter().map(|entry| Source {
                path: entry.path.clone(),
                trace: file_trace.clone(),
                generated: false,
                source_group: "Source Files".into(),
                compile: Some((entry.group, entry.flags.clone())),
            }));
        }

        let source_dir = Lexical::new(&target.metadata.source_dir);
        let units: BTreeMap<_, _> = target
            .compile_units
            .iter()
            .map(|unit| {
                (
                    Lexical::new(&source_dir.append(&unit.source.value)).normalize(),
                    unit,
                )
            })
            .collect();
        let mut seen = BTreeSet::new();
        // Languages of the object libraries whose objects the target uses
        // (`cmGeneratorTarget::GetLanguages` adds them to the target languages).
        let mut object_languages = BTreeSet::<BString>::new();
        if let Some(raw) = raw
            && raw
                .props
                .get(b"UNITY_BUILD")
                .is_some_and(|value| is_on(value))
        {
            let binary_root = Lexical::new(&target.metadata.binary_dir);
            for unit in &target.compile_units {
                let path = Lexical::new(&source_dir.append(&unit.source.value)).normalize();
                if !binary_root.prefix(Lexical::new(&path))
                    || !path
                        .windows(b"/Unity/".len())
                        .any(|window| window == b"/Unity/")
                {
                    continue;
                }
                if seen.insert(path.clone()) {
                    let flags = pch
                        .and_then(|pch| pch.use_flags.get(&unit.source.value))
                        .cloned()
                        .unwrap_or_default();
                    sources.push(self.source(
                        target.id,
                        raw,
                        path,
                        &file_trace,
                        Some((unit.group, flags)),
                    ));
                }
            }
        }
        if let Some(raw) = raw {
            let src_ctx = EvalContext {
                config: target.config.clone(),
                head: u32::try_from(target.id.index())
                    .ok()
                    .map(rcmake_genex::TargetHandle),
                traversal: rcmake_genex::UsageTraversal::Compile,
                ..Default::default()
            };
            let mut object_files: Option<BTreeMap<BString, &BString>> = None;
            let mut push_evaluated_source =
                |source: &BString,
                 backtrace: &Backtrace,
                 sources: &mut Vec<Source>,
                 seen: &mut BTreeSet<BString>,
                 object_languages: &mut BTreeSet<BString>| {
                    let path = self
                        .result
                        .target_depends
                        .resolved_source(target.id, source)
                        .cloned()
                        .unwrap_or_else(|| Lexical::new(&source_dir.append(source)).normalize());
                    let objects = object_files.get_or_insert_with(|| {
                        targets
                            .values()
                            .filter(|dep| dep.target.metadata.kind == TargetKind::ObjectLibrary)
                            .flat_map(|dep| {
                                dep.target.compile_units.iter().filter_map(|unit| {
                                    let group = dep.target.compile_groups.get(unit.group)?;
                                    Some((
                                        Lexical::new(self.build).append(&unit.object),
                                        &group.language,
                                    ))
                                })
                            })
                            .collect()
                    });
                    if let Some(&language) = objects.get(&path) {
                        if !object_languages.contains(language) {
                            object_languages.insert(language.clone());
                        }
                        if seen.insert(path.clone()) {
                            sources.push(Source {
                                path,
                                trace: backtrace.clone(),
                                generated: true,
                                source_group: "Object Libraries".into(),
                                compile: None,
                            });
                        }
                        return;
                    }
                    if seen.insert(path.clone()) {
                        let unit = units.get(&path).copied();
                        sources.push(self.source(
                            target.id,
                            raw,
                            path,
                            backtrace,
                            unit.map(|unit| {
                                (
                                    unit.group,
                                    pch.and_then(|pch| pch.use_flags.get(&unit.source.value))
                                        .cloned()
                                        .unwrap_or_default(),
                                )
                            }),
                        ));
                    }
                };
            for entry in &raw.sources {
                for source in list::split(&entry.value, list::Empty::Drop) {
                    if let Some(name) = source
                        .strip_prefix(b"$<TARGET_OBJECTS:")
                        .and_then(|name| name.strip_suffix(b">"))
                        && rcmake_genex::find_genex(name).is_none()
                    {
                        if let Some(dependency) = self
                            .model
                            .find_target(raw.dir, name)
                            .and_then(|id| targets.get(&id))
                        {
                            append_objects(
                                &mut sources,
                                &mut seen,
                                &mut object_languages,
                                self.build,
                                dependency.target,
                                &entry.backtrace,
                            );
                        }
                        continue;
                    }
                    if rcmake_genex::find_genex(&source).is_some() {
                        let single = [BT::new(source, entry.backtrace.clone())];
                        if let Ok(evaluated) =
                            usage.file_set_paths(target.id, b"SOURCES", &single, &src_ctx)
                        {
                            for item in evaluated {
                                push_evaluated_source(
                                    &item.value,
                                    &item.backtrace,
                                    &mut sources,
                                    &mut seen,
                                    &mut object_languages,
                                );
                            }
                        }
                        continue;
                    }
                    push_evaluated_source(
                        &source,
                        &entry.backtrace,
                        &mut sources,
                        &mut seen,
                        &mut object_languages,
                    );
                }
            }
            if !raw.is_imported()
                && !matches!(
                    raw.kind,
                    TargetKind::InterfaceLibrary | TargetKind::GlobalTarget
                )
                && let Ok(collected) = usage.collect_property(target.id, b"SOURCES", &src_ctx)
            {
                for item in collected.entries {
                    push_evaluated_source(
                        &item.value,
                        &item.backtrace,
                        &mut sources,
                        &mut seen,
                        &mut object_languages,
                    );
                }
            }
        }
        for unit in &target.compile_units {
            let path = Lexical::new(&source_dir.append(&unit.source.value)).normalize();
            if !seen.insert(path.clone()) {
                continue;
            }
            let flags = pch
                .and_then(|pch| pch.use_flags.get(&unit.source.value))
                .cloned()
                .unwrap_or_default();
            let compile = Some((unit.group, flags));
            let trace = &unit.source.backtrace;
            sources.push(match raw {
                Some(raw) => self.source(target.id, raw, path, trace, compile),
                None => Source {
                    path,
                    trace: trace.clone(),
                    generated: false,
                    source_group: "Source Files".into(),
                    compile,
                },
            });
        }

        if let Some(raw) = raw {
            for path in &file_sets.order {
                if let Some(member) = file_sets.sources.get(path).filter(|member| member.in_build)
                    && seen.insert(path.clone())
                {
                    let trace = member.traces.first().unwrap_or(&file_trace);
                    sources.push(self.source(target.id, raw, path.clone(), trace, None));
                }
            }
        }
        for build_graph in &self.result.graphs {
            if let Some(pch) = build_graph.pch_details.get(&target.id) {
                for entry in &pch.create {
                    if seen.insert(entry.header.clone()) {
                        sources.push(Source {
                            path: entry.header.clone(),
                            trace: file_trace.clone(),
                            generated: false,
                            source_group: "Precompile Header File".into(),
                            compile: None,
                        });
                    }
                }
            }
        }
        if raw.is_some() {
            let context = EvalContext {
                config: target.config.clone(),
                traversal: rcmake_genex::UsageTraversal::Link,
                link_language: target.link.as_ref().map(|link| link.language.clone()),
                ..Default::default()
            };
            let links = usage
                .direct_link_items(target.id, false, &context)
                .map_err(|error| GenError::Invalid(error.to_string().into()))?;
            for item in links.iter() {
                if let rcmake_resolve::usage::ResolvedLinkKind::Objects(id) = item.kind
                    && let Some(dependency) = targets.get(&id)
                {
                    append_objects(
                        &mut sources,
                        &mut seen,
                        &mut object_languages,
                        self.build,
                        dependency.target,
                        &item.text.backtrace,
                    );
                }
            }
        }

        if target.metadata.kind == TargetKind::Utility
            && config_indices.project.utility_commands.contains(&target.id)
        {
            let relative: BString = [b"CMakeFiles/".as_slice(), &target.metadata.name]
                .concat()
                .into();
            let path = Lexical::new(&target.metadata.binary_dir).append(&relative);
            seen.insert(path.clone());
            let source_group = default_source_group(&path);
            sources.push(Source {
                path: path.clone(),
                trace: target.metadata.backtrace.clone(),
                generated: true,
                source_group,
                compile: None,
            });
            let mut rule_path = path.clone();
            rule_path.extend_from_slice(b".rule");
            seen.insert(rule_path);
            add_rule(&mut sources, path, &file_trace);
        }
        if !self.result.target_depends.targets().is_empty() {
            let traced = self.result.target_depends.traced_sources(target.id);
            let initial: Vec<&[u8]> = sources
                .iter()
                .map(|source| source.path.as_slice())
                .collect();
            let utility = (target.metadata.kind == TargetKind::Utility).then(|| {
                Lexical::new(&target.metadata.binary_dir)
                    .append(&[b"CMakeFiles/".as_slice(), &target.metadata.name].concat())
            });
            for traced in self.trace_order(
                target.id,
                &initial,
                utility.as_ref().map(|path| path.as_slice()),
                traced,
            ) {
                if !seen.insert(traced.clone()) {
                    continue;
                }
                if traced.ends_with(b".rule") {
                    sources.push(Source {
                        path: traced.clone(),
                        trace: file_trace.clone(),
                        generated: true,
                        source_group: "CMake Rules".into(),
                        compile: None,
                    });
                } else {
                    sources.push(match raw {
                        Some(raw) => self.source(target.id, raw, traced.clone(), &file_trace, None),
                        None => Source {
                            path: traced.clone(),
                            trace: file_trace.clone(),
                            generated: false,
                            source_group: default_source_group(traced),
                            compile: None,
                        },
                    });
                }
            }
        } else if let Some(build_graph) = build_graph
            && let Some(owned_outputs) = config_indices.custom_outputs_by_target.get(&target.id)
        {
            for &(index, command_id) in owned_outputs {
                if let Some(command) = self.model.command(command_id)
                    && command.main_dependency.is_none()
                    && let Some(BuildStep::Custom(step)) = build_graph.steps.get(index)
                    && let Some(output) = step.outputs.first()
                {
                    add_rule(
                        &mut sources,
                        Lexical::new(self.build).append(output),
                        &file_trace,
                    );
                }
            }
        }

        let mut source_groups = IndexMap::<BString, (Vec<usize>, Vec<usize>)>::new();
        let mut group_matcher = SourceGroupMatcher::default();
        let mut source_entries = Vec::with_capacity(sources.len());
        let mut source_traces = Vec::with_capacity(sources.len());
        let mut source_compiles = Vec::with_capacity(sources.len());
        for (index, source) in sources.into_iter().enumerate() {
            let mut entry = members([("path", Value::string(relative(&source.path, self.source)))]);
            let member = file_sets.sources.get(&source.path);
            if let Some(member) = member {
                member.tag(&mut entry);
            }
            let group_name = raw.map_or(source.source_group.clone(), |raw| {
                self.source_group(raw, &source, member, &mut group_matcher)
            });
            source_compiles.push(
                source
                    .compile
                    .filter(|(group, _)| *group < target.compile_groups.len()),
            );
            let group = source_groups.entry(group_name);
            entry.insert("sourceGroupIndex".into(), number(group.index()));
            group.or_default().0.push(index);
            if source.generated {
                entry.insert("isGenerated".into(), Bool(true));
            }
            source_traces.push((source.path, source.trace));
            source_entries.push(entry);
        }
        if let Some(raw) = raw {
            let interfaces = interface_sources(self.model, target.id, raw, &target.config, usage)?
                .into_iter()
                .chain(
                    file_sets
                        .sources
                        .iter()
                        .filter(|(_, member)| member.interface)
                        .map(|(path, _)| path.clone()),
                );
            optional_array(
                body,
                "interfaceSources",
                interfaces.enumerate().map(|(index, path)| {
                    let mut entry =
                        members([("path", Value::string(relative(&path, self.source)))]);
                    let member = file_sets.sources.get(&path);
                    if let Some(member) = member {
                        member.tag(&mut entry);
                    }
                    let source = self.source(target.id, raw, path, &file_trace, None);
                    let group = source_groups.entry(self.source_group(
                        raw,
                        &source,
                        member,
                        &mut group_matcher,
                    ));
                    entry.insert("sourceGroupIndex".into(), number(group.index()));
                    group.or_default().1.push(index);
                    if source.generated {
                        entry.insert("isGenerated".into(), Bool(true));
                    }
                    Object(entry)
                }),
            );
        }
        if raw.is_none_or(|raw| !raw.is_imported()) {
            optional_array(
                body,
                "sourceGroups",
                source_groups
                    .into_iter()
                    .map(|(group, (sources, interfaces))| {
                        let mut entry = members([
                            ("name", Value::string(group)),
                            ("sourceIndexes", array(sources.into_iter().map(number))),
                        ]);
                        super::indices(&mut entry, "interfaceSourceIndexes", interfaces);
                        Object(entry)
                    }),
            );
        }
        let apple = raw.is_some_and(|raw| uses_apple_frameworks(self.model, raw));
        let linked_frameworks = raw
            .map_or_else(
                || Ok(Vec::new()),
                |raw| {
                    codemodel_frameworks(
                        self.model,
                        raw,
                        target,
                        |id| targets.get(&id)?.target.artifacts.file.as_ref(),
                        usage,
                    )
                },
            )
            .map_err(|error| GenError::Invalid(error.to_string().into()))?;
        // `cmFileAPICodemodel` `Target::Dump` adds backtraces to the graph in this
        // order: (1) `ProcessLanguages` computes the target-wide compile data of
        // each target language, in sorted language order, also for languages
        // that no compile group uses; (2) `DumpSources` adds the backtraces of
        // each source, then the source-specific compile data of that source.
        // Compute the compile data first, then add the backtraces in that order,
        // then write the groups.
        let mut compile_data = IndexMap::<(usize, BString), CompileData>::new();
        let mut source_data = Vec::with_capacity(source_compiles.len());
        for compile in source_compiles {
            let Some(key) = compile else {
                source_data.push(None);
                continue;
            };
            if let Some(index) = compile_data.get_index_of(&key) {
                source_data.push(Some(index));
                continue;
            }
            let Some(group) = target.compile_groups.get(key.0) else {
                source_data.push(None);
                continue;
            };
            let context = EvalContext {
                config: target.config.clone(),
                language: Some(group.language.clone()),
                ..EvalContext::default()
            };
            let pch_flags = key.1.clone();
            let data = CompileData::new(self, target, raw, usage, context, group, pch_flags)?;
            source_data.push(Some(compile_data.len()));
            compile_data.insert(key, data);
        }
        let compiling = raw.filter(|raw| can_compile_sources(raw));
        let mut languages = BTreeSet::new();
        for data in compile_data.values() {
            languages.insert(data.group.language.as_slice());
        }
        let mut extra_languages = BTreeSet::new();
        if let Some(raw) = compiling {
            for ((path, _), data) in source_traces.iter().zip(&source_data) {
                if data.is_none()
                    && let Some(language) =
                        rcmake_resolve::lower::source_language(self.model, raw, path)
                {
                    extra_languages.insert(language);
                }
            }
            extra_languages.extend(object_languages);
        }
        languages.extend(extra_languages.iter().map(|language| language.as_slice()));
        for language in languages {
            if let Some(data) = compile_data
                .values()
                .find(|data| data.group.language.as_slice() == language)
            {
                data.add_target_traces(graph, &linked_frameworks);
            } else if let Some(raw) = compiling
                && let Some(data) = self.language_data(target, raw, usage, language)
            {
                data.add_target_traces(graph, &linked_frameworks);
            }
        }
        for ((entry, (path, trace)), data) in source_entries
            .iter_mut()
            .zip(&source_traces)
            .zip(&source_data)
        {
            let member = file_sets.sources.get(path);
            let backtraces: Vec<_> = match member {
                Some(member) if !member.traces.is_empty() => member
                    .traces
                    .iter()
                    .filter_map(|trace| graph.add(trace))
                    .collect(),
                _ => graph.add(trace).into_iter().collect(),
            };
            if let Some(&first) = backtraces.first() {
                entry.insert(
                    "backtrace".into(),
                    number(graph.add(trace).unwrap_or(first)),
                );
                entry.insert(
                    "backtraces".into(),
                    array(backtraces.into_iter().map(number)),
                );
            }
            if let Some(data) = data.and_then(|index| compile_data.get_index(index)) {
                data.1.add_source_traces(graph);
            }
        }

        // Equal compile data of different keys is one compile group.
        let single = compile_data.len() == 1;
        let mut groups = IndexMap::<BString, (Members, Vec<usize>)>::new();
        let mut data_groups = Vec::with_capacity(compile_data.len());
        for data in compile_data.into_values() {
            let mut hidden = BString::default();
            if !single {
                data.hidden_key(graph, &mut hidden);
            }
            let rendered = data.into_members(graph, apple, &linked_frameworks);
            let digest = if single {
                BString::default()
            } else {
                let mut digest = json_bytes(&Object(rendered.clone()));
                digest.extend_from_slice(&hidden);
                digest
            };
            let entry = groups.entry(digest);
            data_groups.push(entry.index());
            entry.or_insert((rendered, Vec::new()));
        }
        for (src_idx, (entry, data)) in source_entries.iter_mut().zip(source_data).enumerate() {
            let Some(group) = data.and_then(|index| data_groups.get(index).copied()) else {
                continue;
            };
            if let Some((_, (_, indices))) = groups.get_index_mut(group) {
                indices.push(src_idx);
            }
            entry.insert("compileGroupIndex".into(), number(group));
        }
        body.insert(
            "sources".into(),
            array(source_entries.into_iter().map(Object)),
        );
        let groups = groups.into_values().map(|(mut group_body, indices)| {
            group_body.insert(
                "sourceIndexes".into(),
                array(indices.into_iter().map(number)),
            );
            Object(group_body)
        });
        optional_array(body, "compileGroups", groups);
        Ok(())
    }

    fn source_group(
        &self,
        raw: &Target,
        source: &Source,
        member: Option<&Membership>,
        matcher: &mut SourceGroupMatcher,
    ) -> BString {
        if source.source_group == b"Object Libraries" {
            return source.source_group.clone();
        }
        if let Some(group) = member.and_then(|member| member.source_group.as_ref()) {
            return group.clone();
        }
        let dir = self.model.dir(raw.dir);
        if let Some(group) = dir
            .source_props
            .get(&source.path)
            .and_then(explicit_source_group)
        {
            return normalize_source_group_name(dir, group);
        }
        let group =
            rcmake_resolve::ide::find_source_group(dir, &source.path, &mut |pattern, path| {
                matcher.matches(pattern, path)
            });
        if group.is_empty() && member.is_some_and(|member| member.header) {
            "Header Files".into()
        } else {
            group
        }
    }

    /// Compute the target-level compile data of `language` for a target that
    /// compiles no source of that language. CMake still computes this data
    /// (for example for the languages of the object libraries whose objects a
    /// target links), so its backtraces are in the graph. Return `None` if the
    /// data cannot be computed; it adds only graph nodes.
    fn language_data(
        &self,
        target: &ResolvedTarget,
        raw: &Target,
        usage: &mut UsageResolver<'_>,
        language: &[u8],
    ) -> Option<CompileData> {
        let language = BString::from(language);
        let context = EvalContext {
            config: target.config.clone(),
            language: Some(language.clone()),
            ..EvalContext::default()
        };
        let flags = usage
            .collect(target.id, UsageField::CompileOptions, &context)
            .ok()?
            .entries;
        let mut defines = usage
            .collect(target.id, UsageField::CompileDefinitions, &context)
            .ok()?
            .entries;
        for define in &mut defines {
            if let Some(name) = define.value.strip_prefix(b"-D") {
                define.value = BString::from(name);
            }
        }
        // `cmLocalGenerator::GetIncludeDirectories` drops the implicit directories.
        let key = [
            b"CMAKE_",
            language.as_slice(),
            b"_IMPLICIT_INCLUDE_DIRECTORIES",
        ]
        .concat();
        let implicit: BTreeSet<_> = self
            .model
            .definition(raw.dir, &key)
            .into_iter()
            .flat_map(|value| list::split(value, list::Empty::Drop))
            .map(|path| include_identity(&path))
            .collect();
        let mut includes = usage.collect_includes(target.id, &context).ok()?;
        includes.retain(|include| !implicit.contains(&include_identity(&include.path.value)));
        let group = CompileGroup {
            language,
            flags,
            defines,
            includes,
            ..CompileGroup::default()
        };
        CompileData::new(
            self,
            target,
            Some(raw),
            usage,
            context,
            &group,
            BString::default(),
        )
        .ok()
    }

    /// Order the traced sources of a target as `cmTargetTraceDependencies` does.
    ///
    /// The trace queue holds the source files of the target. It processes each
    /// entry in order and appends the source that carries the custom command
    /// that generates a file (the `.rule` file or the `MAIN_DEPENDENCY`).
    /// `CheckCustomCommand` collects the `DEPENDS` of a command in a
    /// `std::set<std::string>`, so it follows them in sorted path order. The
    /// first entries of the trace keep their order; each command appends the
    /// commands that generate its dependencies in sorted dependency order.
    fn trace_order<'t>(
        &self,
        target_id: TargetId,
        initial: &[&[u8]],
        utility: Option<&[u8]>,
        traced: &'t [BString],
    ) -> Vec<&'t BString> {
        trace_order_for_graphs(
            &self.model.dir(rcmake_model::DirId::ROOT).binary_dir,
            &self.result.graphs,
            target_id,
            initial,
            utility,
            traced,
        )
    }

    fn source(
        &self,
        target_id: TargetId,
        raw: &Target,
        path: BString,
        trace: &Backtrace,
        compile: Option<(usize, BString)>,
    ) -> Source {
        let dir = self.model.dir(raw.dir);
        let generated = dir.generated_sources.contains(&path)
            || dir
                .source_props
                .get(&path)
                .and_then(|props| props.get(b"GENERATED"))
                .is_some_and(|value| is_on(value))
            || self
                .result
                .target_depends
                .is_generated_source(target_id, &path);
        Source {
            path,
            trace: trace.clone(),
            generated,
            source_group: BString::default(),
            compile,
        }
    }
}

/// Order the traced sources of a target across all configuration graphs as
/// `cmTargetTraceDependencies` does.
fn trace_order_for_graphs<'t>(
    binary_root: &[u8],
    graphs: &[rcmake_resolve::BuildGraph],
    target_id: TargetId,
    initial: &[&[u8]],
    utility: Option<&[u8]>,
    traced: &'t [BString],
) -> Vec<&'t BString> {
    let mut rank = HashMap::<BString, usize>::new();
    for graph in graphs {
        let Some(steps) = graph.custom_source_steps.get(&target_id) else {
            continue;
        };
        for carrier in trace_steps_for_graph(binary_root, graph, steps, initial, utility) {
            let next = rank.len();
            rank.entry(carrier).or_insert(next);
        }
    }
    if rank.is_empty() {
        return traced.iter().collect();
    }
    let fallback_base = rank.len();
    let mut ordered: Vec<(usize, &BString)> = traced
        .iter()
        .enumerate()
        .map(|(index, path)| {
            (
                rank.get(path.as_slice())
                    .copied()
                    .unwrap_or(fallback_base + index),
                path,
            )
        })
        .collect();
    ordered.sort_by_key(|(rank, _)| *rank);
    ordered.into_iter().map(|(_, path)| path).collect()
}

/// Return the carrier sources of the custom command steps of one target in
/// trace order. `steps` is the trace with `DEPENDS` in declared order.
/// `utility` is the output of the custom target command, if any.
fn trace_steps_for_graph(
    binary_root: &[u8],
    graph: &rcmake_resolve::BuildGraph,
    steps: &indexmap::IndexSet<usize>,
    initial: &[&[u8]],
    utility: Option<&[u8]>,
) -> Vec<BString> {
    let root = Lexical::new(binary_root);
    let absolute = |path: &[u8]| Lexical::new(&root.append(path)).normalize();
    let sorted_depends = |custom: &rcmake_resolve::CustomStep| {
        let mut depends: Vec<BString> = custom
            .depends
            .iter()
            .map(|depend| absolute(depend))
            .collect();
        depends.sort();
        depends.dedup();
        depends
    };
    // The `DEPENDS` of the custom target command (`CMakeFiles/<name>.rule`).
    let utility_depends = utility
        .and_then(|utility| {
            graph
                .utility_steps
                .iter()
                .find_map(|&step| match graph.steps.get(step) {
                    Some(BuildStep::Custom(custom))
                        if custom
                            .outputs
                            .first()
                            .is_some_and(|output| absolute(output).as_slice() == utility) =>
                    {
                        Some(sorted_depends(custom))
                    }
                    _ => None,
                })
        })
        .unwrap_or_default();
    let mut carriers = Vec::with_capacity(steps.len());
    let mut producers = HashMap::<BString, usize>::new();
    let mut inputs = Vec::with_capacity(steps.len());
    for (position, &step) in steps.iter().enumerate() {
        let custom = match graph.steps.get(step) {
            Some(BuildStep::Custom(custom)) => Some(custom),
            _ => None,
        };
        let main = graph.custom_main_sources.get(&step);
        let carrier = match (main, custom.and_then(|custom| custom.outputs.first())) {
            (Some(main), _) => main.clone(),
            (None, Some(output)) => {
                let mut rule = absolute(output);
                rule.extend_from_slice(b".rule");
                rule
            }
            (None, None) => BString::default(),
        };
        for output in custom.into_iter().flat_map(|custom| &custom.outputs) {
            producers.entry(absolute(output)).or_insert(position);
        }
        if let Some(main) = main {
            producers
                .entry(Lexical::new(main).normalize())
                .or_insert(position);
        }
        carriers.push(carrier);
        inputs.push(custom.map(sorted_depends).unwrap_or_default());
    }
    // The producers of the dependencies of one step, in sorted dependency order.
    let children = |position: usize| -> Vec<usize> {
        inputs
            .get(position)
            .into_iter()
            .flatten()
            .filter_map(|depend| producers.get(depend).copied())
            .filter(|&child| child != position)
            .collect()
    };
    // The roots are the steps that generate an initial source of the
    // target, and the steps that no earlier step reaches. Roots after the
    // first reached step (the producers of the utility command `DEPENDS`)
    // enter the queue when it has processed the first roots.
    let source_roots: HashSet<usize> = initial
        .iter()
        .filter_map(|path| {
            producers
                .get(Lexical::new(path).normalize().as_slice())
                .copied()
        })
        .collect();
    let mut reached = HashSet::new();
    // The producers of the custom target `DEPENDS`, in sorted dependency order.
    let mut late: Vec<usize> = Vec::new();
    for depend in &utility_depends {
        if let Some(&position) = producers.get(depend)
            && !late.contains(&position)
        {
            late.push(position);
        }
    }
    let utility_roots: HashSet<usize> = late.iter().copied().collect();
    let mut after_reached = false;
    let mut early = Vec::new();
    for position in 0..steps.len() {
        let source_root = source_roots.contains(&position);
        if !source_root && utility_roots.contains(&position) {
            // Queued with the custom target command below.
        } else if source_root || !reached.contains(&position) {
            if source_root || !after_reached {
                early.push(position);
            } else {
                late.push(position);
            }
        } else {
            after_reached = true;
        }
        reached.extend(children(position));
    }
    let first = early.len();
    let mut queue: indexmap::IndexSet<usize> = early.into_iter().collect();
    let mut next = 0;
    loop {
        if next == first {
            queue.extend(late.iter().copied());
        }
        let Some(&position) = queue.get_index(next) else {
            break;
        };
        queue.extend(children(position));
        next += 1;
    }
    queue.extend(0..steps.len());
    queue
        .into_iter()
        .filter_map(|position| carriers.get(position).cloned())
        .collect()
}

/// Return `true` if CMake computes compile data for the target
/// (`cmGeneratorTarget::CanCompileSources`).
fn can_compile_sources(raw: &Target) -> bool {
    !raw.is_imported()
        && matches!(
            raw.kind,
            TargetKind::Executable
                | TargetKind::StaticLibrary
                | TargetKind::SharedLibrary
                | TargetKind::ModuleLibrary
                | TargetKind::ObjectLibrary
        )
}

/// Return the comparison identity of an include directory.
fn include_identity(path: &[u8]) -> BString {
    let normalized = Lexical::new(path).normalize();
    let lexical = Lexical::new(&normalized);
    let root = lexical.root_name().len() + lexical.root_directory().len();
    let end = normalized
        .iter()
        .rposition(|&byte| byte != b'/')
        .map_or(0, |index| index + 1);
    BString::from(normalized.get(..end.max(root)).unwrap_or(&normalized))
}

fn explicit_source_group(props: &PropertyMap) -> Option<&BString> {
    let group = props
        .get(b"SOURCE_GROUP")
        .filter(|value| !value.is_empty())?;
    if props.get(b"\0source_group_file") == Some(group)
        || props.get(b"\0source_group_regex") == Some(group)
    {
        return None;
    }
    Some(group)
}

struct SourceGroupMatcher {
    compiled: HashMap<BString, Option<rcmake_regex::Regex>>,
    scratch: rcmake_regex::Scratch,
    budget: u64,
}

impl SourceGroupMatcher {
    const MAX_CACHED_PATTERNS: usize = 4096;
    const PER_MATCH_STEP_LIMIT: u64 = 250_000;

    fn new(budget: u64) -> Self {
        Self {
            compiled: HashMap::new(),
            scratch: rcmake_regex::Scratch::default(),
            budget,
        }
    }

    fn matches(&mut self, pattern: &[u8], path: &[u8]) -> bool {
        if self.budget == 0 {
            return false;
        }
        if !self.compiled.contains_key(pattern) {
            if self.compiled.len() >= Self::MAX_CACHED_PATTERNS {
                self.compiled.clear();
            }
            self.compiled.insert(
                BString::from(pattern),
                rcmake_regex::Regex::new(pattern).ok(),
            );
        }
        let Some(Some(regex)) = self.compiled.get(pattern) else {
            return false;
        };
        let mut step_budget = self.budget.min(Self::PER_MATCH_STEP_LIMIT);
        let initial = step_budget;
        let matched = regex
            .find_at_with_scratch(
                path,
                0,
                rcmake_regex::MatchOptions::default(),
                &mut step_budget,
                &mut self.scratch,
            )
            .is_ok_and(|found| found.is_some());
        self.budget = self.budget.saturating_sub(initial - step_budget);
        matched
    }
}

impl Default for SourceGroupMatcher {
    fn default() -> Self {
        Self::new(Limits::default().regex_steps)
    }
}

fn default_source_group(path: &[u8]) -> BString {
    let (stem, extension) = Lexical::new(path).stem_extension(true);
    match extension {
        b".h" | b".hxx" if stem == b"cmake_pch" || stem.starts_with(b"cmake_pch_") => {
            "Precompile Header File"
        }
        b".h" | b".hh" | b".h++" | b".hm" | b".hpp" | b".hxx" | b".in" | b".txx" | b".inl" => {
            "Header Files"
        }
        b".C" | b".F" | b".M" | b".c" | b".c++" | b".cc" | b".cpp" | b".mpp" | b".cxx"
        | b".ixx" | b".cppm" | b".ccm" | b".cxxm" | b".c++m" | b".cu" | b".f" | b".f90"
        | b".for" | b".fpp" | b".ftn" | b".m" | b".mm" | b".rc" | b".def" | b".r" | b".odl"
        | b".idl" | b".hpj" | b".bat" => "Source Files",
        b".rule" => "CMake Rules",
        b".pdf" | b".plist" | b".png" | b".jpeg" | b".jpg" | b".storyboard" | b".xcassets" => {
            "Resources"
        }
        b".lo" | b".o" | b".obj" => "Object Files",
        _ => "",
    }
    .into()
}

fn append_objects(
    sources: &mut Vec<Source>,
    seen: &mut BTreeSet<BString>,
    languages: &mut BTreeSet<BString>,
    build: &[u8],
    dependency: &ResolvedTarget,
    trace: &Backtrace,
) {
    for group in &dependency.compile_groups {
        if !languages.contains(&group.language) {
            languages.insert(group.language.clone());
        }
    }
    for unit in &dependency.compile_units {
        let path = Lexical::new(build).append(&unit.object);
        if seen.insert(path.clone()) {
            sources.push(Source {
                path,
                trace: trace.clone(),
                generated: true,
                source_group: "Object Libraries".into(),
                compile: None,
            });
        }
    }
}

fn add_rule(sources: &mut Vec<Source>, mut output: BString, trace: &Backtrace) {
    output.extend_from_slice(b".rule");
    sources.push(Source {
        path: output,
        trace: trace.clone(),
        generated: true,
        source_group: "CMake Rules".into(),
        compile: None,
    });
}

struct CompileData {
    group: CompileGroup,
    standard: Option<LanguageStandard>,
    headers: Vec<BT<BString>>,
    flags: Vec<BT<BString>>,
    /// Index of the first source-level entry in `flags`.
    source_flags: usize,
    /// Backtraces of the target-level definitions in sorted value order.
    target_define_traces: Vec<Backtrace>,
    /// Source-level definitions in sorted value order, before the merge with
    /// the target-level definitions.
    source_defines: Vec<BT<BString>>,
}

impl CompileData {
    fn new(
        api: &FileApi<'_, '_>,
        target: &ResolvedTarget,
        raw: Option<&Target>,
        usage: &mut UsageResolver<'_>,
        context: EvalContext,
        group: &CompileGroup,
        pch: BString,
    ) -> Result<Self, GenError> {
        let error =
            |error: rcmake_resolve::ResolveError| GenError::Invalid(error.to_string().into());
        let mut group = group.clone();
        let mut flags = Vec::new();
        let mut standard = None;
        if let Some(raw) = raw {
            let (base, language_standard) =
                codemodel_compile_info(api.model, raw, &target.config, &group, usage)
                    .map_err(error)?;
            standard = language_standard;
            if !base.is_empty() {
                flags.push(BT::new(base, Backtrace::default()));
            }
        }
        let target_defines = group
            .source_define_start
            .unwrap_or(group.defines.len())
            .min(group.defines.len());
        let target_flags = group
            .source_flag_start
            .unwrap_or(group.flags.len())
            .min(group.flags.len());
        let target_includes = group.source_include_count.min(group.includes.len());
        for (field, entries) in [
            (
                UsageField::CompileDefinitions,
                group
                    .defines
                    .get_mut(..target_defines)
                    .unwrap_or_default()
                    .iter_mut()
                    .collect::<Vec<_>>(),
            ),
            (
                UsageField::CompileOptions,
                group
                    .flags
                    .get_mut(..target_flags)
                    .unwrap_or_default()
                    .iter_mut()
                    .collect(),
            ),
            (
                UsageField::IncludeDirectories,
                group
                    .includes
                    .get_mut(target_includes..)
                    .unwrap_or_default()
                    .iter_mut()
                    .map(|include| &mut include.path)
                    .collect(),
            ),
        ] {
            if entries.is_empty() {
                continue;
            }
            let inherited = usage
                .codemodel_inherited_backtraces(target.id, field, &context)
                .map_err(error)?;
            if inherited.is_empty() {
                continue;
            }
            let mut by_value: HashMap<&[u8], Vec<(&Backtrace, &Backtrace)>> =
                HashMap::with_capacity(inherited.len());
            for (original, link_bt) in &inherited {
                by_value
                    .entry(original.value.as_slice())
                    .or_default()
                    .push((&original.backtrace, link_bt));
            }
            for entry in entries {
                if let Some(candidates) = by_value.get(entry.value.as_slice())
                    && let Some(&(_, link_bt)) = candidates
                        .iter()
                        .find(|(orig_bt, _)| orig_bt.same_call(&entry.backtrace))
                {
                    entry.backtrace.clone_from(link_bt);
                }
            }
        }
        let escape_flag = |flag: &BString| {
            flag.strip_prefix(b"SHELL:").map_or_else(
                || rcmake_resolve::lower::cmake_posix_quote(flag),
                BString::from,
            )
        };
        let option_groups: BTreeMap<usize, usize> = group
            .source_option_groups
            .iter()
            .copied()
            .filter(|&(start, len)| len > 0 && start >= target_flags)
            .collect();
        let all_flags = std::mem::take(&mut group.flags);
        let mut index = 0;
        let mut source_flags = None;
        while let Some(flag) = all_flags.get(index) {
            if index >= target_flags && source_flags.is_none() {
                source_flags = Some(flags.len());
            }
            if let Some(&len) = option_groups.get(&index)
                && let Some(slice) = all_flags.get(index..index + len)
            {
                let mut combined = BString::default();
                for (offset, item) in slice.iter().enumerate() {
                    if offset > 0 {
                        combined.push(b' ');
                    }
                    combined.extend_from_slice(&escape_flag(&item.value));
                }
                flags.push(BT::new(combined, flag.backtrace.clone()));
                index += len;
            } else {
                flags.push(BT::new(escape_flag(&flag.value), flag.backtrace.clone()));
                index += 1;
            }
        }
        let source_flags = source_flags.unwrap_or(flags.len());
        if !pch.is_empty() {
            flags.push(BT::new(pch, Backtrace::default()));
        }
        // `GetTargetDefines` and `BuildCompileData` collect the definitions in
        // a `std::set<BT<std::string>>`, which orders them by value and keeps
        // the first entry of each value.
        let sorted = |defines: &[BT<BString>]| {
            let mut sorted: Vec<_> = defines.iter().collect();
            sorted.sort_by(|a, b| a.value.cmp(&b.value));
            sorted.dedup_by(|a, b| a.value == b.value);
            sorted.into_iter().cloned().collect::<Vec<_>>()
        };
        let (target_level, source_level) = group
            .defines
            .split_at(target_defines.min(group.defines.len()));
        let target_define_traces = sorted(target_level)
            .into_iter()
            .map(|define| define.backtrace)
            .collect();
        let source_defines = sorted(source_level);
        group.defines.sort_by(|a, b| a.value.cmp(&b.value));
        group.defines.dedup_by(|a, b| a.value == b.value);
        let mut seen = BTreeSet::new();
        let mut headers = usage
            .collect(target.id, UsageField::PrecompileHeaders, &context)
            .map_err(error)?
            .entries;
        headers.retain(|header| !header.value.is_empty() && seen.insert(header.value.clone()));
        Ok(Self {
            group,
            standard,
            headers,
            flags,
            source_flags,
            target_define_traces,
            source_defines,
        })
    }

    /// Add the backtraces of the target-level compile data in the order of
    /// `cmFileAPICodemodel` `Target::ProcessLanguage`: flags, definitions,
    /// include directories and frameworks, precompiled headers, then the
    /// language standard.
    fn add_target_traces(
        &self,
        graph: &mut Backtraces<'_>,
        linked_frameworks: &[InterfaceInclude],
    ) {
        for flag in self.flags.get(..self.source_flags).unwrap_or_default() {
            graph.add(&flag.backtrace);
        }
        for trace in &self.target_define_traces {
            graph.add(trace);
        }
        let includes = self.group.includes.get(self.group.source_include_count..);
        for include in includes.unwrap_or_default().iter().chain(linked_frameworks) {
            graph.add(&include.path.backtrace);
        }
        for header in &self.headers {
            graph.add(&header.backtrace);
        }
        for trace in self
            .standard
            .iter()
            .flat_map(|standard| &standard.backtraces)
        {
            graph.add(trace);
        }
    }

    /// Add the backtraces of the source-level compile data in the order of
    /// `cmFileAPICodemodel` `Target::BuildCompileData`: options, include
    /// directories, then definitions.
    fn add_source_traces(&self, graph: &mut Backtraces<'_>) {
        for flag in self.flags.get(self.source_flags..).unwrap_or_default() {
            graph.add(&flag.backtrace);
        }
        let includes = self.group.includes.get(..self.group.source_include_count);
        for include in includes.unwrap_or_default() {
            graph.add(&include.path.backtrace);
        }
        for define in &self.source_defines {
            graph.add(&define.backtrace);
        }
    }

    /// Append the part of the `CompileData` group key that the written group
    /// does not show. CMake keys a group before it removes the source-level
    /// definitions that repeat a target-level definition, so such a source has
    /// its own group with equal JSON.
    fn hidden_key(&self, graph: &mut Backtraces<'_>, key: &mut BString) {
        for define in &self.source_defines {
            key.push(0);
            key.extend_from_slice(&define.value);
            key.push(0);
            if let Some(index) = graph.add(&define.backtrace) {
                key.extend_from_slice(index.to_string().as_bytes());
            }
        }
    }

    fn into_members(
        self,
        graph: &mut Backtraces<'_>,
        apple: bool,
        linked_frameworks: &[InterfaceInclude],
    ) -> Members {
        let mut body = members([("language", Value::string(self.group.language))]);
        for (field, key, items) in [
            ("compileCommandFragments", "fragment", self.flags),
            ("defines", "define", self.group.defines),
        ] {
            optional_array(
                &mut body,
                field,
                items.iter().map(|value| {
                    let mut item = Members::new();
                    graph.tagged_value(&mut item, key, value);
                    Object(item)
                }),
            );
        }
        let (frameworks, includes) = self
            .group
            .includes
            .iter()
            .partition::<Vec<_>, _>(|include| {
                apple
                    && Lexical::new(&include.path.value)
                        .filename()
                        .ends_with(b".framework")
            });
        let mut tagged_include = |value: &InterfaceInclude| {
            let mut item = Members::new();
            graph.tagged_value(&mut item, "path", &value.path);
            if value.system {
                item.insert("isSystem".into(), Bool(true));
            }
            Object(item)
        };
        optional_array(
            &mut body,
            "includes",
            includes.into_iter().map(&mut tagged_include),
        );
        let mut seen = BTreeSet::new();
        optional_array(
            &mut body,
            "frameworks",
            frameworks
                .into_iter()
                .chain(linked_frameworks)
                .filter(|entry| seen.insert(entry.path.value.as_slice()))
                .map(tagged_include),
        );
        optional_array(
            &mut body,
            "precompileHeaders",
            self.headers.iter().map(|value| {
                let mut item = Members::new();
                graph.tagged_value(&mut item, "header", value);
                Object(item)
            }),
        );
        if let Some(standard) = self.standard {
            let mut entry = members([("standard", Value::string(standard.value))]);
            optional_array(
                &mut entry,
                "backtraces",
                standard
                    .backtraces
                    .iter()
                    .filter_map(|trace| graph.add(trace))
                    .map(number),
            );
            body.insert("languageStandard".into(), Object(entry));
        }
        body
    }
}

#[cfg(test)]
mod tests {
    use super::{SourceGroupMatcher, trace_order_for_graphs};
    use rcmake_base::BString;
    use rcmake_genex::ConfigId;
    use rcmake_resolve::{BuildGraph, BuildStep, CustomStep};

    #[test]
    fn source_group_matcher_handles_cmake_bracket_class_and_bounds_work() {
        let mut matcher = SourceGroupMatcher::default();
        assert!(matcher.matches(br"[\]", br"dir\file.c"));
        assert!(!matcher.matches(br"[\]", b"dir/file.c"));
        assert_eq!(matcher.compiled.len(), 1);

        let long_path = vec![b'a'; 2000];
        let before = matcher.budget;
        assert!(!matcher.matches(b"a+b", &long_path));
        let spent = before - matcher.budget;
        assert!(
            spent <= SourceGroupMatcher::PER_MATCH_STEP_LIMIT,
            "spent {spent} steps"
        );
        assert!(
            matcher.budget > 0,
            "single bounded miss should preserve remaining budget for legitimate files"
        );

        let mut exhausted = SourceGroupMatcher::new(100);
        assert!(!exhausted.matches(b"a+b", &long_path));
        assert_eq!(exhausted.budget, 0);
        assert!(!exhausted.matches(br"\.c$", b"main.c"));
    }

    #[test]
    fn trace_order_unions_custom_steps_across_all_configurations()
    -> Result<(), Box<dyn std::error::Error>> {
        use rcmake_model::{DirId, ProjectModel, Target, TargetKind, TargetOrigin};

        let mut model = ProjectModel::default();
        let target_id = model.add_target(Target::new(
            "app".into(),
            TargetKind::Executable,
            TargetOrigin::Normal,
            DirId::ROOT,
            Default::default(),
            Default::default(),
        ))?;
        let mut debug_graph = BuildGraph {
            config: ConfigId("Debug".into()),
            steps: vec![BuildStep::Custom(CustomStep {
                outputs: vec!["Debug/gen.h".into()],
                ..CustomStep::default()
            })],
            ..BuildGraph::default()
        };
        debug_graph
            .custom_source_steps
            .insert(target_id, [0].into_iter().collect());

        let mut release_graph = BuildGraph {
            config: ConfigId("Release".into()),
            steps: vec![
                BuildStep::Custom(CustomStep {
                    outputs: vec!["Release/gen.h".into()],
                    depends: vec!["Release/z.in".into(), "Release/a.in".into()],
                    ..CustomStep::default()
                }),
                BuildStep::Custom(CustomStep {
                    outputs: vec!["Release/z.in".into()],
                    ..CustomStep::default()
                }),
                BuildStep::Custom(CustomStep {
                    outputs: vec!["Release/a.in".into()],
                    ..CustomStep::default()
                }),
            ],
            ..BuildGraph::default()
        };
        release_graph
            .custom_source_steps
            .insert(target_id, [0, 1, 2].into_iter().collect());

        let traced: Vec<BString> = [
            "/build/Debug/gen.h.rule",
            "/build/Release/gen.h.rule",
            "/build/Release/z.in.rule",
            "/build/Release/a.in.rule",
        ]
        .into_iter()
        .map(BString::from)
        .collect();
        let initial = [b"/build/Debug/gen.h".as_slice(), b"/build/Release/gen.h"];
        let ordered = trace_order_for_graphs(
            b"/build",
            &[debug_graph, release_graph],
            target_id,
            &initial,
            None,
            &traced,
        );
        let names: Vec<&[u8]> = ordered.iter().map(|path| path.as_slice()).collect();
        assert_eq!(
            names,
            vec![
                b"/build/Debug/gen.h.rule".as_slice(),
                b"/build/Release/gen.h.rule".as_slice(),
                b"/build/Release/a.in.rule".as_slice(),
                b"/build/Release/z.in.rule".as_slice(),
            ],
            "Release DEPENDS must be ordered from all config graphs, not only graphs[0]"
        );
        Ok(())
    }
}
