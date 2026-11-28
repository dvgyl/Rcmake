use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::Path;

use rcmake_base::{list, truth};
use rcmake_gen_common::{
    GenError, glob,
    ide::{
        IdeProject, UtilityKind, UtilityTarget, first_config_dependencies, project_roots,
        rerun_inputs, targets_for_root_indexed, utility_targets,
    },
};
use rcmake_host::Host;
use rcmake_model::{
    CommandId, CustomCommand, CustomCommandKind, DirId, GeneratorKind, ProjectModel, Target,
    TargetId, TargetKind, VsVersion,
};
use rcmake_resolve::{
    BuildGraph, BuildStep, GenerationResult, GraphTarget, ResolvedTarget,
    ide::{IdeLinkItem, IdeSourceKind},
    usage::UsageResolver,
};

use rcmake_diag::{PolicyId, PolicyStatus};

use crate::{Settings, absolute, diag, folder, guid, join, project_type, relative, string};

type ResolvedProjects<'a> = BTreeMap<TargetId, BTreeMap<String, &'a ResolvedTarget>>;
type TargetIndices = BTreeMap<TargetId, usize>;
type ScopedTargets = BTreeMap<DirId, BTreeSet<usize>>;

pub(crate) struct VsIndices<'a> {
    pub(crate) resolved_by_id: HashMap<(TargetId, &'a [u8]), &'a ResolvedTarget>,
    pub(crate) project_by_target: HashMap<TargetId, usize>,
    pub(crate) graph_by_config: HashMap<&'a [u8], &'a BuildGraph>,
    pub(crate) graph_target: HashMap<(&'a [u8], &'a [u8]), &'a GraphTarget>,
    pub(crate) target_commands: HashMap<TargetId, Vec<(CommandId, &'a CustomCommand)>>,
    pub(crate) output_commands_by_dir: HashMap<DirId, Vec<(CommandId, &'a CustomCommand)>>,
    /// The output command of each first output, by configuration.
    pub(crate) command_by_output: HashMap<(String, String), CommandId>,
    /// The first output of each output command, in graph order.
    pub(crate) command_outputs: HashMap<CommandId, Vec<String>>,
    /// The source group of the `CMakeLists.txt` file of each directory, with
    /// `\` between levels. See [`listfile_groups`].
    pub(crate) listfile_groups: HashMap<DirId, String>,
}

impl<'a> VsIndices<'a> {
    fn new(model: &'a ProjectModel, result: &'a GenerationResult, projects: &[Project]) -> Self {
        let mut resolved_by_id = HashMap::with_capacity(result.targets.len());
        for target in &result.targets {
            resolved_by_id
                .entry((target.id, target.config.0.as_slice()))
                .or_insert(target);
        }
        let mut project_by_target = HashMap::with_capacity(projects.len());
        for (index, project) in projects.iter().enumerate() {
            if let Some(id) = project.target {
                project_by_target.entry(id).or_insert(index);
            }
        }
        let mut graph_by_config = HashMap::with_capacity(result.graphs.len());
        let mut graph_target = HashMap::new();
        for graph in &result.graphs {
            let cfg = graph.config.0.as_slice();
            if let std::collections::hash_map::Entry::Vacant(entry) = graph_by_config.entry(cfg) {
                entry.insert(graph);
                for item in &graph.targets {
                    graph_target
                        .entry((cfg, item.name.as_slice()))
                        .or_insert(item);
                }
            }
        }
        let mut target_commands: HashMap<TargetId, Vec<(CommandId, &'a CustomCommand)>> =
            HashMap::new();
        let mut output_commands_by_dir: HashMap<DirId, Vec<(CommandId, &'a CustomCommand)>> =
            HashMap::new();
        for (index, command) in model.commands() {
            match command.kind {
                CustomCommandKind::TargetEvent { target: id, .. }
                | CustomCommandKind::Utility(id) => {
                    target_commands
                        .entry(id)
                        .or_default()
                        .push((index, command));
                }
                CustomCommandKind::Output => {
                    output_commands_by_dir
                        .entry(command.dir)
                        .or_default()
                        .push((index, command));
                }
            }
        }
        let root = string(&model.dir(DirId::ROOT).binary_dir);
        let mut command_by_output = HashMap::new();
        let mut command_outputs: HashMap<CommandId, Vec<String>> = HashMap::new();
        for graph in &result.graphs {
            for (&index, owners) in &graph.custom_output_owners {
                let Some(BuildStep::Custom(step)) = graph.steps.get(index) else {
                    continue;
                };
                let Some(first) = step.outputs.first() else {
                    continue;
                };
                let first = absolute(&root, &string(first));
                command_outputs
                    .entry(owners.command)
                    .or_default()
                    .push(first.clone());
                command_by_output
                    .entry((string(&graph.config.0), first))
                    .or_insert(owners.command);
            }
        }
        Self {
            resolved_by_id,
            project_by_target,
            graph_by_config,
            graph_target,
            target_commands,
            output_commands_by_dir,
            command_by_output,
            command_outputs,
            listfile_groups: HashMap::new(),
        }
    }
}

pub(super) struct SolutionPlan<'a> {
    pub(super) settings: Settings,
    pub(super) projects: Vec<Project>,
    pub(super) resolved: ResolvedProjects<'a>,
    pub(super) indices: VsIndices<'a>,
    pub(super) inputs: Vec<String>,
    pub(super) solutions: Vec<Solution>,
    pub(super) regenerate: bool,
}

/// The kind of project file behind one solution project.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Flavor {
    /// A generated C++ project (`.vcxproj`).
    Native,
    /// A generated C# project (`.csproj`) for a target that compiles only C#.
    CSharp,
    /// An existing project file from `include_external_msproject`.
    External {
        /// The project file location as given, with `/` separators.
        location: String,
        /// The project configuration for each solution configuration.
        configs: Vec<String>,
    },
}

pub(crate) struct Project {
    pub(crate) name: String,
    pub(crate) guid: String,
    pub(crate) flavor: Flavor,
    /// Solution project type GUID, without braces.
    pub(crate) type_id: String,
    /// Platform of the project in the solution configurations.
    pub(crate) platform: String,
    pub(crate) dir: String,
    pub(crate) source: String,
    pub(crate) folder: String,
    pub(crate) kind: TargetKind,
    pub(crate) target: Option<TargetId>,
    pub(crate) utility: Option<UtilityKind>,
    pub(crate) dependencies: BTreeSet<usize>,
    pub(crate) in_all: Vec<bool>,
    pub(crate) build: Vec<bool>,
    pub(crate) deploy: Vec<bool>,
}

impl Project {
    /// The project file path. A generated project file is in the binary
    /// directory. An external location is returned as given.
    pub(crate) fn path(&self) -> String {
        match &self.flavor {
            Flavor::Native => join(&self.dir, &format!("{}.vcxproj", self.name)),
            Flavor::CSharp => join(&self.dir, &format!("{}.csproj", self.name)),
            Flavor::External { location, .. } => location.clone(),
        }
    }

    /// The project file path as a file in `dir` refers to it.
    ///
    /// CMake writes an external location without change, so a relative
    /// location stays relative.
    pub(crate) fn path_from(&self, dir: &str) -> String {
        match &self.flavor {
            Flavor::External { location, .. } => location.clone(),
            Flavor::Native | Flavor::CSharp => relative(dir, &self.path()),
        }
    }

    /// The project configuration that solution configuration `index` builds.
    pub(crate) fn config<'s>(&'s self, index: usize, solution: &'s str) -> &'s str {
        match &self.flavor {
            Flavor::External { configs, .. } => configs.get(index).map_or(solution, String::as_str),
            Flavor::Native | Flavor::CSharp => solution,
        }
    }

    /// Return `true` when the generator writes the project file.
    pub(crate) fn generated(&self) -> bool {
        !matches!(self.flavor, Flavor::External { .. })
    }
}

pub(crate) struct Solution {
    pub(crate) name: String,
    pub(crate) dir: String,
    members: BTreeSet<usize>,
    pub(crate) folders: BTreeMap<String, String>,
    /// Files of `VS_SOLUTION_ITEMS` for each solution folder.
    pub(crate) items: BTreeMap<String, BTreeSet<String>>,
    /// Sections from `VS_GLOBAL_SECTION_<PRE|POST>_<name>`, sorted by property name.
    pub(crate) sections: Vec<Section>,
    pub(crate) startup: Option<String>,
    pub(crate) guid: String,
    /// GUIDs of the projects that a member project depends on.
    depended_on: BTreeSet<String>,
}

/// One user global section of a solution.
pub(crate) struct Section {
    pub(crate) name: String,
    /// `true` for `postSolution`, `false` for `preSolution`.
    pub(crate) post: bool,
    /// The `key = value` pairs, with white space trimmed.
    pub(crate) entries: Vec<(String, String)>,
}

impl Solution {
    /// Tell if solution configuration `index` builds `project`.
    ///
    /// As in CMake, a utility project, such as a custom target or
    /// `ALL_BUILD`, builds only when another project of the solution depends
    /// on it.
    pub(crate) fn builds(&self, project: &Project, index: usize) -> bool {
        let utility = project.kind == TargetKind::Utility
            && matches!(
                project.utility,
                None | Some(UtilityKind::AllBuild | UtilityKind::ZeroCheck)
            );
        project.build.get(index) == Some(&true)
            && (!utility || self.depended_on.contains(&project.guid))
    }

    /// Return `true` when the user sections replace the default post-solution
    /// section `name`.
    pub(crate) fn overrides(&self, name: &str) -> bool {
        self.sections
            .iter()
            .any(|section| section.post && section.name == name)
    }
}

impl Solution {
    pub(crate) fn members<'a>(
        &'a self,
        projects: &'a [Project],
    ) -> impl Iterator<Item = &'a Project> {
        self.members.iter().filter_map(|&index| projects.get(index))
    }

    /// The solution members that `project` depends on, ordered by name as
    /// CMake orders them.
    pub(crate) fn dependencies<'a>(
        &'a self,
        projects: &'a [Project],
        project: &'a Project,
    ) -> impl Iterator<Item = &'a Project> {
        let mut dependencies = project
            .dependencies
            .intersection(&self.members)
            .filter_map(|&index| projects.get(index))
            .collect::<Vec<_>>();
        dependencies.sort_by(|a, b| a.name.cmp(&b.name));
        dependencies.into_iter()
    }
}

