//! Visual Studio 17 and 18 solution and project generation.
//!
//! The command-line generator registry controls host availability. The writers
//! can also run directly for structural tests on other hosts.

#![forbid(unsafe_code)]

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use rcmake_base::{BString, list, path_lex::Lexical, truth};
use rcmake_gen_common::{
    GenError,
    ide::{UtilityKind, project_roots, rerun_inputs, targets_for_root, utility_targets},
    write_if_changed,
};
use rcmake_host::{Host, path::from_native};
use rcmake_model::{
    DirId, GeneratorKind, ParsedToolset, ProjectModel, TargetId, TargetKind, VsVersion,
};
use rcmake_resolve::{GenerationResult, ResolvedTarget};

pub mod build;
pub mod filters;
pub mod flags_table;
pub mod guid;
pub mod rules;
pub mod sln;
pub mod slnx;
pub mod vcxproj;
pub mod xml;

pub use build::build_commands;

const CPP_TYPE: &str = "{8BC9CEB8-8B4A-11D0-8D11-00A0C91BC942}";
const MSBUILD_NS: &str = "http://schemas.microsoft.com/developer/msbuild/2003";

pub(crate) struct Settings {
    version: VsVersion,
    bin: String,
    source: String,
    configs: Vec<String>,
    platform: String,
    sdk: Option<String>,
    toolset: String,
    tool_host: Option<String>,
    tool_version: Option<String>,
    cmake: String,
    ctest: String,
    cpack: String,
}

pub(crate) struct Project {
    name: String,
    guid: String,
    dir: String,
    source: String,
    folder: String,
    kind: TargetKind,
    target: Option<TargetId>,
    utility: Option<UtilityKind>,
    dependencies: BTreeSet<usize>,
    in_all: Vec<bool>,
    build: Vec<bool>,
    deploy: Vec<bool>,
}

impl Project {
    fn path(&self) -> String {
        join(&self.dir, &format!("{}.vcxproj", self.name))
    }
}

pub(crate) struct Solution<'a> {
    name: &'a str,
    dir: &'a str,
    settings: &'a Settings,
    projects: &'a [Project],
    members: BTreeSet<usize>,
    folders: BTreeMap<String, String>,
    startup: Option<String>,
    guid: String,
}

impl Solution<'_> {
    fn members(&self) -> impl Iterator<Item = &Project> {
        self.members
            .iter()
            .filter_map(|&index| self.projects.get(index))
    }

    fn dependencies<'a>(&'a self, project: &'a Project) -> impl Iterator<Item = &'a Project> {
        project
            .dependencies
            .intersection(&self.members)
            .filter_map(|&index| self.projects.get(index))
    }
}

