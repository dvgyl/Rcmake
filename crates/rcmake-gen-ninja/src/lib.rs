//! Ninja and Ninja Multi-Config manifest generation from the ADR-009 build graph.

#![forbid(unsafe_code)]

mod build;
mod graph;
mod instrumented;
mod modules;
mod multi;
mod prep;
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

use graph::{Graph, qualified};
use prep::PrepTargets;
use syntax::{Edge, Ninja, shell_word};

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
    generate_single(result, host, build_dir, None)
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
    generate_single(result, host, build_dir, Some(model))
}

fn generate_single(
    result: &GenerationResult,
    host: &dyn Host,
    build_dir: &Path,
    model: Option<&ProjectModel>,
) -> Result<(), GenError> {
    let empty = BuildGraph::default();
    let input = result.graphs.first().unwrap_or(&empty);
    let artifacts = config_artifacts(result, input);
    let mut graph = Graph::new(input, &artifacts, build_dir, None)?;
    graph.add_aliases(&artifacts, model);
    graph.resolve_custom_inputs(model, host);
    graph.connect_custom_dependencies(model);
    graph.set_compile_metadata(result);
    let prep = PrepTargets::new(&graph, result, model, host)?;
    let programs = Programs::new(result, host, build_dir, model);
    let uses_modules = modules::uses_modules(input);
    let module_flag = if uses_modules {
        modules::modmap_flag(result, build_dir)?
    } else {
        BString::default()
    };
    prepare_directory(host, build_dir)?;
    let verify_globs = rcmake_gen_common::glob::write_verification(model, build_dir, host)?;
    let exports = model
        .map(|model| write_module_exports(result, model, host, build_dir, false))
        .transpose()?
        .unwrap_or_default();
    let mut manifests = vec![BString::from("build.ninja"), RULES_PATH.into()];
    manifests.extend(exports.iter().map(|job| graph.node(&job.script)));
    if uses_modules {
        manifests.extend(
            modules::write_metadata(result, model, &[input], host, build_dir, false)?
                .iter()
                .map(|file| graph.node(file)),
        );
    }
    let mut ninja = graph.manifest();
    ninja.include(RULES_PATH.as_bytes());
    graph.emit(&mut ninja, &programs.cmake, &module_flag, &exports);
    graph.emit_assumed_sources(&mut ninja, model);
    prep.emit(&mut ninja, &graph);
    emit_regeneration(
        &mut ninja,
        &graph,
        &manifests,
        result,
        &configure_command(result, build_dir, &programs.cmake, true),
        verify_globs.then_some(programs.cmake.as_slice()),
    );
    emit_utilities(
        &mut ninja,
        &graph,
        b"build.ninja",
        &programs,
        &configure_command(result, build_dir, &programs.cmake, false),
    );
    ninja.0.extend_from_slice(b"default all\n");

    write_rules(result, host, build_dir, uses_modules)?;
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
    generate_multi(result, host, build_dir, None)
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
    generate_multi(result, host, build_dir, Some(model))
}

