//! Native CTest test discovery and execution.

#![forbid(unsafe_code)]

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use rcmake_base::BString;
use rcmake_host::{Host, HostError, Stream, WriteMode, path};
use rcmake_json::Value;
use rcmake_presets::{EnvSnapshot, PresetKind, ShowOnly};

use crate::cli::{Invocation, Mode};

mod cli;
mod dart;
mod discover;
mod discovery;
mod execution;
mod junit;
mod reporting;
mod schedule;
#[allow(
    unfulfilled_lint_expectations,
    reason = "CTest calls the script stub before dashboard support is implemented"
)]
mod script;
mod selection;
mod state;

#[cfg(test)]
mod tests;

/// Options for a CTest run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CtestOptions {
    /// Build directory that contains the top-level `CTestTestfile.cmake` (`--test-dir`).
    pub test_dir: PathBuf,
    /// Configuration to test (`-C`, `--build-config`).
    pub config: Option<BString>,
    /// Include tests with matching names (`-R`).
    pub include: Option<BString>,
    /// Exclude tests with matching names (`-E`).
    pub exclude: Option<BString>,
    /// Include tests with a matching label (`-L`). Each pattern must match a label.
    pub include_labels: Vec<BString>,
    /// Exclude tests by label (`-LE`). Each pattern must match a label to exclude a test.
    pub exclude_labels: Vec<BString>,
    /// Maximum number of concurrent test processes (`-j`). Zero uses one job.
    pub jobs: usize,
    /// Print output from failed tests (`--output-on-failure`).
    pub output_on_failure: bool,
    /// Print commands and test output (`-V`, `--verbose`).
    pub verbose: bool,
    /// Suppress run reports (`-Q`, `--quiet`).
    pub quiet: bool,
    /// List selected tests without starting them (`-N`, `--show-only`).
    pub show_only: bool,
    /// Return exit code 8 when there are no tests to run.
    pub error_on_no_tests: bool,
    /// Suppress the diagnostic when there are no tests to run.
    pub ignore_no_tests: bool,
    /// Default timeout per test (`--timeout`).
    pub timeout: Option<Duration>,
    /// Stop starting regular tests after a failure (`--stop-on-failure`).
    pub stop_on_failure: bool,
}

impl Default for CtestOptions {
    fn default() -> Self {
        Self {
            test_dir: PathBuf::new(),
            config: None,
            include: None,
            exclude: None,
            include_labels: Vec::new(),
            exclude_labels: Vec::new(),
            jobs: 1,
            output_on_failure: false,
            verbose: false,
            quiet: false,
            show_only: false,
            error_on_no_tests: false,
            ignore_no_tests: false,
            timeout: None,
            stop_on_failure: false,
        }
    }
}

/// Result of a test run. Test names are in discovery order.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CtestOutcome {
    /// Process exit code. Zero indicates success. Eight indicates failure.
    pub exit_code: i32,
    /// Names of tests that passed.
    pub passed: Vec<BString>,
    /// Names of tests that failed or could not run.
    pub failed: Vec<BString>,
}

/// Error that prevents the runner from starting tests.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum CtestError {
    /// A test filter or property contains an invalid regular expression.
    #[error("Invalid regular expression: {}", .0)]
    BadRegex(BString),
    /// Matching a test name or label exceeded the regular expression step limit.
    #[error("Regular expression matching exceeded the step limit")]
    RegexLimit,
    /// The interpreter could not evaluate the test file.
    #[error("Cannot load test definitions from {}", .0.display())]
    Load(PathBuf),
    /// The host could not access the test file.
    #[error("Cannot access {}: {source}", path.display())]
    Access {
        /// Path to the test file.
        path: PathBuf,
        /// Host error for the file.
        source: HostError,
    },
    /// Test dependencies or fixture repeat modes prevent scheduling.
    #[error("{0}")]
    Schedule(String),
    /// The option or feature is not implemented yet.
    #[error("{0} is not supported yet")]
    Unsupported(&'static str),
}

