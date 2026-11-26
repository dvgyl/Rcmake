//! Ninja and Ninja Multi-Config manifest generation from the ADR-009 build graph.

#![forbid(unsafe_code)]

mod build;
mod clean;
mod commands;
mod cuda;
mod fortran;
mod graph;
mod install;

use install::emit_utilities;
mod instrumented;
mod modules;
mod multi;
mod prep;
mod regenerate;
mod response;
use regenerate::{
    cache_setting, configuration_setting, configure_command, emit_regeneration, read_cache,
};
mod syntax;

pub use build::build_commands;

use std::collections::{BTreeMap, HashSet};
use std::path::Path;

use rcmake_base::BString;
use rcmake_gen_common::export::write_module_exports;
use rcmake_gen_common::{GenError, write_if_changed};
use rcmake_host::{FileKind, Follow, Host, path};
use rcmake_model::{DirId, ProjectModel};
use rcmake_resolve::{ArtifactSet, BuildGraph, BuildStep, GenerationResult};

use commands::Commands;
use graph::{Graph, qualified};
use prep::PrepTargets;
use syntax::{Edge, Ninja};

const RULES_PATH: &str = "CMakeFiles/rules.ninja";
const RULES: &str = "\
ninja_required_version = 1.7
ninja_pipe = |

rule COMPILE
  command = $COMMAND
  description = $DESC

rule COMPILE_DEPFILE
  command = $COMMAND
  description = $DESC
  depfile = $DEP_FILE
  deps = gcc

rule LINK_EXECUTABLE
  command = $COMMAND
  description = $DESC

rule LINK_SHARED_LIBRARY
  command = $COMMAND
  description = $DESC

rule LINK_MODULE_LIBRARY
  command = $COMMAND
  description = $DESC

rule LINK_DEPFILE
  command = $COMMAND
  description = $DESC
  depfile = $DEP_FILE
  deps = gcc

rule STATIC_ARCHIVE
  command = $COMMAND
  description = $DESC

rule CUSTOM_COMMAND
  command = $COMMAND
  description = $DESC

rule VERIFY_GLOBS
  command = $COMMAND
  description = Re-checking globbed directories...
  restat = 1

rule CMAKE_SYMLINK_LIBRARY
  command = $COMMAND
  description = $DESC
  restat = 1

rule RERUN_CMAKE
  command = $COMMAND
  description = Re-running CMake
  generator = 1
  pool = console
  restat = 1

rule CLEAN
  command = $COMMAND
  description = Cleaning built files

rule HELP
  command = $COMMAND
  description = Listing available targets
";

/// Write `build.ninja` and its rule file for the single configuration of `result`.
///
/// # Errors
///
/// Returns an error if the graph is invalid or the host cannot write a manifest.
pub fn generate_single_config(
    result: &GenerationResult,
    host: &dyn Host,
    build_dir: &Path,
) -> Result<(), GenError> {
    generate_single(result, host, build_dir, None, Commands::new(host))
}

/// Write a single-configuration Ninja manifest with test preparation targets
/// from the configure-time project model.
///
/// # Errors
///
/// Returns an error if input resolution fails or the host cannot write a file.
pub fn generate_single_config_with_model(
    result: &GenerationResult,
    host: &dyn Host,
    build_dir: &Path,
    model: &ProjectModel,
) -> Result<(), GenError> {
    generate_single(result, host, build_dir, Some(model), Commands::new(host))
}

fn output_prefix(model: Option<&ProjectModel>, commands: &Commands) -> BString {
    model
        .and_then(|model| model.definition(DirId::ROOT, b"CMAKE_NINJA_OUTPUT_PATH_PREFIX"))
        .map_or_else(BString::default, |prefix| commands.path(prefix))
}

