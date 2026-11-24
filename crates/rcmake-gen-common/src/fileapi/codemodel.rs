use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use rcmake_base::{BString, list, truth::is_on};
use rcmake_diag::{Backtrace, Frame, FrameKind};
use rcmake_host::path::{from_native, to_native};
use rcmake_model::{
    BT, Directory, InstallKind, InstallRule, ProjectModel, Target, TargetId, TargetKind,
};
use rcmake_resolve::{
    ArtifactSet, CompileGroup, InstallActionKind, LinkFragmentRole, LinkKind, ResolvedTarget,
    TargetMetadata,
};

use super::{
    Bool, FileApi, Kind, Members, Object, UInt, Value, array, cache_value, hash_prefix, members,
    object, paths, relative, strings,
};
use crate::{
    GenError,
    ide::{UtilityKind, project_roots, targets_for_root, utility_targets},
};

impl FileApi<'_, '_> {
    pub(super) fn codemodel(&mut self) -> Result<Members, GenError> {
        let mut configurations = Vec::new();
        for config in &self.result.configs {
            let targets = self
                .result
                .targets
                .iter()
                .filter(|target| target.config == *config)
                .collect();
            configurations.push(self.configuration(&config.0, targets)?);
        }
        if configurations.is_empty() {
            configurations.push(self.configuration(b"", Vec::new())?);
        }
        Ok(members([
            ("paths", paths(self.source, self.build)),
            ("configurations", array(configurations)),
        ]))
    }

