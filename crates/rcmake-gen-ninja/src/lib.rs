//! Ninja and Ninja Multi-Config manifest generation from the ADR-009 build graph.

#![forbid(unsafe_code)]

mod build;
mod clean;
mod commands;
mod compile_db;
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
use std::path::{Path, PathBuf};

use rcmake_base::BString;
use rcmake_gen_common::compile_commands_native::MultiConfigCompileDatabase;
use rcmake_gen_common::export::write_module_exports;
use rcmake_gen_common::{GenError, write_if_changed};
use rcmake_host::{FileKind, Follow, Host, WriteMode, path};
use rcmake_model::{DirId, ProjectModel, TargetId};
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
  generator = 1
  restat = 1

rule CMAKE_SYMLINK_LIBRARY
  command = $COMMAND
  description = $DESC

rule CMAKE_SYMLINK_EXECUTABLE
  command = $COMMAND
  description = $DESC

rule RERUN_CMAKE
  command = $COMMAND
  description = Re-running CMake...
  generator = 1
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
    let mut graph = Graph::with_context(
        input,
        &artifacts,
        build_dir,
        None,
        &prefix,
        commands.clone(),
        model,
        Some(result),
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
    let mut manifests = vec![graph.node(b"build.ninja")];
    manifests.extend(exports.iter().map(|job| graph.node(&job.script)));
    manifests.extend(
        result
            .metadata
            .generated_files
            .iter()
            .map(|file| graph.node(file)),
    );
    add_install_scripts(&mut manifests, &graph, result, model);
    add_ctest_files(&mut manifests, &graph, model);
    if uses_modules || fortran::uses_fortran(input) {
        manifests.extend(
            modules::write_metadata(result, model, &[input], host, build_dir, false)?
                .iter()
                .map(|file| graph.node(file)),
        );
    }
    let has_cache = host
        .metadata(&build_dir.join("CMakeCache.txt"), Follow::Yes)
        .is_ok();
    let regenerate = configure_command(result, build_dir, &programs.cmake, &commands, true);
    let mut ninja = graph.manifest();
    ninja.include(&graph.node(RULES_PATH.as_bytes()));
    graph.emit(&mut ninja, &programs.cmake, &module_flag, &exports)?;
    graph.emit_entry_aliases(&mut ninja);
    graph.emit_assumed_sources(&mut ninja, model);
    prep.emit(&mut ninja, &graph);
    let regen_pos = ninja.0.len();
    emit_regeneration(
        &mut ninja,
        &graph,
        host,
        &manifests,
        result,
        &regenerate,
        verify_globs.then_some(programs.cmake.as_slice()),
        has_cache,
    );
    emit_start_instrument_edges(
        &mut ninja, &graph, regen_pos, result, model, host, build_dir,
    );
    emit_utilities(
        &mut ninja,
        &graph,
        b"build.ninja",
        &programs,
        &regenerate,
        install::Scope::Single,
    );
    if graph.prefix.is_empty() {
        ninja.0.extend_from_slice(b"default all\n");
    }

    if !graph.clean_files.is_empty() {
        clean::write_clean_additional_script(host, build_dir, &[&graph])?;
    }
    write_rules(
        result,
        host,
        build_dir,
        uses_modules,
        false,
        model,
        &commands,
        &programs.cmake,
        &programs.ninja,
        !graph.clean_files.is_empty(),
    )?;
    write_always(host, &build_dir.join("build.ninja"), &ninja.0)?;
    refresh_manifest_outputs(
        host,
        build_dir,
        graph.prefix,
        &manifests,
        &[graph.node(b"build.ninja")],
    );
    Ok(())
}