struct UtilityProjects {
    root: BTreeMap<String, usize>,
    all_build: BTreeMap<DirId, usize>,
}

pub(super) fn plan<'a>(
    version: VsVersion,
    result: &'a GenerationResult,
    model: &'a ProjectModel,
    host: &dyn Host,
    bin: &Path,
) -> Result<SolutionPlan<'a>, GenError> {
    let kind = GeneratorKind::VisualStudio(version);
    let settings = Settings::new(version, result, model, host, bin);
    validate_name(&settings.platform)?;
    for config in &settings.configs {
        validate_name(config)?;
    }
    let use_folders = use_folders(model);
    let mut resolved = ResolvedProjects::new();
    for target in &result.targets {
        resolved
            .entry(target.id)
            .or_default()
            .insert(string(&target.config.0), target);
    }
    let mut projects = target_projects(&settings, model, &resolved, use_folders)?;
    let indices = projects
        .iter()
        .enumerate()
        .filter_map(|(index, project)| project.target.map(|id| (id, index)))
        .collect::<TargetIndices>();
    let roots = project_roots(kind, model);
    let utilities = add_utility_projects(
        &mut projects,
        &settings,
        model,
        &roots,
        utility_targets(kind, model, host),
        use_folders,
    )?;
    let first_config_deps = first_config_dependencies(result);
    let scoped_targets: ScopedTargets = roots
        .iter()
        .map(|root| {
            let members = targets_for_root_indexed(model, &first_config_deps, root.dir)
                .into_iter()
                .filter_map(|id| indices.get(&id).copied())
                .collect::<BTreeSet<_>>();
            (root.dir, members)
        })
        .collect();
    let dependencies = target_dependencies(model, result, &resolved);
    connect_projects(
        &mut projects,
        model,
        &dependencies,
        &indices,
        &utilities,
        &scoped_targets,
    )?;
    let regenerate = utilities.root.contains_key("ZERO_CHECK");
    let mut inputs = regeneration_inputs(result, &settings);
    if regenerate && glob::write_verification(Some(model), bin, host)? {
        inputs.push(join(&settings.bin, glob::STAMP));
    }
    // One step budget limits all source group matches of the generation.
    let mut matcher = RegexMatcher::default();
    let solutions = roots
        .iter()
        .map(|root| {
            solution(
                root,
                model,
                host,
                &settings,
                &projects,
                &utilities,
                &scoped_targets,
                &mut matcher,
            )
        })
        .collect::<Result<_, _>>()?;
    let mut vs_indices = VsIndices::new(model, result, &projects);
    vs_indices.listfile_groups = listfile_groups(model, &settings, &mut matcher)?;
    Ok(SolutionPlan {
        settings,
        projects,
        resolved,
        indices: vs_indices,
        inputs,
        solutions,
        regenerate,
    })
}

