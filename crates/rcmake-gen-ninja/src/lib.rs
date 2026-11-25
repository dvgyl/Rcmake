//! Ninja and Ninja Multi-Config manifest generation from the ADR-009 build graph.

#![forbid(unsafe_code)]

mod build;
mod clean;
mod commands;
#[allow(
    dead_code,
    reason = "The language activation wave uses this emitter or planner"
)]
mod cuda;
#[allow(
    dead_code,
    reason = "The language activation wave uses this emitter or planner"
)]
mod fortran;
mod graph;
mod install;

use install::{emit_config_utilities, emit_utilities};
mod instrumented;
mod modules;
mod multi;
mod prep;
mod response;
mod syntax;

pub use build::build_commands;

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};

use rcmake_base::BString;
use rcmake_gen_common::export::write_module_exports;
use rcmake_gen_common::{GenError, write_if_changed};
use rcmake_host::{FileKind, Follow, Host, ReadRange, path};
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

rule STATIC_ARCHIVE
  command = $COMMAND
  description = $DESC

rule CUSTOM_COMMAND
  command = $COMMAND
  description = $DESC
  restat = 1

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
    graph.connect_custom_dependencies(model);
    graph.set_compile_metadata(result);
    graph.set_command_settings(model, host);
    graph.set_clean_files(model, &result.metadata.clean_files)?;
    let prep = PrepTargets::new(&graph, result, model, host)?;
    let programs = Programs::new(result, host, build_dir, model, &commands);
    let uses_modules = modules::uses_modules(input);
    let module_flag = if uses_modules {
        modules::modmap_flag(result, build_dir)?
    } else {
        BString::default()
    };
    prepare_directory(host, build_dir)?;
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
    generate_multi(result, host, build_dir, None, Commands::new(host))
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
    generate_multi(result, host, build_dir, Some(model), Commands::new(host))
}