/// Write the Visual Studio solutions, projects, and filters into the build tree.
///
/// # Errors
///
/// Returns an error if the project data is invalid or the host cannot write a file.
pub fn generate(
    version: VsVersion,
    result: &GenerationResult,
    model: &ProjectModel,
    host: &dyn Host,
    bin: &Path,
) -> Result<(), GenError> {
    let kind = GeneratorKind::VisualStudio(version);
    let settings = Settings::new(version, result, model, bin);
    validate_name(&settings.platform)?;
    for config in &settings.configs {
        validate_name(config)?;
    }
    let use_folders = model
        .global
        .props
        .get(b"USE_FOLDERS")
        .is_none_or(|value| !truth::is_off(value));
    let mut resolved = BTreeMap::<TargetId, BTreeMap<String, &ResolvedTarget>>::new();
    for target in &result.targets {
        resolved
            .entry(target.id)
            .or_default()
            .insert(string(&target.config.0), target);
    }
    let mut indices = BTreeMap::new();
    let mut projects = Vec::new();

    for (id, target) in model.targets().filter(|(_, target)| !target.is_imported()) {
        let dir = model.dir(target.dir);
        let name = string(&target.name);
        validate_name(&name)?;
        let facts = resolved.get(&id);
        if target.kind == TargetKind::InterfaceLibrary
            && target.sources.is_empty()
            && target.file_sets.is_empty()
            && facts.is_none_or(|facts| {
                facts.values().all(|fact| {
                    fact.ide
                        .as_ref()
                        .is_none_or(|ide| ide.sources.is_empty() && ide.custom.is_empty())
                })
            })
        {
            continue;
        }
        let mut in_all = Vec::new();
        let mut build = Vec::new();
        let mut deploy = Vec::new();
        for config in &settings.configs {
            let fact = facts.and_then(|facts| facts.get(config)).copied();
            let excluded = fact.map_or_else(
                || excluded_from_all(model, target, config),
                |fact| fact.metadata.exclude_from_all,
            );
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
        indices.insert(id, projects.len());
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

    let roots = project_roots(kind, model);
    let mut utilities = BTreeMap::new();
    for utility in utility_targets(kind, model, host) {
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
    let scoped_targets: BTreeMap<_, _> = roots
        .iter()
        .map(|root| {
            let members = targets_for_root(model, result, root.dir)
                .into_iter()
                .filter_map(|id| indices.get(&id).copied())
                .collect::<BTreeSet<_>>();
            (root.dir, members)
        })
        .collect();
    let aggregate_dependencies: BTreeMap<_, _> = all_build
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
                    .filter_map(|item| match item {
                        rcmake_resolve::ide::IdeLinkItem::Target(id) => Some(*id),
                        _ => None,
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
            project.dependencies.extend(utilities.get("ALL_BUILD"));
        }
        if let Some(&zero) = utilities.get("ZERO_CHECK")
            && zero != index
        {
            project.dependencies.insert(zero);
        }
    }

    let mut inputs: Vec<_> = rerun_inputs(&result.metadata)
        .iter()
        .map(|path| string(path))
        .collect();
    let listfile = join(&settings.source, "CMakeLists.txt");
    if !inputs.contains(&listfile) {
        inputs.insert(0, listfile);
    }
    let inputs = inputs
        .into_iter()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    let mut solutions = Vec::new();
    let mut solution_dirs = Vec::new();
    for ide in &roots {
        solution_dirs.push(absolute(
            &settings.bin,
            &string(&model.dir(ide.dir).binary_dir),
        ));
    }
    let solution_names = roots
        .iter()
        .map(|ide| string(&ide.name))
        .collect::<Vec<_>>();
    for ((ide, dir), name) in roots.iter().zip(&solution_dirs).zip(&solution_names) {
        validate_name(name)?;
        let mut members = scoped_targets.get(&ide.dir).cloned().unwrap_or_default();
        if ide.dir == DirId::ROOT {
            members.extend(utilities.values().copied());
        } else {
            members.extend(all_build.get(&ide.dir));
            members.extend(utilities.get("ZERO_CHECK"));
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
            .map(|value| string(value));
        solutions.push(Solution {
            name,
            dir,
            settings: &settings,
            projects: &projects,
            members,
            folders,
            startup,
            guid,
        });
    }

    for project in &projects {
        let facts = project.target.and_then(|id| resolved.get(&id));
        let context = vcxproj::Context {
            project,
            projects: &projects,
            settings: &settings,
            facts,
            result,
            model,
            inputs: &inputs,
        };
        let plan = context.plan();
        for item in plan.items.values().filter(|item| item.synthetic) {
            write(host, &item.path, b"# MSBuild custom command\r\n")?;
        }
        let path = project.path();
        write(host, &path, vcxproj::render(&context, &plan).as_bytes())?;
        write(
            host,
            &format!("{path}.filters"),
            filters::render(&context, &plan).as_bytes(),
        )?;
    }
    if utilities.contains_key("ZERO_CHECK") {
        let list = format!("{}\n", inputs.join("\n"));
        write(
            host,
            &join(&settings.bin, "CMakeFiles/generate.stamp.depend"),
            list.as_bytes(),
        )?;
        write(
            host,
            &join(&settings.bin, "CMakeFiles/generate.stamp"),
            b"# CMake generation stamp\n",
        )?;
    }
    for solution in &solutions {
        let data = if version.writes_slnx() {
            slnx::render(solution)
        } else {
            sln::render(solution)
        };
        write(
            host,
            &join(
                solution.dir,
                &format!("{}.{}", solution.name, solution_ext(version)),
            ),
            data.as_bytes(),
        )?;
    }
    Ok(())
}

fn excluded_from_all(model: &ProjectModel, target: &rcmake_model::Target, config: &str) -> bool {
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

impl Settings {
    fn new(
        version: VsVersion,
        result: &GenerationResult,
        model: &ProjectModel,
        bin: &Path,
    ) -> Self {
        let root = model.dir(DirId::ROOT);
        let var = |key: &[u8]| {
            model
                .definition(DirId::ROOT, key)
                .filter(|value| !value.is_empty())
        };
        let platform = model.generator_spec.parsed_platform.as_ref();
        let toolset = match &model.generator_spec.parsed {
            ParsedToolset::Vs(parsed) => Some(parsed),
            _ => None,
        };
        let platform_name = platform
            .and_then(|p| p.name.as_ref())
            .or_else(|| var(b"CMAKE_VS_PLATFORM_NAME"))
            .map_or_else(|| default_platform().into(), |value| string(value));
        let mut configs = Vec::new();
        let mut seen = BTreeSet::new();
        for value in result
            .configs
            .iter()
            .map(|config| &config.0)
            .chain(result.graphs.iter().map(|graph| &graph.config.0))
            .chain(result.targets.iter().map(|target| &target.config.0))
        {
            let value = string(value);
            if !value.is_empty() && seen.insert(value.to_ascii_lowercase()) {
                configs.push(value);
            }
        }
        if configs.is_empty() {
            configs = var(b"CMAKE_CONFIGURATION_TYPES")
                .map(|value| {
                    list::split(value, list::Empty::Drop)
                        .iter()
                        .map(|value| string(value))
                        .collect()
                })
                .unwrap_or_else(|| {
                    ["Debug", "Release", "MinSizeRel", "RelWithDebInfo"]
                        .map(str::to_owned)
                        .to_vec()
                });
        }
        let mut unique = BTreeSet::new();
        configs.retain(|name| unique.insert(name.to_ascii_lowercase()));
        let bin = string(&from_native(bin));
        let executable = |primary: Option<&BString>, key: &[u8], default: &str| {
            primary
                .filter(|path| !path.is_empty())
                .or_else(|| var(key))
                .map_or_else(|| default.into(), |path| string(path))
        };
        Self {
            version,
            source: absolute(&bin, &string(&root.source_dir)),
            bin,
            configs,
            platform: platform_name,
            sdk: platform
                .and_then(|p| p.version.as_ref())
                .or_else(|| var(b"CMAKE_VS_WINDOWS_TARGET_PLATFORM_VERSION"))
                .filter(|v| v.as_slice() != b"none")
                .map(|value| string(value)),
            toolset: toolset
                .and_then(|t| t.name.as_ref())
                .or_else(|| var(b"CMAKE_VS_PLATFORM_TOOLSET"))
                .map_or_else(|| version.default_toolset().into(), |value| string(value)),
            tool_host: toolset
                .and_then(|t| t.host.as_ref())
                .or_else(|| var(b"CMAKE_VS_PLATFORM_TOOLSET_HOST_ARCHITECTURE"))
                .map(|value| string(value))
                .or_else(|| cfg!(target_arch = "x86_64").then(|| "x64".into())),
            tool_version: toolset
                .and_then(|t| t.version.as_ref())
                .or_else(|| var(b"CMAKE_VS_PLATFORM_TOOLSET_VERSION"))
                .map(|value| string(value)),
            cmake: executable(
                result.metadata.cmake_command.as_ref(),
                b"CMAKE_COMMAND",
                "cmake",
            ),
            ctest: executable(
                result.metadata.ctest_command.as_ref(),
                b"CMAKE_CTEST_COMMAND",
                "ctest",
            ),
            cpack: executable(None, b"CMAKE_CPACK_COMMAND", "cpack"),
        }
    }

    fn condition(&self, config: &str) -> String {
        format!(
            "'$(Configuration)|$(Platform)'=='{}|{}'",
            msbuild(config),
            msbuild(&self.platform)
        )
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

fn write(host: &dyn Host, path: &str, data: &[u8]) -> Result<(), GenError> {
    let path = rcmake_host::path::to_native(path.as_bytes());
    if let Some(dir) = path.parent() {
        host.create_dir_all(dir).map_err(|err| GenError::Write {
            path: from_native(dir),
            message: err.to_string().into(),
        })?;
    }
    write_if_changed(host, &path, data)
}

fn string(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

fn windows(path: &str) -> String {
    path.replace('/', "\\")
}

fn join(dir: &str, leaf: &str) -> String {
    if dir.is_empty() {
        leaf.into()
    } else {
        format!("{}/{leaf}", dir.trim_end_matches('/'))
    }
}

fn absolute(base: &str, path: &str) -> String {
    let path = path.replace('\\', "/");
    if path.is_empty() {
        base.into()
    } else if path.starts_with('/') || path.as_bytes().get(1) == Some(&b':') {
        path
    } else {
        string(&Lexical::new(join(base, &path).as_bytes()).normalize())
    }
}

fn relative(base: &str, path: &str) -> String {
    Lexical::new(path.as_bytes())
        .relative(Lexical::new(base.as_bytes()))
        .map_or_else(|| path.into(), |result| string(&result))
}

fn folder(path: &str) -> String {
    path.split(['/', '\\'])
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("/")
}

fn msbuild(value: &str) -> String {
    use std::fmt::Write;
    let mut out = String::new();
    for ch in value.chars() {
        if matches!(ch, '%' | '$' | '@' | '(' | ')' | '\'' | ';' | '?' | '*') {
            let _ = write!(out, "%{:02X}", ch as u32);
        } else {
            out.push(ch);
        }
    }
    out
}

fn default_platform() -> &'static str {
    if cfg!(all(windows, target_arch = "aarch64")) {
        "ARM64"
    } else if cfg!(all(windows, target_arch = "x86")) {
        "Win32"
    } else {
        "x64"
    }
}

fn solution_ext(version: VsVersion) -> &'static str {
    if version.writes_slnx() { "slnx" } else { "sln" }
}

#[cfg(test)]
mod tests;
