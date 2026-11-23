//! Minimal `ctest` runner (ADR-009 Wave 3): executes `CTestTestfile.cmake` with `-C`,
//! `-R`, `-E`, `-j`, and `--output-on-failure`.
//!
//! # Status
//!
//! Wave 0 freezes the contract. [`run`] returns [`CtestError::Unimplemented`].

#![forbid(unsafe_code)]

use std::path::PathBuf;
use std::sync::Arc;

use rcmake_base::BString;
use rcmake_host::Host;

/// `ctest` command-line options that the minimal runner supports.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CtestOptions {
    /// Build directory that holds the top `CTestTestfile.cmake` (`--test-dir`).
    pub test_dir: PathBuf,
    /// `-C <config>`.
    pub config: Option<BString>,
    /// `-R <regex>`: run only matching tests.
    pub include: Option<BString>,
    /// `-E <regex>`: skip matching tests.
    pub exclude: Option<BString>,
    /// `-j <n>`: parallel test processes (at least 1).
    pub jobs: usize,
    /// `--output-on-failure`.
    pub output_on_failure: bool,
}

impl Default for CtestOptions {
    fn default() -> Self {
        Self {
            test_dir: PathBuf::new(),
            config: None,
            include: None,
            exclude: None,
            jobs: 1,
            output_on_failure: false,
        }
    }
}

/// Result of a test run.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CtestOutcome {
    /// Process exit code (`0` when every test passed).
    pub exit_code: i32,
    /// Names of the tests that passed.
    pub passed: Vec<BString>,
    /// Names of the tests that failed.
    pub failed: Vec<BString>,
}

/// Error that stops `ctest` before tests run.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum CtestError {
    /// `-R` or `-E` is not a valid regular expression.
    #[error("invalid regular expression")]
    BadRegex(BString),
    /// The runner is not implemented yet.
    #[error("{0} is not implemented yet")]
    Unimplemented(&'static str),
}

/// Run the tests below `options.test_dir`. Each `CTestTestfile.cmake` runs in a script
/// mode `rcmake_eval::Interp` that records `add_test()` and `subdirs()` calls.
pub fn run(_host: Arc<dyn Host>, _options: &CtestOptions) -> Result<CtestOutcome, CtestError> {
    Err(CtestError::Unimplemented("the ctest runner"))
}