fn target_projects(
    settings: &Settings,
    model: &ProjectModel,
    resolved: &ResolvedProjects<'_>,
    use_folders: bool,
) -> Result<Vec<Project>, GenError> {
    let mut projects = Vec::new();
    for (id, target) in model
        .targets()
        .filter(|(_, target)| model.is_in_build_system(target))
    {
        let dir = model.dir(target.dir);
        let name = string(&target.name);
        validate_name(&name)?;
        let facts = resolved.get(&id);
        let mut in_all = Vec::new();
        let mut build = Vec::new();
        let mut deploy = Vec::new();
        for config in &settings.configs {
            let excluded = excluded_from_all(model, target, config);
            let default = format!("EXCLUDE_FROM_DEFAULT_BUILD_{config}");
            let excluded_default = target
                .props
                .get(default.as_bytes())
                .or_else(|| target.props.get(default.to_ascii_uppercase().as_bytes()))
                .or_else(|| target.props.get(b"EXCLUDE_FROM_DEFAULT_BUILD"))
                .is_some_and(|value| truth::is_on(value));
            in_all.push(!excluded);
            build.push(!excluded && !excluded_default);
            deploy.push(needs_deploy(model, target, facts, config));
        }
        let folder = if use_folders {
            target
                .props
                .get(b"FOLDER")
                .or_else(|| {
                    facts.and_then(|facts| {
                        facts
                            .values()
                            .find_map(|fact| fact.metadata.folder.as_ref())
                    })
                })
                .map_or_else(String::new, |value| folder(&string(value)))
        } else {
            String::new()
        };
        let flavor = flavor(settings, target, facts);
        let type_id = match &flavor {
            Flavor::Native => project_type::DEFAULT.to_owned(),
            Flavor::CSharp => project_type::CSHARP.to_owned(),
            Flavor::External { location, .. } => target
                .props
                .get(b"VS_PROJECT_TYPE")
                .filter(|value| !value.is_empty())
                .map_or_else(
                    || project_type::external(location).to_owned(),
                    |value| string(value),
                ),
        };
        let platform = solution_platform(settings, target, &flavor);
        projects.push(Project {
            guid: guid::for_name(model, &settings.bin, &name),
            name,
            flavor,
            type_id,
            platform,
            dir: absolute(&settings.bin, &string(&dir.binary_dir)),
            source: absolute(&settings.source, &string(&dir.source_dir)),
            folder,
            kind: target.kind,
            target: Some(id),
            utility: None,
            dependencies: BTreeSet::new(),
            in_all,
            build,
            deploy,
        });
    }
    Ok(projects)
}

