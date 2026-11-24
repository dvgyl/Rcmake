use std::collections::BTreeMap;
use std::time::Duration;

use rcmake_base::{BString, truth};
use rcmake_host::path::{from_native, to_native};
use rcmake_host::{CommandSpec, FileKind, Follow, Host, Input, Output, ProcessResult, RunRequest};

use crate::CtestOptions;
use crate::discovery::Test;
use crate::selection::{OutputChecks, PlannedTest, first_match};

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(10_000_000);

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Status {
    Passed,
    Failed,
    Timeout,
    Disabled,
    Skipped,
    NotRun,
}

impl Status {
    pub(crate) fn failed(self) -> bool {
        matches!(self, Self::Failed | Self::Timeout | Self::NotRun)
    }

    pub(crate) fn fixture_ready(self) -> bool {
        matches!(self, Self::Passed | Self::Skipped | Self::Disabled)
    }
}

pub(crate) struct ResultLine {
    pub(crate) status: Status,
    pub(crate) elapsed: Duration,
    pub(crate) output: Vec<u8>,
    pub(crate) detail: Option<String>,
    pub(crate) exit_code: Option<i32>,
    pub(crate) skip_reason: Option<SkipReason>,
    pub(crate) regex_reason: Option<String>,
}

impl ResultLine {
    pub(crate) fn new(status: Status) -> Self {
        Self {
            status,
            elapsed: Duration::ZERO,
            output: Vec::new(),
            detail: None,
            exit_code: None,
            skip_reason: None,
            regex_reason: None,
        }
    }

    pub(crate) fn not_run(detail: impl Into<String>) -> Self {
        Self {
            detail: Some(detail.into()),
            ..Self::new(Status::NotRun)
        }
    }
}

#[derive(Clone, Copy)]
pub(crate) enum SkipReason {
    ReturnCode(i32),
    RegularExpression,
}

pub(crate) type Environment = BTreeMap<BString, BString>;

pub(crate) fn request(
    host: &dyn Host,
    test: &PlannedTest,
    options: &CtestOptions,
) -> Result<RunRequest, String> {
    let mut command = test.command.clone();
    resolve_program(host, test, &mut command, options);
    let cwd = test
        .directory
        .join(to_native(test.property(b"WORKING_DIRECTORY")));
    let timeout = parse_timeout(test.property(b"TIMEOUT")).or(options.timeout);
    Ok(RunRequest {
        pipeline: vec![command],
        cwd,
        env: environment(host, test)?.into_iter().collect(),
        stdin: Input::Null,
        stdout: Output::Capture,
        stderr: Output::SharedWithStdout,
        timeout: Some(
            timeout
                .filter(|timeout| !timeout.is_zero())
                .unwrap_or(DEFAULT_TIMEOUT),
        ),
        tee: false,
    })
}

pub(crate) fn resolve_program(
    host: &dyn Host,
    test: &Test,
    command: &mut CommandSpec,
    options: &CtestOptions,
) {
    let program = to_native(&command.program);
    if command.program.is_empty() || command.program == "NOT_AVAILABLE" {
        return;
    }
    let local = test.directory.join(&program);
    let config_path = options.config.as_ref().and_then(|config| {
        Some(
            local
                .parent()?
                .join(to_native(config))
                .join(local.file_name()?),
        )
    });
    for path in std::iter::once(&local).chain(config_path.as_ref()) {
        if host
            .metadata(path, Follow::Yes)
            .is_ok_and(|metadata| metadata.kind == FileKind::File)
        {
            command.program = from_native(path);
            return;
        }
    }
    if program.components().count() > 1 {
        command.program = from_native(&local);
    }
}

pub(crate) fn parse_timeout(value: &[u8]) -> Option<Duration> {
    let seconds: f64 = std::str::from_utf8(value).ok()?.trim().parse().ok()?;
    Duration::try_from_secs_f64(seconds).ok()
}

fn split(value: &[u8], separator: u8) -> Option<(&[u8], &[u8])> {
    let index = value.iter().position(|&byte| byte == separator)?;
    Some((value.get(..index)?, value.get(index + 1..)?))
}

pub(crate) fn environment(host: &dyn Host, test: &Test) -> Result<Environment, String> {
    let mut env: Environment = host.env_all().into_iter().collect();
    for assignment in test.list(b"ENVIRONMENT") {
        if let Some((key, value)) = split(&assignment, b'=') {
            env.insert(key.into(), value.into());
        }
    }
    let modifications = test.list(b"ENVIRONMENT_MODIFICATION");
    if modifications.is_empty() {
        return Ok(env);
    }
    let initial = env.clone();
    for entry in modifications {
        let fields = split(&entry, b'=').and_then(|(key, action)| {
            let (op, value) = split(action, b':')?;
            (!key.is_empty()).then_some((key, op, value))
        });
        let Some((key, op, value)) = fields else {
            return Err(format!("Invalid environment modification: {entry}"));
        };
        modify_env(&mut env, &initial, key, op, value)?;
    }
    Ok(env)
}

