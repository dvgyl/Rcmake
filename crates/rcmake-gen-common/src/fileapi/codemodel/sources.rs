//! Source and compile group entries in a target reply.

use std::collections::{BTreeMap, BTreeSet};

use indexmap::IndexMap;
use rcmake_base::{BString, list, path_lex::Lexical, truth::is_on};
use rcmake_diag::{Backtrace, Frame, FrameKind};
use rcmake_genex::EvalContext;
use rcmake_model::{BT, CustomCommandKind, Target, TargetId, TargetKind, UsageField};
use rcmake_resolve::{
    BuildStep, CompileGroup, InterfaceInclude, ResolvedTarget,
    lower::{
        LanguageStandard, codemodel_compile_info, codemodel_frameworks, uses_apple_frameworks,
    },
    usage::UsageResolver,
};

use crate::GenError;

use super::super::{Bool, FileApi, Members, Object, Value, array, members, relative};
use super::{
    backtraces::Backtraces,
    file_sets::{FileSets, interface_sources},
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
    pub(super) fn write_target_sources(
        &self,
        target: &ResolvedTarget,
        raw: Option<&Target>,
        targets: &BTreeMap<TargetId, TargetRef<'_>>,
        body: &mut Members,
        graph: &mut Backtraces<'_>,
        usage: &mut UsageResolver<'_>,
    ) -> Result<(), GenError> {
        let build_graph = self
            .result
            .graphs
            .iter()
            .find(|graph| graph.config == target.config);
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
        if let Some(raw) = raw {
            for entry in &raw.sources {
                for source in list::split(&entry.value, list::Empty::Drop) {
                    if let Some(name) = source
                        .strip_prefix(b"$<TARGET_OBJECTS:")
                        .and_then(|name| name.strip_suffix(b">"))
                    {
                        if let Some(dependency) = self
                            .model
                            .find_target(raw.dir, name)
                            .and_then(|id| targets.get(&id))
                        {
                            append_objects(
                                &mut sources,
                                &mut seen,
                                self.build,
                                dependency.target,
                                &entry.backtrace,
                            );
                        }
                        continue;
                    }
                    if source.windows(2).any(|pair| pair == b"$<") {
                        continue;
                    }
                    let path = Lexical::new(&source_dir.append(&source)).normalize();
                    if seen.insert(path.clone()) {
                        let unit = units.get(&path).copied();
                        sources.push(self.source(
                            raw,
                            path,
                            &entry.backtrace,
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
                        self.build,
                        dependency.target,
                        &item.text.backtrace,
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
                Some(raw) => self.source(raw, path, trace, compile),
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
                    sources.push(self.source(raw, path.clone(), trace, None));
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

        if let Some(build_graph) = build_graph {
            for (&index, owner) in &build_graph.custom_output_owners {
                if !owner.targets.contains(&target.id) {
                    continue;
                }
                if let Some(command) = self.model.command(owner.command)
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
        if target.metadata.kind == TargetKind::Utility
            && self
                .model
                .commands()
                .any(|(_, command)| command.kind == CustomCommandKind::Utility(target.id))
        {
            let relative: BString = [b"CMakeFiles/".as_slice(), &target.metadata.name]
                .concat()
                .into();
            let path = Lexical::new(&target.metadata.binary_dir).append(&relative);
            sources.push(Source {
                path: path.clone(),
                trace: target.metadata.backtrace.clone(),
                generated: true,
                source_group: "".into(),
                compile: None,
            });
            add_rule(&mut sources, path, &file_trace);
        }

        let mut source_groups = IndexMap::<BString, (Vec<usize>, Vec<usize>)>::new();
        let mut compile_groups = IndexMap::<(usize, BString), Vec<usize>>::new();
        let values = sources.into_iter().enumerate().map(|(index, source)| {
            let mut entry = members([("path", Value::string(relative(&source.path, self.source)))]);
            let member = file_sets.sources.get(&source.path);
            if let Some(member) = member {
                member.tag(&mut entry);
            }
            let group_name = raw.map_or(source.source_group.clone(), |raw| {
                self.source_group(raw, &source, member.is_some_and(|member| member.header))
            });
            if let Some(compile) = source
                .compile
                .filter(|(group, _)| *group < target.compile_groups.len())
            {
                let group = compile_groups.entry(compile);
                entry.insert("compileGroupIndex".into(), number(group.index()));
                group.or_default().push(index);
            }
            let group = source_groups.entry(group_name);
            entry.insert("sourceGroupIndex".into(), number(group.index()));
            group.or_default().0.push(index);
            let backtraces: Vec<_> = match member {
                Some(member) if !member.traces.is_empty() => member
                    .traces
                    .iter()
                    .filter_map(|trace| graph.add(trace))
                    .collect(),
                _ => graph.add(&source.trace).into_iter().collect(),
            };
            if let Some(&first) = backtraces.first() {
                entry.insert(
                    "backtrace".into(),
                    number(graph.add(&source.trace).unwrap_or(first)),
                );
                entry.insert(
                    "backtraces".into(),
                    array(backtraces.into_iter().map(number)),
                );
            }
            if source.generated {
                entry.insert("isGenerated".into(), Bool(true));
            }
            Object(entry)
        });
        body.insert("sources".into(), array(values));
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
                    let source = self.source(raw, path, &file_trace, None);
                    let group = source_groups.entry(self.source_group(
                        raw,
                        &source,
                        member.is_some_and(|member| member.header),
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
        let groups = compile_groups
            .into_iter()
            .filter_map(|((index, flags), sources)| {
                target
                    .compile_groups
                    .get(index)
                    .map(|group| (group, flags, sources))
            })
            .map(|(group, flags, sources)| {
                let context = EvalContext {
                    config: target.config.clone(),
                    language: Some(group.language.clone()),
                    ..EvalContext::default()
                };
                let data = CompileData::new(self, target, raw, usage, context, group, flags)?;
                Ok(data.write(sources, graph, apple, &linked_frameworks))
            })
            .collect::<Result<Vec<_>, GenError>>()?;
        optional_array(body, "compileGroups", groups);
        Ok(())
    }

    fn source_group(&self, raw: &Target, source: &Source, header: bool) -> BString {
        self.model
            .dir(raw.dir)
            .source_props
            .get(&source.path)
            .and_then(|props| props.get(b"SOURCE_GROUP"))
            .cloned()
            .unwrap_or_else(|| {
                if header {
                    "Header Files".into()
                } else {
                    source.source_group.clone()
                }
            })
    }

    fn source(
        &self,
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
                .is_some_and(|value| is_on(value));
        let (stem, extension) = Lexical::new(&path).stem_extension(true);
        let source_group =
            match extension {
                b".h" | b".hxx" if stem == b"cmake_pch" || stem.starts_with(b"cmake_pch_") => {
                    "Precompile Header File"
                }
                b".h" | b".hh" | b".h++" | b".hm" | b".hpp" | b".hxx" | b".in" | b".txx"
                | b".inl" => "Header Files",
                b".C" | b".F" | b".M" | b".c" | b".c++" | b".cc" | b".cpp" | b".mpp" | b".cxx"
                | b".ixx" | b".cppm" | b".ccm" | b".cxxm" | b".c++m" | b".cu" | b".f" | b".f90"
                | b".for" | b".fpp" | b".ftn" | b".m" | b".mm" | b".rc" | b".def" | b".r"
                | b".odl" | b".idl" | b".hpj" | b".bat" => "Source Files",
                b".rule" => "CMake Rules",
                b".pdf" | b".plist" | b".png" | b".jpeg" | b".jpg" | b".storyboard"
                | b".xcassets" => "Resources",
                b".lo" | b".o" | b".obj" => "Object Files",
                _ => "",
            };
        Source {
            path,
            trace: trace.clone(),
            generated,
            source_group: source_group.into(),
            compile,
        }
    }
}

fn append_objects(
    sources: &mut Vec<Source>,
    seen: &mut BTreeSet<BString>,
    build: &[u8],
    dependency: &ResolvedTarget,
    trace: &Backtrace,
) {
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
        for (field, entries) in [
            (
                UsageField::CompileDefinitions,
                group.defines.iter_mut().collect::<Vec<_>>(),
            ),
            (UsageField::CompileOptions, group.flags.iter_mut().collect()),
            (
                UsageField::IncludeDirectories,
                group
                    .includes
                    .iter_mut()
                    .map(|include| &mut include.path)
                    .collect(),
            ),
        ] {
            let collected = usage.collect(target.id, field, &context).map_err(error)?;
            let traces: BTreeMap<_, _> = collected
                .entries
                .into_iter()
                .map(|entry| (entry.value, entry.backtrace))
                .collect();
            for entry in entries {
                if let Some(trace) = traces.get(&entry.value) {
                    entry.backtrace.clone_from(trace);
                }
            }
        }
        flags.extend(group.flags.drain(..).map(|mut flag| {
            flag.value = flag.value.strip_prefix(b"SHELL:").map_or_else(
                || rcmake_resolve::lower::cmake_posix_quote(&flag.value),
                BString::from,
            );
            flag
        }));
        if !pch.is_empty() {
            flags.push(BT::new(pch, Backtrace::default()));
        }
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
        })
    }

    fn write(
        self,
        sources: Vec<usize>,
        graph: &mut Backtraces<'_>,
        apple: bool,
        linked_frameworks: &[InterfaceInclude],
    ) -> Value {
        let mut body = members([
            ("language", Value::string(self.group.language)),
            ("sourceIndexes", array(sources.into_iter().map(number))),
        ]);
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
        for (field, key, items) in [
            ("compileCommandFragments", "fragment", self.flags),
            ("defines", "define", self.group.defines),
            ("precompileHeaders", "header", self.headers),
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
        Object(body)
    }
}