/// Select the project file kind for a target.
fn flavor(
    settings: &Settings,
    target: &Target,
    facts: Option<&BTreeMap<String, &ResolvedTarget>>,
) -> Flavor {
    if let Some(location) = target
        .props
        .get(b"EXTERNAL_MSPROJECT")
        .filter(|value| !value.is_empty())
    {
        let configs = settings
            .configs
            .iter()
            .map(|config| {
                let key = format!("MAP_IMPORTED_CONFIG_{}", config.to_ascii_uppercase());
                target
                    .props
                    .get(key.as_bytes())
                    .and_then(|value| {
                        list::split(value, list::Empty::Drop)
                            .first()
                            .map(|first| string(first))
                    })
                    .unwrap_or_else(|| config.clone())
            })
            .collect();
        return Flavor::External {
            location: string(location),
            configs,
        };
    }
    if csharp_only(target, facts) {
        Flavor::CSharp
    } else {
        Flavor::Native
    }
}

/// Return `true` when a target compiles C# and no other language.
///
/// This is CMake's rule for a `.csproj` project: an executable or a static or
/// shared library whose sources in all configurations, together with an
/// explicit `LINKER_LANGUAGE`, use exactly one language, `CSharp`.
pub(crate) fn csharp_only(
    target: &Target,
    facts: Option<&BTreeMap<String, &ResolvedTarget>>,
) -> bool {
    if !matches!(
        target.kind,
        TargetKind::Executable | TargetKind::StaticLibrary | TargetKind::SharedLibrary
    ) {
        return false;
    }
    let mut languages = BTreeSet::<&[u8]>::new();
    for fact in facts.into_iter().flat_map(BTreeMap::values) {
        if let Some(ide) = fact.ide.as_deref() {
            languages.extend(ide.sources.iter().filter_map(|source| match &source.kind {
                IdeSourceKind::Compile { language } if !language.is_empty() => {
                    Some(language.as_slice())
                }
                _ => None,
            }));
        }
        languages.extend(
            fact.compile_groups
                .iter()
                .map(|group| group.language.as_slice())
                .filter(|language| !language.is_empty()),
        );
    }
    if let Some(language) = target
        .props
        .get(b"LINKER_LANGUAGE")
        .filter(|value| !value.is_empty())
    {
        languages.insert(language.as_slice());
    }
    languages.len() == 1 && languages.contains(b"CSharp".as_slice())
}

/// Get the non-empty `DOTNET_SDK` of a target. A target with an SDK uses a
/// .NET SDK-style project.
pub(crate) fn dotnet_sdk(target: &Target) -> Option<&[u8]> {
    target
        .props
        .get(b"DOTNET_SDK")
        .filter(|value| !value.is_empty())
        .map(|value| value.as_slice())
}

/// Select the platform of a target project in the solution configurations.
///
/// An external project uses `VS_PLATFORM_MAPPING`. A .NET SDK-style project
/// uses `VS_GLOBAL_PlatformTarget`, or `Any CPU`. Other projects use the
/// solution platform.
fn solution_platform(settings: &Settings, target: &Target, flavor: &Flavor) -> String {
    let property = |key: &[u8]| {
        target
            .props
            .get(key)
            .filter(|value| !value.is_empty())
            .map(|value| string(value))
    };
    match flavor {
        Flavor::External { .. } => property(b"VS_PLATFORM_MAPPING"),
        Flavor::Native | Flavor::CSharp if dotnet_sdk(target).is_some() => {
            Some(property(b"VS_GLOBAL_PlatformTarget").unwrap_or_else(|| "Any CPU".into()))
        }
        Flavor::Native | Flavor::CSharp => None,
    }
    .unwrap_or_else(|| settings.platform.clone())
}

fn add_utility_projects(
    projects: &mut Vec<Project>,
    settings: &Settings,
    model: &ProjectModel,
    roots: &[IdeProject],
    utility_targets: Vec<UtilityTarget>,
    use_folders: bool,
) -> Result<UtilityProjects, GenError> {
    let mut utilities = BTreeMap::new();
    let mut usage: Option<UsageResolver<'_>> = None;
    for utility in utility_targets {
        let name = string(&utility.name);
        let index = projects.len();
        if projects
            .iter()
            .any(|project| project.name.eq_ignore_ascii_case(&name))
        {
            return Err(GenError::Invalid(
                format!("Visual Studio target name is reserved: {name}").into(),
            ));
        }
        utilities.insert(name.clone(), index);
        let folder = if use_folders {
            model
                .global
                .props
                .get(b"PREDEFINED_TARGETS_FOLDER")
                .map_or_else(
                    || "CMakePredefinedTargets".into(),
                    |value| folder(&string(value)),
                )
        } else {
            String::new()
        };
        projects.push(Project {
            guid: guid::for_name(model, &settings.bin, &name),
            name,
            flavor: Flavor::Native,
            type_id: project_type::DEFAULT.to_owned(),
            platform: settings.platform.clone(),
            dir: settings.bin.clone(),
            source: settings.source.clone(),
            folder,
            kind: TargetKind::Utility,
            target: None,
            utility: Some(utility.kind),
            dependencies: BTreeSet::new(),
            in_all: vec![true; settings.configs.len()],
            build: settings
                .configs
                .iter()
                .map(|config| default_build(model, &mut usage, utility.kind, config))
                .collect::<Result<_, _>>()?,
            deploy: vec![false; settings.configs.len()],
        });
    }

    let mut all_build = BTreeMap::new();
    if let Some(&top) = utilities.get("ALL_BUILD") {
        all_build.insert(DirId::ROOT, top);
        if let Some(project) = projects.get(top) {
            let guid = project.guid.clone();
            let folder = project.folder.clone();
            for root in roots.iter().filter(|root| root.dir != DirId::ROOT) {
                let dir = model.dir(root.dir);
                all_build.insert(root.dir, projects.len());
                projects.push(Project {
                    name: "ALL_BUILD".into(),
                    guid: guid.clone(),
                    flavor: Flavor::Native,
                    type_id: project_type::DEFAULT.to_owned(),
                    platform: settings.platform.clone(),
                    dir: absolute(&settings.bin, &string(&dir.binary_dir)),
                    source: absolute(&settings.source, &string(&dir.source_dir)),
                    folder: folder.clone(),
                    kind: TargetKind::Utility,
                    target: None,
                    utility: Some(UtilityKind::AllBuild),
                    dependencies: BTreeSet::new(),
                    in_all: vec![true; settings.configs.len()],
                    build: vec![true; settings.configs.len()],
                    deploy: vec![false; settings.configs.len()],
                });
            }
        }
    }
    Ok(UtilityProjects {
        root: utilities,
        all_build,
    })
}

