//! Ninja and Ninja Multi-Config manifest generation from the ADR-009 build graph.

#![forbid(unsafe_code)]

mod graph;
mod modules;
mod prep;
mod syntax;

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};

use rcmake_base::BString;
use rcmake_gen_common::{GenError, write_if_changed};
use rcmake_host::{Host, ReadRange, path};
use rcmake_model::ProjectModel;
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
  description = Building $out

rule COMPILE_DEPFILE
  command = $COMMAND
  description = Building $out
  depfile = $DEP_FILE
  deps = gcc

rule LINK_EXECUTABLE
  command = $COMMAND
  description = Linking executable $out

rule LINK_SHARED_LIBRARY
  command = $COMMAND
  description = Linking shared library $out

rule LINK_MODULE_LIBRARY
  command = $COMMAND
  description = Linking module library $out

rule STATIC_ARCHIVE
  command = $COMMAND
  description = Linking static library $out

rule CUSTOM_COMMAND
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
    let graph = Graph::new(input, &config_artifacts(result, input), build_dir, None)?;
    let prep = PrepTargets::new(&graph, result, model)?;
    let programs = Programs::new(result, host, build_dir);
    let uses_modules = modules::uses_modules(input);
    let module_flag = if uses_modules {
        modules::modmap_flag(result, build_dir)?
    } else {
        BString::default()
    };
    prepare_directory(host, build_dir)?;
    let mut manifests = vec![BString::from("build.ninja"), RULES_PATH.into()];
    if uses_modules {
        manifests.extend(
            modules::write_metadata(result, model, &[input], host, build_dir, false)?
                .iter()
                .map(|file| graph.node(file)),
        );
    }
    let mut ninja = Ninja::default();
    ninja.include(RULES_PATH.as_bytes());
    graph.emit(&mut ninja, &programs.cmake, &module_flag);
    prep.emit(&mut ninja, &graph);
    emit_regeneration(
        &mut ninja,
        &graph,
        &manifests,
        result,
        &configure_command(result, build_dir, &programs.cmake, true),
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
        let graph = Graph::new(
            input,
            &config_artifacts(result, input),
            build_dir,
            Some(name),
        )?;
        let prep = PrepTargets::new(&graph, result, model)?;
        configs.push((graph, ConfigFiles::new(name, prep)));
    }

    let first = configs.first().ok_or_else(|| {
        generation_error(
            build_dir,
            "Ninja Multi-Config requires at least one configuration.",
        )
    })?;
    let requested = configuration_setting(host, build_dir, b"CMAKE_DEFAULT_BUILD_TYPE");
    let (default, default_files) = match requested.as_ref().map(|name| name.as_slice()) {
        Some(name) if !name.is_empty() => configs
            .iter()
            .find(|(graph, _)| graph.config == Some(name))
            .ok_or_else(|| {
                generation_error(
                    build_dir,
                    "CMAKE_DEFAULT_BUILD_TYPE has no matching Ninja configuration.",
                )
            })?,
        _ => first,
    };
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
    if uses_modules {
        manifests.extend(
            modules::write_metadata(result, model, &graphs, host, build_dir, true)?
                .iter()
                .map(|file| default.node(file)),
        );
    }
    write_rules(result, host, build_dir, uses_modules)?;
    let programs = Programs::new(result, host, build_dir);
    let rebuild = configure_command(result, build_dir, &programs.cmake, false);
    let regenerate = configure_command(result, build_dir, &programs.cmake, true);
    for (graph, files) in &configs {
        let mut ninja = Ninja::default();
        ninja.include(RULES_PATH.as_bytes());
        graph.emit(&mut ninja, &programs.cmake, &module_flag);
        files.prep.emit(&mut ninja, graph);
        emit_regeneration(&mut ninja, graph, &manifests, result, &regenerate);
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
            &programs,
            &rebuild,
        )?;
    }
    write_entry(
        host,
        build_dir,
        default,
        default_files,
        b"build.ninja",
        &programs,
        &rebuild,
    )
}

fn config_artifacts<'a>(
    result: &'a GenerationResult,
    input: &BuildGraph,
) -> HashMap<&'a [u8], &'a ArtifactSet> {
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

fn write_entry(
    host: &dyn Host,
    build_dir: &Path,
    graph: &Graph<'_>,
    files: &ConfigFiles,
    entry: &[u8],
    programs: &Programs,
    rebuild: &[u8],
) -> Result<(), GenError> {
    let mut ninja = Ninja::default();
    // The include path can contain a literal pipe.
    ninja.0.extend_from_slice(b"ninja_pipe = |\n");
    ninja.include(&files.implementation);
    graph.emit_entry_aliases(&mut ninja);
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
) {
    let mut seen = HashSet::new();
    let listfiles: Vec<_> = result
        .metadata
        .listfiles
        .iter()
        .map(|file| graph.node(file))
        .filter(|file| !outputs.contains(file) && seen.insert(file.clone()))
        .collect();

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
        if !graph.has_target(listfile) {
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
}

impl Programs {
    fn new(result: &GenerationResult, host: &dyn Host, build_dir: &Path) -> Self {
        let cache = read_cache(host, build_dir);
        let program = |configured: Option<&BString>, key: &[u8], fallback: &[u8]| {
            let saved = || cache_setting(cache.as_deref()?, key).filter(|value| !value.is_empty());
            let resolved = configured
                .filter(|value| !value.is_empty())
                .cloned()
                .or_else(saved)
                .or_else(|| host.env_get(key).filter(|value| !value.is_empty()));
            let mut command = Vec::new();
            shell_word(
                resolved.as_ref().map_or(fallback, |value| value.as_slice()),
                &mut command,
            );
            command
        };
        Self {
            cmake: program(
                result.metadata.cmake_command.as_ref(),
                b"CMAKE_COMMAND",
                b"cmake",
            ),
            ctest: program(
                result.metadata.ctest_command.as_ref(),
                b"CMAKE_CTEST_COMMAND",
                b"ctest",
            ),
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
