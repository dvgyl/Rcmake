//! Command-line options and test preset resolution.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

use rcmake_base::{BString, truth};
use rcmake_host::{Host, Stream, path::to_native};
use rcmake_presets::{
    EnvSnapshot, NoTestsAction, OutputTruncation, PresetKind, PresetSettings, RepeatMode, ShowOnly,
    TestExecution, TestJobs, TestOutput, TestRepeat, TestSettings, TestVerbosity,
};

use crate::CtestOptions;
use crate::selection::{IndexSource, SelectionOptions};

#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) enum Mode {
    #[default]
    Test,
    Help(Vec<BString>),
    Version {
        json: bool,
        file: Option<PathBuf>,
    },
    ListPresets {
        defined: bool,
    },
    CollectInstrumentation(PathBuf),
    Instrument {
        args: Vec<BString>,
        command: Vec<BString>,
    },
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Script {
    pub(crate) argument: BString,
    pub(crate) new_process: bool,
}

/// Options for the runner and the command that starts it.
#[derive(Debug, Default)]
pub(crate) struct Invocation {
    pub(crate) options: CtestOptions,
    pub(crate) selection: SelectionOptions,
    pub(crate) execution: TestExecution,
    pub(crate) output: TestOutput,
    pub(crate) mode: Mode,
    pub(crate) environment: BTreeMap<BString, Option<BString>>,
    pub(crate) preset: Option<BString>,
    pub(crate) presets_file: Option<PathBuf>,
    pub(crate) source_dir: Option<PathBuf>,
    pub(crate) build_dir: Option<PathBuf>,
    pub(crate) actions: Vec<BString>,
    pub(crate) dashboard_model: Option<crate::dart::Model>,
    pub(crate) definitions: Vec<BString>,
    pub(crate) scripts: Vec<Script>,
    pub(crate) print_labels: bool,
    /// Local clock time, in seconds since midnight.
    pub(crate) stop_time: Option<u32>,
    pub(crate) test_load: Option<u64>,
    pub(crate) coverage_tool: Option<BString>,
    pub(crate) overwrite_configuration_file: Vec<BString>,
}

type Arguments<'a> = std::iter::Peekable<std::slice::Iter<'a, BString>>;

fn split_flag(raw: &[u8]) -> (&[u8], Option<&[u8]>) {
    for flag in [b"--max-width".as_slice(), b"-W"] {
        if let Some(value) = raw
            .strip_prefix(flag)
            .and_then(|rest| rest.strip_prefix(b" "))
        {
            return (flag, Some(value));
        }
    }
    let mut parts = raw.splitn(2, |&byte| byte == b'=');
    (parts.next().unwrap_or_default(), parts.next())
}

fn next_value<'a>(args: &mut Arguments<'a>) -> Option<&'a [u8]> {
    args.next_if(|arg| !arg.starts_with(b"-") || arg.get(1).is_some_and(u8::is_ascii_digit))
        .map(|arg| arg.as_slice())
}

