//! Write target replies and assign their indexes.

use std::collections::{BTreeMap, BTreeSet};

use rcmake_base::{
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
    lower::codemodel_link_flags, usage::UsageResolver,
};

use crate::{
    GenError,
    ide::{UtilityKind, project_roots, targets_for_root, utility_targets},
};

use super::super::{
    Bool, FileApi, Kind, Members, Object, Value, array, cache_value, members, object, relative,
};
use super::{
    backtraces::Backtraces, file_prefix, includes::interface_includes, indices, number,
    optional_array, projects::Project,
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
    pub(super) fn add_utility_targets(
        &mut self,
        config: &[u8],
        identities: &BTreeMap<TargetId, TargetRef<'_>>,
        project_by_dir: &[usize],
        dir_targets: &mut [TargetIndexes],
        projects: &mut [Project],
        target_refs: &mut Vec<Value>,
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
                let dependencies = targets_for_root(self.model, self.result, root.dir)
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
    ) -> Result<Vec<Value>, GenError> {
        let mut usage = UsageResolver::for_codemodel(self.model);
        ids.iter()
            .filter_map(|id| targets.get(id))
            .map(|reference| {
                let mut value = self.target_object(config, reference, targets, &mut usage)?;
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
            .collect()
    }

    fn target_object(
        &self,
        config: &[u8],
        reference: &TargetRef<'_>,
        targets: &BTreeMap<TargetId, TargetRef<'_>>,
        usage: &mut UsageResolver<'_>,
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
            raw.props
                .get(b"FRAMEWORK")
                .is_some_and(|value| is_on(value))
        });
        let main = if framework
            || metadata.kind == TargetKind::SharedLibrary
                && artifacts
                    .linker_file
                    .as_ref()
                    .is_some_and(|file| file.ends_with(b".dylib") || file.ends_with(b".so"))
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
        .map(|value| value.as_slice())
        .chain(
            target
                .compile_units
                .iter()
                .filter(|_| metadata.kind == TargetKind::ObjectLibrary)
                .map(|unit| unit.object.as_slice()),
        )
        .map(|file| relative(file, self.build))
        .filter(|file| seen.insert(file.clone()))
        .map(|file| object([("path", Value::string(file))]));
        optional_array(&mut body, "artifacts", artifact_paths);

        let destinations = self
            .result
            .install
            .target_installers
            .iter()
            .filter(|installer| installer.target == target.id && installer.config.matches(config))
            .filter_map(|installer| {
                let rule = self.model.installs.get(installer.rule)?;
                let mut destination =
                    members([("path", Value::string(installer.destination.clone()))]);
                graph.tag(&mut destination, &rule.backtrace);
                Some(Object(destination))
            })
            .collect::<Vec<_>>();
        if !destinations.is_empty() {
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
                fragments.extend(framework_flags);
                fragments.extend(link.fragments.iter().filter_map(|fragment| {
                    let role = match fragment.role {
                        LinkFragmentRole::LibraryPath => "libraryPath",
                        LinkFragmentRole::Libraries => "libraries",
                        _ => return None,
                    };
                    let mut item = members([("role", Value::string(role))]);
                    graph.tagged_value(&mut item, "fragment", &fragment.text);
                    Some(Object(item))
                }));
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
                self.model, reference, targets, raw, usage, &mut graph, &mut body,
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
        self.write_target_sources(target, raw, targets, &mut body, &mut graph, usage)?;
        body.insert("backtraceGraph".into(), graph.into_json());
        Ok(body)
    }
}

fn target_id(name: &rcmake_base::BString, dir: &[u8]) -> String {
    let hash = HashAlgo::Sha3_256.hex_digest(dir);
    format!("{name}::@{:.20}", hash)
}

pub(super) fn dependency_backtrace<'a>(
    model: &ProjectModel,
    raw: Option<&'a Target>,
    id: TargetId,
) -> Option<&'a Backtrace> {
    let raw = raw?;
    raw.utility_deps
        .iter()
        .find(|item| model.find_target(raw.dir, &item.value) == Some(id))
        .map(|item| &item.backtrace)
        .or_else(|| {
            raw.usage
                .link_libraries
                .as_deref()?
                .iter()
                .find(|item| model.find_target(item.lookup_dir, &item.text.value) == Some(id))
                .map(|item| &item.text.backtrace)
        })
}

fn file_name(path: &[u8]) -> &[u8] {
    path.rsplit(|&byte| byte == b'/').next().unwrap_or(path)
}
