//! Native CTest test discovery and execution.

#![forbid(unsafe_code)]

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use rcmake_base::BString;
use rcmake_host::{FileKind, Follow, Host, HostError, Stream, WriteMode};
use rcmake_presets::{EnvSnapshot, PresetKind};

use crate::cli::{Invocation, Mode};
use crate::runner::{run_tests, run_tests_with_dashboard};

mod build_and_test;
mod cli;
mod dart;
mod dashboard;
mod discover;
mod discovery;
mod execution;
mod instrumentation;
mod jobserver;
mod junit;
mod launch;
mod measurements;
mod memcheck;
mod reporting;
mod runner;
mod schedule;
mod script;
mod selection;
mod state;
mod timeout;

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
    /// Use the jobserver limit when the parallel option has no value.
    pub jobs_automatic: bool,
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
            jobs_automatic: false,
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
    /// The test list file could not be read.
    #[error("Problem reading test list file: {} while generating list of tests to run.", .0.display())]
    TestList(PathBuf),
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
    /// The test dependency graph contains a cycle.
    #[error(
        "Error: a cycle exists in the test dependency graph for the test \"{0}\".\nPlease fix the cycle and run ctest again."
    )]
    DependencyCycle(BString),
    /// Test dependencies or fixture repeat modes prevent scheduling.
    #[error("{0}")]
    Schedule(String),
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
    if let Some((flag, launcher)) = args.split_first()
        && flag == "--launch"
    {
        return launch::run(host.as_ref(), launcher);
    }
    match cli::parse(args, host.as_ref()).and_then(|mut invocation| {
        invocation.command = argv.to_vec();
        dispatch(&host, &mut invocation, args)
    }) {
        Ok(code) => code,
        Err(error) => {
            let prefix = if error.starts_with("Unknown value for CTEST_NO_TESTS_ACTION:") {
                ""
            } else {
                "CMake Error: "
            };
            host.write_out(Stream::Stderr, format!("{prefix}{error}\n").as_bytes());
            1
        }
    }
}

const USAGE: &[u8] = include_bytes!("help.txt");

fn dispatch(
    host: &Arc<dyn Host>,
    invocation: &mut Invocation,
    args: &[BString],
) -> Result<i32, String> {
    match &invocation.mode {
        Mode::BuildAndTest(command) => command.run(host.as_ref(), invocation.command.first()),
        Mode::CollectInstrumentation(dir) => {
            if host
                .metadata(dir, Follow::Yes)
                .is_ok_and(|meta| meta.kind == FileKind::Dir)
            {
                rcmake_instrument::hooks::run_hook(
                    host.as_ref(),
                    dir,
                    rcmake_instrument::Hook::Manual,
                )
                .map_err(|error| error.to_string())?;
            }
            Ok(0)
        }
        Mode::Instrument { .. } => {
            rcmake_instrument::launcher::run_ctest_launcher(host.as_ref(), args)
                .map_err(|error| error.to_string())
        }
        Mode::ListPresets { defined } => list_presets(host.as_ref(), invocation, *defined),
        Mode::Version { json, file } => {
            let version = if *json {
                version_json()
            } else {
                BString::from(format!(
                    "ctest version {}\nrcmake {}\n",
                    rcmake_base::CMAKE_VERSION_STR,
                    env!("CARGO_PKG_VERSION")
                ))
            };
            if let Some(file) = file {
                host.write(
                    &host.initial_cwd().join(file),
                    version.as_slice(),
                    WriteMode::Truncate,
                )
                .map_err(|error| error.to_string())?;
            } else {
                host.write_out(Stream::Stdout, version.as_slice());
            }
            Ok(0)
        }
        Mode::Help(_) => {
            host.write_out(Stream::Stdout, USAGE);
            Ok(0)
        }
        Mode::Test => {
            let _environment = PresetEnvironment::apply(host.as_ref(), &invocation.environment);
            if invocation.scripts.is_empty() {
                if let Some(code) = dashboard::run(host, invocation, |invocation, dashboard| {
                    run_cli_tests(host, invocation, false, Some(dashboard))
                }) {
                    return Ok(code);
                }
                return run_cli_tests(host, invocation, args.is_empty(), None);
            }
            let mut code = 0;
            for script in &invocation.scripts {
                code |= script::run_script(host, invocation, script)
                    .map_err(|error| error.to_string())?;
            }
            Ok(code)
        }
    }
}