pub(crate) fn parse(args: &[BString], host: &dyn Host) -> Result<Invocation, String> {
    if let Some(launcher) = args.strip_prefix(&[BString::from("--instrument")]) {
        let Some(split) = launcher.iter().position(|arg| arg == "--") else {
            return Err("ctest --launch: this mode is for internal CTest use only".into());
        };
        let (options, command) = launcher.split_at(split);
        return Ok(Invocation {
            mode: Mode::Instrument {
                args: options.to_vec(),
                command: command.iter().skip(1).cloned().collect(),
            },
            ..Invocation::default()
        });
    }
    if let Some(mode) = documentation_mode(args) {
        return Ok(Invocation {
            mode,
            ..Invocation::default()
        });
    }
    if args
        .iter()
        .take_while(|arg| *arg != "--")
        .any(|arg| arg == "--build-and-test")
        && args.iter().any(|arg| arg == "--")
    {
        return Err("The -- option cannot be used with --build-and-test. Use --build-options or --test-command to forward arguments.".into());
    }
    let mut cli = Invocation::default();
    let mut timeout = None;
    let mut args = args.iter().peekable();
    while let Some(arg) = args.next() {
        let (flag, inline) = split_flag(arg);
        let mut value = || {
            inline
                .filter(|value| !value.is_empty())
                .or_else(|| inline.is_none().then(|| next_value(&mut args)).flatten())
                .ok_or_else(|| missing_value(flag))
        };
        if inline.is_some()
            && matches!(
                flag,
                b"-N"
                    | b"-V"
                    | b"-VV"
                    | b"-Q"
                    | b"-U"
                    | b"-F"
                    | b"--union"
                    | b"--rerun-failed"
                    | b"--out-of-date"
                    | b"--stop-on-failure"
                    | b"--schedule-random"
                    | b"--output-on-failure"
                    | b"--verbose"
                    | b"--extra-verbose"
                    | b"--quiet"
                    | b"--progress"
                    | b"--print-labels"
                    | b"--no-compress-output"
                    | b"--no-label-summary"
                    | b"--no-subproject-summary"
                    | b"--debug"
                    | b"--"
            )
        {
            return Err(unknown(arg));
        }
        match flag {
            b"--" => {
                cli.execution.test_passthrough_arguments = Some(args.cloned().collect());
                break;
            }
            b"--test-dir" | b"--build-dir" => {
                let dir = to_native(value()?);
                cli.options.test_dir = dir.clone();
                cli.build_dir = Some(dir);
            }
            b"--source-dir" => cli.source_dir = Some(to_native(value()?)),
            b"-C" | b"--build-config" => cli.options.config = Some(value()?.into()),
            b"-R" | b"--tests-regex" => cli.options.include = Some(value()?.into()),
            b"-E" | b"--exclude-regex" => cli.options.exclude = Some(value()?.into()),
            b"-L" | b"--label-regex" => cli.options.include_labels.push(value()?.into()),
            b"-LE" | b"--label-exclude" => cli.options.exclude_labels.push(value()?.into()),
            b"-FA" | b"--fixture-exclude-any" => {
                cli.selection.exclude_fixture = Some(value()?.into())
            }
            b"-FS" | b"--fixture-exclude-setup" => {
                cli.selection.exclude_fixture_setup = Some(value()?.into())
            }
            b"-FC" | b"--fixture-exclude-cleanup" => {
                cli.selection.exclude_fixture_cleanup = Some(value()?.into())
            }
            b"-I" | b"--tests-information" => {
                cli.selection.indices = Some(IndexSource::Argument(value()?.into()))
            }
            b"-U" | b"--union" => cli.selection.union = true,
            b"--tests-from-file" => cli.selection.tests_from_file = Some(to_native(value()?)),
            b"--exclude-from-file" => cli.selection.exclude_from_file = Some(to_native(value()?)),
            b"--rerun-failed" => cli.selection.rerun_failed = true,
            b"--out-of-date" => cli.selection.out_of_date = true,
            b"--timeout" => timeout = Some(number_prefix(value()?).unwrap_or_default()),
            b"--stop-on-failure" => cli.execution.stop_on_failure = Some(true),
            b"--repeat" | b"--repeat-until-fail" => {
                if cli.execution.repeat.is_some_and(|repeat| repeat.count > 1) {
                    return Err("At most one '--repeat' option may be used.".into());
                }
                cli.execution.repeat = Some(parse_repeat(flag, value()?)?);
            }
            b"--schedule-random" => cli.execution.schedule_random = Some(true),
            b"--schedule-random-seed" => {
                if let Some(seed) = warning_number(
                    host,
                    value()?,
                    "Invalid value for '--schedule-random-seed': ",
                ) {
                    cli.selection.random_seed = Some(seed as u32);
                }
            }
            b"--test-load" => {
                cli.test_load = warning_number(host, value()?, "Invalid value for 'Test Load' : ")
                    .or(cli.test_load);
            }
            b"--stop-time" => cli.stop_time = parse_stop_time(value()?),
            b"--no-tests" => cli.execution.no_tests_action = Some(parse_no_tests(value()?)?),
            b"-N" => cli.execution.show_only = Some(ShowOnly::Human),
            b"--show-only" => {
                let format = match inline {
                    Some(b"") => return Err(missing_value(flag)),
                    Some(format) => format,
                    None => next_value(&mut args).unwrap_or(b"human"),
                };
                cli.execution.show_only = Some(parse_show_only(format)?);
            }
            b"--output-on-failure" => cli.output.output_on_failure = Some(true),
            b"--output-junit" => cli.output.output_junit_file = Some(value()?.into()),
            b"-O" | b"--output-log" => cli.output.output_log_file = Some(value()?.into()),
            b"-V" | b"--verbose" => {
                if cli.output.verbosity != Some(TestVerbosity::Extra) {
                    cli.output.verbosity = Some(TestVerbosity::Verbose);
                }
            }
            b"-VV" | b"--extra-verbose" => cli.output.verbosity = Some(TestVerbosity::Extra),
            b"-Q" | b"--quiet" => cli.output.quiet = Some(true),
            b"--progress" => cli.output.short_progress = Some(true),
            b"--print-labels" => cli.print_labels = true,
            b"--no-compress-output" => {}
            b"--no-label-summary" => cli.output.label_summary = Some(false),
            b"--no-subproject-summary" => cli.output.subproject_summary = Some(false),
            b"--debug" => cli.output.debug = Some(true),
            b"-W" | b"--max-width" => {
                cli.output.max_test_name_width = Some(output_integer(flag, value()?)?)
            }
            b"--test-output-size-passed" => {
                cli.output.max_passed_test_output_size = Some(output_integer(flag, value()?)?)
            }
            b"--test-output-size-failed" => {
                cli.output.max_failed_test_output_size = Some(output_integer(flag, value()?)?)
            }
            b"--test-output-truncation" => {
                let raw = value()?;
                cli.output.test_output_truncation =
                    Some(truncation_mode(raw).ok_or_else(|| {
                        format!(
                            "Invalid value for '--test-output-truncation': {}",
                            display(raw)
                        )
                    })?);
            }
            b"-F" => cli.execution.enable_failover = Some(true),
            b"--resource-spec-file" => cli.execution.resource_spec_file = Some(value()?.into()),
            b"--interactive-debug-mode" => {
                cli.execution.interactive_debugging = Some(truth::is_on(value()?));
            }
            b"-M" | b"--test-model" => {
                let raw = value()?;
                cli.dashboard_model = Some(
                    crate::dart::Model::parse(raw)
                        .ok_or_else(|| format!("Unknown dashboard model: {}", display(raw)))?,
                );
            }
            b"-T" | b"--test-action" => cli.actions.push(parse_action(value()?)?),
            b"-D" | b"--dashboard" => cli.definitions.push(parse_definition(value()?)?),
            b"-S" | b"--script" | b"-SP" | b"--script-new-process" => cli.scripts.push(Script {
                argument: value()?.into(),
                new_process: matches!(flag, b"-SP" | b"--script-new-process"),
            }),
            b"--preset" => cli.preset = Some(value()?.into()),
            b"--presets-file" => cli.presets_file = Some(to_native(value()?)),
            b"--collect-instrumentation" => {
                let dir = value().map_err(|_| unknown(arg))?;
                return Ok(Invocation {
                    mode: Mode::CollectInstrumentation(host.initial_cwd().join(to_native(dir))),
                    ..Invocation::default()
                });
            }
            b"--list-presets" => {
                let defined = match inline {
                    None => false,
                    Some(b"defined") => true,
                    Some(_) => return Err("Invalid value specified for --list-presets.\nThe only supported value is \"defined\".".into()),
                };
                cli.mode = Mode::ListPresets { defined };
            }
            b"-j" | b"--parallel" => {
                if inline == Some(b"") {
                    return Err(missing_value(flag));
                }
                let raw = inline.or_else(|| next_value(&mut args));
                cli.execution.jobs = Some(parse_jobs(flag, raw)?);
            }
            b"--overwrite" => cli.overwrite_configuration_file.push(value()?.into()),
            _ if arg.starts_with(b"-j") => {
                cli.execution.jobs = Some(parse_jobs(b"-j", arg.get(2..))?)
            }
            _ if arg.starts_with(b"-D") => cli
                .definitions
                .push(parse_definition(arg.get(2..).unwrap_or_default())?),
            _ if !arg.starts_with(b"-") => {}
            _ => return Err(unknown(arg)),
        }
    }
    cli.resolve(host, timeout)?;
    Ok(cli)
}