/// Discover and run the tests under `options.test_dir`. Write reports through `host`.
///
/// # Errors
///
/// Returns an error if discovery fails or a regular expression cannot be evaluated.
pub fn run(host: Arc<dyn Host>, options: &CtestOptions) -> Result<CtestOutcome, CtestError> {
    run_tests(
        host,
        &Invocation {
            options: options.clone(),
            ..Invocation::default()
        },
        false,
    )
}

/// Run CTest with the executable name in `argv[0]`.
///
/// Return the exit code for this CTest invocation or its launcher command.
pub fn main(argv: &[BString], host: Arc<dyn Host>) -> i32 {
    let args = argv.get(1..).unwrap_or_default();
    match cli::parse(args, host.as_ref()).and_then(|invocation| dispatch(&host, &invocation, args))
    {
        Ok(code) => code,
        Err(error) => {
            host.write_out(Stream::Stderr, format!("CTest Error: {error}\n").as_bytes());
            1
        }
    }
}

fn dispatch(
    host: &Arc<dyn Host>,
    invocation: &Invocation,
    args: &[BString],
) -> Result<i32, String> {
    match &invocation.mode {
        Mode::Instrument { .. } => rcmake_instrument::launcher::run_launcher(host.as_ref(), args)
            .map_err(|error| error.to_string()),
        Mode::ListPresets { .. } => list_presets(host.as_ref(), invocation),
        Mode::Version { json, file } => {
            let version = if *json {
                format!(
                    "{{\"version\":{{\"major\":{},\"minor\":{},\"patch\":{},\"string\":\"{}\"}}}}\n",
                    rcmake_base::CMAKE_VERSION_MAJOR,
                    rcmake_base::CMAKE_VERSION_MINOR,
                    rcmake_base::CMAKE_VERSION_PATCH,
                    rcmake_base::CMAKE_VERSION_STR
                )
            } else {
                format!("ctest version {}\n", rcmake_base::CMAKE_VERSION_STR)
            };
            if let Some(file) = file {
                host.write(
                    &host.initial_cwd().join(file),
                    version.as_bytes(),
                    WriteMode::Truncate,
                )
                .map_err(|error| error.to_string())?;
            } else {
                host.write_out(Stream::Stdout, version.as_bytes());
            }
            Ok(0)
        }
        Mode::Help(_) => {
            host.write_out(Stream::Stdout, b"Usage: ctest [options]\n");
            Ok(0)
        }
        Mode::Test => {
            let _environment = PresetEnvironment::apply(host.as_ref(), &invocation.environment);
            if invocation.scripts.is_empty() {
                return match run_tests(Arc::clone(host), invocation, true) {
                    Ok(outcome) => Ok(outcome.exit_code),
                    Err(error @ (CtestError::Load(_) | CtestError::Schedule(_))) => {
                        host.write_out(
                            Stream::Stderr,
                            format!("CTest Error: {error}\n").as_bytes(),
                        );
                        Ok(8)
                    }
                    Err(error) => Err(error.to_string()),
                };
            }
            let mut code = 0;
            for script in &invocation.scripts {
                let file = script
                    .argument
                    .split(|&byte| byte == b',')
                    .next()
                    .unwrap_or_default();
                code = code.max(
                    script::run_script(
                        host.as_ref(),
                        &host.initial_cwd().join(path::to_native(file)),
                    )
                    .map_err(|error| error.to_string())?,
                );
            }
            Ok(code)
        }
    }
}

struct PresetEnvironment<'a> {
    host: &'a dyn Host,
    previous: Vec<(BString, Option<BString>)>,
}

impl<'a> PresetEnvironment<'a> {
    fn apply(host: &'a dyn Host, settings: &BTreeMap<BString, Option<BString>>) -> Self {
        let previous = settings
            .iter()
            .map(|(key, value)| {
                let previous = host.env_get(key);
                host.env_set(key, value.as_ref().map(|value| value.as_slice()));
                (key.clone(), previous)
            })
            .collect();
        Self { host, previous }
    }
}

impl Drop for PresetEnvironment<'_> {
    fn drop(&mut self) {
        for (key, value) in &self.previous {
            self.host
                .env_set(key, value.as_ref().map(|value| value.as_slice()));
        }
    }
}

