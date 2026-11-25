//! Run and record a command for `ctest --instrument`.

use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::time::Duration;

use rcmake_base::BString;
use rcmake_host::{
    CommandSpec, CopyOptions, FileTime, Follow, Host, Input, Output, ProcessResult, RunOutput,
    RunRequest, Stream, path,
};

use crate::files::{absolute, api_dir, command_hash, epoch_millis, newest_content, read_bytes};
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
    let request = request(host, cwd, command, capture, timeout)?;
    Ok(host.run(&request, &mut |stream, bytes| host.write_out(stream, bytes))?)
}

fn request(
    host: &dyn Host,
    cwd: PathBuf,
    command: &[BString],
    capture: bool,
    timeout: Option<Duration>,
) -> Result<RunRequest, InstrumentError> {
    let Some((program, args)) = command.split_first() else {
        return Err(invalid("The instrumentation command is missing."));
    };
    let output = if capture {
        Output::Capture
    } else {
        Output::Inherit
    };
    Ok(RunRequest {
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
    })
}

/// Format arguments for the command field of an instrumentation snippet.
#[must_use]
pub fn command_line(command: &[BString]) -> BString {
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
    ctest: bool,
    target: Option<BString>,
    config: Option<BString>,
    outputs: Vec<BString>,
    source: Option<BString>,
    language: Option<BString>,
    labels: Vec<BString>,
    test_name: Option<BString>,
    timeout: Option<Duration>,
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

fn trace_suffix(end_millis: u64) -> String {
    let seconds = end_millis / 1000;
    let mut days = seconds / 86_400;
    let days_per_400_years = 146_097;
    let mut year = 1970 + days / days_per_400_years * 400;
    days %= days_per_400_years;
    let leap = |year: u64| {
        year.is_multiple_of(4) && (!year.is_multiple_of(100) || year.is_multiple_of(400))
    };
    loop {
        let year_days = 365 + u64::from(leap(year));
        if days < year_days {
            break;
        }
        days -= year_days;
        year += 1;
    }
    let mut month = 1;
    for month_days in [
        31,
        28 + u64::from(leap(year)),
        31,
        30,
        31,
        30,
        31,
        31,
        30,
        31,
        30,
        31,
    ] {
        if days < month_days {
            break;
        }
        days -= month_days;
        month += 1;
    }
    let day = days + 1;
    let hour = seconds / 3600 % 24;
    let minute = seconds / 60 % 60;
    let second = seconds % 60;
    let millis = end_millis % 1000;
    format!("{year:04}-{month:02}-{day:02}T{hour:02}-{minute:02}-{second:02}-{millis:04}")
}

fn copy_compile_trace(
    host: &dyn Host,
    data: &Path,
    source: &Path,
    old_mtime: Option<FileTime>,
    command: &[u8],
    end_millis: u64,
) -> Result<Option<BString>, InstrumentError> {
    match host.metadata(source, Follow::Yes) {
        Ok(metadata) if Some(metadata.mtime) == old_mtime => return Ok(None),
        Err(error) if error.kind == ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(invalid(format!(
                "Cannot read compile trace file {}. {error}",
                source.display()
            )));
        }
        Ok(_) => {}
    }
    let name = source.file_name().ok_or_else(|| {
        invalid(format!(
            "No compile trace file name in {}.",
            source.display()
        ))
    })?;
    let name = path::from_native(Path::new(name));
    let extension = name
        .iter()
        .rposition(|&byte| byte == b'.')
        .unwrap_or(name.len());
    let (stem, extension) = name.split_at(extension);
    let suffix = format!(
        "-{}-{}",
        command_hash(host, command, None),
        trace_suffix(end_millis)
    );
    let name = [stem, suffix.as_bytes(), extension].concat();
    let dir = Path::new("compile-trace");
    let relative = dir.join(path::to_native(&name));
    let dest = data.join(&relative);
    host.create_dir_all(&data.join(dir))
        .and_then(|()| host.copy_file(source, &dest, CopyOptions::default()))
        .map_err(|error| {
            invalid(format!(
                "Failed to copy compile trace file {} to {}. {error}",
                source.display(),
                dest.display()
            ))
        })?;
    Ok(Some(path::from_native_cmake(&relative)))
}