pub(crate) fn write_swift_output_maps(
    graphs: &[&BuildGraph],
    host: &dyn Host,
    build_dir: &Path,
) -> Result<(), GenError> {
    for graph in graphs {
        for (relative, contents) in &graph.swift_output_file_maps {
            let full = build_dir.join(path::to_native(relative));
            if let Some(parent) = full.parent() {
                host.create_dir_all(parent)
                    .map_err(|error| GenError::Write {
                        path: path::from_native(parent),
                        message: error.to_string().into(),
                    })?;
            }
            write_if_changed(host, &full, contents.as_slice())?;
        }
    }
    Ok(())
}

fn generate_single(
    result: &GenerationResult,
    host: &dyn Host,
    build_dir: &Path,
    model: Option<&ProjectModel>,
    commands: Commands,
) -> Result<(), GenError> {
    let prefix = output_prefix(model, &commands);
    let empty = BuildGraph::default();
    let input = result.graphs.first().unwrap_or(&empty);
    let artifacts = config_artifacts(result, input);
    let mut graph = Graph::with_prefix(
        input,
        &artifacts,
        build_dir,
        None,
        &prefix,
        commands.clone(),
    )?;
    graph.add_aliases(&artifacts, model, result);
    graph.resolve_custom_inputs(model, host);
    graph.set_compile_metadata(result);
    graph.set_command_settings(model, host);
    graph.set_clean_files(model, &result.metadata.clean_files)?;
    let prep = PrepTargets::new(&graph, result, model, host)?;
    prep::warn_install_prep_components(&[&graph], model, host);
    let programs = Programs::new(result, host, build_dir, model, &commands);
    let uses_modules = modules::uses_modules(input);
    let module_flag = if uses_modules {
        modules::modmap_flag(result, build_dir)?
    } else {
        BString::default()
    };
    prepare_directory(host, build_dir)?;
    write_swift_output_maps(&[input], host, build_dir)?;
    graph.prepare_custom_scripts(host)?;
    let verify_globs = rcmake_gen_common::glob::write_verification(model, build_dir, host)?;
    let exports = model
        .map(|model| write_module_exports(result, model, host, build_dir, false))
        .transpose()?
        .unwrap_or_default();
    let mut manifests = vec![
        graph.node(b"build.ninja"),
        graph.node(RULES_PATH.as_bytes()),
    ];
    manifests.extend(exports.iter().map(|job| graph.node(&job.script)));
    manifests.extend(
        result
            .metadata
            .generated_files
            .iter()
            .map(|file| graph.node(file)),
    );
    add_install_scripts(&mut manifests, &graph, result, model);
    if uses_modules || fortran::uses_fortran(input) {
        manifests.extend(
            modules::write_metadata(result, model, &[input], host, build_dir, false)?
                .iter()
                .map(|file| graph.node(file)),
        );
    }
    let mut ninja = graph.manifest();
    ninja.include(&graph.node(RULES_PATH.as_bytes()));
    graph.emit(&mut ninja, &programs.cmake, &module_flag, &exports)?;
    graph.emit_entry_aliases(&mut ninja);
    graph.emit_assumed_sources(&mut ninja, model);
    prep.emit(&mut ninja, &graph);
    emit_regeneration(
        &mut ninja,
        &graph,
        &manifests,
        result,
        &configure_command(result, build_dir, &programs.cmake, &commands, true),
        verify_globs.then_some(programs.cmake.as_slice()),
    );
    emit_utilities(
        &mut ninja,
        &graph,
        b"build.ninja",
        &programs,
        &configure_command(result, build_dir, &programs.cmake, &commands, false),
        install::Scope::Single,
    );
    if graph.prefix.is_empty() {
        ninja.0.extend_from_slice(b"default all\n");
    }

    write_rules(result, host, build_dir, uses_modules, model, &commands)?;
    write_if_changed(host, &build_dir.join("build.ninja"), &ninja.0)
}

