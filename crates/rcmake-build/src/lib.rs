//! In-process executor for a lowered [`BuildGraph`] (ADR-008).

#![forbid(unsafe_code)]

use std::collections::{BTreeMap, BTreeSet, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::time::Duration;

use rcmake_base::{BString, limits::Limits, path_lex::Lexical};
use rcmake_cxxmodules::collate::{CollationInput, collate, read_modules_json, write_modules_json};
use rcmake_cxxmodules::p1689::parse_ddi;
use rcmake_cxxmodules::tdi::write_tdi;
use rcmake_cxxmodules::{CxxModulesJson, ModulesError, P1689File};
use rcmake_host::{
    CommandSpec, Host, HostError, Input, Output, ProcessResult, ReadRange, RunRequest, WriteMode,
    path,
};
use rcmake_resolve::{BuildGraph, BuildStep, Invocation};

mod windows;

/// Options for one build.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BuildOptions {
    /// Build directory. Relative paths are resolved from the host's initial directory.
    pub build_dir: PathBuf,
    /// Targets to build (empty selects all targets marked for the default build).
    pub targets: Vec<BString>,
    /// Requested number of parallel steps. The executor runs steps in order.
    pub jobs: usize,
    /// Request to continue after a failure. The executor stops at the first failure.
    pub keep_going: bool,
    /// Include each command in the captured output.
    pub verbose: bool,
}

impl Default for BuildOptions {
    fn default() -> Self {
        Self {
            build_dir: PathBuf::new(),
            targets: Vec::new(),
            jobs: 1,
            keep_going: false,
            verbose: false,
        }
    }
}

/// Result of one build.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BuildOutcome {
    /// Whether every selected step succeeded.
    pub success: bool,
    /// Exit code of the first failed command, or zero on success.
    pub exit_code: i32,
    /// Number of steps that started.
    pub steps_run: usize,
    /// Combined standard output and standard error in command order.
    pub output: BString,
}

