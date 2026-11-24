use std::path::Path;
use std::sync::Arc;

use rcmake_base::BString;
use rcmake_eval::{CmdError, CmdResult, Cx};
use rcmake_expr::ExpandedArg;
use rcmake_host::{Host, Stream, path};

use super::{
    Arguments, HandlerResult, SESSION, Settings, Start, absolute, configuration, directory, error,
    extra_verbose, prior_start, verbose, with_capture,
};
use crate::cli::{self, Invocation};
use crate::dart::{Dashboard, Tag};
use crate::{CtestError, CtestOptions, runner::run_tests_with_dashboard};

pub(super) fn ctest_test(cx: &mut Cx<'_>, args: &[ExpandedArg]) -> CmdResult {
    with_capture(
        cx,
        args,
        &[
            b"INCLUDE",
            b"EXCLUDE",
            b"INCLUDE_LABEL",
            b"EXCLUDE_LABEL",
            b"EXCLUDE_FIXTURE",
            b"EXCLUDE_FIXTURE_SETUP",
            b"EXCLUDE_FIXTURE_CLEANUP",
            b"INCLUDE_FROM_FILE",
            b"EXCLUDE_FROM_FILE",
            b"COVERAGE_TOOL",
            b"PARALLEL_LEVEL",
            b"PRESET",
            b"PRESETS_FILE",
            b"OUTPUT_JUNIT",
            b"REPEAT",
            b"TEST_LOAD",
        ],
        &[b"STOP_ON_FAILURE", b"APPEND"],
        run_test,
    )
}

fn run_test(cx: &mut Cx<'_>, args: &Arguments<'_>) -> Result<HandlerResult, CmdError> {
    let previous = prior_start();
    let build = directory(
        cx,
        args.value(b"BUILD"),
        b"CTEST_BINARY_DIRECTORY",
        previous.as_ref().map(|start| start.build.as_path()),
    )?;
    let (host, settings) = SESSION
        .with_borrow(|session| {
            session
                .as_ref()
                .map(|session| (Arc::clone(&session.host), session.settings.clone()))
        })
        .ok_or_else(|| error("requires a dashboard script session"))?;
    let invocation = test_invocation(cx, args, host.as_ref(), settings, previous.as_ref(), &build)?;
    let _environment = crate::PresetEnvironment::apply(host.as_ref(), &invocation.environment);
    let configuration = configuration(
        cx,
        &build,
        previous.as_ref().map(|start| start.source.as_path()),
    )?;
    let tag = match previous.filter(|start| start.build == build) {
        Some(start) => start.tag,
        None => Tag::read(host.as_ref(), &build).map_err(|err| error(err.to_string()))?,
    };
    let dashboard = Dashboard {
        directory: build,
        tag,
        configuration,
        show_project: !args.flag(b"QUIET"),
    };
    let run = match run_tests_with_dashboard(Arc::clone(&host), &invocation, true, Some(dashboard))
    {
        Ok(run) => run,
        Err(failure @ (CtestError::DependencyCycle(_) | CtestError::TestList(_))) => {
            host.write_out(Stream::Stderr, format!("{failure}\n").as_bytes());
            cx.set_fatal();
            return Err(CmdError::Reported);
        }
        Err(failure) => return Err(error(failure.to_string())),
    };
    if run.no_tests && !invocation.options.ignore_no_tests && !args.flag(b"RETURN_VALUE") {
        cx.set_fatal();
        return Err(CmdError::Reported);
    }
    Ok(HandlerResult {
        code: if run.outcome.exit_code == 0 { 0 } else { -1 },
        capture_error: run.start_error,
    })
}