fn modify_env(
    env: &mut Environment,
    initial: &Environment,
    key: &[u8],
    op: &[u8],
    value: &[u8],
) -> Result<(), String> {
    let old = env
        .get(key)
        .map_or(b"".as_slice(), |value| value.as_slice());
    let path_separator = if cfg!(windows) { b";" } else { b":" };
    let value = match op {
        b"reset" => initial.get(key).cloned(),
        b"set" => Some(value.into()),
        b"unset" => None,
        b"string_append" => Some(join(old, b"", value)),
        b"string_prepend" => Some(join(value, b"", old)),
        b"path_list_append" => Some(join(old, path_separator, value)),
        b"path_list_prepend" => Some(join(value, path_separator, old)),
        b"cmake_list_append" => Some(join(old, b";", value)),
        b"cmake_list_prepend" => Some(join(value, b";", old)),
        _ => {
            return Err(format!(
                "Unknown environment modification operation: {}",
                BString::from(op)
            ));
        }
    };
    if let Some(value) = value {
        env.insert(key.into(), value);
    } else {
        env.remove(key);
    }
    Ok(())
}

fn join(left: &[u8], separator: &[u8], right: &[u8]) -> BString {
    if left.is_empty() || right.is_empty() {
        return [left, right].concat().into();
    }
    [left, separator, right].concat().into()
}

pub(crate) fn execute(
    host: &dyn Host,
    test: &PlannedTest,
    request: &RunRequest,
    instrument: Option<&crate::instrumentation::Recorder<'_>>,
) -> ResultLine {
    if test.command.program == "NOT_AVAILABLE" {
        return ResultLine::not_run("The test is unavailable in this configuration.");
    }
    let sample = instrument.map(crate::instrumentation::Recorder::start_test);
    let started = host.monotonic();
    let run = if let Some(timeout) = test.timeout.graceful() {
        host.run_with_timeout_signal(request, timeout, &mut |_, _| {})
            .map(|output| (output, None))
    } else if let Some(instrument) = instrument {
        instrument.run_test(request)
    } else {
        host.run(request, &mut |_, _| {})
            .map(|output| (output, None))
    };
    let elapsed = host.monotonic().saturating_sub(started);
    let mut result = match run {
        Ok((run, metrics)) => {
            if let Some((instrument, sample)) = instrument.zip(sample) {
                instrument.test(test, request, (&run, metrics), sample, elapsed);
            }
            let mut output = run.stdout;
            output.extend(run.stderr);
            let mut result = match run.results.first() {
                Some(ProcessResult::Exited(code)) => ResultLine {
                    exit_code: Some(*code),
                    ..exited(test, *code, &output)
                },
                Some(ProcessResult::TimedOut) => ResultLine::new(Status::Timeout),
                Some(ProcessResult::Signaled(signal)) => ResultLine {
                    detail: Some(format!("The test stopped with signal {signal}.")),
                    ..ResultLine::new(Status::Failed)
                },
                Some(ProcessResult::SpawnFailed(error)) => ResultLine::not_run(error.to_string()),
                None => ResultLine::not_run("The host returned no process status."),
            };
            result.output = output;
            result
        }
        Err(error) => ResultLine::not_run(error.to_string()),
    };
    result.elapsed = elapsed;
    result
}

fn exited(test: &PlannedTest, code: i32, output: &[u8]) -> ResultLine {
    let skip_code = std::str::from_utf8(test.property(b"SKIP_RETURN_CODE"))
        .ok()
        .and_then(|value| value.trim().parse::<i32>().ok())
        .filter(|code| (0..=255).contains(code));
    if skip_code == Some(code) {
        return ResultLine {
            skip_reason: Some(SkipReason::ReturnCode(code)),
            ..ResultLine::new(Status::Skipped)
        };
    }
    let OutputChecks { pass, fail, skip } = &test.checks;
    let reason = |message: &str, property: &[u8], matched: Option<usize>| {
        let values = test.list(property);
        let patterns = if let Some(index) = matched {
            values
                .get(index)
                .map_or_else(String::new, ToString::to_string)
        } else {
            values
                .iter()
                .map(|value| format!("{value}\n"))
                .collect::<String>()
        };
        format!("{message} Regex=[{patterns}]")
    };
    let result = (|| {
        if let Some(index) = first_match(skip, output)? {
            return Ok(ResultLine {
                skip_reason: Some(SkipReason::RegularExpression),
                regex_reason: Some(reason(
                    "Skip regular expression found in output.",
                    b"SKIP_REGULAR_EXPRESSION",
                    Some(index),
                )),
                ..ResultLine::new(Status::Skipped)
            });
        }
        let required = first_match(pass, output)?;
        let rejected = first_match(fail, output)?;
        let passed = if pass.is_empty() {
            code == 0
        } else {
            required.is_some()
        };
        let status = if (passed && rejected.is_none()) != truth::is_on(test.property(b"WILL_FAIL"))
        {
            Status::Passed
        } else {
            Status::Failed
        };
        let regex_reason = if rejected.is_some() {
            Some(reason(
                "Error regular expression found in output.",
                b"FAIL_REGULAR_EXPRESSION",
                rejected,
            ))
        } else if !pass.is_empty() {
            let message = if required.is_some() {
                "Required regular expression found."
            } else {
                "Required regular expression not found."
            };
            Some(reason(message, b"PASS_REGULAR_EXPRESSION", required))
        } else {
            None
        };
        Ok::<_, crate::CtestError>(ResultLine {
            regex_reason,
            ..ResultLine::new(status)
        })
    })();
    result.unwrap_or_else(|error| ResultLine {
        detail: Some(error.to_string()),
        ..ResultLine::new(Status::Failed)
    })
}