fn target_dependencies(
    model: &ProjectModel,
    result: &GenerationResult,
    resolved: &ResolvedProjects<'_>,
) -> BTreeMap<TargetId, BTreeSet<TargetId>> {
    let graph_targets: Vec<HashMap<&[u8], &GraphTarget>> = result
        .graphs
        .iter()
        .map(|graph| {
            let mut map = HashMap::with_capacity(graph.targets.len());
            for entry in &graph.targets {
                map.entry(entry.name.as_slice()).or_insert(entry);
            }
            map
        })
        .collect();
    let mut dependencies = BTreeMap::<TargetId, BTreeSet<TargetId>>::new();
    for (id, target) in model.targets() {
        let deps = dependencies.entry(id).or_default();
        if let Some(facts) = resolved.get(&id) {
            deps.extend(
                facts
                    .values()
                    .flat_map(|fact| fact.dependencies.iter().copied()),
            );
            deps.extend(
                facts
                    .values()
                    .filter_map(|fact| fact.ide.as_ref())
                    .flat_map(|ide| &ide.link_items)
                    .filter_map(|item| {
                        let item = match item {
                            IdeLinkItem::VisualStudioFeature { item, .. } => item.as_ref(),
                            item => item,
                        };
                        if let IdeLinkItem::Target(id) = item {
                            Some(*id)
                        } else {
                            None
                        }
                    }),
            );
        }
        for (graph, targets) in result.graphs.iter().zip(&graph_targets) {
            if let Some(entry) = targets.get(target.name.as_slice()) {
                deps.extend(
                    entry
                        .depends
                        .iter()
                        .filter_map(|&index| graph.targets.get(index))
                        .filter_map(|entry| model.find_target(target.dir, &entry.name)),
                );
            }
        }
        deps.extend(
            target
                .utility_deps
                .iter()
                .filter_map(|entry| model.find_target(target.dir, &entry.value)),
        );
        for link in [
            &target.usage.link_libraries,
            &target.usage.interface_link_libraries,
        ]
        .into_iter()
        .flatten()
        .flatten()
        {
            if let Some(id) = model.find_target(link.lookup_dir, &link.text.value) {
                deps.insert(id);
            }
        }
    }
    dependencies
}

fn connect_projects(
    projects: &mut [Project],
    model: &ProjectModel,
    dependencies: &BTreeMap<TargetId, BTreeSet<TargetId>>,
    indices: &TargetIndices,
    utilities: &UtilityProjects,
    scoped_targets: &ScopedTargets,
) -> Result<(), GenError> {
    let aggregate_dependencies: BTreeMap<_, _> = utilities
        .all_build
        .iter()
        .map(|(root, &index)| {
            let deps = scoped_targets
                .get(root)
                .into_iter()
                .flatten()
                .copied()
                .filter(|&index| {
                    projects
                        .get(index)
                        .is_some_and(|project| project.in_all.contains(&true))
                })
                .collect::<BTreeSet<_>>();
            (index, deps)
        })
        .collect();

    let mut paths = BTreeSet::new();
    for (index, project) in projects.iter_mut().enumerate() {
        // External locations may be relative to the solution, so only the
        // generated files have a known path.
        if project.generated() && !paths.insert(project.path().to_ascii_lowercase()) {
            return Err(GenError::Invalid(
                format!("Duplicate Visual Studio project path: {}", project.path()).into(),
            ));
        }
        if let Some(id) = project.target {
            let mut pending: Vec<_> = dependencies
                .get(&id)
                .into_iter()
                .flatten()
                .copied()
                .collect();
            let mut visited = BTreeSet::from([id]);
            while let Some(dependency) = pending.pop() {
                if !visited.insert(dependency) {
                    continue;
                }
                if let Some(&index) = indices.get(&dependency) {
                    project.dependencies.insert(index);
                } else {
                    pending.extend(dependencies.get(&dependency).into_iter().flatten().copied());
                }
            }
        } else if let Some(deps) = aggregate_dependencies.get(&index) {
            project.dependencies.extend(deps);
        } else if matches!(
            project.utility,
            Some(UtilityKind::Install | UtilityKind::Package)
        ) && !(project.utility == Some(UtilityKind::Install)
            && model
                .definition(DirId::ROOT, b"CMAKE_SKIP_INSTALL_ALL_DEPENDENCY")
                .is_some_and(|value| truth::is_on(value)))
        {
            project.dependencies.extend(utilities.root.get("ALL_BUILD"));
        }
        if let Some(&zero) = utilities.root.get("ZERO_CHECK")
            && zero != index
        {
            project.dependencies.insert(zero);
        }
    }
    Ok(())
}