fn list_presets(host: &dyn Host, invocation: &Invocation) -> Result<i32, String> {
    let cwd = host.initial_cwd();
    let presets = match &invocation.presets_file {
        Some(file) => rcmake_presets::load_file(&cwd, file, host),
        None => rcmake_presets::load(&cwd, host),
    }
    .map_err(|error| error.message)?;
    let summaries = presets.list_visible(PresetKind::Test, &EnvSnapshot::from_host(host));
    if summaries.is_empty() {
        return Ok(0);
    }
    let width = summaries
        .iter()
        .map(|preset| preset.name.len())
        .max()
        .unwrap_or(0);
    host.write_out(Stream::Stdout, b"Available test presets:\n\n");
    for preset in summaries {
        let mut line = format!("  \"{}\"", preset.name);
        if let Some(display_name) = preset.display_name.filter(|name| !name.is_empty()) {
            let padding = " ".repeat(width - preset.name.len());
            line.push_str(&format!("{padding} - {display_name}"));
        }
        line.push('\n');
        host.write_out(Stream::Stdout, line.as_bytes());
    }
    Ok(0)
}

fn run_tests(
    host: Arc<dyn Host>,
    invocation: &Invocation,
    full_selection: bool,
) -> Result<CtestOutcome, CtestError> {
    let options = &invocation.options;
    let deadline = schedule::stop_deadline(host.as_ref(), invocation.stop_time);
    let dir = host.initial_cwd().join(&options.test_dir);
    let dir =
        path::to_native(&rcmake_base::path_lex::Lexical::new(&path::from_native(&dir)).normalize());
    let mut tests = discovery::load(Arc::clone(&host), &dir, options.config.as_ref())?;
    if let Some(args) = &invocation.execution.test_passthrough_arguments {
        for test in &mut tests {
            test.command.args.extend_from_slice(args);
        }
    }
    let plan = if full_selection {
        selection::plan_with(host.as_ref(), &dir, tests, options, &invocation.selection)?
    } else {
        selection::plan(tests, options)?
    };
    let report = reporting::Report::new(host.as_ref(), options, &plan);
    let show_only = invocation.execution.show_only;
    if let Some(format @ (ShowOnly::JsonV1 | ShowOnly::JsonV1Raw)) = show_only {
        if !options.quiet {
            show_json(host.as_ref(), &plan, options, format == ShowOnly::JsonV1Raw);
        }
        return Ok(CtestOutcome::default());
    }
    report.project(&dir);
    if invocation.print_labels {
        if !options.quiet {
            print_labels(host.as_ref(), &plan);
        }
        return Ok(CtestOutcome::default());
    }
    if options.show_only {
        report.list(&plan);
        return Ok(CtestOutcome::default());
    }

    if !plan.is_acyclic() {
        if !options.quiet {
            host.write_out(
                Stream::Stderr,
                b"Error: a cycle exists in the test dependency graph.\n",
            );
        }
        return Ok(CtestOutcome {
            exit_code: 8,
            ..CtestOutcome::default()
        });
    }

    let started = host.monotonic();
    let results = schedule::run(host.as_ref(), &dir, invocation, &plan, &report, deadline)?;
    if let Err(error) =
        rcmake_instrument::hooks::run_hook(host.as_ref(), &dir, rcmake_instrument::Hook::PostCTest)
    {
        host.write_out(
            Stream::Stderr,
            format!("CTest instrumentation error: {error}\n").as_bytes(),
        );
    }
    report.summary(&plan, &results, host.monotonic().saturating_sub(started));
    let no_tests = results
        .iter()
        .flatten()
        .all(|status| *status == execution::Status::Disabled);
    let mut outcome = CtestOutcome::default();
    for (test, result) in plan.tests.iter().zip(results) {
        match result {
            Some(execution::Status::Passed) => outcome.passed.push(test.name.clone()),
            Some(status) if status.failed() => outcome.failed.push(test.name.clone()),
            _ => {}
        }
    }
    if !outcome.failed.is_empty() || (options.error_on_no_tests && no_tests) {
        outcome.exit_code = 8;
    }
    Ok(outcome)
}