fn generate_multi(
    result: &GenerationResult,
    host: &dyn Host,
    build_dir: &Path,
    model: Option<&ProjectModel>,
    commands: Commands,
) -> Result<(), GenError> {
    let prefix = output_prefix(model, &commands);
    let mut configs = Vec::new();
    let mut names = HashSet::new();
    for config in &result.configs {
        let name = config.0.as_slice();
        if name.is_empty()
            || name
                .iter()
                .any(|byte| matches!(byte, b'/' | b'\\' | b'\n' | b'\r' | 0))
            || !names.insert(name)
        {
            return Err(generation_error(
                build_dir,
                "Invalid or duplicate Ninja configuration name.",
            ));
        }
        let input = result
            .graphs
            .iter()
            .find(|graph| graph.config == *config)
            .ok_or_else(|| {
                generation_error(build_dir, "A Ninja configuration has no build graph.")
            })?;
        let artifacts = config_artifacts(result, input);
        let mut graph = Graph::with_prefix(
            input,
            &artifacts,
            build_dir,
            Some(name),
            &prefix,
            commands.clone(),
        )?;
        graph.add_aliases(&artifacts, model, result);
        graph.resolve_custom_inputs(model, host);
        graph.set_compile_metadata(result);
        graph.set_command_settings(model, host);
        graph.set_clean_files(model, &result.metadata.clean_files)?;
        let prep = PrepTargets::new(&graph, result, model, host)?;
        configs.push((graph, ConfigFiles::new(name, prep)));
    }

    prepare_directory(host, build_dir)?;
    for (graph, _) in &mut configs {
        graph.prepare_custom_scripts(host)?;
    }
    let selection = multi::Selection::new(&result.configs, build_dir, |name| {
        model
            .and_then(|model| model.definition(DirId::ROOT, name))
            .cloned()
            .or_else(|| configuration_setting(host, build_dir, name))
    })?;
    let (default, default_files) = configs
        .get(selection.default)
        .ok_or_else(|| generation_error(build_dir, "Missing default Ninja configuration."))?;
    let cross: Vec<_> = selection
        .cross
        .iter()
        .filter_map(|&index| configs.get(index).map(|(graph, _)| graph))
        .collect();
    let defaults: Vec<_> = selection
        .defaults
        .iter()
        .filter_map(|&index| configs.get(index).map(|(graph, _)| graph))
        .collect();
    let mut manifests = vec![
        default.node(b"build.ninja"),
        default.node(RULES_PATH.as_bytes()),
    ];
    for (_, files) in &configs {
        manifests.extend([
            default.node(&files.entry),
            default.node(&files.implementation),
        ]);
    }

    let graphs: Vec<_> = configs.iter().map(|(graph, _)| graph.input).collect();
    let uses_modules = graphs.iter().any(|graph| modules::uses_modules(graph));
    let module_flag = if uses_modules {
        modules::modmap_flag(result, build_dir)?
    } else {
        BString::default()
    };
    prepare_directory(host, build_dir)?;
    let verify_globs = rcmake_gen_common::glob::write_verification(model, build_dir, host)?;
    let exports = model
        .map(|model| write_module_exports(result, model, host, build_dir, true))
        .transpose()?
        .unwrap_or_default();
    manifests.extend(exports.iter().map(|job| default.node(&job.script)));
    manifests.extend(
        result
            .metadata
            .generated_files
            .iter()
            .map(|file| default.node(file)),
    );
    add_install_scripts(&mut manifests, default, result, model);
    if uses_modules || graphs.iter().any(|graph| fortran::uses_fortran(graph)) {
        manifests.extend(
            modules::write_metadata(result, model, &graphs, host, build_dir, true)?
                .iter()
                .map(|file| default.node(file)),
        );
    }
    write_rules(result, host, build_dir, uses_modules, model, &commands)?;
    let programs = Programs::new(result, host, build_dir, model, &commands);
    let rebuild = configure_command(result, build_dir, &programs.cmake, &commands, false);
    let regenerate = configure_command(result, build_dir, &programs.cmake, &commands, true);
    for (index, (graph, files)) in configs.iter().enumerate() {
        let mut ninja = graph.manifest();
        ninja.include(&graph.node(RULES_PATH.as_bytes()));
        let mut emitted = HashSet::new();
        let included = std::iter::once(index).chain(
            selection
                .cross
                .iter()
                .copied()
                .filter(|&other| other != index),
        );
        for (output_index, (output, files)) in included
            .clone()
            .filter_map(|selected| configs.get(selected).map(|config| (selected, config)))
        {
            let cross = (index != output_index)
                .then(|| {
                    result
                        .cross_graphs
                        .get(&(graph.input.config.clone(), output.input.config.clone()))
                })
                .flatten()
                .map(|input| {
                    let artifacts = config_artifacts(result, input);
                    let mut variant = Graph::with_prefix(
                        input,
                        &artifacts,
                        build_dir,
                        output.config,
                        &prefix,
                        commands.clone(),
                    )?;
                    variant.add_aliases(&artifacts, model, result);
                    variant.resolve_custom_inputs(model, host);
                    variant.set_compile_metadata(result);
                    variant.set_command_settings(model, host);
                    variant.prepare_custom_scripts(host)?;
                    Ok::<_, GenError>(variant)
                })
                .transpose()?;
            let graph = cross.as_ref().unwrap_or(output);
            graph.emit_into(
                &mut ninja,
                &programs.cmake,
                &module_flag,
                &exports,
                Some(&mut emitted),
            )?;
            graph.emit_clean_inputs(&mut ninja);
            files.prep.emit(&mut ninja, graph);
            emit_config_utilities(&mut ninja, output, &programs, &rebuild);
        }
        if !cross.is_empty() {
            files.prep.emit_all_aliases(&mut ninja, &cross);
        }
        multi::emit_clean(
            &mut ninja,
            graph,
            &files.implementation,
            &programs,
            included.filter_map(|selected| configs.get(selected).map(|(graph, _)| graph)),
            &cross,
        );
        if !cross.is_empty() {
            graph.emit_aliases(&mut ninja, Some(b"all"), &cross, &emitted);
        }
        emit_regeneration(
            &mut ninja,
            graph,
            &manifests,
            result,
            &regenerate,
            verify_globs.then_some(programs.cmake.as_slice()),
        );
        write_if_changed(
            host,
            &build_dir.join(path::to_native(&files.implementation)),
            &ninja.0,
        )?;
        write_entry(
            host,
            build_dir,
            graph,
            files,
            &files.entry,
            (&[graph], &[]),
            &emitted,
            (&programs, &rebuild),
        )?;
        if index == selection.default {
            write_entry(
                host,
                build_dir,
                default,
                default_files,
                b"build.ninja",
                (&defaults, &cross),
                &emitted,
                (&programs, &rebuild),
            )?;
        }
    }
    Ok(())
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

struct ConfigFiles {
    entry: BString,
    implementation: BString,
    prep: PrepTargets,
}

impl ConfigFiles {
    fn new(config: &[u8], prep: PrepTargets) -> Self {
        let entry = [b"build-", config, b".ninja"].concat().into();
        let implementation = [b"CMakeFiles/impl-", config, b".ninja"].concat().into();
        Self {
            entry,
            implementation,
            prep,
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn write_entry(
    host: &dyn Host,
    build_dir: &Path,
    graph: &Graph<'_>,
    files: &ConfigFiles,
    entry: &[u8],
    (aliases, all): (&[&Graph<'_>], &[&Graph<'_>]),
    emitted: &HashSet<BString>,
    (programs, rebuild): (&Programs, &[u8]),
) -> Result<(), GenError> {
    let mut ninja = Ninja::default();
    // The include path can contain a literal pipe.
    ninja.0.extend_from_slice(b"ninja_pipe = |\n");
    ninja.include(&graph.node(&files.implementation));
    graph.emit_aliases(&mut ninja, None, aliases, emitted);
    if !graph.has_target(&"clean".into()) {
        let targets = aliases
            .iter()
            .map(|graph| qualified(&graph.node(b"clean"), graph.config))
            .collect::<Vec<_>>();
        ninja.phony(&graph.node(b"clean"), &targets);
    }
    files.prep.emit_aliases(&mut ninja, graph, aliases);
    emit_utilities(
        &mut ninja,
        graph,
        entry,
        programs,
        rebuild,
        install::Scope::Entry { aliases, all },
    );
    if graph.prefix.is_empty() {
        ninja.0.extend_from_slice(b"default all\n");
    }
    write_if_changed(host, &build_dir.join(path::to_native(entry)), &ninja.0)
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
        instrumented::emit_rules(&mut ninja, result);
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

fn emit_regeneration(
    ninja: &mut Ninja,
    graph: &Graph<'_>,
    outputs: &[BString],
    result: &GenerationResult,
    regenerate: &[u8],
    verify: Option<&[u8]>,
) {
    let mut seen = HashSet::new();
    let mut listfiles: Vec<_> = result
        .metadata
        .listfiles
        .iter()
        .map(|file| graph.node(file))
        .filter(|file| !outputs.contains(file) && seen.insert(file.clone()))
        .collect();

    if let Some(cmake) = verify {
        let script = graph.node(rcmake_gen_common::glob::SCRIPT.as_bytes());
        let stamp = graph.node(rcmake_gen_common::glob::STAMP.as_bytes());
        let force = graph.node(b"CMakeFiles/VerifyGlobs.force");
        ninja.phony(&force, &[]);
        ninja.edge(
            b"VERIFY_GLOBS",
            Edge {
                outputs: std::slice::from_ref(&stamp),
                implicit_inputs: &[script.clone(), force],
                ..Edge::default()
            },
        );
        let mut command = cmake.to_vec();
        command.extend_from_slice(b" -P ");
        graph.commands.word(&script, &mut command);
        ninja.binding(b"COMMAND", &command);
        listfiles.push(stamp);
    }

    ninja.edge(
        b"RERUN_CMAKE",
        Edge {
            outputs,
            implicit_inputs: &listfiles,
            ..Edge::default()
        },
    );
    ninja.binding(b"COMMAND", regenerate);

    for listfile in &listfiles {
        // Missing listfiles must trigger regeneration so CMake can check them.
        if *listfile != graph.node(rcmake_gen_common::glob::STAMP.as_bytes())
            && !graph.has_target(listfile)
        {
            ninja.phony(listfile, &[]);
        }
    }
}

fn source_directory(result: &GenerationResult, build_dir: &Path) -> PathBuf {
    result
        .metadata
        .listfiles
        .iter()
        .map(|file| path::to_native(file))
        .find(|file| {
            file.file_name()
                .is_some_and(|name| name == "CMakeLists.txt")
        })
        .and_then(|file| file.parent().map(Path::to_path_buf))
        .or_else(|| {
            result
                .targets
                .first()
                .map(|target| path::to_native(&target.metadata.source_dir))
        })
        .map(|source| build_dir.join(source))
        .unwrap_or_else(|| build_dir.to_path_buf())
}

fn configure_command(
    result: &GenerationResult,
    build_dir: &Path,
    cmake: &[u8],
    commands: &Commands,
    rerun: bool,
) -> Vec<u8> {
    let source_dir = source_directory(result, build_dir);
    let mut command = cmake.to_vec();
    if rerun {
        command.extend_from_slice(b" --regenerate-during-build");
    }
    let option = |prefix: &[u8], directory: &Path, command: &mut Vec<u8>| {
        command.push(b' ');
        let directory = path::from_native(directory);
        if commands.windows() {
            commands.word(&[prefix, &directory].concat(), command);
        } else {
            command.extend_from_slice(prefix);
            commands.word(&directory, command);
        }
    };
    option(b"-S", &source_dir, &mut command);
    option(b"-B", build_dir, &mut command);
    command
}

fn configuration_setting(host: &dyn Host, build_dir: &Path, name: &[u8]) -> Option<BString> {
    host.env_get(name)
        .or_else(|| cache_setting(&read_cache(host, build_dir)?, name))
}

fn read_cache(host: &dyn Host, build_dir: &Path) -> Option<Vec<u8>> {
    host.read(
        &build_dir.join("CMakeCache.txt"),
        ReadRange::All {
            limit: 16 * 1024 * 1024,
        },
    )
    .ok()
}

fn cache_setting(cache: &[u8], name: &[u8]) -> Option<BString> {
    cache.split(|&byte| byte == b'\n').find_map(|line| {
        let entry = line.strip_prefix(name)?.strip_prefix(b":")?;
        let value = entry.splitn(2, |&byte| byte == b'=').nth(1)?;
        Some(BString::from(value.strip_suffix(b"\r").unwrap_or(value)))
    })
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
    install: bool,
    strip: bool,
    parallel_install: bool,
    directories: Vec<BString>,
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