fn regeneration_inputs(result: &GenerationResult, settings: &Settings) -> Vec<String> {
    let mut inputs: BTreeSet<_> = rerun_inputs(&result.metadata)
        .iter()
        .map(|path| string(path))
        .collect();
    inputs.insert(join(&settings.source, "CMakeLists.txt"));
    inputs.into_iter().collect()
}

#[expect(
    clippy::too_many_arguments,
    reason = "A solution combines the projects, the utilities, and the shared regex budget."
)]
fn solution(
    ide: &IdeProject,
    model: &ProjectModel,
    host: &dyn Host,
    settings: &Settings,
    projects: &[Project],
    utilities: &UtilityProjects,
    scoped_targets: &ScopedTargets,
    matcher: &mut RegexMatcher,
) -> Result<Solution, GenError> {
    let name = string(&ide.name);
    let dir = absolute(&settings.bin, &string(&model.dir(ide.dir).binary_dir));
    validate_name(&name)?;
    let mut members = scoped_targets.get(&ide.dir).cloned().unwrap_or_default();
    if ide.dir == DirId::ROOT {
        members.extend(utilities.root.values().copied());
    } else {
        members.extend(utilities.all_build.get(&ide.dir));
        members.extend(utilities.root.get("ZERO_CHECK"));
    }
    let mut pending: Vec<_> = members.iter().copied().collect();
    while let Some(index) = pending.pop() {
        if let Some(project) = projects.get(index) {
            pending.extend(
                project
                    .dependencies
                    .iter()
                    .copied()
                    .filter(|dep| members.insert(*dep)),
            );
        }
    }
    let items = solution_items(model, ide.dir, matcher)?;
    let mut folders = BTreeMap::new();
    for folder in members
        .iter()
        .filter_map(|&index| projects.get(index))
        .map(|project| project.folder.as_str())
        .chain(items.keys().map(String::as_str))
    {
        let mut path = folder;
        while !path.is_empty() {
            folders.entry(path.to_owned()).or_insert_with(|| {
                guid::for_name(model, &settings.bin, &format!("CMAKE_FOLDER_GUID_{path}"))
            });
            path = path.rsplit_once('/').map_or("", |(parent, _)| parent);
        }
    }
    let guid = guid::for_name(model, &settings.bin, &format!("{name}.sln"));
    let mut ids = BTreeSet::from([guid.as_str()]);
    if folders
        .values()
        .map(String::as_str)
        .chain(
            members
                .iter()
                .filter_map(|&index| projects.get(index))
                .map(|project| project.guid.as_str()),
        )
        .any(|id| !ids.insert(id))
    {
        return Err(GenError::Invalid(
            format!("Duplicate GUID in Visual Studio solution {name}.").into(),
        ));
    }
    let depended_on = members
        .iter()
        .filter_map(|&index| projects.get(index))
        .flat_map(|project| &project.dependencies)
        .filter_map(|&index| projects.get(index))
        .map(|project| project.guid.clone())
        .collect();
    let requested = model
        .dir(ide.dir)
        .props
        .get(b"VS_STARTUP_PROJECT")
        .filter(|value| !value.is_empty())
        .map(|value| string(value));
    // As in CMake, a name that is not a target anywhere in the build gives a
    // warning. A target outside this solution is ignored without a warning.
    if let Some(name) = &requested
        && !projects.iter().any(|project| project.name == *name)
    {
        diag::emit_author_warning(
            host,
            model,
            ide.dir,
            format!(
                "Directory property VS_STARTUP_PROJECT specifies target '{name}' that does not exist.  Ignoring."
            ),
        );
    }
    let startup = requested
        .filter(|name| {
            members.iter().any(|&index| {
                projects
                    .get(index)
                    .is_some_and(|project| project.name == *name)
            })
        })
        .or_else(|| {
            utilities
                .all_build
                .get(&ide.dir)
                .map(|_| "ALL_BUILD".into())
        });
    Ok(Solution {
        name,
        dir,
        members,
        folders,
        items,
        sections: global_sections(model, ide.dir),
        startup,
        guid,
        depended_on,
    })
}

