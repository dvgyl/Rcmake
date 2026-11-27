//! Write target replies and assign their indexes.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use rcmake_base::{
    BString,
    hash::HashAlgo,
    list,
    path_lex::{Lexical, cmake_style},
    truth::is_on,
};
use rcmake_diag::{Backtrace, Frame, FrameKind};
use rcmake_genex::{ArtifactKind, ArtifactPart, EvalContext};
use rcmake_host::path::{from_native_cmake, to_native};
use rcmake_model::{DirId, Directory, GeneratorKind, ProjectModel, Target, TargetId, TargetKind};
use rcmake_resolve::{
    ArtifactSet, LinkFragmentRole, LinkKind, ResolvedTarget, TargetMetadata,
    lower::{codemodel_link_flags, codemodel_runtime_link_fragments},
    usage::UsageResolver,
};

use crate::{
    GenError,
    ide::{UtilityKind, project_roots, targets_for_root_indexed, utility_targets},
};

use super::super::{
    Bool, FileApi, Kind, Members, Object, Value, array, cache_value, members, object, relative,
};
use super::{
    ConfigIndices, ProjectIndices, backtraces::Backtraces, file_prefix,
    includes::interface_includes, indices, number, optional_array, projects::Project,
};

pub(super) struct TargetRef<'a> {
    pub(super) id: String,
    pub(super) target: &'a ResolvedTarget,
    pub(super) index: usize,
    dir_index: usize,
    project_index: usize,
    pub(super) abstract_target: bool,
}

#[derive(Clone, Default)]
pub(super) struct TargetIndexes {
    build_targets: Vec<usize>,
    abstract_targets: Vec<usize>,
}

impl TargetIndexes {
    pub(super) fn insert(&self, body: &mut Members) {
        indices(body, "targetIndexes", self.build_targets.iter().copied());
        indices(
            body,
            "abstractTargetIndexes",
            self.abstract_targets.iter().copied(),
        );
    }
}

pub(super) struct TargetReferences<'a> {
    pub(super) build: Vec<TargetId>,
    pub(super) abstract_targets: Vec<TargetId>,
    pub(super) identities: BTreeMap<TargetId, TargetRef<'a>>,
}

impl<'a> TargetReferences<'a> {
    pub(super) fn collect(
        model: &ProjectModel,
        build: &[u8],
        mut targets: Vec<&'a ResolvedTarget>,
        directories: &[(DirId, &Directory)],
        project_by_dir: &[usize],
        directory_targets: &mut [TargetIndexes],
        projects: &mut [Project],
    ) -> Self {
        targets.sort_by_key(|target| (&target.metadata.name, target.id.index()));
        let mut references = Vec::new();
        let mut abstract_refs = Vec::new();
        let mut identities = BTreeMap::new();
        let directories_by_build: BTreeMap<_, _> = directories
            .iter()
            .enumerate()
            .map(|(index, (_, dir))| (dir.binary_dir.as_slice(), index))
            .collect();
        for target in targets {
            let raw = model.target(target.id);
            let dir_index = raw
                .map(|raw| raw.dir.index())
                .or_else(|| {
                    directories_by_build
                        .get(target.metadata.binary_dir.as_slice())
                        .copied()
                })
                .filter(|&index| index < directories.len())
                .unwrap_or_default();
            let Some(&project_index) = project_by_dir.get(dir_index) else {
                continue;
            };
            let Some(directory_targets) = directory_targets.get_mut(dir_index) else {
                continue;
            };
            let Some(project) = projects.get_mut(project_index) else {
                continue;
            };
            let abstract_target = raw.map_or_else(
                || {
                    target.metadata.kind == TargetKind::InterfaceLibrary
                        && target.compile_units.is_empty()
                },
                |raw| !model.is_in_build_system(raw),
            );
            let (list, directory, project) = if abstract_target {
                (
                    &mut abstract_refs,
                    &mut directory_targets.abstract_targets,
                    &mut project.targets.abstract_targets,
                )
            } else {
                (
                    &mut references,
                    &mut directory_targets.build_targets,
                    &mut project.targets.build_targets,
                )
            };
            let index = list.len();
            directory.push(index);
            project.push(index);
            let dir = relative(&target.metadata.binary_dir, build);
            let id = target_id(&target.metadata.name, &dir);
            identities.insert(
                target.id,
                TargetRef {
                    id,
                    target,
                    index,
                    dir_index,
                    project_index,
                    abstract_target,
                },
            );
            list.push(target.id);
        }
        Self {
            build: references,
            abstract_targets: abstract_refs,
            identities,
        }
    }
}

