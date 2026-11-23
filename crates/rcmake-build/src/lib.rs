//! In-process executor for a lowered [`BuildGraph`] (ADR-008).

#![forbid(unsafe_code)]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use rcmake_base::BString;
use rcmake_host::{CommandSpec, Host, HostError, Input, Output, ProcessResult, RunRequest, path};
use rcmake_resolve::{BuildGraph, BuildStep, Invocation};

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
    /// The graph has a step kind that the executor can not run yet.
    #[error("{0} is not supported yet")]
    Unsupported(&'static str),
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
        for step in order {
            let (outputs, invocations): (&[BString], &[Invocation]) = match step {
                BuildStep::Custom(step) => {
                    for byproduct in &step.byproducts {
                        create_output_parent(host, &build_dir, byproduct)?;
                    }
                    (&step.outputs, &step.invocations)
                }
                BuildStep::Compile {
                    object,
                    invocation,
                    depfile,
                    ..
                } => {
                    if let Some(depfile) = depfile {
                        create_output_parent(host, &build_dir, depfile)?;
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
                    ..
                } => {
                    if let Some(depfile) = depfile {
                        create_output_parent(host, &build_dir, depfile)?;
                    }
                    (std::slice::from_ref(ddi), std::slice::from_ref(invocation))
                }
                BuildStep::Collate { .. } => {
                    return Err(BuildError::Unsupported("C++ module collation"));
                }
                BuildStep::Link {
                    step, invocations, ..
                } => (std::slice::from_ref(&step.output), invocations),
            };
            for file in outputs {
                create_output_parent(host, &build_dir, file)?;
            }
            steps_run += 1;
            for invocation in invocations {
                exit_code = run(host, &build_dir, invocation, options.verbose, &mut output)?;
                if exit_code != 0 {
                    break;
                }
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

fn create_output_parent(host: &dyn Host, build_dir: &Path, file: &[u8]) -> Result<(), HostError> {
    if let Some(parent) = build_dir.join(path::to_native(file)).parent() {
        host.create_dir_all(parent)?;
    }
    Ok(())
}

fn run(
    host: &dyn Host,
    build_dir: &Path,
    invocation: &Invocation,
    verbose: bool,
    output: &mut Vec<u8>,
) -> Result<i32, HostError> {
    let (program, args) = match invocation.argv.split_first() {
        Some((program, args)) => (program.clone(), args.to_vec()),
        None => (
            "/bin/sh".into(),
            vec!["-c".into(), invocation.shell.clone()],
        ),
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
    let cwd = invocation.working_dir.as_ref().map_or_else(
        || build_dir.to_path_buf(),
        |dir| build_dir.join(path::to_native(dir)),
    );
    if let [e, rm, flag, files @ ..] = args.as_slice()
        && e.as_slice() == b"-E"
        && rm.as_slice() == b"rm"
        && flag.as_slice() == b"-f"
    {
        for file in files {
            let target = cwd.join(path::to_native(file));
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
            env: host.env_all(),
            stdin: Input::Null,
            stdout: Output::Capture,
            stderr: Output::SharedWithStdout,
            timeout: Some(Duration::from_secs(120)),
            tee: false,
        },
        &mut |_, _| {},
    )?;
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