/// Write a Ninja entry file and implementation file for each configuration.
///
/// The generator reads `CMAKE_DEFAULT_BUILD_TYPE` from the host environment or
/// `CMakeCache.txt`. Otherwise, it uses the first configuration in `result`.
/// Each entry file exposes the aliases for its configuration.
///
/// # Errors
///
/// Returns an error if a configuration has no valid graph or the host cannot write
/// a manifest.
pub fn generate_multi_config(
    result: &GenerationResult,
    host: &dyn Host,
    build_dir: &Path,
) -> Result<(), GenError> {
    multi::generate_multi(result, host, build_dir, None, Commands::new(host))
}

/// Write Ninja Multi-Config manifests with test preparation targets from the
/// configure-time project model.
///
/// # Errors
///
/// Returns an error if input resolution fails or the host cannot write a file.
pub fn generate_multi_config_with_model(
    result: &GenerationResult,
    host: &dyn Host,
    build_dir: &Path,
    model: &ProjectModel,
) -> Result<(), GenError> {
    multi::generate_multi(result, host, build_dir, Some(model), Commands::new(host))
}

fn config_artifacts<'a>(
    result: &'a GenerationResult,
    input: &BuildGraph,
) -> BTreeMap<&'a [u8], &'a ArtifactSet> {
    result
        .targets
        .iter()
        .filter(|target| target.config == input.config)
        .map(|target| (target.metadata.name.as_slice(), &target.artifacts))
        .collect()
}

fn prepare_directory(host: &dyn Host, build_dir: &Path) -> Result<(), GenError> {
    let directory = build_dir.join("CMakeFiles");
    host.create_dir_all(&directory)
        .map_err(|error| generation_error(&directory, error.to_string()))
}

fn write_rules(
    result: &GenerationResult,
    host: &dyn Host,
    build_dir: &Path,
    uses_modules: bool,
    model: Option<&ProjectModel>,
    commands: &Commands,
) -> Result<(), GenError> {
    let uses_fortran = result.graphs.iter().any(fortran::uses_fortran);
    let mut ninja = if uses_modules || uses_fortran {
        let mut rules = RULES.replacen(
            "ninja_required_version = 1.7",
            "ninja_required_version = 1.10",
            1,
        );
        if uses_modules {
            rules.push_str(modules::RULES);
        }
        if uses_fortran {
            rules.push_str(fortran::RULES);
        }
        Ninja(rules.into_bytes())
    } else {
        Ninja(RULES.as_bytes().to_vec())
    };
    if result
        .graphs
        .iter()
        .flat_map(|graph| &graph.steps)
        .any(|step| matches!(step, rcmake_resolve::BuildStep::DeviceLink { .. }))
    {
        ninja.0.extend_from_slice(cuda::RULES.as_bytes());
    }
    response::emit_rules(&mut ninja);
    if !commands.windows() {
        let prefix = output_prefix(model, commands);
        instrumented::emit_rules(&mut ninja, result, build_dir, !prefix.is_empty(), commands);
    }
    if let Some(prefix) = model
        .and_then(|model| model.definition(DirId::ROOT, b"CMAKE_CL_SHOWINCLUDES_PREFIX"))
        .cloned()
        .or_else(|| configuration_setting(host, build_dir, b"CMAKE_CL_SHOWINCLUDES_PREFIX"))
        .filter(|prefix| !prefix.is_empty())
    {
        ninja.global_binding(b"msvc_deps_prefix", &prefix);
    }
    let mut pools = BTreeMap::new();
    let specs = if let Some(model) = model {
        model
            .global
            .props
            .get(b"JOB_POOLS")
            .or_else(|| model.definition(DirId::ROOT, b"CMAKE_JOB_POOLS"))
            .cloned()
    } else {
        configuration_setting(host, build_dir, b"CMAKE_JOB_POOLS")
    };
    if let Some(specs) = specs {
        for spec in rcmake_base::list::split(&specs, rcmake_base::list::Empty::Drop) {
            let invalid = || {
                generation_error(
                    build_dir,
                    format!("Invalid pool defined by property 'JOB_POOLS': {spec}"),
                )
            };
            let mut fields = spec.splitn(2, |&byte| byte == b'=');
            let name = fields
                .next()
                .filter(|name| {
                    !name.is_empty()
                        && name
                            .iter()
                            .all(|byte| byte.is_ascii_alphanumeric() || b"_.-".contains(byte))
                })
                .ok_or_else(invalid)?;
            let depth = fields
                .next()
                .and_then(|depth| std::str::from_utf8(depth).ok())
                .and_then(|depth| depth.parse::<u32>().ok())
                .filter(|&depth| depth > 0)
                .ok_or_else(invalid)?;
            pools.insert(BString::from(name), depth);
        }
    }
    if model.is_none() {
        for step in result.graphs.iter().flat_map(|graph| &graph.steps) {
            if let BuildStep::Custom(custom) = step
                && !custom.uses_terminal
                && let Some(name) = custom
                    .job_pool
                    .as_ref()
                    .filter(|name| !name.is_empty() && *name != "console")
            {
                pools.entry(name.clone()).or_insert(1);
            }
        }
    }
    for (name, depth) in pools {
        ninja.0.extend_from_slice(b"\npool ");
        ninja.0.extend_from_slice(&name);
        ninja.0.push(b'\n');
        ninja.binding(b"depth", depth.to_string().as_bytes());
    }
    write_if_changed(host, &build_dir.join(RULES_PATH), &ninja.0)
}

