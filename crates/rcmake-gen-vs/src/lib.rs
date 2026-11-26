//! Visual Studio 17 and 18 solution and project generation.
//!
//! The command-line generator registry controls host availability. The writers
//! can also run directly for structural tests on other hosts.

#![forbid(unsafe_code)]

use std::collections::{BTreeSet, HashSet};
use std::path::Path;

use rcmake_base::{BString, list, path_lex::Lexical};
use rcmake_gen_common::{GenError, write_output};
use rcmake_host::{FileKind, Follow, Host, SystemInfo, path::from_native};
use rcmake_model::{DirId, ParsedToolset, ProjectModel, Target, VsVersion};
use rcmake_resolve::{GenerationResult, usage::UsageResolver};

use plan::Project;

pub mod build;
mod cuda;
pub mod filters;
pub mod flags_table;
pub mod guid;
mod plan;
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
    cuda_version: Option<String>,
    cuda_toolkit: Option<String>,
    cuda_integration: Option<String>,
    nasm_templates: Option<String>,
    cmake: String,
    ctest: String,
    ctest_arguments: Vec<String>,
    cpack: String,
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
    generate_with_restore_requirements(version, result, model, host, bin).map(drop)
}

/// Write the Visual Studio files and return the names of the projects that
/// need a NuGet package restore before a build.
///
/// A project needs a restore when its target or a direct project dependency
/// has `VS_PACKAGE_REFERENCES`. The caller stores
/// `<name>_REQUIRES_VS_PACKAGE_RESTORE` in the cache for each returned name.
///
/// # Errors
///
/// Returns an error if the project data is invalid or the host cannot write a file.
pub fn generate_with_restore_requirements(
    version: VsVersion,
    result: &GenerationResult,
    model: &ProjectModel,
    host: &dyn Host,
    bin: &Path,
) -> Result<Vec<String>, GenError> {
    let plan = plan::plan(version, result, model, host, bin)?;
    let mut usage: Option<UsageResolver<'_>> = None;
    let mut nasm_props_template: Option<String> = None;
    for project in &plan.projects {
        let facts = project.target.and_then(|id| plan.resolved.get(&id));
        let context = vcxproj::Context {
            project,
            projects: &plan.projects,
            settings: &plan.settings,
            facts,
            indices: &plan.indices,
            model,
            inputs: &plan.inputs,
        };
        write_project(host, &context, &mut usage, &mut nasm_props_template)?;
    }
    if plan.regenerate {
        let list = format!("{}\n", plan.inputs.join("\n"));
        write(
            host,
            &join(&plan.settings.bin, "CMakeFiles/generate.stamp.depend"),
            list.as_bytes(),
        )?;
        write(
            host,
            &join(&plan.settings.bin, "CMakeFiles/generate.stamp"),
            b"# CMake generation stamp\n",
        )?;
    }
    for solution in &plan.solutions {
        let data = if version.writes_slnx() {
            slnx::render(solution, &plan.settings, &plan.projects)
        } else {
            sln::render(solution, &plan.settings, &plan.projects)
        };
        write(
            host,
            &join(
                &solution.dir,
                &format!("{}.{}", solution.name, solution_ext(version)),
            ),
            data.as_bytes(),
        )?;
    }
    Ok(restore_requirements(&plan.projects, model))
}

/// Target property that lists NuGet packages as `<name>_<version>` items.
pub(crate) const PACKAGE_REFERENCES: &[u8] = b"VS_PACKAGE_REFERENCES";

/// Return `true` when a target declares NuGet package references.
pub(crate) fn has_package_references(target: &Target) -> bool {
    target
        .props
        .get(PACKAGE_REFERENCES)
        .is_some_and(|value| !value.is_empty())
}

/// Names of the projects whose target, or any transitive project dependency of
/// it, has package references.
fn restore_requirements(projects: &[Project], model: &ProjectModel) -> Vec<String> {
    let mut required = vec![false; projects.len()];
    let mut dependents: Vec<Vec<usize>> = vec![Vec::new(); projects.len()];
    let mut pending = Vec::new();
    for (index, project) in projects.iter().enumerate() {
        if project
            .target
            .and_then(|id| model.target(id))
            .is_some_and(has_package_references)
        {
            if let Some(slot) = required.get_mut(index) {
                *slot = true;
            }
            pending.push(index);
        }
        for &dep in &project.dependencies {
            if let Some(parents) = dependents.get_mut(dep) {
                parents.push(index);
            }
        }
    }
    while let Some(node) = pending.pop() {
        if let Some(parents) = dependents.get(node) {
            for &parent in parents {
                if let Some(slot) = required.get_mut(parent)
                    && !*slot
                {
                    *slot = true;
                    pending.push(parent);
                }
            }
        }
    }
    projects
        .iter()
        .zip(required)
        .filter(|&(_, needs)| needs)
        .map(|(project, _)| project.name.clone())
        .collect()
}

fn write_project<'a>(
    host: &dyn Host,
    context: &vcxproj::Context<'a>,
    usage: &mut Option<UsageResolver<'a>>,
    nasm_props_template: &mut Option<String>,
) -> Result<(), GenError> {
    let plan = context.plan(usage)?;
    context.write_tool_imports(host, &plan, nasm_props_template)?;
    for item in plan.items.values().filter(|item| item.synthetic) {
        write(host, &item.path, b"# MSBuild custom command\r\n")?;
    }
    let path = context.project.path();
    write(host, &path, vcxproj::render(context, &plan).as_bytes())?;
    write(
        host,
        &format!("{path}.filters"),
        filters::render(context, &plan).as_bytes(),
    )
}

