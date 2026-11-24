use std::path::{Path, PathBuf};

use rcmake_base::{BString, list};
use rcmake_build::{BuildOptions, BuildOutcome};
use rcmake_eval::Cx;
use rcmake_genex::ConfigId;
use rcmake_host::{CommandSpec, Input, Output, ProcessResult, RunRequest, path};
use rcmake_model::DirId;
use rcmake_resolve::BuildStep;

use super::args::Options;
use super::seed::toolchain_variable;
use super::{TIMEOUT, absolute, set_result};

pub(super) fn build_child(
    child: &mut Cx<'_>,
    directory: &Path,
    target: Option<&[u8]>,
    verbose: bool,
) -> Result<(BuildOutcome, Option<PathBuf>), String> {
    let config = ConfigId(child.var_or_empty(b"CMAKE_BUILD_TYPE"));
    let mut model = child.project().clone();
    // Lowering reads toolchain variables from the directory model or cache.
    for name in child.variable_names() {
        if toolchain_variable(&name) || name == "CMAKE_COMMAND" {
            let value = child.var_or_empty(&name);
            model.dir_mut(DirId::ROOT).props.set(name, value);
        }
    }
    let graph = rcmake_resolve::resolve(&model, child.host(), std::slice::from_ref(&config))
        .map_err(|error| error.to_string())?
        .graphs
        .into_iter()
        .next()
        .ok_or_else(|| "The resolver returned no build graph.".to_owned())?;
    let artifact = target
        .and_then(|name| {
            graph
                .targets
                .iter()
                .find(|target| target.name.as_slice() == name)
        })
        .and_then(|target| {
            target
                .steps
                .iter()
                .rev()
                .find_map(|&id| match graph.steps.get(id) {
                    Some(BuildStep::Link { step, .. }) => {
                        Some(directory.join(path::to_native(&step.output)))
                    }
                    _ => None,
                })
        });
    let options = BuildOptions {
        build_dir: directory.to_path_buf(),
        targets: target.into_iter().map(BString::from).collect(),
        verbose,
        ..BuildOptions::default()
    };
    let outcome =
        rcmake_build::execute(&graph, child.host(), &options).map_err(|error| error.to_string())?;
    Ok((outcome, artifact))
}

pub(super) struct RunResult<'a> {
    pub(super) variable: &'a [u8],
    pub(super) value: BString,
    pub(super) stdout: Vec<u8>,
    pub(super) stderr: Vec<u8>,
}

pub(super) fn run_executable<'a>(
    cx: &mut Cx<'_>,
    options: &Options<'_>,
    artifact: Option<&Path>,
    variable: &'a [u8],
    build_output: &[u8],
) -> RunResult<'a> {
    let default_cwd = cx.host().initial_cwd();
    let working_dir = options
        .one(b"WORKING_DIRECTORY")
        .map(|dir| absolute(&default_cwd, dir));
    let combined = options.has(b"RUN_OUTPUT_VARIABLE") || options.has(b"OUTPUT_VARIABLE");
    let mut command = Vec::new();
    let cross = cx.var_is_on(b"CMAKE_CROSSCOMPILING");
    if cross {
        command = list::split(
            &cx.var_or_empty(b"CMAKE_CROSSCOMPILING_EMULATOR"),
            list::Empty::Drop,
        );
    }
    let result = if let Some(artifact) = artifact
        && (!cross || !command.is_empty())
    {
        command.push(path::from_native(artifact));
        command.extend(options.many(b"ARGS").iter().copied().map(BString::from));
        command.split_first().and_then(|(program, args)| {
            if let Some(dir) = &working_dir {
                cx.host().create_dir_all(dir).ok()?;
            }
            cx.host()
                .run(
                    &RunRequest {
                        pipeline: vec![CommandSpec {
                            program: program.clone(),
                            args: args.to_vec(),
                        }],
                        cwd: working_dir.unwrap_or(default_cwd),
                        env: cx.host().env_all(),
                        stdin: Input::Null,
                        stdout: Output::Capture,
                        stderr: if combined {
                            Output::SharedWithStdout
                        } else {
                            Output::Capture
                        },
                        timeout: Some(TIMEOUT),
                        tee: false,
                    },
                    &mut |_, _| {},
                )
                .ok()
        })
    } else {
        None
    }
    .unwrap_or_default();

    let value: BString = match result.results.first() {
        Some(ProcessResult::Exited(code)) => code.to_string().into(),
        _ => "FAILED_TO_RUN".into(),
    };
    set_result(cx, variable, value.clone(), options.has(b"NO_CACHE"));
    for (key, output) in [
        (b"RUN_OUTPUT_VARIABLE".as_slice(), result.stdout.as_slice()),
        (b"RUN_OUTPUT_STDOUT_VARIABLE", result.stdout.as_slice()),
        (b"RUN_OUTPUT_STDERR_VARIABLE", result.stderr.as_slice()),
    ] {
        if let Some(var) = options.one(key) {
            cx.set_var(var, output.into());
        }
    }
    if let Some(var) = options.one(b"OUTPUT_VARIABLE") {
        cx.set_var(
            var,
            [
                build_output,
                result.stdout.as_slice(),
                result.stderr.as_slice(),
            ]
            .concat()
            .into(),
        );
    }
    RunResult {
        variable,
        value,
        stdout: result.stdout,
        stderr: result.stderr,
    }
}
