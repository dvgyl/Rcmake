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
    pub(crate) snippet: Option<Box<std::path::PathBuf>>,
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
            snippet: None,
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
    let cwd = test.working_directory();
    let timeout = parse_timeout(test.property(b"TIMEOUT")).or(options.timeout);
    Ok(RunRequest {
        pipeline: vec![command],
        cwd,
        env: environment(host, test)?.into_iter().collect(),
        stdin: Input::Inherit,
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
    resolve_program_at(
        host,
        &mut command.program,
        &test.working_directory(),
        options.config.as_ref(),
    );
}

pub(crate) fn resolve_program_at(
    host: &dyn Host,
    command: &mut BString,
    directory: &std::path::Path,
    config: Option<&BString>,
) {
    if command.is_empty() || command.as_slice() == b"NOT_AVAILABLE" {
        return;
    }
    let program = to_native(command);
    let local = directory.join(&program);
    let windows = host.system().sysname.starts_with(b"Windows");
    if let Some(path) = find_local_program(host, &local, config, windows) {
        *command = from_native(&path);
    } else if program.components().count() > 1 {
        *command = from_native(&local);
    }
}

fn find_local_program(
    host: &dyn Host,
    local: &std::path::Path,
    config: Option<&BString>,
    windows: bool,
) -> Option<std::path::PathBuf> {
    let config_path = config.and_then(|config| {
        Some(
            local
                .parent()?
                .join(to_native(config))
                .join(local.file_name()?),
        )
    });
    let is_file = |path: &std::path::Path| {
        host.metadata(path, Follow::Yes)
            .is_ok_and(|metadata| metadata.kind == FileKind::File)
    };
    for path in std::iter::once(local).chain(config_path.as_deref()) {
        if is_file(path) {
            return Some(path.to_path_buf());
        }
        if windows {
            let mut executable = path.to_path_buf();
            executable.as_mut_os_string().push(".exe");
            if is_file(&executable) {
                return Some(executable);
            }
        }
    }
    None
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
    environment_for_system(host, test, host.system().sysname == "Windows")
}

fn environment_for_system(
    host: &dyn Host,
    test: &Test,
    windows: bool,
) -> Result<Environment, String> {
    let key = |key: &[u8]| -> BString {
        if windows {
            key.to_ascii_uppercase().into()
        } else {
            key.into()
        }
    };
    let path_separator = if windows { b";" } else { b":" };
    let mut env: Environment = host
        .env_all()
        .into_iter()
        .map(|(name, value)| (key(&name), value))
        .collect();
    for assignment in test.list(b"ENVIRONMENT") {
        if let Some((name, value)) = split(&assignment, b'=') {
            env.insert(key(name), value.into());
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
        let Some((name, op, value)) = fields else {
            return Err(format!("Invalid environment modification: {entry}"));
        };
        modify_env(&mut env, &initial, &key(name), op, value, path_separator)?;
    }
    Ok(env)
}

fn modify_env(
    env: &mut Environment,
    initial: &Environment,
    key: &[u8],
    op: &[u8],
    value: &[u8],
    path_separator: &[u8],
) -> Result<(), String> {
    let old = env
        .get(key)
        .map_or(b"".as_slice(), |value| value.as_slice());
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
            let snippet = instrument.zip(sample).and_then(|(instrument, sample)| {
                instrument.test(test, request, (&run, metrics), sample, elapsed)
            });
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
            result.snippet = snippet.map(Box::new);
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

#[cfg(test)]
mod tests {
    use super::{environment_for_system, find_local_program};
    use crate::discovery::Test;
    use rcmake_host::{CommandSpec, Host};

    #[test]
    fn environment_uses_host_case_and_path_list_rules() -> Result<(), String> {
        let host = PureHost::default();
        host.env_set(b"PATH", Some(b"original"));
        let mut test = Test {
            name: "env".into(),
            command: CommandSpec {
                program: "tool".into(),
                args: Vec::new(),
            },
            directory: host.initial_cwd(),
            properties: [
                ("ENVIRONMENT".into(), "Path=declared".into()),
                (
                    "ENVIRONMENT_MODIFICATION".into(),
                    "path=unset:;Path=set:local;PATH=reset:;path=path_list_prepend:prefix".into(),
                ),
            ]
            .into_iter()
            .collect(),
            timeout: Default::default(),
        };
        let env = environment_for_system(&host, &test, true)?;
        assert_eq!(env.len(), 1);
        assert_eq!(
            env.get(b"PATH".as_slice()).map(|value| value.as_slice()),
            Some(&b"prefix;declared"[..])
        );
        test.properties.insert(
            "ENVIRONMENT_MODIFICATION".into(),
            "PATH=path_list_prepend:prefix;path=set:local".into(),
        );
        let env = environment_for_system(&host, &test, false)?;
        assert_eq!(
            env.get(b"PATH".as_slice()).map(|value| value.as_slice()),
            Some(&b"prefix:original"[..])
        );
        assert_eq!(
            env.get(b"Path".as_slice()).map(|value| value.as_slice()),
            Some(&b"declared"[..])
        );
        assert_eq!(
            env.get(b"path".as_slice()).map(|value| value.as_slice()),
            Some(&b"local"[..])
        );
        Ok(())
    }

    use rcmake_host::{HostResult, PureHost};
    use std::path::Path;

    #[test]
    fn windows_test_program_suffix_and_config_order() -> HostResult<()> {
        let host = PureHost::default();
        let program = Path::new("/work/sub/parse.number");
        let local = Path::new("/work/sub/parse.number.exe");
        let configured = Path::new("/work/sub/Release/parse.number.exe");
        let release = "Release".into();
        host.seed_file(configured, b"")?;
        assert_eq!(
            find_local_program(&host, program, Some(&release), false),
            None
        );
        assert_eq!(
            find_local_program(&host, program, Some(&release), true).as_deref(),
            Some(configured)
        );

        host.seed_file(local, b"")?;
        assert_eq!(
            find_local_program(&host, program, Some(&release), true).as_deref(),
            Some(local)
        );

        host.seed_file(program, b"")?;
        assert_eq!(
            find_local_program(&host, program, Some(&release), true).as_deref(),
            Some(program)
        );
        Ok(())
    }
}