/// Error that prevents the executor from running a build step.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum BuildError {
    /// A requested target does not exist.
    #[error("unknown target {0}")]
    UnknownTarget(BString),
    /// The graph has a dependency cycle.
    #[error("dependency cycle")]
    Cycle,
    /// A dependency index does not identify a graph target.
    #[error("invalid target index {0}")]
    InvalidTarget(usize),
    /// A step index does not identify a graph step.
    #[error("invalid step index {0}")]
    InvalidStep(usize),
    /// The host could not perform a build operation.
    #[error(transparent)]
    Host(#[from] HostError),
    /// Module scans or collation produced an error.
    #[error(transparent)]
    Modules(#[from] ModulesError),
}

/// Run the selected build steps in dependency order through `host`.
///
/// A nonzero process status stops the build and returns its captured output.
pub fn execute(
    graph: &BuildGraph,
    host: &dyn Host,
    options: &BuildOptions,
) -> Result<BuildOutcome, BuildError> {
    let order = ordered_steps(graph, &options.targets)?;
    let mut output = Vec::new();
    let mut steps_run = 0;
    let mut exit_code = 0;
    if !order.is_empty() {
        let build_dir = host.initial_cwd().join(&options.build_dir);
        host.create_dir_all(&build_dir)?;
        let mut created_dirs = HashSet::new();
        created_dirs.insert(build_dir.clone());
        let base_env = host.env_all();
        let windows = host.system().sysname.eq_ignore_ascii_case(b"Windows");
        let scan_objects: BTreeMap<_, _> = order
            .iter()
            .filter_map(|step| match step {
                BuildStep::Scan { ddi, object, .. } => Some((ddi, object)),
                _ => None,
            })
            .collect();
        let mut compile_steps: BTreeMap<_, BTreeMap<_, _>> = BTreeMap::new();
        for &step in &order {
            if let BuildStep::Compile {
                object,
                dyndep: Some(dd),
                ..
            } = step
            {
                compile_steps.entry(dd).or_default().insert(object, step);
            }
        }
        for step in &order {
            if let BuildStep::Collate {
                tdi, depend_info, ..
            } = step
                && let rcmake_resolve::DependInfo::Fortran(info) = depend_info.as_ref()
            {
                let data = rcmake_resolve::lower::fortran_depend_info_json(info);
                write_module_file(host, &build_dir, tdi, &data, &mut created_dirs)?;
            }
        }
        let mut module_order: BTreeMap<_, VecDeque<_>> = BTreeMap::new();
        for step in order {
            let step = match step {
                BuildStep::Collate { ddis, dd, .. } => {
                    let scans = collate_modules(host, &build_dir, step, &mut created_dirs)?;
                    let compiles = compile_steps.remove(dd).unwrap_or_default();
                    if compiles.len() != ddis.len() {
                        return Err(module_error("Module scans and compile steps do not match."));
                    }
                    let sorted = module_compile_order(&scans)?
                        .into_iter()
                        .map(|index| {
                            ddis.get(index)
                                .and_then(|ddi| scan_objects.get(ddi))
                                .and_then(|object| compiles.get(object).copied())
                                .ok_or_else(|| module_error("A module scan has no compile step."))
                        })
                        .collect::<Result<VecDeque<_>, _>>()?;
                    module_order.insert(dd, sorted);
                    steps_run += 1;
                    continue;
                }
                BuildStep::Compile {
                    dyndep: Some(dd), ..
                } => module_order
                    .get_mut(dd)
                    .and_then(VecDeque::pop_front)
                    .ok_or_else(|| module_error("A compile step has no collated scan."))?,
                _ => step,
            };
            let device_invocations;
            let (outputs, invocations): (&[BString], &[Invocation]) = match step {
                BuildStep::Custom(step) => {
                    for byproduct in &step.byproducts {
                        create_output_parent(host, &build_dir, byproduct, &mut created_dirs)?;
                    }
                    (&step.outputs, &step.invocations)
                }
                BuildStep::Compile {
                    object,
                    byproducts,
                    invocation,
                    depfile,
                    ..
                } => {
                    if let Some(depfile) = depfile {
                        create_output_parent(host, &build_dir, depfile, &mut created_dirs)?;
                    }
                    for byproduct in byproducts {
                        create_output_parent(host, &build_dir, byproduct, &mut created_dirs)?;
                    }
                    (
                        std::slice::from_ref(object),
                        std::slice::from_ref(invocation),
                    )
                }
                BuildStep::Scan {
                    ddi,
                    invocation,
                    depfile,
                    preprocessed,
                    ..
                } => {
                    if let Some(depfile) = depfile {
                        create_output_parent(host, &build_dir, depfile, &mut created_dirs)?;
                    }
                    if let Some(preprocessed) = preprocessed {
                        create_output_parent(host, &build_dir, preprocessed, &mut created_dirs)?;
                    }
                    (std::slice::from_ref(ddi), std::slice::from_ref(invocation))
                }
                BuildStep::DeviceLink { output, mode, .. } => {
                    use rcmake_resolve::DeviceLinkMode;
                    device_invocations = match mode {
                        DeviceLinkMode::Driver { invocation, .. } => vec![invocation.clone()],
                        DeviceLinkMode::Explicit {
                            cubins,
                            fatbinary,
                            fatbin_header,
                            register_header,
                            compile,
                        } => {
                            for file in cubins
                                .iter()
                                .map(|cubin| &cubin.output)
                                .chain([fatbin_header, register_header])
                            {
                                create_output_parent(host, &build_dir, file, &mut created_dirs)?;
                            }
                            cubins
                                .iter()
                                .map(|cubin| cubin.invocation.clone())
                                .chain([fatbinary.clone(), compile.clone()])
                                .collect()
                        }
                    };
                    (std::slice::from_ref(output), &device_invocations)
                }
                BuildStep::Collate { .. } => {
                    return Err(module_error("Expected a compiler or scanner invocation."));
                }
                BuildStep::Link {
                    step, invocations, ..
                } => (std::slice::from_ref(&step.output), invocations),
            };
            for file in outputs {
                create_output_parent(host, &build_dir, file, &mut created_dirs)?;
            }
            steps_run += 1;
            for invocation in invocations {
                exit_code = run_with_env(
                    host,
                    &build_dir,
                    &base_env,
                    invocation,
                    windows,
                    options.verbose,
                    &mut output,
                    &mut created_dirs,
                )?;
                if exit_code != 0 {
                    break;
                }
            }
            if !invocations.is_empty() {
                // External commands may remove directories created by earlier steps.
                created_dirs.clear();
            }
            if exit_code != 0 {
                break;
            }
        }
    }
    Ok(BuildOutcome {
        success: exit_code == 0,
        exit_code,
        steps_run,
        output: output.into(),
    })
}

fn module_error(message: impl Into<BString>) -> BuildError {
    ModulesError::Invalid(message.into()).into()
}

fn collate_modules(
    host: &dyn Host,
    build_dir: &Path,
    step: &BuildStep,
    created_dirs: &mut HashSet<PathBuf>,
) -> Result<Vec<P1689File>, BuildError> {
    let BuildStep::Collate {
        ddis,
        dd,
        tdi,
        depend_info,
        modmap_format,
        ..
    } = step
    else {
        return Err(module_error("Expected a C++ module collation step."));
    };
    let limit = Limits::default().max_value_bytes as u64;
    let read = |file: &[u8]| {
        host.read(
            &build_dir.join(path::to_native(file)),
            ReadRange::All { limit },
        )
    };
    let read_table = |directory: &BString| -> Result<CxxModulesJson, BuildError> {
        let file = Lexical::new(directory).append(b"CXXModules.json");
        Ok(read_modules_json(&read(&file)?)?)
    };
    let rcmake_resolve::DependInfo::Cxx(info) = depend_info.as_ref() else {
        return ddis
            .iter()
            .map(|ddi| Ok(parse_ddi(&read(ddi)?, ddi)?))
            .collect();
    };
    let mut info = info.clone();
    let module_dir = Lexical::new(dd).parent();
    info.dir_top_bld = path::from_native(build_dir);
    info.module_dir = Lexical::new(&info.dir_top_bld).append(module_dir);
    write_module_file(host, build_dir, tdi, &write_tdi(&info)?, created_dirs)?;
    let input = CollationInput {
        linked: info
            .linked_target_dirs
            .iter()
            .map(read_table)
            .collect::<Result<_, _>>()?,
        forwarded: info
            .forward_modules_from_target_dirs
            .iter()
            .map(read_table)
            .collect::<Result<_, _>>()?,
        native: info
            .native_target_dir
            .as_ref()
            .map(read_table)
            .transpose()?,
        tdi: info,
        ddis: ddis
            .iter()
            .map(|ddi| Ok(parse_ddi(&read(ddi)?, ddi)?))
            .collect::<Result<_, BuildError>>()?,
        modmap_format: *modmap_format,
    };
    let output = collate(&input)?;
    if !output.diagnostics.is_empty() {
        let message = output
            .diagnostics
            .iter()
            .map(|message| message.as_slice())
            .collect::<Vec<_>>()
            .join(b"\n".as_slice());
        return Err(module_error(message));
    }
    for (file, contents) in output.modmaps {
        write_module_file(host, build_dir, &file, &contents, created_dirs)?;
    }
    let modules = Lexical::new(module_dir).append(b"CXXModules.json");
    write_module_file(
        host,
        build_dir,
        &modules,
        &write_modules_json(&output.modules)?,
        created_dirs,
    )?;
    write_module_file(host, build_dir, dd, &output.dyndep, created_dirs)?;
    Ok(input.ddis)
}

fn write_module_file(
    host: &dyn Host,
    build_dir: &Path,
    file: &[u8],
    contents: &[u8],
    created_dirs: &mut HashSet<PathBuf>,
) -> Result<(), HostError> {
    create_output_parent(host, build_dir, file, created_dirs)?;
    host.write(
        &build_dir.join(path::to_native(file)),
        contents,
        WriteMode::Truncate,
    )
}

fn module_compile_order(scans: &[P1689File]) -> Result<Vec<usize>, BuildError> {
    let rules: Vec<_> = scans.iter().filter_map(|scan| scan.rules.first()).collect();
    let providers: BTreeMap<_, _> = rules
        .iter()
        .enumerate()
        .flat_map(|(index, rule)| {
            rule.provides
                .iter()
                .map(move |module| (&module.logical_name, index))
        })
        .collect();
    let mut dependents = vec![Vec::new(); rules.len()];
    let mut pending = Vec::with_capacity(rules.len());
    let mut ready = BTreeSet::new();
    for (index, rule) in rules.iter().enumerate() {
        let dependencies: BTreeSet<_> = rule
            .requires
            .iter()
            .filter_map(|module| providers.get(&module.logical_name).copied())
            .filter(|provider| *provider != index)
            .collect();
        pending.push(dependencies.len());
        if dependencies.is_empty() {
            ready.insert(index);
        }
        for provider in dependencies {
            if let Some(dependents) = dependents.get_mut(provider) {
                dependents.push(index);
            }
        }
    }
    let mut order = Vec::with_capacity(rules.len());
    while let Some(index) = ready.pop_first() {
        order.push(index);
        for &dependent in dependents.get(index).into_iter().flatten() {
            if let Some(count) = pending.get_mut(dependent) {
                *count -= 1;
                if *count == 0 {
                    ready.insert(dependent);
                }
            }
        }
    }
    if order.len() != scans.len() {
        return Err(BuildError::Cycle);
    }
    Ok(order)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Visit {
    Pending,
    Active,
    Done,
}

fn ordered_steps<'a>(
    graph: &'a BuildGraph,
    requested: &[BString],
) -> Result<Vec<&'a BuildStep>, BuildError> {
    let mut roots = Vec::new();
    if requested.is_empty() {
        roots.extend(
            graph
                .targets
                .iter()
                .enumerate()
                .filter_map(|(id, target)| target.in_all.then_some(id)),
        );
    } else {
        let names: BTreeMap<_, _> = graph
            .targets
            .iter()
            .enumerate()
            .map(|(id, target)| (&target.name, id))
            .collect();
        for name in requested {
            roots.push(
                *names
                    .get(name)
                    .ok_or_else(|| BuildError::UnknownTarget(name.clone()))?,
            );
        }
    }

    let mut visits = vec![Visit::Pending; graph.targets.len()];
    let mut emitted = vec![false; graph.steps.len()];
    let mut order = Vec::new();
    for root in roots {
        let mut stack = vec![(root, false)];
        while let Some((id, leaving)) = stack.pop() {
            let visit = visits.get_mut(id).ok_or(BuildError::InvalidTarget(id))?;
            let target = graph.targets.get(id).ok_or(BuildError::InvalidTarget(id))?;
            if leaving {
                *visit = Visit::Done;
                for &step in &target.steps {
                    let seen = emitted.get_mut(step).ok_or(BuildError::InvalidStep(step))?;
                    if !*seen {
                        order.push(graph.steps.get(step).ok_or(BuildError::InvalidStep(step))?);
                        *seen = true;
                    }
                }
                continue;
            }
            match visit {
                Visit::Done => {}
                Visit::Active => return Err(BuildError::Cycle),
                Visit::Pending => {
                    *visit = Visit::Active;
                    stack.push((id, true));
                    stack.extend(target.depends.iter().rev().map(|&dep| (dep, false)));
                }
            }
        }
    }
    Ok(order)
}

