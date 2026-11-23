//! Run and record a command for `ctest --instrument`.

use std::path::{Path, PathBuf};
use std::time::Duration;

use rcmake_base::BString;
use rcmake_host::{
    CommandSpec, Follow, Host, Input, Output, ProcessResult, RunOutput, RunRequest, path,
};

use crate::files::{
    absolute, api_dir, epoch_millis, newest_content, publish_at, read_json, timestamp,
};
use crate::query::read_queries;
use crate::snippet::{SnippetDetails, SystemSample, write_snippet_with_details};
use crate::{InstrumentError, InstrumentOption, Snippet, SnippetRole, invalid};

pub(crate) fn execute(
    host: &dyn Host,
    cwd: PathBuf,
    command: &[BString],
    capture: bool,
    timeout: Option<Duration>,
) -> Result<RunOutput, InstrumentError> {
    let Some((program, args)) = command.split_first() else {
        return Err(invalid("The instrumentation command is missing."));
    };
    let output = if capture {
        Output::Capture
    } else {
        Output::Inherit
    };
    let request = RunRequest {
        pipeline: vec![CommandSpec {
            program: program.clone(),
            args: args.to_vec(),
        }],
        cwd,
        env: host.env_all(),
        stdin: Input::Inherit,
        stdout: output.clone(),
        stderr: output,
        timeout,
        tee: capture,
    };
    Ok(host.run(&request, &mut |stream, bytes| host.write_out(stream, bytes))?)
}

pub(crate) fn command_line(command: &[BString]) -> BString {
    let mut line = BString::default();
    for (index, arg) in command.iter().enumerate() {
        if index > 0 {
            line.push(b' ');
        }
        let quote = arg.is_empty()
            || arg
                .iter()
                .any(|byte| byte.is_ascii_whitespace() || matches!(byte, b'"' | b'\'' | b'\\'));
        if quote {
            line.push(b'"');
        }
        for byte in arg.iter().copied() {
            if quote && matches!(byte, b'"' | b'\\') {
                line.push(b'\\');
            }
            line.push(byte);
        }
        if quote {
            line.push(b'"');
        }
    }
    line
}

#[derive(Default)]
struct Metadata {
    target: Option<BString>,
    config: Option<BString>,
    outputs: Vec<BString>,
    source: Option<BString>,
    language: Option<BString>,
    labels: Vec<BString>,
    test_name: Option<BString>,
}

fn compile_trace_path(
    command: &[BString],
    build_dir: &Path,
    metadata: &Metadata,
) -> Option<PathBuf> {
    for arg in command {
        if let Some(name) = arg.strip_prefix(b"-ftime-trace=") {
            return Some(build_dir.join(path::to_native(name)));
        }
    }
    if command.iter().any(|arg| arg.as_slice() == b"-ftime-trace") {
        return metadata.outputs.first().map(|output| {
            build_dir
                .join(path::to_native(output))
                .with_extension("json")
        });
    }
    None
}