pub(crate) fn refresh_manifest_outputs(
    host: &dyn Host,
    build_dir: &Path,
    prefix: &[u8],
    manifests: &[BString],
    written: &[BString],
) {
    let manifest_path = |node: &BString| -> std::path::PathBuf {
        let stripped = if !prefix.is_empty() {
            node.strip_prefix(prefix)
                .and_then(|rest| rest.strip_prefix(b"/"))
                .unwrap_or(node.as_slice())
        } else {
            node.as_slice()
        };
        let native = path::to_native(stripped);
        if native.is_absolute() {
            native
        } else {
            build_dir.join(native)
        }
    };
    let mut target_mtime = None;
    for candidate in written.iter().map(&manifest_path).chain([
        build_dir.join(rcmake_gen_common::glob::STAMP),
        build_dir.join(rcmake_gen_common::glob::SCRIPT),
    ]) {
        if let Ok(meta) = host.metadata(&candidate, Follow::Yes) {
            target_mtime = Some(match target_mtime {
                Some(current) if current >= meta.mtime => current,
                _ => meta.mtime,
            });
        }
    }
    let Some(target_mtime) = target_mtime else {
        return;
    };
    for node in manifests {
        if written.contains(node) {
            continue;
        }
        let path = manifest_path(node);
        if let Ok(meta) = host.metadata(&path, Follow::Yes)
            && meta.mtime < target_mtime
        {
            let _ = host.set_times(&path, None, Some(target_mtime));
        }
    }
}

pub(crate) fn write_always(host: &dyn Host, target: &Path, content: &[u8]) -> Result<(), GenError> {
    host.write(target, content, WriteMode::Truncate)
        .map_err(|error| GenError::Write {
            path: path::from_native(target),
            message: error.to_string().into(),
        })
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
    multi::generate_multi(result, host, build_dir, None, Commands::new(host), None).map(drop)
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
    multi::generate_multi(
        result,
        host,
        build_dir,
        Some(model),
        Commands::new(host),
        None,
    )
    .map(drop)
}