fn add_install_scripts(
    outputs: &mut Vec<BString>,
    graph: &Graph<'_>,
    result: &GenerationResult,
    model: Option<&ProjectModel>,
) {
    let Some(model) = model.filter(|model| {
        !model
            .definition(DirId::ROOT, b"CMAKE_SKIP_INSTALL_RULES")
            .is_some_and(|value| rcmake_base::truth::is_on(value))
    }) else {
        return;
    };
    let mut seen: HashSet<_> = outputs.iter().cloned().collect();
    for &id in result.install.directories.keys() {
        let dir = rcmake_base::path_lex::Lexical::new(&model.dir(id).binary_dir);
        let script = graph.node(&dir.append(b"cmake_install.cmake"));
        if seen.insert(script.clone()) {
            outputs.push(script);
        }
    }
}

struct Programs {
    cmake: Vec<u8>,
    ctest: Vec<u8>,
    ctest_arguments: Vec<BString>,
    cpack: Vec<u8>,
    ninja: Vec<u8>,
    package: bool,
    package_source: bool,
    package_inputs: Vec<BString>,
    testing: bool,
    test_depends_all: bool,
    install: bool,
    strip: bool,
    parallel_install: bool,
    directories: Vec<BString>,
    edit_cache: Option<Vec<u8>>,
    directory_sources: std::collections::HashMap<BString, BString>,
    install_directories: Vec<BString>,
}