pub(super) fn abstract_targets(
    model: &ProjectModel,
    config: &[u8],
    targets: &[&ResolvedTarget],
) -> Vec<ResolvedTarget> {
    let usage = UsageResolver::new(model);
    let config = rcmake_genex::ConfigId(config.into());
    let resolved_ids: BTreeSet<_> = targets.iter().map(|target| target.id).collect();
    model
        .targets()
        .filter(|(id, raw)| !model.is_in_build_system(raw) && !resolved_ids.contains(id))
        .map(|(id, raw)| {
            let dir = model.dir(raw.dir);
            let artifact = |kind| usage.artifact(id, kind, ArtifactPart::Full, &config);
            let artifacts = if raw.is_imported()
                && matches!(
                    raw.kind,
                    TargetKind::Executable
                        | TargetKind::StaticLibrary
                        | TargetKind::SharedLibrary
                        | TargetKind::ModuleLibrary
                ) {
                ArtifactSet {
                    file: artifact(ArtifactKind::File),
                    import_file: artifact(ArtifactKind::ImportFile),
                    ..Default::default()
                }
            } else {
                ArtifactSet::default()
            };
            ResolvedTarget {
                fortran: None,
                id,
                config: config.clone(),
                metadata: TargetMetadata {
                    name: raw.name.clone(),
                    kind: raw.kind,
                    source_dir: dir.source_dir.clone(),
                    binary_dir: dir.binary_dir.clone(),
                    backtrace: raw.backtrace.clone(),
                    folder: raw.props.get(b"FOLDER").cloned(),
                    exclude_from_all: false,
                    generator_provided: false,
                },
                artifacts,
                compile_groups: Vec::new(),
                compile_units: Vec::new(),
                link: None,
                dependencies: Vec::new(),
                cxx_modules: None,
                ide: None,
            }
        })
        .collect()
}

impl FileApi<'_, '_> {
    #[allow(clippy::too_many_arguments)]
    pub(super) fn add_utility_targets(
        &mut self,
        config: &[u8],
        identities: &BTreeMap<TargetId, TargetRef<'_>>,
        project_by_dir: &[usize],
        dir_targets: &mut [TargetIndexes],
        projects: &mut [Project],
        target_refs: &mut Vec<Value>,
        project_indices: &ProjectIndices<'_>,
    ) -> Result<(), GenError> {
        if let Some(kind) = self.generator().filter(|kind| kind.is_ide()) {
            let utilities = utility_targets(kind, self.model, self.writer.host);
            let zero_check = utilities
                .iter()
                .find(|target| target.kind == UtilityKind::ZeroCheck);
            for root in project_roots(kind, self.model) {
                let dir_index = root.dir.index();
                let Some(&project_index) = project_by_dir.get(dir_index) else {
                    continue;
                };
                let (Some(directory_targets), Some(project)) = (
                    dir_targets.get_mut(dir_index),
                    projects.get_mut(project_index),
                ) else {
                    continue;
                };
                let dir = self.model.dir(root.dir);
                let build = relative(&dir.binary_dir, self.build);
                let dependencies = targets_for_root_indexed(
                    self.model,
                    &project_indices.first_config_deps,
                    root.dir,
                )
                .into_iter()
                .filter_map(|id| identities.get(&id))
                .filter(|target| {
                    !target.abstract_target && !target.target.metadata.exclude_from_all
                })
                .map(|target| object([("id", Value::string(target.id.as_str()))]))
                .collect::<Vec<_>>();
                for utility in utilities.iter().filter(|utility| {
                    matches!(utility.kind, UtilityKind::AllBuild | UtilityKind::ZeroCheck)
                }) {
                    let name = &utility.name;
                    let id = target_id(name, &build);
                    let mut body = members([
                        ("codemodelVersion", Kind::Codemodel.version_json()),
                        ("name", Value::string(name.clone())),
                        ("id", Value::string(id.as_str())),
                        ("type", Value::string("UTILITY")),
                        (
                            "paths",
                            self.relative_paths(&dir.source_dir, &dir.binary_dir),
                        ),
                        ("isGeneratorProvided", Bool(true)),
                    ]);
                    let listfile = to_native(&dir.source_dir).join("CMakeLists.txt");
                    let trace = Backtrace::default().push(Frame {
                        file: from_native_cmake(&listfile).as_slice().into(),
                        line: 0,
                        name: None,
                        kind: FrameKind::Command,
                    });
                    let mut graph = Backtraces::new(self.source);
                    graph.tag(&mut body, &trace);
                    body.insert("backtraceGraph".into(), graph.into_json());
                    if utility.kind == UtilityKind::AllBuild {
                        optional_array(
                            &mut body,
                            "orderDependencies",
                            dependencies.iter().cloned(),
                        );
                        optional_array(
                            &mut body,
                            "dependencies",
                            dependencies.iter().cloned().chain(zero_check.map(|zero| {
                                object([("id", Value::string(target_id(&zero.name, &build)))])
                            })),
                        );
                    }
                    let file = self
                        .writer
                        .hashed(&file_prefix("target", name, config), &Object(body))?;
                    let index = target_refs.len();
                    directory_targets.build_targets.push(index);
                    project.targets.build_targets.push(index);
                    target_refs.push(object([
                        ("name", Value::string(name.clone())),
                        ("id", Value::string(id.as_str())),
                        ("directoryIndex", number(dir_index)),
                        ("projectIndex", number(project_index)),
                        ("jsonFile", file),
                    ]));
                }
            }
        }
        Ok(())
    }