fn create_dir_cached(
    host: &dyn Host,
    dir: &Path,
    created_dirs: &mut HashSet<PathBuf>,
) -> Result<(), HostError> {
    if !created_dirs.contains(dir) {
        host.create_dir_all(dir)?;
        created_dirs.insert(dir.to_path_buf());
    }
    Ok(())
}

fn create_output_parent(
    host: &dyn Host,
    build_dir: &Path,
    file: &[u8],
    created_dirs: &mut HashSet<PathBuf>,
) -> Result<(), HostError> {
    if let Some(parent) = build_dir.join(path::to_native(file)).parent() {
        create_dir_cached(host, parent, created_dirs)?;
    }
    Ok(())
}

#[cfg(test)]
fn run(
    host: &dyn Host,
    build_dir: &Path,
    invocation: &Invocation,
    windows: bool,
    verbose: bool,
    output: &mut Vec<u8>,
) -> Result<i32, HostError> {
    let base_env = host.env_all();
    let mut created_dirs = HashSet::new();
    run_with_env(
        host,
        build_dir,
        &base_env,
        invocation,
        windows,
        verbose,
        output,
        &mut created_dirs,
    )
}

#[allow(clippy::too_many_arguments)]
fn run_with_env(
    host: &dyn Host,
    build_dir: &Path,
    base_env: &[(BString, BString)],
    invocation: &Invocation,
    windows: bool,
    verbose: bool,
    output: &mut Vec<u8>,
    created_dirs: &mut HashSet<PathBuf>,
) -> Result<i32, HostError> {
    let cwd = invocation.working_dir.as_ref().map_or_else(
        || build_dir.to_path_buf(),
        |dir| {
            if windows {
                windows::absolute(build_dir, dir)
            } else {
                build_dir.join(path::to_native(dir))
            }
        },
    );
    let mut script = None;
    let batch = windows.then(|| windows::batch_rule(invocation)).flatten();
    let (program, args) = if let Some((launcher, shell)) = batch {
        let (command, file) = windows::shell_command(host, &cwd, base_env, shell, created_dirs)?;
        script = Some(file);
        if let Some((program, args)) = launcher.split_first() {
            let mut args = args.to_vec();
            args.push(command.program);
            args.extend(command.args);
            (program.clone(), args)
        } else {
            (command.program, command.args)
        }
    } else if let Some((program, args)) = invocation.argv.split_first() {
        if windows {
            windows::prepare_outputs(host, &cwd, args, created_dirs)?;
        }
        (program.clone(), args.to_vec())
    } else {
        (
            "/bin/sh".into(),
            vec!["-c".into(), invocation.shell.clone()],
        )
    };
    if verbose {
        if invocation.shell.is_empty() {
            output.extend_from_slice(&program);
            for arg in &args {
                output.push(b' ');
                output.extend_from_slice(arg);
            }
        } else {
            output.extend_from_slice(&invocation.shell);
        }
        output.push(b'\n');
    }
    if let [e, rm, flag, files @ ..] = args.as_slice()
        && e.as_slice() == b"-E"
        && rm.as_slice() == b"rm"
        && flag.as_slice() == b"-f"
    {
        for file in files {
            let target = if windows {
                windows::absolute(&cwd, file)
            } else {
                cwd.join(path::to_native(file))
            };
            if let Err(err) = host.remove_file(&target)
                && err.kind != std::io::ErrorKind::NotFound
            {
                return Err(err);
            }
        }
        return Ok(0);
    }
    let result = host.run(
        &RunRequest {
            pipeline: vec![CommandSpec { program, args }],
            cwd,
            env: base_env.to_vec(),
            stdin: Input::Null,
            stdout: Output::Capture,
            stderr: Output::SharedWithStdout,
            timeout: Some(Duration::from_secs(120)),
            tee: false,
            default_error_mode: false,
        },
        &mut |_, _| {},
    );
    let cleanup = script.map(|file| host.remove_file(&file)).transpose();
    let result = result?;
    cleanup?;
    output.extend_from_slice(&result.stdout);
    output.extend_from_slice(&result.stderr);
    match result.results.first() {
        Some(ProcessResult::Exited(code)) => Ok(*code),
        Some(ProcessResult::Signaled(signal)) => Ok(128_i32.saturating_add(*signal)),
        Some(ProcessResult::TimedOut) => {
            output.extend_from_slice(b"Build command timed out.\n");
            Ok(124)
        }
        Some(ProcessResult::SpawnFailed(error)) => {
            output.extend_from_slice(error.to_string().as_bytes());
            output.push(b'\n');
            Ok(127)
        }
        None => Err(HostError::unsupported(
            "Build command returned no process result.",
        )),
    }
}

#[cfg(test)]
mod tests;
