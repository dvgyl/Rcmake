//! Native CTest test discovery and execution.

#![forbid(unsafe_code)]

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use rcmake_base::BString;
use rcmake_host::{Host, HostError, Stream};

mod cli;
mod dart;
mod discover;
mod discovery;
mod execution;
mod junit;
mod reporting;
mod schedule;
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
    let mut dir = host.initial_cwd();
    if !options.test_dir.as_os_str().is_empty() {
        dir.push(&options.test_dir);
    }
    let dir = rcmake_host::path::to_native(
        &rcmake_base::path_lex::Lexical::new(&rcmake_host::path::from_native(&dir)).normalize(),
    );
    let tests = discovery::load(Arc::clone(&host), &dir, options.config.as_ref())?;
    let plan = selection::plan(tests, options)?;
    let report = reporting::Report::new(host.as_ref(), options, &plan);
    report.project(&dir);
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
    let results = schedule::run(host.as_ref(), options, &plan, &report);
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