    pub(super) fn target_references(
        &mut self,
        config: &[u8],
        ids: &[TargetId],
        targets: &BTreeMap<TargetId, TargetRef<'_>>,
        config_indices: &ConfigIndices<'_>,
    ) -> Result<Vec<Value>, GenError> {
        let refs: Vec<&TargetRef<'_>> = ids.iter().filter_map(|id| targets.get(id)).collect();
        let mut usage = UsageResolver::for_codemodel(self.model);
        if refs.len() < 32 {
            return refs
                .into_iter()
                .map(|reference| {
                    let mut value =
                        self.target_object(reference, targets, &mut usage, config_indices)?;
                    if let Some(target) = self.model.target(reference.target.id) {
                        let includes =
                            interface_includes(self.model, &mut usage, reference.target, target)?;
                        optional_array(&mut value, "interfaceIncludes", includes);
                    }
                    let name = &reference.target.metadata.name;
                    let file = self
                        .writer
                        .hashed(&file_prefix("target", name, config), &Object(value))?;
                    Ok(object([
                        ("name", Value::string(name.clone())),
                        ("id", Value::string(reference.id.as_str())),
                        ("directoryIndex", number(reference.dir_index)),
                        ("projectIndex", number(reference.project_index)),
                        ("jsonFile", file),
                    ]))
                })
                .collect();
        }
        let mut payloads: Vec<(&TargetRef<'_>, String, Value)> = Vec::with_capacity(refs.len());
        for reference in refs {
            let mut value = self.target_object(reference, targets, &mut usage, config_indices)?;
            if let Some(target) = self.model.target(reference.target.id) {
                let includes =
                    interface_includes(self.model, &mut usage, reference.target, target)?;
                optional_array(&mut value, "interfaceIncludes", includes);
            }
            let prefix = file_prefix("target", &reference.target.metadata.name, config);
            payloads.push((reference, prefix, Object(value)));
        }
        let workers = std::thread::available_parallelism()
            .map_or(1, usize::from)
            .clamp(1, 4)
            .min(payloads.len());
        let encoded: Vec<(String, BString)> = if workers <= 1 {
            payloads
                .iter()
                .map(|(_, prefix, value)| {
                    let bytes = super::super::json_bytes(value);
                    let file = format!("{prefix}-{}.json", super::super::hash_prefix(&bytes));
                    (file, bytes)
                })
                .collect()
        } else {
            let chunk_size = payloads.len().div_ceil(workers);
            let chunks: Vec<Vec<(String, BString)>> = std::thread::scope(|scope| {
                let handles: Vec<_> = payloads
                    .chunks(chunk_size)
                    .map(|chunk| {
                        scope.spawn(move || {
                            chunk
                                .iter()
                                .map(|(_, prefix, value)| {
                                    let bytes = super::super::json_bytes(value);
                                    let file = format!(
                                        "{prefix}-{}.json",
                                        super::super::hash_prefix(&bytes)
                                    );
                                    (file, bytes)
                                })
                                .collect::<Vec<_>>()
                        })
                    })
                    .collect();
                handles
                    .into_iter()
                    .map(|handle| handle.join().unwrap_or_default())
                    .collect()
            });
            chunks.into_iter().flatten().collect()
        };
        let mut out = Vec::with_capacity(payloads.len());
        for ((reference, _, _), (file_name, bytes)) in payloads.into_iter().zip(encoded) {
            let file = self.writer.write_hashed(&file_name, &bytes)?;
            out.push(object([
                (
                    "name",
                    Value::string(reference.target.metadata.name.clone()),
                ),
                ("id", Value::string(reference.id.as_str())),
                ("directoryIndex", number(reference.dir_index)),
                ("projectIndex", number(reference.project_index)),
                ("jsonFile", file),
            ]));
        }
        Ok(out)
    }

    fn target_object(
        &self,
        reference: &TargetRef<'_>,
        targets: &BTreeMap<TargetId, TargetRef<'_>>,
        usage: &mut UsageResolver<'_>,
        config_indices: &ConfigIndices<'_>,
    ) -> Result<Members, GenError> {
        let target = reference.target;
        let metadata = &target.metadata;
        let raw = self.model.target(target.id);
        let mut graph = Backtraces::new(self.source);
        let mut body = members([
            ("codemodelVersion", Kind::Codemodel.version_json()),
            ("name", Value::string(metadata.name.clone())),
            ("id", Value::string(reference.id.as_str())),
            ("type", Value::string(metadata.kind.as_str())),
            (
                "paths",
                self.relative_paths(&metadata.source_dir, &metadata.binary_dir),
            ),
        ]);
        if let Some(raw) = raw.filter(|raw| !raw.is_imported()) {
            let specific = match self.model.generator {
                Some(GeneratorKind::VisualStudio(_)) => {
                    raw.props.get(b"VS_DEBUGGER_WORKING_DIRECTORY")
                }
                Some(GeneratorKind::Xcode) => raw.props.get(b"XCODE_SCHEME_WORKING_DIRECTORY"),
                _ => None,
            };
            if let Some(path) = specific.or_else(|| raw.props.get(b"DEBUGGER_WORKING_DIRECTORY")) {
                body.insert(
                    "debugger".into(),
                    object([("workingDirectory", Value::string(path.clone()))]),
                );
            }
        }
        graph.tag(&mut body, &metadata.backtrace);
        if let Some(folder) = &metadata.folder {
            body.insert(
                "folder".into(),
                object([("name", Value::string(folder.clone()))]),
            );
        }
        if metadata.generator_provided {
            body.insert("isGeneratorProvided".into(), Bool(true));
        }
        if reference.abstract_target {
            body.insert("abstract".into(), Bool(true));
        }
        if let Some(raw) = raw.filter(|raw| raw.is_imported()) {
            body.insert("imported".into(), Bool(true));
            if !raw.is_global() {
                body.insert("local".into(), Bool(true));
            }
        }
        if raw
            .and_then(|raw| raw.props.get(b"SYMBOLIC"))
            .is_some_and(|value| is_on(value))
        {
            body.insert("symbolic".into(), Bool(true));
        }

        let artifacts = &target.artifacts;
        let framework = raw.is_some_and(|raw| {
            matches!(
                rcmake_resolve::ide::platform_bundle_kind(self.model, raw),
                rcmake_resolve::ide::BundleKind::Framework { .. }
            )
        });
        let main = if framework
            || (matches!(
                metadata.kind,
                TargetKind::SharedLibrary | TargetKind::Executable | TargetKind::ModuleLibrary
            ) && artifacts
                .linker_file
                .as_ref()
                .is_some_and(|file| artifacts.import_file.as_ref() != Some(file)))
        {
            artifacts.linker_file.as_ref().or(artifacts.file.as_ref())
        } else {
            artifacts.file.as_ref()
        };
        if let Some(file) = main {
            let name = if framework {
                let wrapper = Lexical::new(file).parent();
                let wrapper = file_name(wrapper);
                [wrapper, b"/", file_name(file)].concat().into()
            } else {
                rcmake_base::BString::from(file_name(file))
            };
            body.insert("nameOnDisk".into(), Value::string(name));
        }
        let mut seen = BTreeSet::new();
        let artifact_paths = [
            main,
            artifacts.import_file.as_ref(),
            artifacts.pdb_file.as_ref(),
        ]
        .into_iter()
        .flatten()
        .map(|file| Lexical::new(&relative(file, self.build)).normalize())
        .chain(
            target
                .compile_units
                .iter()
                .filter(|_| metadata.kind == TargetKind::ObjectLibrary)
                .map(|unit| {
                    let object = raw.map_or_else(
                        || unit.object.clone(),
                        |raw| {
                            rcmake_resolve::lower::file_api_object_path(
                                self.model,
                                raw,
                                &target.config,
                                &unit.object,
                            )
                        },
                    );
                    relative_preserve_dots(&object, self.build)
                }),
        )
        .filter(|file| seen.insert(file.clone()))
        .map(|file| object([("path", Value::string(file))]));
        optional_array(&mut body, "artifacts", artifact_paths);

        let destinations = config_indices
            .installers_by_target
            .get(&target.id)
            .map(Vec::as_slice)
            .unwrap_or_default()
            .iter()
            .filter_map(|installer| {
                let rule = self.model.installs.get(installer.rule)?;
                let mut destination =
                    members([("path", Value::string(installer.destination.clone()))]);
                graph.tag(&mut destination, &rule.backtrace);
                Some(Object(destination))
            })
            .collect::<Vec<_>>();
        let has_install_rule = self
            .result
            .install_targets
            .have_install_rule
            .contains(&target.id);
        let has_bmi_install = reference.abstract_target
            && self
                .model
                .installs
                .iter()
                .enumerate()
                .any(|(index, rule)| {
                    matches!(&rule.kind, rcmake_model::InstallKind::Targets { targets: ids, .. } if ids.contains(&target.id))
                        && rcmake_resolve::lower::install_target_group_options(
                            self.model,
                            index,
                            b"CXX_MODULES_BMI",
                        )
                        .is_some_and(|options| {
                            options
                                .destination
                                .as_ref()
                                .is_some_and(|path| !path.is_empty())
                        })
                });
        if !destinations.is_empty() || has_install_rule || has_bmi_install {
            body.insert(
                "install".into(),
                object([
                    (
                        "prefix",
                        object([(
                            "path",
                            Value::string(
                                cache_value(self.model, b"CMAKE_INSTALL_PREFIX")
                                    .unwrap_or(b"/usr/local"),
                            ),
                        )]),
                    ),
                    ("destinations", array(destinations)),
                ]),
            );
        }

        if let Some(link) = &target.link {
            let static_archive = link.kind == LinkKind::StaticArchive;
            let mut step = Members::new();
            if !static_archive {
                step.insert("language".into(), Value::string(link.language.clone()));
            }
            let mut fragments = Vec::new();
            if let Some(raw) = raw {
                let (flags, ipo) = codemodel_link_flags(self.model, raw, &target.config, link)
                    .map_err(|error| GenError::Invalid(error.to_string().into()))?;
                if ipo {
                    step.insert("lto".into(), Bool(true));
                }
                fragments.extend(flags.into_iter().map(|flags| {
                    object([
                        ("role", Value::string("flags")),
                        ("fragment", Value::string(flags)),
                    ])
                }));
            }
            let framework_flags: Vec<_> = link
                .fragments
                .iter()
                .filter(|fragment| fragment.role == LinkFragmentRole::FrameworkPath)
                .map(|fragment| fragment.text.value.as_slice())
                .collect();
            let framework_flags = (!framework_flags.is_empty()).then(|| {
                object([
                    ("role", Value::string("frameworkPath")),
                    (
                        "fragment",
                        Value::string(rcmake_base::BString::from(framework_flags.join(&b' '))),
                    ),
                ])
            });
            fragments.extend(
                link.fragments
                    .iter()
                    .filter(|fragment| fragment.role == LinkFragmentRole::Flags)
                    .map(|fragment| {
                        let mut item = members([("role", Value::string("flags"))]);
                        if static_archive {
                            item.insert(
                                "fragment".into(),
                                Value::string(fragment.text.value.clone()),
                            );
                        } else {
                            graph.tagged_value(&mut item, "fragment", &fragment.text);
                        }
                        Object(item)
                    }),
            );
            if !static_archive {
                let mut runtime_paths_frags = Vec::new();
                let mut runtime_libs_frags = Vec::new();
                if let Some(raw) = raw {
                    let mut languages = indexmap::IndexSet::new();
                    for group in &target.compile_groups {
                        languages.insert(group.language.clone());
                    }
                    for &dep_id in &target.dependencies {
                        if let Some(dep_ref) = targets.get(&dep_id) {
                            for group in &dep_ref.target.compile_groups {
                                languages.insert(group.language.clone());
                            }
                        }
                    }
                    let extra = codemodel_runtime_link_fragments(
                        self.model,
                        raw,
                        &target.config,
                        link,
                        &languages,
                        usage,
                    )
                    .map_err(|error| GenError::Invalid(error.to_string().into()))?;
                    for (role, text) in extra {
                        if role == "libraryPath" {
                            runtime_paths_frags.push(text);
                        } else {
                            runtime_libs_frags.push(text);
                        }
                    }
                }
                fragments.extend(framework_flags);
                fragments.extend(link.fragments.iter().filter_map(|fragment| {
                    if fragment.role != LinkFragmentRole::LibraryPath {
                        return None;
                    }
                    let mut item = members([("role", Value::string("libraryPath"))]);
                    graph.tagged_value(&mut item, "fragment", &fragment.text);
                    Some(Object(item))
                }));
                for text in runtime_paths_frags {
                    fragments.push(object([
                        ("role", Value::string("libraryPath")),
                        ("fragment", Value::string(text)),
                    ]));
                }
                let ninja = self.generator().is_some_and(GeneratorKind::is_ninja);
                let runtime_paths = config_indices
                    .build_graph
                    .and_then(|graph| graph.link_runtime_paths.get(&link.output));
                if let Some(rpath) = runtime_paths
                    .and_then(|paths| paths.rpath.as_deref())
                    .map(|value| format_runtime_path_fragment(value, ninja))
                    .filter(|value| !value.is_empty())
                {
                    fragments.push(object([
                        ("role", Value::string("libraries")),
                        ("fragment", Value::string(rpath)),
                    ]));
                }
                fragments.extend(link.fragments.iter().filter_map(|fragment| {
                    if fragment.role != LinkFragmentRole::Libraries {
                        return None;
                    }
                    let mut item = members([("role", Value::string("libraries"))]);
                    let mut entry = fragment.text.clone();
                    if !ninja && !entry.value.starts_with(b"-") {
                        let rel = relative_preserve_dots(&entry.value, self.build);
                        if rel != entry.value {
                            entry.value = rel;
                        }
                    }
                    graph.tagged_value(&mut item, "fragment", &entry);
                    Some(Object(item))
                }));
                for text in runtime_libs_frags {
                    fragments.push(object([
                        ("role", Value::string("libraries")),
                        ("fragment", Value::string(text)),
                    ]));
                }
                if let Some(rpath_link) = runtime_paths
                    .and_then(|paths| paths.rpath_link.as_deref())
                    .map(|value| format_runtime_path_fragment(value, ninja))
                    .filter(|value| !value.is_empty())
                {
                    fragments.push(object([
                        ("role", Value::string("libraries")),
                        ("fragment", Value::string(rpath_link)),
                    ]));
                }
            }
            optional_array(&mut step, "commandFragments", fragments);
            body.insert(
                if static_archive { "archive" } else { "link" }.into(),
                Object(step),
            );
        }

        if let Some(raw) = raw.filter(|raw| raw.kind == TargetKind::Executable) {
            let cross = self
                .model
                .definition(raw.dir, b"CMAKE_CROSSCOMPILING")
                .is_some_and(|value| is_on(value));
            let context = EvalContext {
                config: target.config.clone(),
                ..Default::default()
            };
            let mut launchers = Vec::new();
            for (property, kind) in [
                (b"TEST_LAUNCHER".as_slice(), "test"),
                (b"CROSSCOMPILING_EMULATOR", "emulator"),
            ] {
                if kind == "emulator" && !cross {
                    continue;
                }
                let Some(value) = raw.props.get(property) else {
                    continue;
                };
                let expanded = crate::genex(usage, raw.dir, &context, value, None)?;
                let args = list::split(&expanded, list::Empty::Keep);
                let Some((command, args)) = args
                    .split_first()
                    .filter(|(command, _)| !command.is_empty())
                else {
                    continue;
                };
                let mut launcher = members([
                    ("type", Value::string(kind)),
                    (
                        "command",
                        Value::string(relative(&cmake_style(command), self.source)),
                    ),
                ]);
                optional_array(
                    &mut launcher,
                    "arguments",
                    args.iter().cloned().map(Value::string),
                );
                launchers.push(Object(launcher));
            }
            optional_array(&mut body, "launchers", launchers);
        }
        if let Some(raw) = raw {
            super::dependencies::write(
                self.model,
                &self.result.target_depends,
                reference,
                targets,
                raw,
                usage,
                &mut graph,
                &mut body,
            )?;
        }
        if raw.is_none() {
            optional_array(
                &mut body,
                "dependencies",
                target.dependencies.iter().filter_map(|id| {
                    let entry = targets.get(id).filter(|entry| !entry.abstract_target)?;
                    Some(object([("id", Value::string(entry.id.as_str()))]))
                }),
            );
        }
        self.write_target_sources(
            target,
            raw,
            targets,
            &mut body,
            &mut graph,
            usage,
            config_indices,
        )?;
        body.insert("backtraceGraph".into(), graph.into_json());
        Ok(body)
    }
}

fn target_id(name: &rcmake_base::BString, dir: &[u8]) -> String {
    let hash = HashAlgo::Sha3_256.hex_digest(dir);
    format!("{name}::@{:.20}", hash)
}

pub(super) fn dependency_backtraces<'a>(
    model: &ProjectModel,
    raw: &'a Target,
) -> HashMap<TargetId, &'a Backtrace> {
    let mut map = HashMap::new();
    for item in &raw.utility_deps {
        if let Some(id) = model.find_target(raw.dir, &item.value) {
            map.entry(id).or_insert(&item.backtrace);
        }
    }
    if let Some(links) = raw.usage.link_libraries.as_deref() {
        for item in links {
            if let Some(id) = model.find_target(item.lookup_dir, &item.text.value) {
                map.entry(id).or_insert(&item.text.backtrace);
            }
        }
    }
    map
}

