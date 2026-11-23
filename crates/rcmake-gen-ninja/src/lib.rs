//! Ninja and Ninja Multi-Config manifest generation from the ADR-009 build graph.

#![forbid(unsafe_code)]

mod graph;
mod syntax;

use std::collections::{BTreeMap, HashSet};
use std::path::Path;

use rcmake_base::BString;
use rcmake_gen_common::{GenError, write_if_changed};
use rcmake_host::{Host, ReadRange, path};
use rcmake_resolve::{BuildGraph, BuildStep, GenerationResult};

use graph::{Graph, qualified};
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
    let empty = BuildGraph::default();
    let input = result.graphs.first().unwrap_or(&empty);
    let graph = Graph::new(input, build_dir, None)?;
    let manifests = [BString::from("build.ninja"), RULES_PATH.into()];
    let mut ninja = Ninja::default();
    ninja.include(RULES_PATH.as_bytes());
    graph.emit(&mut ninja);
    emit_regeneration(&mut ninja, &graph, &manifests, result, host, build_dir);
    emit_utilities(
        &mut ninja,
        &graph,
        b"build.ninja",
        host,
        &configure_command(result, host, build_dir, false),
    );
    ninja.0.extend_from_slice(b"default all\n");

    prepare_directory(host, build_dir)?;
    write_rules(result, host, build_dir)?;
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
        configs.push((
            Graph::new(input, build_dir, Some(name))?,
            ConfigFiles::new(name),
        ));
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

    prepare_directory(host, build_dir)?;
    write_rules(result, host, build_dir)?;
    let rebuild = configure_command(result, host, build_dir, false);
    for (graph, files) in &configs {
        let mut ninja = Ninja::default();
        ninja.include(RULES_PATH.as_bytes());
        graph.emit(&mut ninja);
        emit_regeneration(&mut ninja, graph, &manifests, result, host, build_dir);
        write_if_changed(
            host,
            &build_dir.join(path::to_native(&files.implementation)),
            &ninja.0,
        )?;
        write_entry(host, build_dir, graph, files, &files.entry, &rebuild)?;
    }
    write_entry(
        host,
        build_dir,
        default,
        default_files,
        b"build.ninja",
        &rebuild,
    )
}

struct ConfigFiles {
    entry: BString,
    implementation: BString,
}

impl ConfigFiles {
    fn new(config: &[u8]) -> Self {
        let entry = [b"build-", config, b".ninja"].concat().into();
        let implementation = [b"CMakeFiles/impl-", config, b".ninja"].concat().into();
        Self {
            entry,
            implementation,
        }
    }
}

fn write_entry(
    host: &dyn Host,
    build_dir: &Path,
    graph: &Graph<'_>,
    files: &ConfigFiles,
    entry: &[u8],
    rebuild: &[u8],
) -> Result<(), GenError> {
    let mut ninja = Ninja::default();
    // The include path can contain a literal pipe.
    ninja.0.extend_from_slice(b"ninja_pipe = |\n");
    ninja.include(&files.implementation);
    graph.emit_entry_aliases(&mut ninja);
    emit_utilities(&mut ninja, graph, entry, host, rebuild);
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
) -> Result<(), GenError> {
    let mut ninja = Ninja(RULES.as_bytes().to_vec());
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
    host: &dyn Host,
    build_dir: &Path,
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
    ninja.binding(
        b"COMMAND",
        &configure_command(result, host, build_dir, true),
    );

    for listfile in &listfiles {
        // Missing listfiles must trigger regeneration so CMake can check them.
        if !graph.has_target(listfile) {
            ninja.phony(listfile, &[]);
        }
    }
}

fn configure_command(
    result: &GenerationResult,
    host: &dyn Host,
    build_dir: &Path,
    rerun: bool,
) -> Vec<u8> {
    let source_dir = result
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
        .unwrap_or_else(|| build_dir.to_path_buf());

    let mut command = program(host, b"CMAKE_COMMAND", b"cmake");
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
    host: &dyn Host,
    rebuild: &[u8],
) {
    let mut install = program(host, b"CMAKE_COMMAND", b"cmake");
    let mut test = program(host, b"CMAKE_CTEST_COMMAND", b"ctest");
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
    if let Some(value) = host.env_get(name) {
        return Some(value);
    }
    let cache = host
        .read(
            &build_dir.join("CMakeCache.txt"),
            ReadRange::All {
                limit: 16 * 1024 * 1024,
            },
        )
        .ok()?;
    cache.split(|&byte| byte == b'\n').find_map(|line| {
        let entry = line.strip_prefix(name)?.strip_prefix(b":")?;
        let value = entry.splitn(2, |&byte| byte == b'=').nth(1)?;
        Some(BString::from(value.strip_suffix(b"\r").unwrap_or(value)))
    })
}

fn program(host: &dyn Host, var: &[u8], fallback: &[u8]) -> Vec<u8> {
    let mut command = Vec::new();
    let name = host.env_get(var).filter(|value| !value.is_empty());
    shell_word(
        name.as_ref()
            .map(|name| name.as_slice())
            .unwrap_or(fallback),
        &mut command,
    );
    command
}

fn generation_error(path: &Path, message: impl Into<BString>) -> GenError {
    GenError::Write {
        path: path::from_native(path),
        message: message.into(),
    }
}

#[cfg(test)]
mod tests;