fn documentation_mode(args: &[BString]) -> Option<Mode> {
    // The pass-through separator ends option processing.
    let tokens = args.iter().take_while(|arg| arg.as_slice() != b"--");
    for (index, arg) in tokens.enumerate() {
        let (flag, format) = split_flag(arg);
        if matches!(flag, b"--version" | b"-version" | b"/V" | b"/version")
            && matches!(format, None | Some(b"json-v1"))
        {
            let file = args
                .get(index + 1)
                .filter(|arg| !arg.starts_with(b"-"))
                .map(|raw| to_native(raw));
            return Some(Mode::Version {
                json: format.is_some(),
                file,
            });
        }
        if matches!(
            flag,
            b"-h" | b"-H" | b"--help" | b"-help" | b"-usage" | b"/?"
        ) || flag.starts_with(b"--help-")
        {
            return Some(Mode::Help(
                args.iter()
                    .skip(index)
                    .take_while(|arg| arg.as_slice() != b"--")
                    .cloned()
                    .collect(),
            ));
        }
    }
    None
}

fn display(raw: &[u8]) -> std::borrow::Cow<'_, str> {
    String::from_utf8_lossy(raw)
}

pub(crate) fn preset_diagnostic(error: rcmake_presets::PresetsError) -> String {
    if let Some((header, details)) = error.message.split_once('\n')
        && details
            .lines()
            .nth(1)
            .is_some_and(|line| line.trim_ascii_start().starts_with("\"jobs\":"))
    {
        if details
            .lines()
            .next()
            .is_some_and(|line| line.ends_with(": Expected an integer"))
            && details
                .lines()
                .nth(1)
                .is_some_and(|line| line.trim_ascii_end().ends_with("\"\""))
        {
            return format!(
                "{header}\nFile version must be 11 or higher for processor-count-based jobs preset support"
            );
        }
        if let Some((location, detail)) = details.split_once(": Expected a non-negative integer") {
            return format!("{header}\n{location}: Invalid preset{detail}");
        }
    }
    error
        .message
        .replace(
            "for testOutputTruncation support",
            "for testOutputTruncation preset support",
        )
        .replace(
            "for outputJUnitFile support",
            "for CTest JUnit output support",
        )
        .replace(
            "for testPassthroughArguments support",
            "for testPassthroughArguments preset support",
        )
        .replace(
            "Invalid TestIndexRange",
            "Invalid \"filter.include.index\" field: expected a string or object",
        )
}

fn missing_value(flag: &[u8]) -> String {
    match flag {
        b"--test-dir"
        | b"--build-dir"
        | b"--source-dir"
        | b"--repeat-until-fail"
        | b"--preset"
        | b"--presets-file" => {
            format!("'{}' requires an argument", display(flag))
        }
        b"-D" => "-D must be followed by dashboard mode or VAR=VALUE.".into(),
        _ => format!("Invalid value used with {}", display(flag)),
    }
}

fn unknown(arg: &[u8]) -> String {
    const OPTIONS: &[&str] = &[
        "--build-dir",
        "--debug",
        "--exclude-regex",
        "--interactive-debug-mode",
        "--label-regex",
        "--list-presets",
        "--no-tests",
        "--output-junit",
        "--output-log",
        "--output-on-failure",
        "--parallel",
        "--preset",
        "--presets-file",
        "--quiet",
        "--repeat",
        "--show-only",
        "--source-dir",
        "--stop-on-failure",
        "--test-dir",
        "--tests-regex",
        "--test-load",
        "--timeout",
        "--verbose",
    ];
    let suggestion = OPTIONS
        .iter()
        .find(|option| {
            let (long, short) = if option.len() > arg.len() {
                (option.as_bytes(), arg)
            } else {
                (arg, option.as_bytes())
            };
            let common = long
                .iter()
                .zip(short)
                .take_while(|(left, right)| left == right)
                .count();
            match (long.get(common..), short.get(common..)) {
                (Some(left), Some(right)) if left.len() == right.len() + 1 => {
                    left.get(1..) == Some(right)
                }
                (Some(left), Some(right)) if left.len() == right.len() => {
                    left.get(1..) == right.get(1..)
                }
                _ => false,
            }
        })
        .map_or_else(String::new, |option| format!(". Did you mean: {option}?"));
    format!(
        "Unknown argument: {}{suggestion}\nCMake Error: Run 'ctest --help' for all supported options.",
        display(arg)
    )
}