fn print_labels(host: &dyn Host, plan: &selection::Plan) {
    let labels: BTreeSet<_> = plan
        .tests
        .iter()
        .flat_map(|test| test.list(b"LABELS"))
        .collect();
    if labels.is_empty() {
        host.write_out(Stream::Stdout, b"No Labels Exist\n");
    } else {
        host.write_out(Stream::Stdout, b"All Labels:\n");
        for label in labels {
            host.write_out(Stream::Stdout, format!("  {label}\n").as_bytes());
        }
    }
}

fn object<const N: usize>(fields: [(&str, Value); N]) -> Value {
    Value::Object(
        fields
            .into_iter()
            .map(|(name, value)| (name.into(), value))
            .collect(),
    )
}

#[derive(PartialEq, Eq, PartialOrd, Ord)]
struct BacktraceNode {
    file: u64,
    line: Option<u64>,
    command: Option<u64>,
    parent: Option<u64>,
}

#[derive(Default)]
struct BacktraceGraph {
    files: Vec<Value>,
    commands: Vec<Value>,
    nodes: Vec<Value>,
    file_ids: BTreeMap<BString, u64>,
    command_ids: BTreeMap<BString, u64>,
    node_ids: BTreeMap<BacktraceNode, u64>,
}

impl BacktraceGraph {
    fn push(&mut self, triples: &[u8]) -> Option<Value> {
        let parts = rcmake_base::list::split(triples, rcmake_base::list::Empty::Keep);
        if !parts.len().is_multiple_of(3) {
            return None;
        }
        let mut parent = None;
        for frame in parts.chunks(3).rev() {
            let Some(file) = frame.first().filter(|file| !file.is_empty()) else {
                continue;
            };
            let file_id = *self.file_ids.entry(file.clone()).or_insert_with(|| {
                let index = self.files.len() as u64;
                self.files.push(Value::string(file.clone()));
                index
            });
            let line = frame
                .get(1)
                .and_then(|line| json_integer(line))
                .filter(|line| *line > 0)
                .map(|line| line as u64);
            let command = frame
                .get(2)
                .filter(|command| !command.is_empty())
                .map(|command| {
                    *self.command_ids.entry(command.clone()).or_insert_with(|| {
                        let index = self.commands.len() as u64;
                        self.commands.push(Value::string(command.clone()));
                        index
                    })
                });
            let id = *self
                .node_ids
                .entry(BacktraceNode {
                    file: file_id,
                    line,
                    command,
                    parent,
                })
                .or_insert_with(|| {
                    let mut node = BTreeMap::from([("file".into(), Value::UInt(file_id))]);
                    for (field, value) in [("line", line), ("command", command), ("parent", parent)]
                    {
                        if let Some(value) = value {
                            node.insert(field.into(), Value::UInt(value));
                        }
                    }
                    let index = self.nodes.len() as u64;
                    self.nodes.push(Value::Object(node));
                    index
                });
            parent = Some(id);
        }
        parent.map(Value::UInt)
    }
}

fn json_integer(value: &[u8]) -> Option<i64> {
    std::str::from_utf8(value).ok()?.trim().parse().ok()
}

fn json_property(key: &[u8], value: &BString, raw: bool) -> Value {
    if raw {
        return Value::string(value.clone());
    }
    use rcmake_base::{list, truth};
    match key {
        b"DISABLED" | b"RUN_SERIAL" | b"WILL_FAIL" => Value::Bool(truth::is_on(value)),
        b"PROCESSORS" | b"SKIP_RETURN_CODE" => Value::Int(json_integer(value).unwrap_or(0)),
        b"TIMEOUT" | b"COST" => Value::Real(
            cli::number_prefix(value)
                .filter(|value| value.is_finite())
                .unwrap_or_default(),
        ),
        b"DEPENDS" | b"FIXTURES_SETUP" | b"FIXTURES_CLEANUP" | b"FIXTURES_REQUIRED" | b"LABELS"
        | b"RESOURCE_LOCK" => {
            let values: BTreeSet<_> = list::split(value, list::Empty::Drop).into_iter().collect();
            Value::Array(values.into_iter().map(Value::string).collect())
        }
        b"ATTACHED_FILES"
        | b"ATTACHED_FILES_ON_FAIL"
        | b"ENVIRONMENT"
        | b"ENVIRONMENT_MODIFICATION"
        | b"FAIL_REGULAR_EXPRESSION"
        | b"PASS_REGULAR_EXPRESSION"
        | b"SKIP_REGULAR_EXPRESSION" => Value::Array(
            list::split(value, list::Empty::Drop)
                .into_iter()
                .map(Value::string)
                .collect(),
        ),
        _ => Value::string(value.clone()),
    }
}