    fn configuration(
        &mut self,
        config: &[u8],
        targets: Vec<&ResolvedTarget>,
    ) -> Result<Value, GenError> {
        let resolved_ids: BTreeSet<_> = targets.iter().map(|target| target.id).collect();
        let abstract_targets: Vec<_> = self
            .model
            .targets()
            .filter(|(id, raw)| is_abstract(raw) && !resolved_ids.contains(id))
            .map(|(id, raw)| {
                let dir = self.model.dir(raw.dir);
                let mut target = ResolvedTarget {
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
            .collect();
        let mut targets: Vec<_> = targets.into_iter().chain(&abstract_targets).collect();
        targets.sort_by_key(|target| (&target.metadata.name, target.id.index()));
        let (dir_ids, directories): (Vec<_>, Vec<_>) = self.model.dirs().unzip();
        let (mut projects, project_by_dir) = project_tree(self.model, &directories);
        let mut dir_targets = vec![TargetIndexes::default(); directories.len()];
        let mut references = Vec::new();
        let mut abstract_refs = Vec::new();
        let mut identities = BTreeMap::new();
        let mut rules_by_target: BTreeMap<_, Vec<_>> = BTreeMap::new();
        let mut rules_by_dir: BTreeMap<_, Vec<_>> = BTreeMap::new();
        for rule in &self.model.installs {
            rules_by_dir.entry(rule.dir).or_default().push(rule);
            if let InstallKind::Targets { targets, .. } = &rule.kind {
                for id in targets {
                    rules_by_target.entry(*id).or_default().push(rule);
                }
            }
        }
        let mut has_install = vec![false; directories.len()];
        for (id, dir) in dir_ids.iter().zip(&directories).rev() {
            let Some(has_rule) = has_install.get_mut(id.index()) else {
                continue;
            };
            *has_rule |= rules_by_dir.contains_key(id);
            if *has_rule
                && let Some(parent) = dir.parent.and_then(|id| has_install.get_mut(id.index()))
            {
                *parent = true;
            }
        }
        let directories_by_build: BTreeMap<_, _> = directories
            .iter()
            .enumerate()
            .map(|(index, dir)| (dir.binary_dir.as_slice(), index))
            .collect();
        for target in targets {
            let raw = self.model.target(target.id);
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
            let Some(directory_targets) = dir_targets.get_mut(dir_index) else {
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
                is_abstract,
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
            let dir = relative(&target.metadata.binary_dir, self.build);
            let id = format!("{}::@{}", target.metadata.name, hash_prefix(&dir));
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
        let mut target_refs =
            self.target_references(config, &references, &identities, &rules_by_target)?;
        let abstract_refs =
            self.target_references(config, &abstract_refs, &identities, &rules_by_target)?;

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
                    let id = format!("{name}::@{}", hash_prefix(&build));
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
                        file: from_native(&listfile).as_slice().into(),
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
                                object([(
                                    "id",
                                    Value::string(format!(
                                        "{}::@{}",
                                        zero.name,
                                        hash_prefix(&build)
                                    )),
                                )])
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

        let mut directory_refs = Vec::new();
        for (((id, dir), project_index), target_indexes) in dir_ids
            .iter()
            .zip(&directories)
            .zip(project_by_dir)
            .zip(dir_targets)
        {
            let source = relative_or_root(&dir.source_dir, self.source);
            let build = relative_or_root(&dir.binary_dir, self.build);
            let directory_rules = rules_by_dir.get(id).map_or(&[][..], Vec::as_slice);
            let directory = self.directory_object(dir, config, directory_rules, &identities);
            let prefix = file_prefix("directory", &build, config);
            let file = self.writer.hashed(&prefix, &directory)?;
            let mut reference = members([
                ("source", Value::string(source)),
                ("build", Value::string(build)),
                ("projectIndex", number(project_index)),
                ("jsonFile", file),
            ]);
            if let Some(parent) = dir.parent {
                reference.insert("parentIndex".into(), number(parent.index()));
            }
            indices(
                &mut reference,
                "childIndexes",
                dir.children.iter().map(|id| id.index()),
            );
            target_indexes.insert(&mut reference);
            if has_install.get(id.index()) == Some(&true) {
                reference.insert("hasInstallRule".into(), Bool(true));
            }
            if let Some(minimum) = dir
                .props
                .get(b"CMAKE_MINIMUM_REQUIRED_VERSION")
                .map(|value| value.as_slice())
                .or_else(|| cache_value(self.model, b"CMAKE_MINIMUM_REQUIRED_VERSION"))
            {
                reference.insert(
                    "minimumCMakeVersion".into(),
                    object([("string", Value::string(minimum))]),
                );
            }
            directory_refs.push(Object(reference));
        }
        Ok(object([
            ("name", Value::string(config)),
            ("directories", array(directory_refs)),
            (
                "projects",
                array(projects.into_iter().map(Project::into_json)),
            ),
            ("targets", array(target_refs)),
            ("abstractTargets", array(abstract_refs)),
        ]))
    }

    fn target_references(
        &mut self,
        config: &[u8],
        ids: &[TargetId],
        targets: &BTreeMap<TargetId, TargetRef<'_>>,
        install_rules: &BTreeMap<TargetId, Vec<&InstallRule>>,
    ) -> Result<Vec<Value>, GenError> {
        ids.iter()
            .filter_map(|id| targets.get(id))
            .map(|reference| {
                let rules = install_rules
                    .get(&reference.target.id)
                    .map_or(&[][..], Vec::as_slice);
                let value = self.target_object(config, reference, targets, rules);
                let name = &reference.target.metadata.name;
                let file = self
                    .writer
                    .hashed(&file_prefix("target", name, config), &value)?;
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
        rules: &[&InstallRule],
    ) -> Value {
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

        let destinations = rules
            .iter()
            .filter_map(|rule| {
                let path = self.target_destination(target, rule, config)?;
                let mut destination = members([("path", Value::string(path))]);
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
        Object(body)
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
            let file = from_native(&file);
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

    fn target_destination(
        &self,
        target: &ResolvedTarget,
        rule: &InstallRule,
        config: &[u8],
    ) -> Option<BString> {
        if !applies(&rule.options.configurations, config) {
            return None;
        }
        if let Some(destination) = &rule.options.destination {
            return Some(destination.clone());
        }
        if let Some(file) = &target.artifacts.file {
            let file = relative(file, self.build);
            let component = install_component(self.model, rule);
            let matching = self.result.install.actions.iter().find(|action| {
                let InstallActionKind::Files { files, .. } = &action.kind else {
                    return false;
                };
                applies(&action.configurations, config)
                    && action.component == component
                    && files
                        .iter()
                        .any(|source| relative(source, self.build) == file)
            });
            if let Some(action) = matching {
                return Some(action.destination.clone());
            }
        }
        let (var, default) = match target.metadata.kind {
            TargetKind::Executable => (b"CMAKE_INSTALL_BINDIR".as_slice(), b"bin".as_slice()),
            TargetKind::StaticLibrary | TargetKind::SharedLibrary | TargetKind::ModuleLibrary => {
                (b"CMAKE_INSTALL_LIBDIR".as_slice(), b"lib".as_slice())
            }
            _ => return None,
        };
        Some(cache_value(self.model, var).unwrap_or(default).into())
    }

    fn relative_paths(&self, source: &[u8], build: &[u8]) -> Value {
        paths(
            &relative_or_root(source, self.source),
            &relative_or_root(build, self.build),
        )
    }

    fn directory_object(
        &self,
        dir: &Directory,
        config: &[u8],
        rules: &[&InstallRule],
        targets: &BTreeMap<TargetId, TargetRef<'_>>,
    ) -> Value {
        let mut graph = Backtraces::new(self.source);
        let mut installers = Vec::new();
        for rule in rules
            .iter()
            .copied()
            .filter(|rule| applies(&rule.options.configurations, config))
        {
            let mut base = members([(
                "component",
                Value::string(install_component(self.model, rule)),
            )]);
            graph.tag(&mut base, &rule.backtrace);
            if rule.options.optional {
                base.insert("isOptional".into(), Bool(true));
            }
            if rule.options.exclude_from_all {
                base.insert("isExcludeFromAll".into(), Bool(true));
            }
            if let Some(destination) = &rule.options.destination {
                base.insert("destination".into(), Value::string(destination.clone()));
            }
            let kind = match &rule.kind {
                InstallKind::Targets { targets: ids, .. } => {
                    for target in ids.iter().filter_map(|id| targets.get(id)) {
                        let Some(destination) =
                            self.target_destination(target.target, rule, config)
                        else {
                            continue;
                        };
                        let mut item = base.clone();
                        item.insert("type".into(), Value::string("target"));
                        item.insert("targetId".into(), Value::string(target.id.as_str()));
                        if !target.abstract_target {
                            item.insert("targetIndex".into(), number(target.index));
                        }
                        item.insert("destination".into(), Value::string(destination));
                        if let Some(file) = &target.target.artifacts.file {
                            item.insert(
                                "paths".into(),
                                array([Value::string(relative(file, self.build))]),
                            );
                        }
                        installers.push(Object(item));
                    }
                    continue;
                }
                InstallKind::Files(files)
                | InstallKind::Programs(files)
                | InstallKind::Directory(files) => {
                    let files = files.iter().map(|file| {
                        let from = relative(&file.value, self.source);
                        if let Some(to) = &rule.options.rename {
                            object([
                                ("from", Value::string(from)),
                                ("to", Value::string(to.clone())),
                            ])
                        } else {
                            Value::string(from)
                        }
                    });
                    base.insert("paths".into(), array(files));
                    if matches!(rule.kind, InstallKind::Directory(_)) {
                        "directory"
                    } else {
                        "file"
                    }
                }
                InstallKind::Script(script) => {
                    base.insert(
                        "scriptFile".into(),
                        Value::string(relative(script, self.source)),
                    );
                    "script"
                }
                InstallKind::Code(_) => "code",
                // The codemodel has no installer type for CPS and SBOM files.
                InstallKind::PackageInfo(_) | InstallKind::Sbom(_) => continue,
                InstallKind::Export { set, file, .. } => {
                    base.insert("exportName".into(), Value::string(set.clone()));
                    let file = file
                        .clone()
                        .unwrap_or_else(|| format!("{set}.cmake").into());
                    base.insert("paths".into(), array([Value::string(file)]));
                    if let Some(export) = self.model.exports.get(set) {
                        base.insert(
                            "exportTargets".into(),
                            array(export.targets.iter().filter_map(|id| targets.get(id)).map(
                                |target| {
                                    let mut item =
                                        members([("id", Value::string(target.id.as_str()))]);
                                    if !target.abstract_target {
                                        item.insert("index".into(), number(target.index));
                                    }
                                    Object(item)
                                },
                            )),
                        );
                    }
                    "export"
                }
            };
            base.insert("type".into(), Value::string(kind));
            installers.push(Object(base));
        }

        object([
            ("codemodelVersion", Kind::Codemodel.version_json()),
            (
                "paths",
                self.relative_paths(&dir.source_dir, &dir.binary_dir),
            ),
            ("installers", array(installers)),
            ("backtraceGraph", graph.into_json()),
        ])
    }
}

struct TargetRef<'a> {
    id: String,
    target: &'a ResolvedTarget,
    index: usize,
    dir_index: usize,
    project_index: usize,
    abstract_target: bool,
}

#[derive(Clone, Default)]
struct TargetIndexes {
    build_targets: Vec<usize>,
    abstract_targets: Vec<usize>,
}

impl TargetIndexes {
    fn insert(&self, body: &mut Members) {
        indices(body, "targetIndexes", self.build_targets.iter().copied());
        indices(
            body,
            "abstractTargetIndexes",
            self.abstract_targets.iter().copied(),
        );
    }
}

#[derive(Default)]
struct Project {
    name: BString,
    parent: Option<usize>,
    children: Vec<usize>,
    directories: Vec<usize>,
    targets: TargetIndexes,
}

impl Project {
    fn into_json(self) -> Value {
        let mut body = members([
            ("name", Value::string(self.name)),
            (
                "directoryIndexes",
                array(self.directories.into_iter().map(number)),
            ),
        ]);
        if let Some(parent) = self.parent {
            body.insert("parentIndex".into(), number(parent));
        }
        indices(&mut body, "childIndexes", self.children);
        self.targets.insert(&mut body);
        Object(body)
    }
}

fn project_tree(model: &ProjectModel, directories: &[&Directory]) -> (Vec<Project>, Vec<usize>) {
    let mut project_by_dir = Vec::<usize>::new();
    let mut projects = Vec::<Project>::new();
    let mut project_names = BTreeMap::new();
    for (key, entry) in model.cache.iter() {
        if let Some(name) = key.strip_suffix(b"_SOURCE_DIR")
            && let Some(binary) = cache_value(model, &[name, b"_BINARY_DIR"].concat())
        {
            project_names.insert((entry.value.as_slice(), binary), name);
        }
    }
    for (index, dir) in directories.iter().enumerate() {
        let parent = dir
            .parent
            .and_then(|id| project_by_dir.get(id.index()))
            .copied();
        let name = dir
            .props
            .get(b"PROJECT_NAME")
            .map(|value| value.as_slice())
            .or_else(|| {
                if index == 0 {
                    cache_value(model, b"CMAKE_PROJECT_NAME")
                } else {
                    project_names
                        .get(&(dir.source_dir.as_slice(), dir.binary_dir.as_slice()))
                        .copied()
                }
            });
        let project_index = match parent {
            Some(parent)
                if projects.get(parent).is_some_and(|project| {
                    name.is_none_or(|name| name == project.name.as_slice())
                }) =>
            {
                parent
            }
            _ => {
                let index = projects.len();
                if let Some(parent) = parent.and_then(|parent| projects.get_mut(parent)) {
                    parent.children.push(index);
                }
                projects.push(Project {
                    name: name.unwrap_or(b"Project").into(),
                    parent,
                    ..Project::default()
                });
                index
            }
        };
        if let Some(project) = projects.get_mut(project_index) {
            project.directories.push(index);
        }
        project_by_dir.push(project_index);
    }
    (projects, project_by_dir)
}

fn is_abstract(target: &Target) -> bool {
    target.is_imported()
        || (target.kind == TargetKind::InterfaceLibrary
            && target.sources.is_empty()
            && target.file_sets.is_empty())
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

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct BacktraceNode {
    file: usize,
    line: u32,
    command: Option<usize>,
    parent: Option<usize>,
}

struct Backtraces<'a> {
    source: &'a [u8],
    files: Vec<BString>,
    file_indexes: BTreeMap<BString, usize>,
    commands: Vec<BString>,
    command_indexes: BTreeMap<BString, usize>,
    nodes: Vec<BacktraceNode>,
    node_indexes: BTreeMap<BacktraceNode, usize>,
}

impl<'a> Backtraces<'a> {
    fn new(source: &'a [u8]) -> Self {
        Self {
            source,
            files: Vec::new(),
            file_indexes: BTreeMap::new(),
            commands: Vec::new(),
            command_indexes: BTreeMap::new(),
            nodes: Vec::new(),
            node_indexes: BTreeMap::new(),
        }
    }

    fn add(&mut self, trace: &Backtrace) -> Option<usize> {
        let frames: Vec<_> = trace.frames().collect();
        let mut parent = None;
        for frame in frames.into_iter().rev() {
            let file = intern(
                &mut self.files,
                &mut self.file_indexes,
                relative(&frame.file, self.source),
            );
            let command = frame
                .name
                .as_deref()
                .map(|name| intern(&mut self.commands, &mut self.command_indexes, name.into()));
            if parent.is_none() && frame.line != 0 {
                parent = Some(self.node(BacktraceNode {
                    file,
                    line: 0,
                    command: None,
                    parent: None,
                }));
            }
            parent = Some(self.node(BacktraceNode {
                file,
                line: frame.line,
                command,
                parent,
            }));
        }
        parent
    }

    fn node(&mut self, node: BacktraceNode) -> usize {
        *self.node_indexes.entry(node).or_insert_with(|| {
            let index = self.nodes.len();
            self.nodes.push(node);
            index
        })
    }

    fn tag(&mut self, body: &mut Members, trace: &Backtrace) {
        if let Some(index) = self.add(trace) {
            body.insert("backtrace".into(), number(index));
        }
    }

    fn tagged_value(&mut self, body: &mut Members, key: &str, item: &BT<BString>) {
        body.insert(key.into(), Value::string(item.value.clone()));
        self.tag(body, &item.backtrace);
    }

    fn into_json(self) -> Value {
        let nodes = self.nodes.into_iter().map(|node| {
            let mut body = members([("file", number(node.file))]);
            if node.line != 0 {
                body.insert("line".into(), UInt(node.line.into()));
            }
            if let Some(command) = node.command {
                body.insert("command".into(), number(command));
            }
            if let Some(parent) = node.parent {
                body.insert("parent".into(), number(parent));
            }
            Object(body)
        });
        object([
            ("files", strings(&self.files)),
            ("commands", strings(&self.commands)),
            ("nodes", array(nodes)),
        ])
    }
}

fn intern(
    values: &mut Vec<BString>,
    indexes: &mut BTreeMap<BString, usize>,
    value: BString,
) -> usize {
    *indexes.entry(value.clone()).or_insert_with(|| {
        let index = values.len();
        values.push(value);
        index
    })
}

fn install_component<'a>(model: &'a ProjectModel, rule: &'a InstallRule) -> &'a [u8] {
    rule.options
        .component
        .as_ref()
        .map(|value| value.as_slice())
        .or_else(|| cache_value(model, b"CMAKE_INSTALL_DEFAULT_COMPONENT_NAME"))
        .unwrap_or(b"Unspecified")
}

fn applies(configurations: &[BString], config: &[u8]) -> bool {
    configurations.is_empty()
        || configurations
            .iter()
            .any(|value| value.eq_ignore_ascii_case(config))
}

fn number(index: usize) -> Value {
    UInt(index as u64)
}

fn indices(body: &mut Members, field: &str, indices: impl IntoIterator<Item = usize>) {
    optional_array(body, field, indices.into_iter().map(number));
}

fn optional_array(body: &mut Members, field: &str, items: impl IntoIterator<Item = Value>) {
    let items: Vec<_> = items.into_iter().collect();
    if !items.is_empty() {
        body.insert(field.into(), Value::Array(items));
    }
}

fn file_name(path: &[u8]) -> &[u8] {
    path.rsplit(|&byte| byte == b'/').next().unwrap_or(path)
}

fn file_prefix(kind: &str, name: &[u8], config: &[u8]) -> String {
    fn safe(bytes: &[u8]) -> String {
        bytes
            .iter()
            .take(96)
            .map(|&byte| {
                if byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_') {
                    char::from(byte)
                } else {
                    '_'
                }
            })
            .collect()
    }
    let name = safe(name);
    if config.is_empty() {
        format!("{kind}-{name}")
    } else {
        format!("{kind}-{name}-{}", safe(config))
    }
}

fn relative_or_root(path: &[u8], root: &[u8]) -> BString {
    if path.is_empty() {
        ".".into()
    } else {
        relative(path, root)
    }
}