pub(crate) fn processor_count(host: &dyn Host) -> usize {
    host.env_get(b"__CTEST_FAKE_PROCESSOR_COUNT_FOR_TESTING")
        .and_then(|value| value.to_string().parse::<usize>().ok())
        .unwrap_or_else(|| host.system().cpu_count as usize)
}

fn parse_jobs(flag: &[u8], raw: Option<&[u8]>) -> Result<TestJobs, String> {
    match raw {
        None | Some(b"") => Ok(TestJobs::Automatic),
        Some(raw) => std::str::from_utf8(raw)
            .ok()
            .and_then(|raw| raw.trim_start().parse().ok())
            .map(TestJobs::Count)
            .ok_or_else(|| format!("'{}' given invalid value '{}'", display(flag), display(raw))),
    }
}

pub(crate) fn parse_repeat(flag: &[u8], raw: &[u8]) -> Result<TestRepeat, String> {
    let parse_count = |count| {
        std::str::from_utf8(count)
            .ok()?
            .trim_start()
            .parse::<i32>()
            .ok()
    };
    if flag == b"--repeat-until-fail" {
        let count = parse_count(raw).ok_or_else(|| {
            format!(
                "'--repeat-until-fail' given non-integer value '{}'",
                display(raw)
            )
        })?;
        return Ok(TestRepeat {
            mode: RepeatMode::UntilFail,
            count,
        });
    }
    let mut parts = raw.splitn(2, |&byte| byte == b':');
    let repeat = parts.next().zip(parts.next()).and_then(|(mode, count)| {
        let mode = match mode {
            b"until-fail" => RepeatMode::UntilFail,
            b"until-pass" => RepeatMode::UntilPass,
            b"after-timeout" => RepeatMode::AfterTimeout,
            _ => return None,
        };
        if count.is_empty() || !count.iter().all(u8::is_ascii_digit) {
            return None;
        }
        Some(TestRepeat {
            mode,
            count: parse_count(count)?,
        })
    });
    repeat.ok_or_else(|| format!("'--repeat' given invalid value '{}'", display(raw)))
}

fn parse_no_tests(raw: &[u8]) -> Result<NoTestsAction, String> {
    match raw {
        b"error" => Ok(NoTestsAction::Error),
        b"ignore" => Ok(NoTestsAction::Ignore),
        _ => Err(format!(
            "'--no-tests=' given unknown value '{}'",
            display(raw)
        )),
    }
}

fn parse_show_only(raw: &[u8]) -> Result<ShowOnly, String> {
    match raw {
        b"human" => Ok(ShowOnly::Human),
        b"json-v1" => Ok(ShowOnly::JsonV1),
        b"json-v1-raw" => Ok(ShowOnly::JsonV1Raw),
        _ => Err(format!(
            "'--show-only=' given unknown value '{}'",
            display(raw)
        )),
    }
}

pub(crate) fn truncation_mode(raw: &[u8]) -> Option<OutputTruncation> {
    match raw {
        b"head" => Some(OutputTruncation::Head),
        b"middle" => Some(OutputTruncation::Middle),
        b"tail" => Some(OutputTruncation::Tail),
        _ => None,
    }
}

fn output_integer(flag: &[u8], raw: &[u8]) -> Result<i32, String> {
    std::str::from_utf8(raw)
        .ok()
        .and_then(|value| value.trim_start().parse().ok())
        .ok_or_else(|| format!("Invalid value for '{}': {}", display(flag), display(raw)))
}

pub(crate) fn warning_number(host: &dyn Host, raw: &[u8], message: &str) -> Option<u64> {
    let number = std::str::from_utf8(raw)
        .ok()
        .and_then(|value| value.trim_start().parse().ok());
    if number.is_none() {
        host.write_out(
            Stream::Stderr,
            format!("{message}{}\n", display(raw)).as_bytes(),
        );
    }
    number
}

/// Read a decimal number at the start of a CTest option value.
pub(crate) fn number_prefix(raw: &[u8]) -> Option<f64> {
    let raw = raw.trim_ascii_start();
    let mut end = 0;
    let mut exponent = false;
    let mut dot = false;
    for (index, byte) in raw.iter().copied().enumerate() {
        match byte {
            b'0'..=b'9' => {}
            b'.' if !dot && !exponent => dot = true,
            b'e' | b'E' if !exponent => exponent = true,
            b'+' | b'-'
                if index == 0 || raw.get(index - 1).is_some_and(|byte| b"eE".contains(byte)) => {}
            _ => break,
        }
        end = index + 1;
    }
    let candidate = std::str::from_utf8(raw.get(..end)?).ok()?;
    candidate
        .parse()
        .ok()
        .or_else(|| candidate.split_once(['e', 'E'])?.0.parse().ok())
}