/// Read the user solution sections of directory `dir`, sorted by property
/// name as CMake sorts them.
///
/// A property `VS_GLOBAL_SECTION_PRE_<name>` or `VS_GLOBAL_SECTION_POST_<name>`
/// is a list of `key=value` pairs. An item without `=` is ignored.
fn global_sections(model: &ProjectModel, dir: DirId) -> Vec<Section> {
    let props = &model.dir(dir).props;
    let mut keys = props.names();
    keys.sort();
    keys.iter()
        .filter_map(|key| {
            let rest = key.strip_prefix(b"VS_GLOBAL_SECTION_")?;
            let (post, name) = if let Some(name) = rest.strip_prefix(b"PRE_") {
                (false, name)
            } else {
                (true, rest.strip_prefix(b"POST_")?)
            };
            if name.is_empty() {
                return None;
            }
            let entries = props
                .get(key)
                .map(|value| list::split(value, list::Empty::Drop))
                .unwrap_or_default()
                .iter()
                .filter_map(|pair| {
                    let pair = string(pair);
                    let (key, value) = pair.split_once('=')?;
                    Some((key.trim().to_owned(), value.trim().to_owned()))
                })
                .collect();
            Some(Section {
                name: string(name),
                post,
                entries,
            })
        })
        .collect()
}

/// Group the `VS_SOLUTION_ITEMS` files of directory `dir` in solution folders.
///
/// As in CMake, a relative item is relative to the source directory. The
/// source group of the file names the folder. A file in no named group goes
/// in the folder `Solution Items`. The folder uses `/` between levels.
///
/// # Errors
///
/// Returns an error when the regular expressions use up the step budget.
fn solution_items(
    model: &ProjectModel,
    dir: DirId,
    matcher: &mut RegexMatcher,
) -> Result<BTreeMap<String, BTreeSet<String>>, GenError> {
    let directory = model.dir(dir);
    let mut items = BTreeMap::<String, BTreeSet<String>>::new();
    let Some(value) = directory.props.get(b"VS_SOLUTION_ITEMS") else {
        return Ok(items);
    };
    let source = string(&directory.source_dir);
    for item in list::split(value, list::Empty::Drop) {
        let path = absolute(&source, &string(&item));
        let group = rcmake_resolve::ide::find_source_group(
            directory,
            path.as_bytes(),
            &mut |pattern, path| matcher.matches(pattern, path),
        );
        let group = folder(&string(&group));
        let group = if group.is_empty() {
            "Solution Items".to_owned()
        } else {
            group
        };
        items.entry(group).or_default().insert(path);
    }
    matcher.check()?;
    Ok(items)
}

/// Find the source group of the `CMakeLists.txt` file of each directory,
/// once for each directory. The project files of a directory use the group
/// for the custom build item of the file.
///
/// # Errors
///
/// Returns an error when the regular expressions use up the step budget.
fn listfile_groups(
    model: &ProjectModel,
    settings: &Settings,
    matcher: &mut RegexMatcher,
) -> Result<HashMap<DirId, String>, GenError> {
    let mut groups = HashMap::new();
    for (id, dir) in model.dirs() {
        let path = join(
            &absolute(&settings.source, &string(&dir.source_dir)),
            "CMakeLists.txt",
        );
        let group =
            rcmake_resolve::ide::find_source_group(dir, path.as_bytes(), &mut |pattern, text| {
                matcher.matches(pattern, text)
            });
        matcher.check()?;
        groups.insert(id, crate::windows(&string(&group)));
    }
    Ok(groups)
}

/// Match CMake regular expressions with a bounded number of steps.
///
/// The step budget is shared by all matches. When it is used up, the
/// matches stop and [`Self::check`] gives an error, so a large or slow
/// pattern does not silently put a file in the wrong group.
pub(crate) struct RegexMatcher {
    compiled: HashMap<Vec<u8>, Option<rcmake_regex::Regex>>,
    scratch: rcmake_regex::Scratch,
    budget: u64,
    exhausted: bool,
}

impl Default for RegexMatcher {
    fn default() -> Self {
        Self::with_budget(rcmake_base::limits::Limits::default().regex_steps)
    }
}

impl RegexMatcher {
    /// Make a matcher with a budget of `budget` steps.
    pub(crate) fn with_budget(budget: u64) -> Self {
        Self {
            compiled: HashMap::new(),
            scratch: rcmake_regex::Scratch::default(),
            budget,
            exhausted: false,
        }
    }

    /// Return `true` when `pattern` matches a part of `text`. An invalid
    /// pattern does not match. After the budget is used up, nothing matches.
    pub(crate) fn matches(&mut self, pattern: &[u8], text: &[u8]) -> bool {
        if self.exhausted {
            return false;
        }
        let regex = self
            .compiled
            .entry(pattern.to_vec())
            .or_insert_with(|| rcmake_regex::Regex::new(pattern).ok());
        let Some(regex) = regex else {
            return false;
        };
        match regex.find_at_with_scratch(
            text,
            0,
            rcmake_regex::MatchOptions::default(),
            &mut self.budget,
            &mut self.scratch,
        ) {
            Ok(found) => found.is_some(),
            Err(_) => {
                self.exhausted = true;
                false
            }
        }
    }

    /// Give an error when a match used up the step budget.
    ///
    /// # Errors
    ///
    /// Returns [`GenError::Invalid`] after the budget is used up.
    pub(crate) fn check(&self) -> Result<(), GenError> {
        if self.exhausted {
            return Err(GenError::Invalid(
                "The regular expressions of source_group() used up the step limit of the Visual Studio generator."
                    .into(),
            ));
        }
        Ok(())
    }
}