fn comma_list(value: &[u8]) -> Vec<BString> {
    value
        .split(|byte| *byte == b',')
        .filter(|item| !item.is_empty())
        .map(BString::from)
        .collect()
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
    let collect_metrics = enabled(InstrumentOption::ProcessMetrics)
        && matches!(
            role,
            SnippetRole::Compile
                | SnippetRole::Link
                | SnippetRole::Custom
                | SnippetRole::Install
                | SnippetRole::Test
        );
    let request = request(host, cwd.to_path_buf(), command, capture, metadata.timeout)?;
    let (output, metrics) = crate::metrics::run(
        host,
        &request,
        &data,
        collect_metrics,
        &mut |stream, bytes| host.write_out(stream, bytes),
    )?;
    let duration =
        u64::try_from(host.monotonic().saturating_sub(start).as_millis()).unwrap_or(u64::MAX);
    let dynamic = before.map(|before| (before, SystemSample::collect(host)));
    let (code, result, interrupt_signal) = match output.results.first() {
        Some(ProcessResult::Exited(code)) => (*code, Some(i64::from(*code)), None),
        Some(ProcessResult::Signaled(_)) if metadata.ctest => (1, Some(1), None),
        Some(ProcessResult::Signaled(signal)) => {
            (128_i32.saturating_add(*signal), None, Some(*signal))
        }
        Some(ProcessResult::TimedOut) => (124, None, None),
        Some(ProcessResult::SpawnFailed(error)) => return Err(error.clone().into()),
        None => return Err(invalid("The instrumentation command returned no result.")),
    };
    let command = command_line(command);
    let saved_trace = compiler_trace.and_then(|file| {
        match copy_compile_trace(
            host,
            &data,
            &file,
            trace_mtime,
            &command,
            time_start.saturating_add(duration),
        ) {
            Ok(trace) => trace,
            Err(error) => {
                host.write_out(Stream::Stderr, format!("CMake Error: {error}\n").as_bytes());
                None
            }
        }
    });
    let snippet = Snippet {
        role,
        command,
        working_dir: path::from_native_cmake(cwd),
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
    let cmake_content = if role == SnippetRole::Test {
        Ok(None)
    } else {
        newest_content(host, &data)
    };
    let written = cmake_content.and_then(|cmake_content| {
        let details = SnippetDetails {
            output_base: Some(build_dir.to_path_buf()),
            interrupt_signal,
            dynamic,
            process_metrics: collect_metrics.then_some(metrics),
            cmake_content,
            trace_file: (role == SnippetRole::Compile && enabled(InstrumentOption::CompileTrace))
                .then_some(saved_trace),
            target_labels: Some(metadata.labels),
            show_only: false,
            test_index: None,
        };
        write_snippet_with_details(host, &data, &snippet, &details)
    });
    if let Err(error) = written {
        host.write_out(
            Stream::Stderr,
            format!(
                "CMake Error: Failed to write instrumentation snippet in {}. {error}\n",
                data.display()
            )
            .as_bytes(),
        );
    }
    Ok(code)
}

/// Run a command in `build_dir` and return its exit code.
///
/// Report trace and snippet write errors to stderr.
///
/// # Errors
///
/// Returns an error if command setup or execution fails before an exit code is available.
pub fn run_instrumented(
    host: &dyn Host,
    build_dir: &Path,
    role: SnippetRole,
    command: &[BString],
) -> Result<i32, InstrumentError> {
    let build = absolute(host, build_dir);
    run(host, &build, &build, role, command, Metadata::default())
}

/// Run an install script with its configuration and record its process result.
///
/// # Errors
///
/// Returns an error if the command cannot start.
pub fn run_install(
    host: &dyn Host,
    build_dir: &Path,
    command: &[BString],
    config: Option<BString>,
) -> Result<i32, InstrumentError> {
    run(
        host,
        build_dir,
        &host.initial_cwd(),
        SnippetRole::Install,
        command,
        Metadata {
            config,
            timeout: Some(Duration::from_secs(600)),
            ..Metadata::default()
        },
    )
}

/// Run the arguments of an instrumentation wrapper, with or without the leading flag.
///
/// The command runs in the current working directory. The build directory is
/// used to find queries and outputs and to store the snippet.
/// Report trace and snippet write errors to stderr.
///
/// # Errors
///
/// Returns an error if command setup or execution fails before an exit code is available.
pub fn run_launcher(host: &dyn Host, argv: &[BString]) -> Result<i32, InstrumentError> {
    run_launcher_with_policy(host, argv, false)
}

/// Run the CTest instrumentation wrapper and report a signaled child as a failed test.
///
/// # Errors
///
/// Returns an error if command setup or execution fails before an exit code is available.
pub fn run_ctest_launcher(host: &dyn Host, argv: &[BString]) -> Result<i32, InstrumentError> {
    run_launcher_with_policy(host, argv, true)
}

fn run_launcher_with_policy(
    host: &dyn Host,
    argv: &[BString],
    ctest: bool,
) -> Result<i32, InstrumentError> {
    let mut args = argv.iter().peekable();
    if argv
        .first()
        .is_some_and(|arg| arg.as_slice() == b"--instrument")
    {
        args.next();
    }
    let mut metadata = Metadata {
        ctest,
        ..Metadata::default()
    };
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
            let command: Vec<_> = args.cloned().collect();
            return run(host, &build, &cwd, role, &command, metadata);
        }
        if flag.as_slice() == b"--role" {
            continue;
        }
        if matches!(flag.as_slice(), b"--target-name" | b"--config")
            && args.peek().is_some_and(|arg| arg.starts_with(b"--"))
        {
            continue;
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
            b"--target-name" => metadata.target = (!value.is_empty()).then(|| value.clone()),
            b"--config" => metadata.config = Some(value.clone()),
            b"--source" => metadata.source = Some(value.clone()),
            b"--language" => metadata.language = Some(value.clone()),
            b"--test-name" => metadata.test_name = Some(value.clone()),
            b"--output" => metadata.outputs.extend(comma_list(value)),
            b"--output-path" => metadata.outputs.push(value.clone()),
            b"--output-as-file-name" => {
                let file = absolute(host, &path::to_native(value));
                metadata.outputs = comma_list(&read_bytes(host, &file)?);
            }
            b"--target-labels" => metadata.labels.extend(comma_list(value)),
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