fn parse_stop_time(raw: &[u8]) -> Option<u32> {
    let (hour, rest) = std::str::from_utf8(raw).ok()?.split_once(':')?;
    let (minute, second) = rest.split_once(':').unwrap_or((rest, "0"));
    let (hour, minute, second): (u32, u32, u32) = (
        hour.parse().ok()?,
        minute.parse().ok()?,
        second.parse().ok()?,
    );
    (hour < 24 && minute < 60 && second < 60).then(|| hour * 3600 + minute * 60 + second)
}

const ACTIONS: &[&str] = &[
    "all",
    "start",
    "update",
    "configure",
    "build",
    "test",
    "coverage",
    "memcheck",
    "notes",
    "submit",
];

fn parse_action(raw: &[u8]) -> Result<BString, String> {
    if let Some(action) = ACTIONS
        .iter()
        .find(|action| raw.eq_ignore_ascii_case(action.as_bytes()))
    {
        return Ok((*action).into());
    }
    let mut message = format!(
        "CTest -T called with incorrect option: {}\nAvailable options are:",
        display(raw)
    );
    for action in ACTIONS {
        message.push_str(&format!("\n  ctest -T {action}"));
    }
    Err(message)
}

fn parse_definition(raw: &[u8]) -> Result<BString, String> {
    let dashboard = display(raw);
    let valid_dashboard = ["Continuous", "Experimental", "Nightly"]
        .iter()
        .any(|mode| {
            dashboard.strip_prefix(mode).is_some_and(|step| {
                matches!(
                    step,
                    "" | "Start"
                        | "Update"
                        | "Configure"
                        | "Build"
                        | "Test"
                        | "Coverage"
                        | "MemCheck"
                        | "Submit"
                )
            })
        })
        || raw == b"NightlyMemoryCheck";
    if raw.contains(&b'=') || valid_dashboard {
        return Ok(raw.into());
    }
    let mut message =
        format!("CTest -D called with incorrect option: {dashboard}\nAvailable options are:");
    for mode in ["Continuous", "Experimental", "Nightly"] {
        message.push_str(&format!("\n  ctest -D {mode}\n  ctest -D {mode}(Start|Update|Configure|Build)\n  ctest -D {mode}(Test|Coverage|MemCheck|Submit)"));
    }
    message.push_str("\n  ctest -D NightlyMemoryCheck\n  ctest -D CTEST_<VAR>=<value>\n  ctest -D <var>:<type>=<value>");
    Err(message)
}

macro_rules! fill_missing {
    ($target:expr, $preset:expr, $($field:ident),+ $(,)?) => {
        $(if $target.$field.is_none() {
            $target.$field = $preset.$field;
        })+
    };
}

impl Invocation {
    fn resolve(&mut self, host: &dyn Host, timeout: Option<f64>) -> Result<(), String> {
        let cwd = host.initial_cwd();
        self.presets_file = self.presets_file.take().map(|file| cwd.join(file));
        if !matches!(self.mode, Mode::ListPresets { .. }) {
            if self.preset.is_some() {
                self.load_test_preset(host, &cwd, true)?;
            } else if self.scripts.is_empty()
                && (self.actions.is_empty() || self.actions.iter().any(|action| action == "test"))
            {
                let preset = self
                    .definitions
                    .iter()
                    .find_map(|definition| definition.strip_prefix(b"CTEST_TEST_PRESET="))
                    .or_else(|| {
                        self.definitions
                            .iter()
                            .find_map(|definition| definition.strip_prefix(b"CTEST_PRESET="))
                    });
                if let Some(preset) = preset {
                    let build = cwd.join(&self.options.test_dir);
                    let config = crate::dart::Configuration::load(
                        host,
                        &build,
                        &self.overwrite_configuration_file,
                    )
                    .map_err(|error| error.to_string())?;
                    self.preset = Some(preset.into());
                    let source = build.join(to_native(&config.source_directory));
                    self.load_test_preset(host, &source, false)?;
                }
            }
        }
        self.options.test_dir = cwd.join(&self.options.test_dir).components().collect();
        self.resolve_options(host, timeout)
    }

    pub(crate) fn load_test_preset(
        &mut self,
        host: &dyn Host,
        source: &std::path::Path,
        use_binary_dir: bool,
    ) -> Result<(), String> {
        let Some(name) = &self.preset else {
            return Ok(());
        };
        let presets = match &self.presets_file {
            Some(file) => rcmake_presets::load_file(source, file, host),
            None => rcmake_presets::load(source, host),
        }
        .map_err(preset_diagnostic)?;
        let preset = presets
            .resolve(PresetKind::Test, name, &EnvSnapshot::from_host(host))
            .map_err(preset_diagnostic)?;
        if use_binary_dir
            && self.options.test_dir.as_os_str().is_empty()
            && let Some(binary) = preset.binary_dir
        {
            self.options.test_dir = to_native(&binary);
        }
        self.environment = preset.environment;
        if let PresetSettings::Test(settings) = preset.settings {
            self.apply_preset(*settings);
        }
        Ok(())
    }