fn show_json(host: &dyn Host, plan: &selection::Plan, options: &CtestOptions, raw: bool) {
    let mut graph = BacktraceGraph::default();
    let tests = plan
        .tests
        .iter()
        .map(|test| {
            let mut fields = BTreeMap::from([("name".into(), Value::string(test.name.clone()))]);
            if test.command.program != "NOT_AVAILABLE" {
                let mut command = test.command.clone();
                execution::resolve_program(host, test, &mut command, options);
                fields.insert(
                    "command".into(),
                    Value::Array(
                        std::iter::once(command.program)
                            .chain(command.args)
                            .map(Value::string)
                            .collect(),
                    ),
                );
            }
            if let Some(config) = &options.config {
                fields.insert("config".into(), Value::string(config.clone()));
            }
            if let Some(frame) = graph.push(test.property(b"_BACKTRACE_TRIPLES")) {
                fields.insert("backtrace".into(), frame);
            }
            let mut properties = test.properties.clone();
            properties.retain(|key, _| !key.starts_with(b"_"));
            properties
                .entry("WORKING_DIRECTORY".into())
                .or_insert_with(|| path::from_native(&test.directory));
            fields.insert(
                "properties".into(),
                Value::Array(
                    properties
                        .into_iter()
                        .map(|(name, value)| {
                            let value = json_property(&name, &value, raw);
                            object([("name", Value::string(name)), ("value", value)])
                        })
                        .collect(),
                ),
            );
            Value::Object(fields)
        })
        .collect();
    let data = object([
        ("kind", Value::string("ctestInfo")),
        (
            "version",
            object([("major", Value::UInt(1)), ("minor", Value::UInt(0))]),
        ),
        (
            "backtraceGraph",
            object([
                ("commands", Value::Array(graph.commands)),
                ("files", Value::Array(graph.files)),
                ("nodes", Value::Array(graph.nodes)),
            ]),
        ),
        ("tests", Value::Array(tests)),
    ]);
    host.write_out(Stream::Stdout, &rcmake_json::write_styled(&data, "  "));
}

#[cfg(test)]
mod wave_two_tests {
    use std::error::Error;
    use std::path::Path;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    use rcmake_host::{HostResult, ProcessResult, PureHost, RunOutput};

    use super::*;

    type TestResult = Result<(), Box<dyn Error>>;

    fn tree(script: &str) -> HostResult<Arc<PureHost>> {
        let host = Arc::new(PureHost::default());
        host.seed_file(Path::new("/work/CTestTestfile.cmake"), script.as_bytes())?;
        Ok(host)
    }

    fn invoke(host: &Arc<PureHost>, args: &[&str]) -> i32 {
        let argv: Vec<_> = std::iter::once("ctest")
            .chain(args.iter().copied())
            .map(BString::from)
            .collect();
        main(&argv, host.clone())
    }

    fn output(status: ProcessResult, stdout: &[u8]) -> RunOutput {
        RunOutput {
            results: vec![status],
            stdout: stdout.to_vec(),
            stderr: Vec::new(),
        }
    }

    #[test]
    fn evaluates_generated_test_includes() -> TestResult {
        let host = tree("include(\"/work/test_include.cmake\")\n")?;
        host.seed_file(
            Path::new("/work/test_include.cmake"),
            br#"
            function(register prefix)
                execute_process(COMMAND list-cases OUTPUT_VARIABLE cases)
                string(STRIP "${cases}" cases)
                foreach(case IN LISTS cases)
                    add_test("${prefix}-${case}" run-case "${case}")
                    set_tests_properties("${prefix}-${case}" PROPERTIES LABELS included)
                endforeach()
            endfunction()
            register(generated)
        "#,
        )?;
        host.set_command_handler(|_, request, _| {
            assert_eq!(request.pipeline[0].program, "list-cases");
            Ok(output(ProcessResult::Exited(0), b"one;two\n"))
        })?;
        assert_eq!(invoke(&host, &["-N", "-L", "^included$"]), 0);
        let stdout = String::from_utf8(host.stream_bytes(Stream::Stdout).to_vec())?;
        assert!(stdout.contains("Test #1: generated-one"));
        assert!(stdout.contains("Test #2: generated-two"));
        Ok(())
    }