/// Tell if solution configuration `config` builds a project that CMake adds.
///
/// `ZERO_CHECK` and `ALL_BUILD` build when a project depends on them (see
/// [`Solution::builds`]). As in CMake, `INSTALL` and `PACKAGE` build when the
/// top-level `CMAKE_VS_INCLUDE_INSTALL_TO_DEFAULT_BUILD` or
/// `CMAKE_VS_INCLUDE_PACKAGE_TO_DEFAULT_BUILD` is true for the
/// configuration. The value can use generator expressions, such as
/// `$<CONFIG:Release>`.
///
/// # Errors
///
/// Returns an error when the generator expression is invalid.
fn default_build<'m>(
    model: &'m ProjectModel,
    usage: &mut Option<UsageResolver<'m>>,
    kind: UtilityKind,
    config: &str,
) -> Result<bool, GenError> {
    let key: &[u8] = match kind {
        UtilityKind::ZeroCheck | UtilityKind::AllBuild => return Ok(true),
        UtilityKind::Install => b"CMAKE_VS_INCLUDE_INSTALL_TO_DEFAULT_BUILD",
        UtilityKind::Package => b"CMAKE_VS_INCLUDE_PACKAGE_TO_DEFAULT_BUILD",
        _ => return Ok(false),
    };
    let Some(value) = model.definition(DirId::ROOT, key) else {
        return Ok(false);
    };
    if !value.windows(2).any(|pair| pair == b"$<") {
        return Ok(truth::is_on(value));
    }
    // The resolver evaluates an expression for a target. CMake evaluates this
    // variable without a target, and a configuration or boolean expression
    // gives the same value for each target.
    let Some((id, _)) = model
        .targets()
        .find(|(_, target)| model.is_in_build_system(target))
    else {
        return Ok(truth::is_on(value));
    };
    let evaluated = usage
        .get_or_insert_with(|| UsageResolver::new(model))
        .target_toolchain_property(id, config.as_bytes(), b"", b"", Some(value))
        .map_err(|error| GenError::Invalid(error.to_string().into()))?
        .unwrap_or_default();
    Ok(truth::is_on(&evaluated))
}

/// Tell if the solutions group projects in folders, as CMake does.
///
/// A `USE_FOLDERS` global property decides when it is set. Otherwise policy
/// `CMP0143` of the top-level directory decides: `NEW` uses folders.
fn use_folders(model: &ProjectModel) -> bool {
    if let Some(value) = model.global.props.get(b"USE_FOLDERS") {
        return truth::is_on(value);
    }
    model
        .dir(DirId::ROOT)
        .final_policies
        .as_ref()
        .is_some_and(|policies| policies.get(PolicyId::new(143)) == PolicyStatus::New)
}

/// Tell if solution configuration `config` deploys the project of `target`.
///
/// As in CMake, only an executable, a shared library, or a utility deploys.
/// `VS_SOLUTION_DEPLOY`, evaluated for the configuration, decides when it is
/// set. Otherwise a true `VS_NO_SOLUTION_DEPLOY` turns deployment off, and the
/// target system decides: Windows CE, Windows Phone, and Windows Store deploy.
fn needs_deploy(
    model: &ProjectModel,
    target: &Target,
    facts: Option<&BTreeMap<String, &ResolvedTarget>>,
    config: &str,
) -> bool {
    if !matches!(
        target.kind,
        TargetKind::Executable | TargetKind::SharedLibrary | TargetKind::Utility
    ) {
        return false;
    }
    let property = |key: &[u8]| {
        let raw = target.props.get(key)?;
        Some(
            facts
                .and_then(|facts| facts.get(config))
                .and_then(|fact| fact.ide.as_deref())
                .and_then(|ide| ide.attributes.get(key))
                .unwrap_or(raw),
        )
    };
    if let Some(value) = property(b"VS_SOLUTION_DEPLOY") {
        return truth::is_on(value);
    }
    if property(b"VS_NO_SOLUTION_DEPLOY").is_some_and(|value| truth::is_on(value)) {
        return false;
    }
    model
        .definition(DirId::ROOT, b"CMAKE_SYSTEM_NAME")
        .is_some_and(|name| {
            matches!(
                name.as_slice(),
                b"WindowsCE" | b"WindowsPhone" | b"WindowsStore"
            )
        })
}

fn excluded_from_all(model: &ProjectModel, target: &Target, config: &str) -> bool {
    let key = format!("EXCLUDE_FROM_ALL_{}", config.to_ascii_uppercase());
    if let Some(value) = target
        .props
        .get(key.as_bytes())
        .or_else(|| target.props.get(b"EXCLUDE_FROM_ALL"))
    {
        return truth::is_on(value);
    }
    let mut dir = target.dir;
    loop {
        let entry = model.dir(dir);
        if entry.exclude_from_all {
            return true;
        }
        match entry.parent {
            Some(parent) if parent < dir => dir = parent,
            _ => return false,
        }
    }
}

fn validate_name(name: &str) -> Result<(), GenError> {
    if name.is_empty()
        || name.contains(" = ")
        || matches!(name, "." | "..")
        || name.chars().any(|ch| {
            ch.is_control() || matches!(ch, '/' | '\\' | ':' | '"' | '<' | '>' | '|' | '?' | '*')
        })
    {
        return Err(GenError::Invalid(
            format!("Invalid Visual Studio name: {name}").into(),
        ));
    }
    Ok(())
}