fn test_invocation(
    cx: &mut Cx<'_>,
    args: &Arguments<'_>,
    host: &dyn Host,
    settings: Settings,
    previous: Option<&Start>,
    build: &Path,
) -> Result<Invocation, CmdError> {
    let config = [
        b"CTEST_CONFIGURATION_TYPE".as_slice(),
        b"CTEST_BUILD_CONFIGURATION",
    ]
    .into_iter()
    .map(|var| cx.var_or_empty(var))
    .find(|value| !value.is_empty())
    .or(settings.config);
    let timeout = settings
        .timeout
        .map(|timeout| timeout.as_secs_f64())
        .or_else(|| cli::number_prefix(&cx.var_or_empty(b"CTEST_TEST_TIMEOUT")));
    let parallel = args
        .value(b"PARALLEL_LEVEL")
        .map(|value| value.as_slice())
        .or_else(|| args.flag(b"PARALLEL_LEVEL").then_some(b"".as_slice()));
    let jobs = parallel
        .map(|value| {
            if value.is_empty() {
                return Ok(rcmake_presets::TestJobs::Automatic);
            }
            parse_number(value)
                .and_then(|jobs| u32::try_from(jobs).ok())
                .map(rcmake_presets::TestJobs::Count)
                .ok_or_else(|| {
                    dashboard_argument_error(
                        cx,
                        args,
                        format!("ParallelLevel invalid value: {}", BString::from(value)),
                    )
                })
        })
        .transpose()?;
    let (test_load, load_diagnostic) = args.value(b"TEST_LOAD").map_or_else(
        || {
            (
                cx.var_or_empty(b"CTEST_TEST_LOAD"),
                "Invalid value for 'CTEST_TEST_LOAD' : ",
            )
        },
        |value| (value.clone(), "Invalid value for 'TEST_LOAD' : "),
    );
    let test_load = if test_load.is_empty() {
        None
    } else {
        cli::warning_number(host, &test_load, load_diagnostic)
    };
    let setting = |key, vars: &[&[u8]]| {
        args.value(key).cloned().or_else(|| {
            vars.iter()
                .map(|var| cx.var_or_empty(var))
                .find(|value| !value.is_empty())
        })
    };
    let source = previous.map_or_else(
        || {
            absolute(
                &host.initial_cwd(),
                &cx.var_or_empty(b"CTEST_SOURCE_DIRECTORY"),
            )
        },
        |start| start.source.clone(),
    );
    let mut invocation = Invocation {
        options: CtestOptions {
            test_dir: build.to_path_buf(),
            config,
            include: args.value(b"INCLUDE").cloned().or(settings.include),
            exclude: args.value(b"EXCLUDE").cloned().or(settings.exclude),
            include_labels: if args.flag(b"INCLUDE_LABEL") {
                args.labels(b"INCLUDE_LABEL")
            } else {
                settings.include_labels
            },
            exclude_labels: if args.flag(b"EXCLUDE_LABEL") {
                args.labels(b"EXCLUDE_LABEL")
            } else {
                settings.exclude_labels
            },
            ..CtestOptions::default()
        },
        preset: setting(b"PRESET", &[b"CTEST_TEST_PRESET", b"CTEST_PRESET"]),
        presets_file: setting(b"PRESETS_FILE", &[b"CTEST_PRESETS_FILE"])
            .map(|file| absolute(&source, &file)),
        selection: settings.selection,
        test_load,
        ..Invocation::default()
    };
    for (option, var) in [
        (
            &mut invocation.selection.exclude_fixture,
            b"EXCLUDE_FIXTURE".as_slice(),
        ),
        (
            &mut invocation.selection.exclude_fixture_setup,
            b"EXCLUDE_FIXTURE_SETUP",
        ),
        (
            &mut invocation.selection.exclude_fixture_cleanup,
            b"EXCLUDE_FIXTURE_CLEANUP",
        ),
    ] {
        if let Some(value) = args.value(var) {
            *option = Some(value.clone());
        }
    }
    if let Some(file) = args.value(b"INCLUDE_FROM_FILE") {
        invocation.selection.tests_from_file = Some(path::to_native(file));
    }
    if let Some(file) = args.value(b"EXCLUDE_FROM_FILE") {
        invocation.selection.exclude_from_file = Some(path::to_native(file));
    }
    invocation.coverage_tool = args.value(b"COVERAGE_TOOL").cloned();
    invocation.execution.no_tests_action = settings.no_tests_action;
    invocation.execution.interactive_debugging = settings.interactive_debugging.or(Some(false));
    invocation.output.max_passed_test_output_size =
        parse_output_limit(cx, b"CTEST_CUSTOM_MAXIMUM_PASSED_TEST_OUTPUT_SIZE");
    invocation.output.max_failed_test_output_size =
        parse_output_limit(cx, b"CTEST_CUSTOM_MAXIMUM_FAILED_TEST_OUTPUT_SIZE");
    let truncation = cx.var_or_empty(b"CTEST_CUSTOM_TEST_OUTPUT_TRUNCATION");
    if !truncation.is_empty() {
        invocation.output.test_output_truncation =
            Some(cli::truncation_mode(&truncation).ok_or_else(|| {
                dashboard_argument_error(
                    cx,
                    args,
                    format!("Invalid value for CTEST_CUSTOM_TEST_OUTPUT_TRUNCATION: {truncation}"),
                )
            })?);
    }
    invocation.output.quiet = (!verbose()).then_some(true);
    invocation.output.verbosity = extra_verbose().then_some(rcmake_presets::TestVerbosity::Extra);
    invocation.output.output_junit_file = args.value(b"OUTPUT_JUNIT").cloned();
    invocation.execution.stop_on_failure = args.flag(b"STOP_ON_FAILURE").then_some(true);
    invocation.execution.jobs = jobs;
    if let Some(repeat) = args.value(b"REPEAT") {
        let mut repeat = repeat.to_ascii_lowercase();
        for byte in &mut repeat {
            if *byte == b'_' {
                *byte = b'-';
            }
        }
        invocation.execution.repeat =
            Some(cli::parse_repeat(b"--repeat", &repeat).map_err(|_| {
                let value = args.value(b"REPEAT").cloned().unwrap_or_default();
                dashboard_argument_error(cx, args, format!("Repeat option invalid value: {value}"))
            })?);
    }
    invocation
        .load_test_preset(host, &source, false)
        .map_err(|message| {
            host.write_out(
                Stream::Stderr,
                b"Cannot instantiate test handler ctest_test\n",
            );
            error(message)
        })?;
    invocation.resolve_options(host, timeout).map_err(error)?;
    Ok(invocation)
}

fn parse_output_limit(cx: &Cx<'_>, name: &[u8]) -> Option<i32> {
    cx.var_or_empty(name).to_string().parse().ok()
}

fn parse_number(value: &[u8]) -> Option<usize> {
    std::str::from_utf8(value).ok()?.trim().parse().ok()
}

fn dashboard_argument_error(cx: &mut Cx<'_>, args: &Arguments<'_>, message: String) -> CmdError {
    if args.flag(b"CAPTURE_CMAKE_ERROR") {
        return error(message);
    }
    cx.host()
        .write_out(Stream::Stderr, format!("{message}\n").as_bytes());
    cx.set_fatal();
    CmdError::Reported
}