    #[test]
    fn discovers_tests_in_show_only_mode() -> TestResult {
        let host = tree(
            r#"
            discover_tests(
                COMMAND test-binary base
                DISCOVERY_ARGS --list
                DISCOVERY_MATCH "case:([0-9]+)"
                DISCOVERY_PROPERTIES ENVIRONMENT "TYPE=listing"
                TEST_NAME [[case-\1]]
                TEST_ARGS [[--id=\1]] [[\0]]
                TEST_PROPERTIES LABELS [[group-\1]]
                BUILD_DEPENDS /work/bin
                TEST_LIST names
            )
            foreach(test IN LISTS names)
                set_tests_properties("${test}" PROPERTIES RESOURCE_LOCK db)
            endforeach()
        "#,
        )?;
        host.set_command_handler(|_, request, _| {
            assert_eq!(request.pipeline[0].program, "test-binary");
            assert_eq!(request.pipeline[0].args, ["base", "--list"]);
            assert_eq!(request.env, [("TYPE".into(), "listing".into())]);
            Ok(output(
                ProcessResult::Exited(0),
                b"case:3\r\nnoise case:4\ncase:8\n",
            ))
        })?;
        let tests = discovery::load(host.clone(), Path::new("/work"), None)?;
        assert_eq!(tests.len(), 2);
        assert_eq!(tests[0].name, "case-3");
        assert_eq!(tests[0].command.args, ["base", "--id=3", "case:3"]);
        assert_eq!(tests[0].list(b"LABELS"), ["group-3"]);
        assert_eq!(
            tests[0].property(b"_CMAKE_TEST_BUILD_DEPENDS"),
            b"/work/bin"
        );
        assert_eq!(tests[1].property(b"RESOURCE_LOCK"), b"db");
        assert_eq!(invoke(&host, &["--show-only=json-v1"]), 0);
        let stdout = host.stream_bytes(Stream::Stdout);
        let json = rcmake_json::parse(&stdout, &Default::default())?;
        assert!(matches!(json, Value::Object(_)));
        Ok(())
    }

    #[test]
    fn repeats_until_the_selected_result() -> TestResult {
        for (mode, first, second, code) in [
            (
                "until-fail:3",
                ProcessResult::Exited(0),
                ProcessResult::Exited(9),
                8,
            ),
            (
                "until-pass:3",
                ProcessResult::Exited(9),
                ProcessResult::Exited(0),
                0,
            ),
            (
                "after-timeout:3",
                ProcessResult::TimedOut,
                ProcessResult::Exited(0),
                0,
            ),
        ] {
            let host = tree("add_test(test runner)")?;
            let count = Arc::new(AtomicUsize::new(0));
            let requests = count.clone();
            host.set_command_handler(move |_, _, _| {
                let status = if requests.fetch_add(1, Ordering::SeqCst) == 0 {
                    &first
                } else {
                    &second
                };
                Ok(output(status.clone(), b"attempt output"))
            })?;
            assert_eq!(invoke(&host, &["--repeat", mode]), code);
            assert_eq!(count.load(Ordering::SeqCst), 2);
            let log =
                state::last_log(host.as_ref(), Path::new("/work"))?.ok_or("Missing test log")?;
            assert_eq!(String::from_utf8(log)?.matches("attempt output").count(), 2);
        }
        Ok(())
    }