fn file_name(path: &[u8]) -> &[u8] {
    path.rsplit(|&byte| byte == b'/').next().unwrap_or(path)
}

fn format_runtime_path_fragment(raw: &[u8], ninja: bool) -> rcmake_base::BString {
    let trimmed = raw.trim_ascii();
    if !ninja || !trimmed.contains(&b'$') {
        return trimmed.into();
    }
    let mut out = Vec::with_capacity(trimmed.len() + 4);
    for &byte in trimmed {
        if byte == b'$' {
            out.push(b'$');
        }
        out.push(byte);
    }
    out.into()
}

fn relative_preserve_dots(path: &[u8], root: &[u8]) -> rcmake_base::BString {
    let path = cmake_style(path);
    let root = cmake_style(root);
    if !root.is_empty() {
        let trimmed = if root.len() > 1 {
            root.strip_suffix(b"/").unwrap_or(&root)
        } else {
            root.as_slice()
        };
        if trimmed == b"/" {
            if let Some(rest) = path.strip_prefix(b"/") {
                return if rest.is_empty() {
                    ".".into()
                } else {
                    rest.into()
                };
            }
        } else if let Some(rest) = path.strip_prefix(trimmed) {
            if rest.is_empty() {
                return ".".into();
            }
            if let Some(after) = rest.strip_prefix(b"/") {
                return if after.is_empty() {
                    ".".into()
                } else {
                    after.into()
                };
            }
        }
    }
    path
}