impl Programs {
    fn new(
        result: &GenerationResult,
        host: &dyn Host,
        build_dir: &Path,
        model: Option<&ProjectModel>,
        commands: &Commands,
    ) -> Self {
        let cache = read_cache(host, build_dir);
        let definition = |key: &[u8]| model.and_then(|model| model.definition(DirId::ROOT, key));
        let setting = |configured: Option<&BString>, key: &[u8]| {
            let saved = || cache_setting(cache.as_deref()?, key).filter(|value| !value.is_empty());
            configured
                .filter(|value| !value.is_empty())
                .cloned()
                .or_else(saved)
                .or_else(|| host.env_get(key).filter(|value| !value.is_empty()))
        };
        let program = |resolved: Option<BString>, fallback: &[u8]| {
            let mut command = Vec::new();
            commands.word(
                resolved.as_ref().map_or(fallback, |value| value.as_slice()),
                &mut command,
            );
            command
        };
        let cmake = setting(result.metadata.cmake_command.as_ref(), b"CMAKE_COMMAND");
        let cpack =
            setting(definition(b"CMAKE_CPACK_COMMAND"), b"CMAKE_CPACK_COMMAND").or_else(|| {
                let sibling = |executable: &Path| {
                    let name = if executable
                        .extension()
                        .is_some_and(|ext| ext.eq_ignore_ascii_case("exe"))
                    {
                        "cpack.exe"
                    } else {
                        "cpack"
                    };
                    executable.with_file_name(name)
                };
                let configured = cmake.as_ref().map(|path| sibling(&path::to_native(path)));
                configured
                    .filter(|path| path.components().count() > 1)
                    .or_else(|| host.current_exe().ok().map(|exe| sibling(&exe)))
                    .map(|path| path::from_native_cmake(&path))
            });
        let has_config = |name| {
            host.metadata(&build_dir.join(name), Follow::Yes)
                .is_ok_and(|metadata| metadata.kind == FileKind::File)
        };
        let skip_all = setting(
            definition(b"CMAKE_SKIP_PACKAGE_ALL_DEPENDENCY"),
            b"CMAKE_SKIP_PACKAGE_ALL_DEPENDENCY",
        )
        .is_some_and(|value| rcmake_base::truth::is_on(&value));
        Self {
            cmake: program(cmake, b"cmake"),
            ctest: program(
                setting(
                    result.metadata.ctest_command.as_ref(),
                    b"CMAKE_CTEST_COMMAND",
                ),
                b"ctest",
            ),
            ctest_arguments: result.metadata.ctest_arguments.clone(),
            cpack: program(cpack, b"cpack"),
            ninja: program(
                setting(definition(b"CMAKE_MAKE_PROGRAM"), b"CMAKE_MAKE_PROGRAM"),
                b"ninja",
            ),
            package: has_config("CPackConfig.cmake"),
            package_source: has_config("CPackSourceConfig.cmake"),
            testing: model.is_none()
                || definition(b"CMAKE_TESTING_ENABLED")
                    .is_some_and(|value| rcmake_base::truth::is_on(value)),
            test_depends_all: definition(b"CMAKE_SKIP_TEST_ALL_DEPENDENCY")
                .is_some_and(|value| rcmake_base::truth::is_off(value)),
            install: model.is_none()
                || (!result.install.actions.is_empty()
                    && !definition(b"CMAKE_SKIP_INSTALL_RULES")
                        .is_some_and(|value| rcmake_base::truth::is_on(value))),
            strip: definition(b"CMAKE_STRIP")
                .is_some_and(|value| !value.is_empty() && !value.ends_with(b"-NOTFOUND")),
            parallel_install: model
                .and_then(|model| model.global.props.get(b"INSTALL_PARALLEL"))
                .is_some_and(|value| rcmake_base::truth::is_on(value)),
            directories: model
                .into_iter()
                .flat_map(|model| model.dirs())
                .filter_map(|(id, dir)| (id != DirId::ROOT).then_some(dir.binary_dir.clone()))
                .collect(),
            edit_cache: setting(definition(b"CMAKE_EDIT_COMMAND"), b"CMAKE_EDIT_COMMAND")
                .map(|editor| program(Some(editor), b"")),
            directory_sources: model
                .into_iter()
                .flat_map(|model| model.dirs())
                .map(|(_, dir)| (commands.path(&dir.binary_dir), dir.source_dir.clone()))
                .collect(),
            install_directories: model
                .into_iter()
                .flat_map(|model| model.dirs())
                .filter_map(|(id, dir)| {
                    result
                        .install
                        .directories
                        .contains_key(&id)
                        .then_some(dir.binary_dir.clone())
                })
                .collect(),
            package_inputs: if skip_all {
                Vec::new()
            } else {
                vec!["all".into()]
            },
        }
    }
}

fn generation_error(path: &Path, message: impl Into<BString>) -> GenError {
    GenError::Write {
        path: path::from_native(path),
        message: message.into(),
    }
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod windows_tests;
