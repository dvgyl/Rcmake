use rcmake_base::BString;
use rcmake_eval::{CmdError, Cx, TryBuildOutcome, TryCompileEvent, TryRunOutcome};
use rcmake_host::path;

use super::ProbeDirs;
use super::args::Options;
use super::run::RunResult;

/// Add the `try_compile-v1` or `try_run-v1` event of one probe to the configure log.
#[allow(clippy::too_many_arguments)]
pub(super) fn log_probe(
    cx: &mut Cx<'_>,
    dirs: &ProbeDirs,
    options: &Options<'_>,
    variables: &[(BString, BString)],
    build_var: &[u8],
    exit_code: i32,
    output: &[u8],
    run: Option<(&[u8], Option<&RunResult>)>,
) -> Result<(), CmdError> {
    let cached = !options.has(b"NO_CACHE");
    let merged = options.has(b"RUN_OUTPUT_VARIABLE");
    let source = path::from_native(&dirs.source);
    let binary = path::from_native(&dirs.binary);
    let run = run.map(|(variable, result)| TryRunOutcome {
        variable,
        cached,
        stdout: result.map(|result| result.stdout.as_slice()),
        stderr: result
            .filter(|_| !merged)
            .map(|result| result.stderr.as_slice()),
        exit_code: result.map(|result| result.value.as_slice()),
    });
    cx.log_configure_try_compile(TryCompileEvent {
        kind: if run.is_some() {
            "try_run-v1"
        } else {
            "try_compile-v1"
        },
        description: options.one(b"LOG_DESCRIPTION"),
        source_dir: &source,
        binary_dir: &binary,
        variables,
        build: TryBuildOutcome {
            variable: build_var,
            cached,
            stdout: output,
            exit_code,
        },
        run,
    });
    Ok(())
}
