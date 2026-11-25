use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use rcmake_base::truth;
use rcmake_gen_common::{
    GenError, glob,
    ide::{
        IdeProject, UtilityKind, UtilityTarget, project_roots, rerun_inputs, targets_for_root,
        utility_targets,
    },
};
use rcmake_host::Host;
use rcmake_model::{DirId, GeneratorKind, ProjectModel, Target, TargetId, TargetKind, VsVersion};
use rcmake_resolve::{GenerationResult, ResolvedTarget, ide::IdeLinkItem};

use crate::{Settings, absolute, folder, guid, join, string};

type ResolvedProjects<'a> = BTreeMap<TargetId, BTreeMap<String, &'a ResolvedTarget>>;
type TargetIndices = BTreeMap<TargetId, usize>;
type ScopedTargets = BTreeMap<DirId, BTreeSet<usize>>;

pub(super) struct SolutionPlan<'a> {
    pub(super) settings: Settings,
    pub(super) projects: Vec<Project>,
    pub(super) resolved: ResolvedProjects<'a>,
    pub(super) inputs: Vec<String>,
    pub(super) solutions: Vec<Solution>,
    pub(super) regenerate: bool,
}

pub(crate) struct Project {
    pub(crate) name: String,
    pub(crate) guid: String,
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
    pub(crate) fn path(&self) -> String {
        join(&self.dir, &format!("{}.vcxproj", self.name))
    }
}

pub(crate) struct Solution {
    pub(crate) name: String,
    pub(crate) dir: String,
    members: BTreeSet<usize>,
    pub(crate) folders: BTreeMap<String, String>,
    pub(crate) startup: Option<String>,
    pub(crate) guid: String,
}

impl Solution {
    pub(crate) fn members<'a>(
        &'a self,
        projects: &'a [Project],
    ) -> impl Iterator<Item = &'a Project> {
        self.members.iter().filter_map(|&index| projects.get(index))
    }

    pub(crate) fn dependencies<'a>(
        &'a self,
        projects: &'a [Project],
        project: &'a Project,
    ) -> impl Iterator<Item = &'a Project> {
        project
            .dependencies
            .intersection(&self.members)
            .filter_map(|&index| projects.get(index))
    }
}

struct UtilityProjects {
    root: BTreeMap<String, usize>,
    all_build: BTreeMap<DirId, usize>,
}

pub(super) fn plan<'a>(
    version: VsVersion,
    result: &'a GenerationResult,
    model: &ProjectModel,
    host: &dyn Host,
    bin: &Path,
) -> Result<SolutionPlan<'a>, GenError> {
    let kind = GeneratorKind::VisualStudio(version);
    let settings = Settings::new(version, result, model, host, bin);
    validate_name(&settings.platform)?;
    for config in &settings.configs {
        validate_name(config)?;
    }
    let use_folders = model
        .global
        .props
        .get(b"USE_FOLDERS")
        .is_none_or(|value| !truth::is_off(value));
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
    let scoped_targets: ScopedTargets = roots
        .iter()
        .map(|root| {
            let members = targets_for_root(model, result, root.dir)
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
    let solutions = roots
        .iter()
        .map(|root| {
            solution(
                root,
                model,
                &settings,
                &projects,
                &utilities,
                &scoped_targets,
            )
        })
        .collect::<Result<_, _>>()?;
    Ok(SolutionPlan {
        settings,
        projects,
        resolved,
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
            deploy.push(
                target
                    .props
                    .get(b"VS_DEPLOYMENT_ENABLED")
                    .is_some_and(|value| truth::is_on(value)),
            );
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
        projects.push(Project {
            guid: guid::for_name(model, &settings.bin, &name),
            name,
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

fn add_utility_projects(
    projects: &mut Vec<Project>,
    settings: &Settings,
    model: &ProjectModel,
    roots: &[IdeProject],
    utility_targets: Vec<UtilityTarget>,
    use_folders: bool,
) -> Result<UtilityProjects, GenError> {
    let mut utilities = BTreeMap::new();
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
            dir: settings.bin.clone(),
            source: settings.source.clone(),
            folder,
            kind: TargetKind::Utility,
            target: None,
            utility: Some(utility.kind),
            dependencies: BTreeSet::new(),
            in_all: vec![true; settings.configs.len()],
            build: vec![utility.kind == UtilityKind::ZeroCheck; settings.configs.len()],
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
                    dir: absolute(&settings.bin, &string(&dir.binary_dir)),
                    source: absolute(&settings.source, &string(&dir.source_dir)),
                    folder: folder.clone(),
                    kind: TargetKind::Utility,
                    target: None,
                    utility: Some(UtilityKind::AllBuild),
                    dependencies: BTreeSet::new(),
                    in_all: vec![true; settings.configs.len()],
                    build: vec![false; settings.configs.len()],
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
        for graph in &result.graphs {
            if let Some(entry) = graph.targets.iter().find(|entry| entry.name == target.name) {
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
        if !paths.insert(project.path().to_ascii_lowercase()) {
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

fn solution(
    ide: &IdeProject,
    model: &ProjectModel,
    settings: &Settings,
    projects: &[Project],
    utilities: &UtilityProjects,
    scoped_targets: &ScopedTargets,
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
    let mut folders = BTreeMap::new();
    for project in members.iter().filter_map(|&index| projects.get(index)) {
        let mut path = project.folder.as_str();
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
    let startup = model
        .dir(ide.dir)
        .props
        .get(b"VS_STARTUP_PROJECT")
        .map(|value| string(value))
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
        startup,
        guid,
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