    #[test]
    fn repeats_fixture_lifecycles_with_the_selected_mode() -> TestResult {
        for (mode, expected) in [
            (
                "AROUND_EACH_REPEAT",
                &["setup", "run", "clean", "setup", "run", "clean", "after"][..],
            ),
            (
                "AROUND_ALL_REPEATS",
                &["setup", "run", "run", "clean", "after", "after"][..],
            ),
            (
                "EACH_TEST_SEPARATELY",
                &[
                    "setup", "setup", "run", "run", "clean", "clean", "after", "after",
                ][..],
            ),
        ] {
            let host = tree(&format!(
                r#"
                add_test(setup runner setup)
                add_test(run runner run)
                add_test(clean runner clean)
                add_test(after runner after)
                set_tests_properties(setup PROPERTIES FIXTURES_SETUP db _CMAKE_DEFAULT_FIXTURE_REPEAT_MODE {mode})
                set_tests_properties(run PROPERTIES FIXTURES_REQUIRED db)
                set_tests_properties(clean PROPERTIES FIXTURES_CLEANUP db)
                set_tests_properties(after PROPERTIES DEPENDS run)
            "#
            ))?;
            let calls = Arc::new(Mutex::new(Vec::new()));
            let seen = calls.clone();
            host.set_command_handler(move |_, request, _| {
                seen.lock()
                    .map_err(|_| HostError::unsupported("Cannot record command."))?
                    .push(request.pipeline[0].args[0].clone());
                let code = if mode == "AROUND_EACH_REPEAT" && request.pipeline[0].args == ["after"]
                {
                    1
                } else {
                    0
                };
                Ok(output(ProcessResult::Exited(code), b""))
            })?;
            assert_eq!(
                invoke(&host, &["--repeat", "until-fail:2"]),
                i32::from(mode == "AROUND_EACH_REPEAT") * 8
            );
            assert_eq!(
                calls
                    .lock()
                    .map_err(|_| "Cannot read commands.")?
                    .as_slice(),
                expected
            );
        }
        Ok(())
    }

    #[test]
    fn reruns_all_when_the_failure_log_is_empty_and_keeps_previous_failures() -> TestResult {
        let host = tree("add_test(first runner first)\nadd_test(second runner second)")?;
        let failing = Arc::new(AtomicBool::new(true));
        let fail = failing.clone();
        let calls = Arc::new(Mutex::new(Vec::new()));
        let seen = calls.clone();
        host.set_command_handler(move |_, request, _| {
            let name = &request.pipeline[0].args[0];
            seen.lock()
                .map_err(|_| HostError::unsupported("Cannot record command."))?
                .push(name.clone());
            let failure = name == "second" && fail.load(Ordering::SeqCst);
            Ok(output(ProcessResult::Exited(i32::from(failure)), b""))
        })?;
        assert_eq!(invoke(&host, &["--rerun-failed", "-R", "no-match"]), 8);
        failing.store(false, Ordering::SeqCst);
        assert_eq!(invoke(&host, &["--rerun-failed"]), 0);
        assert_eq!(invoke(&host, &["--rerun-failed"]), 0);
        assert_eq!(
            calls
                .lock()
                .map_err(|_| "Cannot read commands.")?
                .as_slice(),
            ["first", "second", "second", "second"]
        );
        assert!(state::read_stamp(host.as_ref(), Path::new("/work"), b"second")?.is_some());
        Ok(())
    }

    #[test]
    fn limits_workers_by_processor_slots_and_exclusive_properties() -> TestResult {
        let host = tree(
            r#"
            add_test(a runner 2 db)
            add_test(b runner 1 db)
            add_test(c runner 1 serial)
            add_test(d runner 2 free)
            set_tests_properties(a PROPERTIES PROCESSORS 2 RESOURCE_LOCK db)
            set_tests_properties(b PROPERTIES RESOURCE_LOCK db)
            set_tests_properties(c PROPERTIES RUN_SERIAL ON)
            set_tests_properties(d PROPERTIES PROCESSORS 2)
        "#,
        )?;
        let active = Arc::new(Mutex::new((0, false, false)));
        host.set_command_handler(move |_, request, _| {
            let command = &request.pipeline[0];
            let slots: usize = command.args[0].to_string().parse().unwrap_or_default();
            let lock = command.args[1] == "db";
            let serial = command.args[1] == "serial";
            {
                let mut state = active
                    .lock()
                    .map_err(|_| HostError::unsupported("Cannot record command."))?;
                assert!(state.0 + slots <= 3);
                assert!(!(state.1 && lock));
                assert!(!state.2);
                assert!(!serial || state.0 == 0);
                state.0 += slots;
                state.1 |= lock;
                state.2 |= serial;
            }
            std::thread::sleep(Duration::from_millis(20));
            let mut state = active
                .lock()
                .map_err(|_| HostError::unsupported("Cannot record command."))?;
            state.0 -= slots;
            state.1 &= !lock;
            state.2 &= !serial;
            Ok(output(ProcessResult::Exited(0), b""))
        })?;
        assert_eq!(invoke(&host, &["--parallel", "3"]), 0);
        Ok(())
    }