    pub(crate) fn resolve_options(
        &mut self,
        host: &dyn Host,
        timeout: Option<f64>,
    ) -> Result<(), String> {
        let env = |name: &[u8]| {
            self.environment
                .get(name)
                .cloned()
                .unwrap_or_else(|| host.env_get(name))
        };
        let jobs = self.execution.jobs.or_else(|| {
            let raw = env(b"CTEST_PARALLEL_LEVEL")?;
            parse_jobs(b"--parallel", Some(&raw)).ok()
        });
        self.options.jobs = match jobs {
            None => 1,
            Some(TestJobs::Automatic) => processor_count(host).max(2),
            Some(TestJobs::Count(0)) => usize::MAX,
            Some(TestJobs::Count(count)) => count as usize,
        };
        let no_tests = match self.execution.no_tests_action {
            Some(NoTestsAction::Error) => Some(NoTestsAction::Error),
            Some(NoTestsAction::Ignore) => Some(NoTestsAction::Ignore),
            _ => env(b"CTEST_NO_TESTS_ACTION")
                .filter(|raw| !raw.is_empty())
                .map(|raw| {
                    parse_no_tests(&raw)
                        .map_err(|_| format!("Unknown value for CTEST_NO_TESTS_ACTION: '{raw}'"))
                })
                .transpose()?,
        };
        self.options.error_on_no_tests = no_tests == Some(NoTestsAction::Error);
        self.options.ignore_no_tests = no_tests == Some(NoTestsAction::Ignore);
        self.options.timeout = timeout
            .or(self.execution.timeout.map(f64::from))
            .and_then(|timeout| Duration::try_from_secs_f64(timeout.max(0.0)).ok());
        self.options.show_only = self.execution.show_only.is_some();
        self.options.stop_on_failure = self.execution.stop_on_failure.unwrap_or(false);
        self.options.output_on_failure = self.output.output_on_failure.unwrap_or(false)
            || env(b"CTEST_OUTPUT_ON_FAILURE").is_some_and(|raw| truth::is_on(&raw));
        self.options.quiet = self.output.quiet.unwrap_or(false);
        self.options.verbose = matches!(
            self.output.verbosity,
            Some(TestVerbosity::Verbose | TestVerbosity::Extra)
        );
        self.selection.schedule_random |= self.execution.schedule_random.unwrap_or(false);
        self.test_load = self.test_load.or(self
            .execution
            .test_load
            .and_then(|load| u64::try_from(load).ok()));
        Ok(())
    }