fn generate_multi(
    result: &GenerationResult,
    host: &dyn Host,
    build_dir: &Path,
    model: Option<&ProjectModel>,
) -> Result<(), GenError> {
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
        let mut graph = Graph::new(input, &artifacts, build_dir, Some(name))?;
        graph.add_aliases(&artifacts, model);
        graph.resolve_custom_inputs(model, host);
        graph.set_compile_metadata(result);
        let prep = PrepTargets::new(&graph, result, model, host)?;
        configs.push((graph, ConfigFiles::new(name, prep)));
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
    let mut manifests = vec!["build.ninja".into(), RULES_PATH.into()];
    for (_, files) in &configs {
        manifests.extend([files.entry.clone(), files.implementation.clone()]);
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
    if uses_modules {
        manifests.extend(
            modules::write_metadata(result, model, &graphs, host, build_dir, true)?
                .iter()
                .map(|file| default.node(file)),
        );
    }
    write_rules(result, host, build_dir, uses_modules)?;
    let programs = Programs::new(result, host, build_dir, model);
    let rebuild = configure_command(result, build_dir, &programs.cmake, false);
    let regenerate = configure_command(result, build_dir, &programs.cmake, true);
    for (index, (graph, files)) in configs.iter().enumerate() {
        let mut ninja = graph.manifest();
        ninja.include(RULES_PATH.as_bytes());
        let mut emitted = HashSet::new();
        let included = std::iter::once(index).chain(
            selection
                .cross
                .iter()
                .copied()
                .filter(|&other| other != index),
        );
        for (graph, files) in included
            .clone()
            .filter_map(|selected| configs.get(selected))
        {
            graph.emit_into(
                &mut ninja,
                &programs.cmake,
                &module_flag,
                &exports,
                Some(&mut emitted),
            );
            graph.emit_clean_inputs(&mut ninja);
            files.prep.emit(&mut ninja, graph);
        }
        multi::emit_clean(
            &mut ninja,
            &files.implementation,
            included
                .filter_map(|selected| configs.get(selected).and_then(|(graph, _)| graph.config)),
            cross.iter().filter_map(|graph| graph.config),
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
            &[graph],
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
                &defaults,
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
    aliases: &[&Graph<'_>],
    emitted: &HashSet<BString>,
    (programs, rebuild): (&Programs, &[u8]),
) -> Result<(), GenError> {
    let mut ninja = Ninja::default();
    // The include path can contain a literal pipe.
    ninja.0.extend_from_slice(b"ninja_pipe = |\n");
    ninja.include(&files.implementation);
    graph.emit_aliases(&mut ninja, None, aliases, emitted);
    if !graph.has_target(&"clean".into()) {
        let targets = aliases
            .iter()
            .map(|graph| qualified(&"clean".into(), graph.config))
            .collect::<Vec<_>>();
        ninja.phony(&"clean".into(), &targets);
    }
    files.prep.emit_aliases(&mut ninja, graph);
    emit_utilities(&mut ninja, graph, entry, programs, rebuild);
    ninja.0.extend_from_slice(b"default all\n");
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
) -> Result<(), GenError> {
    let mut ninja = if uses_modules {
        let mut rules = RULES.replacen(
            "ninja_required_version = 1.7",
            "ninja_required_version = 1.10",
            1,
        );
        rules.push_str(modules::RULES);
        Ninja(rules.into_bytes())
    } else {
        Ninja(RULES.as_bytes().to_vec())
    };
    instrumented::emit_rules(&mut ninja, result);
    let mut pools = BTreeMap::new();
    if let Some(specs) = configuration_setting(host, build_dir, b"CMAKE_JOB_POOLS") {
        for spec in specs.split(|&byte| byte == b';') {
            let mut fields = spec.splitn(2, |&byte| byte == b'=');
            let (Some(name), Some(depth)) = (fields.next(), fields.next()) else {
                continue;
            };
            if let Some(depth) = std::str::from_utf8(depth)
                .ok()
                .and_then(|value| value.parse::<u32>().ok())
                && depth != 0
            {
                pools.insert(BString::from(name), depth);
            }
        }
    }

    // The graph contains pool names. Use one job per pool when no depth is available.
    let mut selected = BTreeMap::new();
    for step in result.graphs.iter().flat_map(|graph| &graph.steps) {
        if let BuildStep::Custom(custom) = step
            && !custom.uses_terminal
            && let Some(name) = custom
                .job_pool
                .as_ref()
                .filter(|name| !name.is_empty() && *name != "console")
        {
            selected.insert(name, pools.get(name).copied().unwrap_or(1));
        }
    }
    for (name, depth) in selected {
        if !name
            .iter()
            .all(|byte| byte.is_ascii_alphanumeric() || b"_.-".contains(byte))
        {
            return Err(generation_error(build_dir, "Invalid Ninja job pool name."));
        }
        ninja.0.extend_from_slice(b"\npool ");
        ninja.0.extend_from_slice(name);
        ninja.0.push(b'\n');
        ninja.binding(b"depth", depth.to_string().as_bytes());
    }
    write_if_changed(host, &build_dir.join(RULES_PATH), &ninja.0)
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
        let script: BString = rcmake_gen_common::glob::SCRIPT.into();
        let stamp: BString = rcmake_gen_common::glob::STAMP.into();
        let force: BString = "CMakeFiles/VerifyGlobs.force".into();
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
        shell_word(&script, &mut command);
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
        if listfile != rcmake_gen_common::glob::STAMP.as_bytes() && !graph.has_target(listfile) {
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
    rerun: bool,
) -> Vec<u8> {
    let source_dir = source_directory(result, build_dir);
    let mut command = cmake.to_vec();
    if rerun {
        command.extend_from_slice(b" --regenerate-during-build");
    }
    command.extend_from_slice(b" -S");
    shell_word(&path::from_native(&source_dir), &mut command);
    command.extend_from_slice(b" -B");
    shell_word(&path::from_native(build_dir), &mut command);
    command
}

fn emit_utilities(
    ninja: &mut Ninja,
    graph: &Graph<'_>,
    entry: &[u8],
    programs: &Programs,
    rebuild: &[u8],
) {
    let mut install = programs.cmake.clone();
    let mut test = programs.ctest.clone();
    test.extend_from_slice(b" --force-new-ctest-process");
    if let Some(config) = graph.config {
        install.extend_from_slice(b" -DCMAKE_INSTALL_CONFIG_NAME=");
        shell_word(config, &mut install);
        test.extend_from_slice(b" -C ");
        shell_word(config, &mut test);
    }
    install.extend_from_slice(b" -P cmake_install.cmake");
    let mut clean = b"ninja".to_vec();
    if entry != b"build.ninja" {
        clean.extend_from_slice(b" -f ");
        shell_word(entry, &mut clean);
    }
    let mut help = clean.clone();
    clean.extend_from_slice(b" -t clean");
    help.extend_from_slice(b" -t targets");
    for (name, command, inputs, pool, rule) in [
        (
            "install",
            install,
            vec!["all".into()],
            b"console".as_slice(),
            b"CUSTOM_COMMAND".as_slice(),
        ),
        (
            "test",
            test,
            vec![],
            b"console".as_slice(),
            b"CUSTOM_COMMAND".as_slice(),
        ),
        ("clean", clean, vec![], b"".as_slice(), b"CLEAN".as_slice()),
        ("help", help, vec![], b"".as_slice(), b"HELP".as_slice()),
    ] {
        if name == "clean" && graph.config.is_some() {
            continue;
        }
        emit_utility(
            ninja,
            graph,
            Utility {
                name,
                command: &command,
                inputs: &inputs,
                pool,
                rule,
            },
        );
    }
    for (name, config_file, enabled, inputs) in [
        (
            "package",
            b"./CPackConfig.cmake".as_slice(),
            programs.package,
            programs.package_inputs.as_slice(),
        ),
        (
            "package_source",
            b"./CPackSourceConfig.cmake",
            programs.package_source,
            &[],
        ),
    ] {
        if enabled {
            let mut command = programs.cpack.clone();
            if let Some(config) = graph.config {
                command.extend_from_slice(b" -C ");
                shell_word(config, &mut command);
            }
            command.extend_from_slice(b" --config ");
            shell_word(config_file, &mut command);
            emit_utility(
                ninja,
                graph,
                Utility {
                    name,
                    command: &command,
                    inputs,
                    pool: b"console",
                    rule: b"CUSTOM_COMMAND",
                },
            );
        }
    }
    emit_utility(
        ninja,
        graph,
        Utility {
            name: "rebuild_cache",
            command: rebuild,
            inputs: &[],
            pool: b"console",
            rule: b"CUSTOM_COMMAND",
        },
    );
    let mut edit_cache = programs.cmake.clone();
    edit_cache.extend_from_slice(b" -E echo ");
    shell_word(b"No interactive CMake dialog available.", &mut edit_cache);
    emit_utility(
        ninja,
        graph,
        Utility {
            name: "edit_cache",
            command: &edit_cache,
            inputs: &[],
            pool: b"console",
            rule: b"CUSTOM_COMMAND",
        },
    );
}

struct Utility<'a> {
    name: &'a str,
    command: &'a [u8],
    inputs: &'a [BString],
    pool: &'a [u8],
    rule: &'a [u8],
}

fn emit_utility(ninja: &mut Ninja, graph: &Graph<'_>, utility: Utility<'_>) {
    let name = BString::from(utility.name);
    if graph.has_target(&name) {
        return;
    }
    let output = BString::from(format!("CMakeFiles/{}.util", utility.name));
    ninja.edge(
        utility.rule,
        Edge {
            outputs: std::slice::from_ref(&output),
            inputs: utility.inputs,
            ..Edge::default()
        },
    );
    if !utility.command.is_empty() {
        ninja.binding(b"COMMAND", utility.command);
    }
    if !utility.pool.is_empty() {
        ninja.binding(b"pool", utility.pool);
        ninja.binding(b"DESC", &name);
    }
    ninja.phony(&name, std::slice::from_ref(&output));
    if graph.config.is_some() {
        ninja.phony(&qualified(&name, graph.config), &[output]);
    }
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
    cpack: Vec<u8>,
    package: bool,
    package_source: bool,
    package_inputs: Vec<BString>,
}

impl Programs {
    fn new(
        result: &GenerationResult,
        host: &dyn Host,
        build_dir: &Path,
        model: Option<&ProjectModel>,
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
            shell_word(
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
                    .map(|path| path::from_native(&path))
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
            cpack: program(cpack, b"cpack"),
            package: has_config("CPackConfig.cmake"),
            package_source: has_config("CPackSourceConfig.cmake"),
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