    #[test]
    fn test_load_waits_until_the_stop_time() -> TestResult {
        let host = tree("add_test(test runner)")?;
        host.seed_file(Path::new("/proc/loadavg"), b"4.1 4.0 4.0 1/2 10\n")?;
        let started = Arc::new(AtomicBool::new(false));
        let seen = started.clone();
        host.set_command_handler(move |_, _, _| {
            seen.store(true, Ordering::SeqCst);
            Ok(output(ProcessResult::Exited(0), b""))
        })?;
        assert_eq!(
            invoke(&host, &["--test-load", "5", "--stop-time", "22:13:22"]),
            0
        );
        assert_eq!(host.monotonic().as_secs(), 2);
        assert!(!started.load(Ordering::SeqCst));
        Ok(())
    }

    #[test]
    fn stop_time_caps_the_process_timeout() -> TestResult {
        let host = tree("add_test(test runner)\nadd_test(late runner)")?;
        let calls = Arc::new(AtomicUsize::new(0));
        let seen = calls.clone();
        host.set_command_handler(move |host, request, _| {
            let timeout = request
                .timeout
                .ok_or_else(|| HostError::unsupported("Missing timeout."))?;
            assert!((Duration::from_millis(1999)..=Duration::from_secs(2)).contains(&timeout));
            seen.fetch_add(1, Ordering::SeqCst);
            host.sleep(timeout);
            Ok(output(ProcessResult::TimedOut, b""))
        })?;
        assert_eq!(
            invoke(
                &host,
                &["--stop-time", "22:13:22", "--repeat", "after-timeout:3"]
            ),
            8
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        Ok(())
    }

    #[test]
    fn calls_post_ctest_after_the_test() -> TestResult {
        let host = tree("add_test(test runner)")?;
        host.seed_file(
            Path::new("/work/.cmake/instrumentation/v1/query/test.json"),
            br#"{"version":1,"hooks":["postCTest"],"callbacks":["callback"]}"#,
        )?;
        let calls = Arc::new(Mutex::new(Vec::new()));
        let seen = calls.clone();
        host.set_command_handler(move |host, request, _| {
            let command = &request.pipeline[0];
            seen.lock()
                .map_err(|_| HostError::unsupported("Cannot record command."))?
                .push(command.program.clone());
            if command.program == "callback" {
                let file = command
                    .args
                    .first()
                    .ok_or_else(|| HostError::unsupported("Missing index file."))?;
                let index = host.read(
                    &path::to_native(file),
                    rcmake_host::ReadRange::All { limit: 4096 },
                )?;
                assert!(String::from_utf8_lossy(&index).contains("postCTest"));
            }
            Ok(output(ProcessResult::Exited(0), b""))
        })?;
        assert_eq!(invoke(&host, &[]), 0);
        assert_eq!(
            calls
                .lock()
                .map_err(|_| "Cannot read commands.")?
                .as_slice(),
            ["runner", "callback"]
        );
        Ok(())
    }

    #[test]
    fn passes_the_instrumented_command_exit_code_through() -> TestResult {
        let host = tree("add_test(other unexpected)")?;
        host.set_command_handler(|_, request, _| {
            assert_eq!(request.pipeline[0].program, "instrumented");
            assert_eq!(request.pipeline[0].args, ["--flag"]);
            Ok(output(ProcessResult::Exited(23), b""))
        })?;
        assert_eq!(
            invoke(
                &host,
                &[
                    "--instrument",
                    "--command-type",
                    "test",
                    "--",
                    "instrumented",
                    "--flag"
                ]
            ),
            23
        );
        Ok(())
    }
}