fn run(
    host: &dyn Host,
    build_dir: &Path,
    cwd: &Path,
    role: SnippetRole,
    command: &[BString],
    metadata: Metadata,
) -> Result<i32, InstrumentError> {
    let queries = read_queries(host, build_dir)?;
    let enabled = |option| queries.iter().any(|query| query.options.contains(&option));
    let data = api_dir(build_dir).join("data");
    let capture = enabled(InstrumentOption::CaptureOutput);
    let before =
        enabled(InstrumentOption::DynamicSystemInformation).then(|| SystemSample::collect(host));
    let compiler_trace = (role == SnippetRole::Compile && enabled(InstrumentOption::CompileTrace))
        .then(|| compile_trace_path(command, cwd, &metadata))
        .flatten();
    let trace_mtime = compiler_trace
        .as_ref()
        .and_then(|file| host.metadata(file, Follow::Yes).ok().map(|meta| meta.mtime));
    let time_start = epoch_millis(host);
    let start = host.monotonic();
    let output = execute(host, cwd.to_path_buf(), command, capture, None)?;
    let duration =
        u64::try_from(host.monotonic().saturating_sub(start).as_millis()).unwrap_or(u64::MAX);
    let dynamic = before.map(|before| (before, SystemSample::collect(host)));
    let (code, result, interrupt_signal) = match output.results.first() {
        Some(ProcessResult::Exited(code)) => (*code, Some(i64::from(*code)), None),
        Some(ProcessResult::Signaled(signal)) => {
            (128_i32.saturating_add(*signal), None, Some(*signal))
        }
        Some(ProcessResult::TimedOut) => (124, None, None),
        Some(ProcessResult::SpawnFailed(error)) => return Err(error.clone().into()),
        None => return Err(invalid("The instrumentation command returned no result.")),
    };
    let mut saved_trace = None;
    if let Some(file) = compiler_trace
        && let Ok(meta) = host.metadata(&file, Follow::Yes)
        && Some(meta.mtime) != trace_mtime
    {
        let value = read_json(host, &file)?;
        let saved = publish_at(
            host,
            &data.join("compile-trace"),
            &format!("compile-{}", timestamp(host)),
            &value,
        )?;
        saved_trace = saved.strip_prefix(&data).ok().map(path::from_native);
    }
    let snippet = Snippet {
        role,
        command: command_line(command),
        working_dir: path::from_native(cwd),
        result,
        time_start,
        duration,
        target: metadata.target,
        outputs: metadata.outputs,
        source: metadata.source,
        language: metadata.language,
        test_name: metadata.test_name,
        config: metadata.config,
        stdout: capture.then(|| BString::from(output.stdout)),
        stderr: capture.then(|| BString::from(output.stderr)),
    };
    let details = SnippetDetails {
        output_base: Some(build_dir.to_path_buf()),
        interrupt_signal,
        dynamic,
        process_metrics: (enabled(InstrumentOption::ProcessMetrics)
            && matches!(
                role,
                SnippetRole::Compile
                    | SnippetRole::Link
                    | SnippetRole::Custom
                    | SnippetRole::Install
                    | SnippetRole::Test
            ))
        .then_some(None),
        cmake_content: newest_content(host, &data)?,
        trace_file: (role == SnippetRole::Compile && enabled(InstrumentOption::CompileTrace))
            .then_some(saved_trace),
        target_labels: Some(metadata.labels),
        show_only: false,
    };
    write_snippet_with_details(host, &data, &snippet, &details)?;
    Ok(code)
}

/// Run a command in `build_dir`, write a snippet, and return the command exit code.
///
/// # Errors
///
/// Returns an error for invalid arguments or failed host operations.
pub fn run_instrumented(
    host: &dyn Host,
    build_dir: &Path,
    role: SnippetRole,
    command: &[BString],
) -> Result<i32, InstrumentError> {
    let build = absolute(host, build_dir);
    run(host, &build, &build, role, command, Metadata::default())
}

/// Run the arguments of `ctest --instrument`, with or without the leading flag.
///
/// The command runs in the current working directory. The build directory is
/// used to find queries and outputs and to store the snippet.
///
/// # Errors
///
/// Returns an error if the arguments are invalid or a host operation fails.
pub fn run_launcher(host: &dyn Host, argv: &[BString]) -> Result<i32, InstrumentError> {
    let mut args = argv.iter();
    if argv
        .first()
        .is_some_and(|arg| arg.as_slice() == b"--instrument")
    {
        args.next();
    }
    let mut metadata = Metadata::default();
    let mut role = None;
    let mut build = None;
    while let Some(flag) = args.next() {
        if flag.as_slice() == b"--" {
            let cwd = host.initial_cwd();
            let build = build
                .as_deref()
                .map_or_else(|| cwd.clone(), |dir| absolute(host, dir));
            let role =
                role.ok_or_else(|| invalid("The instrumentation command type is missing."))?;
            return run(host, &build, &cwd, role, args.as_slice(), metadata);
        }
        let value = args
            .next()
            .ok_or_else(|| invalid(format!("Missing value for {flag}.")))?;
        match flag.as_slice() {
            b"--command-type" => {
                role = Some(SnippetRole::from_name(value).ok_or_else(|| {
                    invalid(format!("Unknown instrumentation command type: {value}"))
                })?)
            }
            b"--build-dir" => build = Some(path::to_native(value)),
            b"--target-name" => metadata.target = Some(value.clone()),
            b"--config" => metadata.config = Some(value.clone()),
            b"--source" => metadata.source = Some(value.clone()),
            b"--language" => metadata.language = Some(value.clone()),
            b"--test-name" => metadata.test_name = Some(value.clone()),
            b"--output" => metadata.outputs.extend(
                value
                    .split(|byte| *byte == b',')
                    .filter(|item| !item.is_empty())
                    .map(BString::from),
            ),
            b"--target-labels" => metadata.labels.extend(
                value
                    .split(|byte| *byte == b',')
                    .filter(|item| !item.is_empty())
                    .map(BString::from),
            ),
            _ => {
                return Err(invalid(format!(
                    "Unknown instrumentation launcher option: {flag}"
                )));
            }
        }
    }
    Err(invalid(
        "The instrumentation launcher requires '--' before the command.",
    ))
}
