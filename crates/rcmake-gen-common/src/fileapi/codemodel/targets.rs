//! Write target replies and assign their indexes.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use rcmake_base::{hash::HashAlgo, list, truth::is_on};
use rcmake_diag::{Backtrace, Frame, FrameKind};
use rcmake_host::path::{from_native, from_native_cmake, to_native};
use rcmake_model::{BT, DirId, Directory, ProjectModel, Target, TargetId, TargetKind};
use rcmake_resolve::{
    ArtifactSet, CompileGroup, LinkFragmentRole, LinkKind, ResolvedTarget, TargetMetadata,
    usage::UsageResolver,
};

use crate::{
    GenError,
    ide::{UtilityKind, project_roots, targets_for_root, utility_targets},
};

use super::super::{
    Bool, FileApi, Kind, Members, Object, UInt, Value, array, cache_value, members, object,
    relative,
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
    let resolved_ids: BTreeSet<_> = targets.iter().map(|target| target.id).collect();
    model
        .targets()
        .filter(|(id, raw)| !model.is_in_build_system(raw) && !resolved_ids.contains(id))
        .map(|(id, raw)| {
            let dir = model.dir(raw.dir);
            let mut target = ResolvedTarget {
                fortran: None,
                id,
                config: Default::default(),
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
                artifacts: ArtifactSet::default(),
                compile_groups: Vec::new(),
                compile_units: Vec::new(),
                link: None,
                dependencies: Vec::new(),
                cxx_modules: None,
                ide: None,
            };
            target.config.0 = config.into();
            target
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
        let mut usage = UsageResolver::new(self.model);
        ids.iter()
            .filter_map(|id| targets.get(id))
            .map(|reference| {
                let mut value = self.target_object(config, reference, targets);
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
    ) -> Members {
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
        if let Some(file) = &artifacts.file {
            body.insert("nameOnDisk".into(), Value::string(file_name(file)));
        }
        let mut seen = BTreeSet::new();
        let artifact_paths = [
            &artifacts.file,
            &artifacts.linker_file,
            &artifacts.soname_file,
            &artifacts.import_file,
            &artifacts.pdb_file,
        ]
        .into_iter()
        .filter_map(Option::as_ref)
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
            optional_array(
                &mut step,
                "commandFragments",
                link.fragments.iter().map(|fragment| {
                    let role = match fragment.role {
                        LinkFragmentRole::Flags => "flags",
                        LinkFragmentRole::Libraries => "libraries",
                        LinkFragmentRole::LibraryPath => "libraryPath",
                        LinkFragmentRole::FrameworkPath => "frameworkPath",
                    };
                    let mut item = members([("role", Value::string(role))]);
                    graph.tagged_value(&mut item, "fragment", &fragment.text);
                    Object(item)
                }),
            );
            body.insert(
                if static_archive { "archive" } else { "link" }.into(),
                Object(step),
            );
        }

        optional_array(
            &mut body,
            "dependencies",
            target.dependencies.iter().filter_map(|id| {
                let dependency = targets.get(id).filter(|target| !target.abstract_target)?;
                let mut item = members([("id", Value::string(dependency.id.as_str()))]);
                if let Some(backtrace) = dependency_backtrace(self.model, raw, *id) {
                    graph.tag(&mut item, backtrace);
                }
                Some(Object(item))
            }),
        );
        self.target_sources(target, raw, &mut body, &mut graph);
        body.insert("backtraceGraph".into(), graph.into_json());
        body
    }

    fn target_sources(
        &self,
        target: &ResolvedTarget,
        raw: Option<&Target>,
        body: &mut Members,
        graph: &mut Backtraces<'_>,
    ) {
        let mut sources = Vec::new();
        let mut source_groups = [Vec::new(), Vec::new(), Vec::new()];
        let mut seen = BTreeSet::new();
        let raw_sources = raw
            .into_iter()
            .flat_map(|target| &target.sources)
            .flat_map(|entry| {
                list::split(&entry.value, list::Empty::Drop)
                    .into_iter()
                    .filter(|file| !file.windows(2).any(|pair| pair == b"$<"))
                    .map(|file| (BT::new(file, entry.backtrace.clone()), None))
            });
        let units = target
            .compile_units
            .iter()
            .map(|unit| (unit.source.clone(), Some(unit.group)));
        for (source, group) in units.chain(raw_sources) {
            let file = to_native(&target.metadata.source_dir).join(to_native(&source.value));
            let file = from_native_cmake(&file);
            if !seen.insert(file.clone()) {
                continue;
            }
            let mut entry = members([("path", Value::string(relative(&file, self.source)))]);
            if let Some(group) = group.filter(|&index| index < target.compile_groups.len()) {
                entry.insert("compileGroupIndex".into(), number(group));
            }
            let extension = to_native(&file)
                .extension()
                .map(|ext| from_native(Path::new(ext)));
            let group = match extension.as_deref().map(|ext| ext.as_slice()) {
                Some(b"h" | b"hh" | b"hpp" | b"hxx" | b"inl") => 1,
                _ if group.is_some() => 0,
                _ => 2,
            };
            if let Some(indices) = source_groups.get_mut(group) {
                indices.push(sources.len());
            }
            if let Some(index) = graph.add(&source.backtrace) {
                entry.insert("backtrace".into(), number(index));
                entry.insert("backtraces".into(), array([number(index)]));
            }
            if raw
                .and_then(|target| self.model.dir(target.dir).source_props.get(&file))
                .and_then(|props| props.get(b"GENERATED"))
                .is_some_and(|value| is_on(value))
            {
                entry.insert("isGenerated".into(), Bool(true));
            }
            sources.push(entry);
        }
        let mut groups = Vec::new();
        for (name, indices) in ["Source Files", "Header Files", ""]
            .into_iter()
            .zip(source_groups)
        {
            if indices.is_empty() {
                continue;
            }
            for &index in &indices {
                if let Some(source) = sources.get_mut(index) {
                    source.insert("sourceGroupIndex".into(), number(groups.len()));
                }
            }
            groups.push(object([
                ("name", Value::string(name)),
                ("sourceIndexes", array(indices.into_iter().map(number))),
            ]));
        }
        optional_array(body, "sourceGroups", groups);
        let mut units_by_group = vec![Vec::new(); target.compile_groups.len()];
        for (index, source) in sources.iter().enumerate() {
            if let Some(UInt(group)) = source.get(b"compileGroupIndex".as_slice())
                && let Some(units) = units_by_group.get_mut(*group as usize)
            {
                units.push(index);
            }
        }
        optional_array(
            body,
            "compileGroups",
            target
                .compile_groups
                .iter()
                .zip(units_by_group)
                .map(|(group, units)| compile_group(group, units, graph)),
        );
        body.insert("sources".into(), array(sources.into_iter().map(Object)));
    }
}

fn target_id(name: &rcmake_base::BString, dir: &[u8]) -> String {
    let hash = HashAlgo::Sha3_256.hex_digest(dir);
    format!("{name}::@{:.20}", hash)
}

fn dependency_backtrace<'a>(
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

fn compile_group(group: &CompileGroup, sources: Vec<usize>, graph: &mut Backtraces<'_>) -> Value {
    let mut body = members([
        ("language", Value::string(group.language.clone())),
        ("sourceIndexes", array(sources.into_iter().map(number))),
    ]);
    for (field, key, items) in [
        ("compileCommandFragments", "fragment", &group.flags),
        ("defines", "define", &group.defines),
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
    optional_array(
        &mut body,
        "includes",
        group.includes.iter().map(|value| {
            let mut item = Members::new();
            graph.tagged_value(&mut item, "path", &value.path);
            if value.system {
                item.insert("isSystem".into(), Bool(true));
            }
            Object(item)
        }),
    );
    Object(body)
}

fn file_name(path: &[u8]) -> &[u8] {
    path.rsplit(|&byte| byte == b'/').next().unwrap_or(path)
}