/// Write Ninja Multi-Config manifests and, when `export_targets` is `Some`, collect
/// per-configuration native compile database rows using the generator's selected default
/// configuration.
///
/// # Errors
///
/// Returns an error if input resolution fails or the host cannot write a file.
pub fn generate_multi_config_with_compile_db(
    result: &GenerationResult,
    host: &dyn Host,
    build_dir: &Path,
    model: &ProjectModel,
    export_targets: Option<&HashSet<TargetId>>,
) -> Result<Option<MultiConfigCompileDatabase>, GenError> {
    multi::generate_multi(
        result,
        host,
        build_dir,
        Some(model),
        Commands::new(host),
        export_targets,
    )
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

#[allow(clippy::too_many_arguments)]
#[expect(
    clippy::too_many_arguments,
    reason = "Rule generation gathers generator metadata, programs, and clean settings."
)]
fn write_rules(
    result: &GenerationResult,
    host: &dyn Host,
    build_dir: &Path,
    uses_modules: bool,
    multi_config: bool,
    model: Option<&ProjectModel>,
    commands: &Commands,
    cmake_cmd: &[u8],
    ninja_cmd: &[u8],
    custom_clean: bool,
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
    ninja.0.extend_from_slice(b"\nrule CLEAN\n");
    let mut clean_cmd = ninja_cmd.to_vec();
    clean_cmd.extend_from_slice(b" $FILE_ARG -t clean $TARGETS");
    ninja.0.extend_from_slice(b"  command = ");
    ninja.0.extend_from_slice(&clean_cmd);
    ninja
        .0
        .extend_from_slice(b"\n  description = Cleaning all built files...\n");
    let prefix = output_prefix(model, commands);
    if custom_clean {
        let script = if prefix.is_empty() {
            BString::from("CMakeFiles/clean_additional.cmake")
        } else {
            commands.path(&[prefix.as_slice(), b"/CMakeFiles/clean_additional.cmake"].concat())
        };
        let mut quoted_script = Vec::new();
        commands.word(&script, &mut quoted_script);
        ninja
            .0
            .extend_from_slice(b"\nrule CLEAN_ADDITIONAL\n  command = ");
        ninja.0.extend_from_slice(cmake_cmd);
        ninja.0.extend_from_slice(b" -DCONFIG=$CONFIG -P ");
        for &byte in &quoted_script {
            if byte == b'$' {
                ninja.0.push(b'$');
            }
            ninja.0.push(byte);
        }
        ninja
            .0
            .extend_from_slice(b"\n  description = Cleaning additional files...\n");
    }
    ninja.0.extend_from_slice(b"\nrule HELP\n");
    ninja.0.extend_from_slice(b"  command = ");
    ninja.0.extend_from_slice(ninja_cmd);
    ninja
        .0
        .extend_from_slice(b" -t targets\n  description = All primary targets available:\n");
    if result
        .graphs
        .iter()
        .flat_map(|graph| &graph.steps)
        .any(|step| matches!(step, rcmake_resolve::BuildStep::DeviceLink { .. }))
    {
        ninja.0.extend_from_slice(cuda::RULES.as_bytes());
    }
    response::emit_rules(&mut ninja);
    instrumented::emit_rules(&mut ninja, result, build_dir, !prefix.is_empty(), commands);
    if has_instrumentation(result, model, host, build_dir) {
        emit_start_instrument_rule(&mut ninja, result, host, build_dir, commands, cmake_cmd);
    }
    modules::emit_synthetic_rules(&mut ninja, result, model, multi_config, commands.windows());
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

fn add_ctest_files(outputs: &mut Vec<BString>, graph: &Graph<'_>, model: Option<&ProjectModel>) {
    let Some(model) = model else {
        return;
    };
    let root_testing = model
        .definition(DirId::ROOT, b"CMAKE_TESTING_ENABLED")
        .is_some_and(|value| rcmake_base::truth::is_on(value));
    let mut seen: HashSet<_> = outputs.iter().cloned().collect();
    for (id, dir) in model.dirs() {
        let testing_enabled = model
            .definition(id, b"CMAKE_TESTING_ENABLED")
            .is_some_and(|value| rcmake_base::truth::is_on(value))
            || (dir.vars.is_empty() && root_testing)
            || dir
                .props
                .get(b"CMAKE_TESTING_ENABLED")
                .is_some_and(|value| rcmake_base::truth::is_on(value));
        if !testing_enabled {
            continue;
        }
        let dir_lex = rcmake_base::path_lex::Lexical::new(&dir.binary_dir);
        let file = graph.node(&dir_lex.append(b"CTestTestfile.cmake"));
        if seen.insert(file.clone()) {
            outputs.push(file);
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
    source_dir: Option<BString>,
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
            source_dir: model.map(|model| model.dir(DirId::ROOT).source_dir.clone()),
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

fn is_instrumented_invocation(invocation: &rcmake_resolve::Invocation) -> bool {
    invocation
        .argv
        .get(1)
        .is_some_and(|arg| arg == b"--instrument")
        || (invocation.argv.get(1).is_some_and(|arg| arg == b"-E")
            && invocation
                .argv
                .get(2)
                .is_some_and(|arg| arg == b"__instrument"))
}

fn has_json_query_files(host: &dyn Host, dir: &Path) -> bool {
    let Ok(entries) = host.read_dir(dir) else {
        return false;
    };
    entries.into_iter().any(|entry| {
        matches!(entry.kind, FileKind::File | FileKind::Symlink)
            && !entry.name.starts_with(b".")
            && entry.name.ends_with(b".json")
    })
}

fn user_instrumentation_query_dir(host: &dyn Host) -> Option<PathBuf> {
    let env = |name: &[u8]| {
        host.env_get(name)
            .filter(|value| !value.is_empty())
            .map(|value| path::to_native(&value))
    };
    let config = env(b"CMAKE_CONFIG_DIR").or_else(|| match host.system().sysname.as_slice() {
        b"Windows" | b"Windows_NT" => env(b"LOCALAPPDATA").map(|dir| dir.join("CMake")),
        b"Darwin" | b"macOS" => env(b"XDG_CONFIG_HOME")
            .or_else(|| env(b"HOME").map(|dir| dir.join("Library/Application Support")))
            .map(|dir| dir.join("CMake")),
        _ => env(b"XDG_CONFIG_HOME")
            .or_else(|| env(b"HOME").map(|dir| dir.join(".config")))
            .map(|dir| dir.join("cmake")),
    })?;
    let abs = if config.is_absolute() {
        config
    } else {
        host.initial_cwd().join(config)
    };
    Some(abs.join("instrumentation/v1/query"))
}

fn has_instrumentation(
    result: &GenerationResult,
    model: Option<&ProjectModel>,
    host: &dyn Host,
    build_dir: &Path,
) -> bool {
    if model.is_some_and(|model| {
        !model.global.instrumentation.is_empty()
            || model
                .global
                .props
                .get(b"_RCMAKE_INSTRUMENTATION_QUERIES")
                .is_some()
    }) {
        return true;
    }
    let query_dir = build_dir.join(".cmake/instrumentation/v1/query");
    if has_json_query_files(host, &query_dir)
        || has_json_query_files(host, &query_dir.join("generated"))
        || user_instrumentation_query_dir(host).is_some_and(|dir| has_json_query_files(host, &dir))
    {
        return true;
    }
    result
        .graphs
        .iter()
        .flat_map(|graph| &graph.steps)
        .any(|step| match step {
            BuildStep::Compile { invocation, .. } => is_instrumented_invocation(invocation),
            BuildStep::Link { invocations, .. } => {
                invocations.iter().any(is_instrumented_invocation)
            }
            BuildStep::Custom(custom) => custom.invocations.iter().any(is_instrumented_invocation),
            _ => false,
        })
}

pub(crate) fn emit_start_instrument_edges(
    ninja: &mut Ninja,
    graph: &Graph<'_>,
    regen_pos: usize,
    result: &GenerationResult,
    model: Option<&ProjectModel>,
    host: &dyn Host,
    build_dir: &Path,
) {
    if !has_instrumentation(result, model, host, build_dir) {
        return;
    }
    let phony_node = graph.node(b"CMakeFiles/instrument");
    let start_node = graph.node(b"start_instrument");
    insert_rerun_input(&mut ninja.0, regen_pos, &start_node);
    ninja.phony(&phony_node, &[]);
    ninja.edge(
        b"START_INSTRUMENT",
        Edge {
            outputs: std::slice::from_ref(&start_node),
            inputs: std::slice::from_ref(&phony_node),
            ..Edge::default()
        },
    );
}

fn escape_ninja_command_arg(commands: &Commands, word: &[u8], out: &mut Vec<u8>) {
    let mut quoted = Vec::new();
    commands.word(word, &mut quoted);
    for byte in quoted {
        if byte == b'$' {
            out.push(b'$');
        }
        out.push(byte);
    }
}

fn emit_start_instrument_rule(
    ninja: &mut Ninja,
    result: &GenerationResult,
    host: &dyn Host,
    build_dir: &Path,
    commands: &Commands,
    cmake_cmd: &[u8],
) {
    ninja
        .0
        .extend_from_slice(b"\nrule START_INSTRUMENT\n  command = ");
    if !commands.windows() {
        ninja.0.extend_from_slice(b"exec ");
    }
    let ctest = result.metadata.ctest_command.as_ref().filter(|ctest| {
        host.metadata(&path::to_native(ctest), Follow::Yes)
            .is_ok_and(|meta| meta.kind == FileKind::File)
    });
    if let Some(ctest) = ctest {
        escape_ninja_command_arg(commands, ctest, &mut ninja.0);
        ninja
            .0
            .extend_from_slice(b" --instrument --start-instrumentation ");
    } else {
        for &byte in cmake_cmd {
            if byte == b'$' {
                ninja.0.push(b'$');
            }
            ninja.0.push(byte);
        }
        ninja
            .0
            .extend_from_slice(b" -E __instrument --start-instrumentation ");
    }
    let build = path::from_native_cmake(build_dir);
    escape_ninja_command_arg(commands, &build, &mut ninja.0);
    ninja
        .0
        .extend_from_slice(b"\n  description = Collecting build metrics\n  restat = 1\n");
}

fn insert_rerun_input(buf: &mut Vec<u8>, start: usize, node: &BString) {
    const MARKER: &[u8] = b": RERUN_CMAKE";
    let Some(tail) = buf.get(start..) else {
        return;
    };
    let Some(offset) = tail
        .windows(MARKER.len())
        .position(|window| window == MARKER)
    else {
        return;
    };
    let insert_at = start + offset + MARKER.len();
    let mut encoded = Vec::with_capacity(node.len() + 1);
    encoded.push(b' ');
    for &byte in node.as_slice() {
        match byte {
            b'$' | b' ' | b':' | b'\n' => encoded.extend_from_slice(&[b'$', byte]),
            b'|' => encoded.extend_from_slice(b"${ninja_pipe}"),
            _ => encoded.push(byte),
        }
    }
    buf.splice(insert_at..insert_at, encoded);
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod windows_tests;