impl Settings {
    fn new(
        version: VsVersion,
        result: &GenerationResult,
        model: &ProjectModel,
        host: &dyn Host,
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
        let system = host.system();
        let platform_name = platform
            .and_then(|p| p.name.as_ref())
            .or_else(|| var(b"CMAKE_VS_PLATFORM_NAME"))
            .map_or_else(|| default_platform(&system).into(), |value| string(value));
        let mut configs = Vec::new();
        let mut seen_raw: HashSet<&[u8]> = HashSet::new();
        let mut seen = BTreeSet::new();
        for value in result
            .configs
            .iter()
            .map(|config| config.0.as_slice())
            .chain(result.graphs.iter().map(|graph| graph.config.0.as_slice()))
            .chain(
                result
                    .targets
                    .iter()
                    .map(|target| target.config.0.as_slice()),
            )
        {
            if !value.is_empty() && seen_raw.insert(value) {
                let value = string(value);
                if seen.insert(value.to_ascii_lowercase()) {
                    configs.push(value);
                }
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
        let cuda_toolset = toolset.and_then(|toolset| toolset.cuda.as_ref());
        let cuda_toolkit = var(b"CMAKE_VS_PLATFORM_TOOLSET_CUDA_CUSTOM_DIR")
            .or_else(|| {
                cuda_toolset.filter(|value| value.iter().any(|byte| matches!(byte, b'/' | b'\\')))
            })
            .map(|value| absolute(&bin, &string(value)));
        let cuda_integration = cuda_toolkit.as_ref().map(|root| {
            let split_root = join(root, "CUDAVisualStudioIntegration");
            let root = if host
                .metadata(
                    &rcmake_host::path::to_native(split_root.as_bytes()),
                    Follow::Yes,
                )
                .is_ok_and(|metadata| metadata.kind == FileKind::Dir)
            {
                &split_root
            } else {
                root
            };
            join(root, "extras/visual_studio_integration/MSBuildExtensions")
        });
        let cuda_version = var(b"CMAKE_VS_PLATFORM_TOOLSET_CUDA")
            .or_else(|| cuda_toolset.filter(|_| cuda_toolkit.is_none()))
            .or_else(|| var(b"CMAKE_CUDA_COMPILER_VERSION"))
            .map(|value| string(value))
            .and_then(|version| {
                let mut parts = version.split('.');
                let (major, minor) = (parts.next()?, parts.next()?);
                (major.parse::<u32>().is_ok() && minor.parse::<u32>().is_ok())
                    .then(|| format!("{major}.{minor}"))
            });
        let nasm_templates = var(b"CMAKE_ROOT")
            .map(|root| string(root))
            .or_else(|| {
                rcmake_host::discover_cmake_root(host, result.metadata.cmake_command.as_ref())
                    .1
                    .map(|root| string(&from_native(&root)))
            })
            .map(|root| join(&root, "Templates/MSBuild"));
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
                .or_else(|| {
                    matches!(system.machine.as_slice(), b"x86_64" | b"AMD64").then(|| "x64".into())
                }),
            tool_version: toolset
                .and_then(|t| t.version.as_ref())
                .or_else(|| var(b"CMAKE_VS_PLATFORM_TOOLSET_VERSION"))
                .map(|value| string(value)),
            cuda_version,
            cuda_toolkit,
            cuda_integration,
            nasm_templates,
            cmake: executable(
                result.metadata.cmake_command.as_ref(),
                b"CMAKE_COMMAND",
                "cmake",
            ),
            ctest_arguments: result
                .metadata
                .ctest_arguments
                .iter()
                .map(|arg| string(arg))
                .collect(),
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

fn write(host: &dyn Host, path: &str, data: &[u8]) -> Result<(), GenError> {
    let path = rcmake_host::path::to_native(path.as_bytes());
    write_output(host, &path, data)
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
    let path = path.replace('\\', "/");
    let base = base.replace('\\', "/");
    let split = |text: &str| {
        let root = Lexical::windows(text.as_bytes()).root_name();
        if root.starts_with(b"//") {
            text.get(root.len() + 1..)
                .and_then(|tail| tail.split('/').next())
                .map_or(root.len(), |share| root.len() + 1 + share.len())
        } else {
            root.len()
        }
    };
    let (path_root, path_tail) = path.split_at(split(&path));
    let (base_root, base_tail) = base.split_at(split(&base));
    if !path_root.eq_ignore_ascii_case(base_root) {
        return path;
    }
    Lexical::new(path_tail.as_bytes())
        .relative(Lexical::new(base_tail.as_bytes()))
        .map_or(path, |result| string(&result))
}

fn folder(path: &str) -> String {
    path.split(['/', '\\'])
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("/")
}

fn msbuild_value(value: &str) -> String {
    value.replace(';', "%3B")
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

fn default_platform(system: &SystemInfo) -> &'static str {
    if !system.sysname.eq_ignore_ascii_case(b"Windows") {
        return "x64";
    }
    match system.machine.as_slice() {
        b"arm64" | b"aarch64" | b"ARM64" => "ARM64",
        b"x86" | b"i386" | b"i686" => "Win32",
        _ => "x64",
    }
}

fn solution_ext(version: VsVersion) -> &'static str {
    if version.writes_slnx() { "slnx" } else { "sln" }
}

#[cfg(test)]
mod tests;