    fn apply_preset(&mut self, settings: TestSettings) {
        self.options.config = self.options.config.take().or(settings.configuration);
        if let Some(filter) = settings.filter {
            if let Some(include) = filter.include {
                self.options.include = self.options.include.take().or(include.name);
                self.options.include_labels.splice(0..0, include.label);
                self.selection.union |= include.use_union.unwrap_or(false);
                if self.selection.indices.is_none() {
                    self.selection.indices = include.index.map(IndexSource::Preset);
                }
            }
            if let Some(exclude) = filter.exclude {
                self.options.exclude = self.options.exclude.take().or(exclude.name);
                self.options.exclude_labels.splice(0..0, exclude.label);
                if let Some(fixtures) = exclude.fixtures {
                    self.selection.exclude_fixture =
                        self.selection.exclude_fixture.take().or(fixtures.any);
                    self.selection.exclude_fixture_setup = self
                        .selection
                        .exclude_fixture_setup
                        .take()
                        .or(fixtures.setup);
                    self.selection.exclude_fixture_cleanup = self
                        .selection
                        .exclude_fixture_cleanup
                        .take()
                        .or(fixtures.cleanup);
                }
            }
        }
        if let Some(output) = settings.output {
            fill_missing!(
                self.output,
                output,
                short_progress,
                verbosity,
                debug,
                output_on_failure,
                quiet,
                output_log_file,
                output_junit_file,
                label_summary,
                subproject_summary,
                max_passed_test_output_size,
                max_failed_test_output_size,
                test_output_truncation,
                max_test_name_width
            );
        }
        if let Some(execution) = settings.execution {
            fill_missing!(
                self.execution,
                execution,
                stop_on_failure,
                enable_failover,
                jobs,
                resource_spec_file,
                test_load,
                show_only,
                repeat,
                interactive_debugging,
                schedule_random,
                timeout,
                no_tests_action
            );
            if let Some(mut args) = execution.test_passthrough_arguments {
                args.extend(
                    self.execution
                        .test_passthrough_arguments
                        .take()
                        .unwrap_or_default(),
                );
                self.execution.test_passthrough_arguments = Some(args);
            }
        }
        self.overwrite_configuration_file.splice(
            0..0,
            settings.overwrite_configuration_file.into_iter().flatten(),
        );
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use rcmake_host::PureHost;
    use rcmake_presets::{TestIndex, TestIndexRange};

    use super::*;

    fn parse_args(host: &dyn Host, args: &[&str]) -> Result<Invocation, String> {
        parse(
            &args.iter().copied().map(BString::from).collect::<Vec<_>>(),
            host,
        )
    }

    #[test]
    fn reads_selection_and_execution_flags() -> Result<(), String> {
        let host = PureHost::default();
        let invocation = parse_args(
            &host,
            &[
                "-R",
                "^app",
                "-E=slow",
                "-L",
                "unit",
                "--label-regex",
                "fast",
                "-LE",
                "linux",
                "-LE",
                "net",
                "-FA",
                "both",
                "-FS",
                "setup",
                "-FC",
                "cleanup",
                "-I",
                "1,5,2,4",
                "-U",
                "--tests-from-file",
                "include.txt",
                "--exclude-from-file",
                "exclude.txt",
                "--rerun-failed",
                "--out-of-date",
                "-C",
                "Release",
                "-j4",
                "--timeout",
                "1.5",
                "--stop-on-failure",
                "--repeat=after-timeout:2",
                "--schedule-random",
                "--schedule-random-seed",
                "42",
                "--test-load",
                "5",
                "--stop-time",
                "13:02:03",
                "--test-dir",
                "out",
                "--no-tests=error",
            ],
        )?;
        assert_eq!(
            invocation.selection,
            SelectionOptions {
                indices: Some(IndexSource::Argument("1,5,2,4".into())),
                union: true,
                exclude_fixture: Some("both".into()),
                exclude_fixture_setup: Some("setup".into()),
                exclude_fixture_cleanup: Some("cleanup".into()),
                tests_from_file: Some("include.txt".into()),
                exclude_from_file: Some("exclude.txt".into()),
                rerun_failed: true,
                out_of_date: true,
                schedule_random: true,
                random_seed: Some(42),
            }
        );
        assert_eq!(
            invocation.options,
            CtestOptions {
                test_dir: "/work/out".into(),
                config: Some("Release".into()),
                include: Some("^app".into()),
                exclude: Some("slow".into()),
                include_labels: vec!["unit".into(), "fast".into()],
                exclude_labels: vec!["linux".into(), "net".into()],
                jobs: 4,
                timeout: Some(Duration::from_millis(1500)),
                stop_on_failure: true,
                error_on_no_tests: true,
                ..CtestOptions::default()
            }
        );
        assert_eq!(
            invocation.execution.repeat,
            Some(TestRepeat {
                mode: RepeatMode::AfterTimeout,
                count: 2
            })
        );
        assert_eq!(invocation.test_load, Some(5));
        assert_eq!(invocation.stop_time, Some(46_923));
        Ok(())
    }

    #[test]
    fn reads_output_scripts_and_arguments_for_test_processes() -> Result<(), String> {
        let host = PureHost::default();
        let invocation = parse_args(
            &host,
            &[
                "-N",
                "--show-only=json-v1-raw",
                "--output-on-failure",
                "--output-junit",
                "junit.xml",
                "-O",
                "ctest.log",
                "-V",
                "-VV",
                "-Q",
                "--progress",
                "--print-labels",
                "--no-label-summary",
                "-T",
                "Test",
                "-D",
                "COUNT:STRING=1",
                "-DVAR=2",
                "-S",
                "run.cmake,arg",
                "-SP",
                "other.cmake",
                "--",
                "--help",
                "data",
            ],
        )?;
        assert_eq!(invocation.execution.show_only, Some(ShowOnly::JsonV1Raw));
        assert_eq!(
            invocation.execution.test_passthrough_arguments,
            Some(vec!["--help".into(), "data".into()])
        );
        assert_eq!(
            invocation.output,
            TestOutput {
                output_on_failure: Some(true),
                output_junit_file: Some("junit.xml".into()),
                output_log_file: Some("ctest.log".into()),
                verbosity: Some(TestVerbosity::Extra),
                quiet: Some(true),
                short_progress: Some(true),
                label_summary: Some(false),
                ..TestOutput::default()
            }
        );
        assert!(invocation.print_labels);
        assert_eq!(invocation.actions, ["test"]);
        assert_eq!(invocation.definitions, ["COUNT:STRING=1", "VAR=2"]);
        assert_eq!(
            invocation.scripts,
            [
                Script {
                    argument: "run.cmake,arg".into(),
                    new_process: false
                },
                Script {
                    argument: "other.cmake".into(),
                    new_process: true
                },
            ]
        );
        assert_eq!(
            parse_args(&host, &["--help"])?.mode,
            Mode::Help(vec!["--help".into()])
        );
        assert_eq!(
            parse_args(&host, &["--version"])?.mode,
            Mode::Version {
                json: false,
                file: None
            }
        );
        assert_eq!(
            parse_args(&host, &["--list-presets=defined"])?.mode,
            Mode::ListPresets { defined: true }
        );
        assert_eq!(
            parse_args(&host, &["--list-presets"])?.mode,
            Mode::ListPresets { defined: false }
        );
        assert_eq!(
            parse_args(
                &host,
                &["--instrument", "--role", "test", "--", "program", "--arg"]
            )?
            .mode,
            Mode::Instrument {
                args: vec!["--role".into(), "test".into()],
                command: vec!["program".into(), "--arg".into()]
            }
        );
        Ok(())
    }

    #[test]
    fn accepts_repeat_modes_and_reports_invalid_values() -> Result<(), String> {
        let host = PureHost::default();
        for (args, mode) in [
            (["--repeat", "until-fail:3"], RepeatMode::UntilFail),
            (["--repeat", "until-pass:3"], RepeatMode::UntilPass),
            (["--repeat", "after-timeout:3"], RepeatMode::AfterTimeout),
            (["--repeat-until-fail", "3"], RepeatMode::UntilFail),
        ] {
            assert_eq!(
                parse_args(&host, &args)?.execution.repeat,
                Some(TestRepeat { mode, count: 3 })
            );
        }
        for (args, error) in [
            (
                vec!["--repeat", "bad:1"],
                "'--repeat' given invalid value 'bad:1'",
            ),
            (
                vec!["--repeat-until-fail", "bad"],
                "'--repeat-until-fail' given non-integer value 'bad'",
            ),
            (
                vec!["--repeat=until-fail:2", "--repeat=until-pass:1"],
                "At most one '--repeat' option may be used.",
            ),
            (
                vec!["--no-tests=action"],
                "'--no-tests=' given unknown value 'action'",
            ),
            (
                vec!["--show-only=unknown"],
                "'--show-only=' given unknown value 'unknown'",
            ),
            (vec!["-I"], "Invalid value used with -I"),
            (
                vec!["--parallel", "-1"],
                "'--parallel' given invalid value '-1'",
            ),
        ] {
            assert_eq!(parse_args(&host, &args).err().as_deref(), Some(error));
        }
        assert_eq!(
            parse_args(&host, &["--parallel", "0"])?.options.jobs,
            usize::MAX
        );
        assert!(parse_args(&host, &["-j"])?.options.jobs >= 2);
        assert_eq!(
            parse_args(&host, &["--test-load=bad", "--schedule-random-seed=bad"])?.test_load,
            None
        );
        assert_eq!(
            host.stream_bytes(Stream::Stderr),
            "Invalid value for 'Test Load' : bad\nInvalid value for '--schedule-random-seed': bad\n"
        );
        Ok(())
    }

    #[test]
    fn resolves_v13_presets_and_applies_cli_overrides() -> Result<(), String> {
        let host = PureHost::default();
        host.env_set(b"UNSET", Some(b"parent"));
        host.seed_file(Path::new("/work/CMakePresets.json"), br#"{
          "version": 13,
          "configurePresets": [{
            "name": "cfg", "generator": "Ninja", "binaryDir": "${sourceDir}/build",
            "environment": { "FROM_CONFIGURE": "1" }
          }],
          "testPresets": [{
            "name": "test", "configurePreset": "cfg", "configuration": "Debug",
            "environment": { "CTEST_OUTPUT_ON_FAILURE": "ON", "UNSET": null },
            "filter": {
              "include": { "name": "^test", "label": "smoke", "index": { "start": 2, "stride": 3 }, "useUnion": true },
              "exclude": { "name": "slow", "label": "network", "fixtures": { "setup": "setup" } }
            },
            "execution": {
              "jobs": 2, "stopOnFailure": true, "scheduleRandom": true, "testLoad": 4,
              "noTestsAction": "error", "timeout": 15, "showOnly": "json-v1",
              "repeat": { "mode": "until-pass", "count": 3 }, "testPassthroughArguments": ["preset"]
            },
            "output": { "outputJUnitFile": "preset.xml", "outputLogFile": "preset.log", "verbosity": "extra", "labelSummary": true }
          }]
        }"#).map_err(|error| error.to_string())?;
        let invocation = parse_args(
            &host,
            &[
                "--preset=test",
                "-C",
                "Release",
                "-E",
                "manual",
                "-L",
                "quick",
                "--parallel",
                "7",
                "--no-tests=ignore",
                "--output-junit",
                "cli.xml",
                "--no-label-summary",
                "--schedule-random-seed",
                "3",
            ],
        )?;
        assert_eq!(invocation.options.test_dir, Path::new("/work/build"));
        assert_eq!(invocation.options.config, Some("Release".into()));
        assert_eq!(invocation.options.include, Some("^test".into()));
        assert_eq!(invocation.options.exclude, Some("manual".into()));
        assert_eq!(invocation.options.include_labels, ["smoke", "quick"]);
        assert_eq!(invocation.options.exclude_labels, ["network"]);
        assert_eq!(invocation.options.jobs, 7);
        assert!(invocation.options.stop_on_failure);
        assert!(invocation.options.ignore_no_tests);
        assert!(invocation.options.output_on_failure);
        assert_eq!(invocation.options.timeout, Some(Duration::from_secs(15)));
        assert_eq!(
            invocation.selection.indices,
            Some(IndexSource::Preset(TestIndex::Range(TestIndexRange {
                start: Some(2),
                stride: Some(3),
                ..TestIndexRange::default()
            })))
        );
        assert_eq!(
            invocation.selection.exclude_fixture_setup,
            Some("setup".into())
        );
        assert!(invocation.selection.union);
        assert!(invocation.selection.schedule_random);
        assert_eq!(invocation.selection.random_seed, Some(3));
        assert_eq!(invocation.test_load, Some(4));
        assert_eq!(invocation.output.output_junit_file, Some("cli.xml".into()));
        assert_eq!(invocation.output.output_log_file, Some("preset.log".into()));
        assert_eq!(invocation.output.label_summary, Some(false));
        assert_eq!(invocation.output.verbosity, Some(TestVerbosity::Extra));
        assert_eq!(invocation.execution.show_only, Some(ShowOnly::JsonV1));
        assert_eq!(
            invocation.execution.repeat,
            Some(TestRepeat {
                mode: RepeatMode::UntilPass,
                count: 3
            })
        );
        assert_eq!(
            invocation.execution.test_passthrough_arguments,
            Some(vec!["preset".into()])
        );
        assert_eq!(
            invocation.environment.get(b"FROM_CONFIGURE".as_slice()),
            Some(&Some("1".into()))
        );
        assert_eq!(invocation.environment.get(b"UNSET".as_slice()), Some(&None));
        let overridden = parse_args(
            &host,
            &["--preset", "test", "--test-dir", "local", "--", "cli"],
        )?;
        assert_eq!(overridden.options.test_dir, Path::new("/work/local"));
        assert_eq!(
            overridden.execution.test_passthrough_arguments,
            Some(vec!["preset".into(), "cli".into()])
        );
        Ok(())
    }
}