fn run_cli_tests(
    host: &Arc<dyn Host>,
    invocation: &Invocation,
    no_args: bool,
    dashboard: Option<dart::Dashboard>,
) -> Result<i32, String> {
    let dir = &invocation.options.test_dir;
    let metadata = host.metadata(dir, Follow::Yes);
    if !metadata
        .as_ref()
        .is_ok_and(|meta| meta.kind == FileKind::Dir)
        && (invocation.build_dir.is_some()
            || invocation.preset.is_none()
            || !metadata
                .as_ref()
                .is_err_and(|error| error.kind == std::io::ErrorKind::NotFound))
    {
        let message = metadata
            .err()
            .map_or_else(|| "Not a directory".into(), |error| error.os_message);
        host.write_out(
            Stream::Stderr,
            format!(
                "Failed to change working directory to \"{}\": {message}\n",
                dir.display()
            )
            .as_bytes(),
        );
        return Ok(1);
    }
    if no_args
        && host
            .metadata(&dir.join("CTestTestfile.cmake"), Follow::Yes)
            .is_err()
    {
        host.write_out(Stream::Stdout, USAGE);
        host.write_out(Stream::Stderr, b"*********************************\nNo test configuration file found!\n*********************************\n");
        return Ok(1);
    }
    if invocation.actions.iter().any(|action| action == "test")
        && [
            "CTestConfiguration.ini",
            "DartConfiguration.tcl",
            "CTestTestfile.cmake",
        ]
        .iter()
        .all(|name| host.metadata(&dir.join(name), Follow::Yes).is_err())
    {
        host.write_out(
            Stream::Stderr,
            format!(
                "Cannot find file: {}\n",
                dir.join("DartConfiguration.tcl").display()
            )
            .as_bytes(),
        );
    }
    match run_tests_with_dashboard(Arc::clone(host), invocation, true, dashboard) {
        Ok(run) => Ok(run.outcome.exit_code),
        Err(error @ CtestError::DependencyCycle(_)) => {
            host.write_out(Stream::Stderr, format!("{error}\n").as_bytes());
            Ok(8)
        }
        Err(CtestError::Load(_)) => {
            let log = dir.join("Testing/Temporary/LastTest.log");
            let _ = junit::write_file(host.as_ref(), &log, b"");
            reporting::failure_summary(host.as_ref(), dir);
            Ok(8)
        }
        Err(error @ CtestError::Schedule(_)) => {
            host.write_out(Stream::Stderr, format!("{error}\n").as_bytes());
            reporting::failure_summary(host.as_ref(), dir);
            Ok(8)
        }
        Err(CtestError::TestList(file)) => {
            if !invocation.options.quiet {
                host.write_out(
                    Stream::Stdout,
                    format!("Test project {}\n", dir.display()).as_bytes(),
                );
            }
            host.write_out(
                Stream::Stderr,
                format!("{}\n", CtestError::TestList(file)).as_bytes(),
            );
            Ok(1)
        }
        Err(error) => Err(error.to_string()),
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

fn version_json() -> BString {
    use rcmake_json::Value;

    let mut prog_ver = BTreeMap::new();
    prog_ver.insert(
        BString::from("major"),
        Value::UInt(u64::from(rcmake_base::CMAKE_VERSION_MAJOR)),
    );
    prog_ver.insert(
        BString::from("minor"),
        Value::UInt(u64::from(rcmake_base::CMAKE_VERSION_MINOR)),
    );
    prog_ver.insert(
        BString::from("patch"),
        Value::UInt(u64::from(rcmake_base::CMAKE_VERSION_PATCH)),
    );
    prog_ver.insert(
        BString::from("string"),
        Value::string(rcmake_base::CMAKE_VERSION_STR),
    );

    let mut program = BTreeMap::new();
    program.insert(BString::from("name"), Value::string("ctest"));
    program.insert(BString::from("version"), Value::Object(prog_ver));

    let mut schema_ver = BTreeMap::new();
    schema_ver.insert(BString::from("major"), Value::UInt(1));
    schema_ver.insert(BString::from("minor"), Value::UInt(0));

    let mut root = BTreeMap::new();
    root.insert(BString::from("dependencies"), Value::Array(Vec::new()));
    root.insert(BString::from("program"), Value::Object(program));
    root.insert(BString::from("version"), Value::Object(schema_ver));

    let mut text = rcmake_json::write_styled(&Value::Object(root), "  ");
    text.push(b'\n');
    text
}

fn unavailable_test_preset(
    presets: &rcmake_presets::Presets,
    env: &EnvSnapshot,
    kind: PresetKind,
    name: &[u8],
) -> Option<String> {
    let resolved = match presets.resolve_definition(kind, name, env) {
        Ok(preset) => preset,
        Err(err) if err.message.starts_with("Cannot use hidden ") => return Some("hidden".into()),
        Err(err) if err.message.starts_with("Cannot use disabled ") => {
            let condition = presets.preset_condition(kind, name);
            return Some(condition.map_or_else(
                || "condition evaluated to false".into(),
                |condition| {
                    format!(
                        "condition evaluated to false: {}",
                        rcmake_json::write_compact(condition)
                    )
                },
            ));
        }
        Err(_) => return Some("unsupported macro expansion".into()),
    };
    if let Some(configure) = resolved.configure_preset
        && let Some(reason) =
            unavailable_test_preset(presets, env, PresetKind::Configure, &configure)
    {
        return Some(format!("configure preset \"{configure}\": {reason}"));
    }
    None
}

fn list_presets(host: &dyn Host, invocation: &Invocation, defined: bool) -> Result<i32, String> {
    let cwd = host.initial_cwd();
    let presets = match &invocation.presets_file {
        Some(file) => rcmake_presets::load_file(&cwd, file, host),
        None => rcmake_presets::load(&cwd, host),
    }
    .map_err(cli::preset_diagnostic)?;
    let env = EnvSnapshot::from_host(host);
    let entries: Vec<_> = presets
        .list_defined(PresetKind::Test)
        .filter_map(|preset| {
            let reason = unavailable_test_preset(&presets, &env, PresetKind::Test, &preset.name);
            (defined || reason.is_none()).then_some((preset, reason))
        })
        .collect();
    let Some(width) = entries.iter().map(|(preset, _)| preset.name.len()).max() else {
        return Ok(0);
    };
    let heading = if defined { "Defined" } else { "Available" };
    let mut output = format!("{heading} test presets:\n\n");
    for (index, (preset, reason)) in entries.iter().enumerate() {
        output.push_str(&format!("  \"{}\"", preset.name));
        if let Some(display_name) = preset.display_name.as_ref().filter(|name| !name.is_empty()) {
            let padding = width.saturating_sub(preset.name.len()) + 1;
            output.push_str(&format!("{:padding$}- {display_name}", ""));
        }
        output.push('\n');
        if let Some(reason) = reason {
            output.push_str(&format!("    Unavailable: {reason}\n"));
            if index + 1 < entries.len() {
                output.push('\n');
            }
        }
    }
    host.write_out(Stream::Stdout, output.as_bytes());
    Ok(0)
}
