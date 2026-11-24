//! Safe-Rust adversarial fuzz harness for the Rcmake engine crates (ADR-016).
//!
//! The harness uses the stable toolchain and no `unsafe` code. It does not use
//! `libfuzzer-sys`. Each input runs on its own thread with an 8 MiB stack, inside
//! [`std::panic::catch_unwind`], with a wall-clock budget. A deterministic seeded
//! generator ([`Rng`]) makes every failure replayable from its seed.
//!
//! The harness uses [`rcmake_base::Limits::fuzz`] for input and property checks.
//! This crate is never linked into the `rcmake` binary.

#![forbid(unsafe_code)]

pub mod harness;
pub mod options;
pub mod rng;
pub mod surface;
mod targets;

pub use harness::{Outcome, check_input};
pub use options::{FuzzOptions, parse_args};
pub use rng::Rng;
pub use surface::Surface;

/// Summary of one fuzz run.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FuzzReport {
    /// Inputs that ran.
    pub iterations: u64,
    /// Inputs that the surface rejected with an error (not a failure).
    pub rejected: u64,
    /// Failing inputs, with their iteration seed.
    pub failures: Vec<Failure>,
}

/// One failing input.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Failure {
    /// Seed that makes the input again.
    pub seed: u64,
    /// The failure.
    pub outcome: Outcome,
}

/// Error of a fuzz run (not a failure of an input).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FuzzError {
    /// The surface has no target yet.
    Unimplemented(Surface),
    /// The harness could not run an input.
    Input {
        /// Surface under test.
        surface: Surface,
        /// Seed of the input.
        seed: u64,
        /// Harness error.
        message: String,
    },
}

impl core::fmt::Display for FuzzError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Unimplemented(surface) => {
                write!(
                    f,
                    "fuzz surface '{}' is not implemented yet",
                    surface.name()
                )
            }
            Self::Input {
                surface,
                seed,
                message,
            } => {
                write!(f, "{} seed {seed}: {message}", surface.name())
            }
        }
    }
}

impl core::error::Error for FuzzError {}

/// Run up to `options.iterations` inputs on `options.surface`.
///
/// Stop after a timeout because the input thread can still run in the background.
///
/// # Errors
///
/// Returns an error if the harness cannot run an input.
pub fn run(options: &FuzzOptions) -> Result<FuzzReport, FuzzError> {
    let mut report = FuzzReport::default();
    for offset in 0..options.iterations {
        let seed = options.seed.wrapping_add(offset);
        let surface = options.surface;
        let outcome = harness::check_property(options.budget, move || targets::run(surface, seed));
        report.iterations += 1;
        match outcome {
            Outcome::Accepted => {}
            Outcome::Rejected => report.rejected += 1,
            Outcome::HarnessError(message) => {
                return Err(FuzzError::Input {
                    surface,
                    seed,
                    message,
                });
            }
            failure => {
                let timed_out = failure == Outcome::TimedOut;
                report.failures.push(Failure {
                    seed,
                    outcome: failure,
                });
                if timed_out {
                    break;
                }
            }
        }
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fuzz_limits_are_the_harness_limits() {
        let limits = rcmake_base::Limits::fuzz();
        assert_eq!(limits.wall_budget, Some(options::DEFAULT_BUDGET));
    }
}
