//! Safe-Rust adversarial fuzz harness for the Rcmake engine crates (ADR-016).
//!
//! The harness uses the stable toolchain and no `unsafe` code. It does not use
//! `libfuzzer-sys`. Each input runs on its own thread with an 8 MiB stack, inside
//! [`std::panic::catch_unwind`], with a wall-clock budget. A deterministic seeded
//! generator ([`Rng`]) makes every failure replayable from its seed.
//!
//! The harness uses [`rcmake_base::Limits::fuzz`], never the production limits.
//! This crate is never linked into the `rcmake` binary.
//!
//! Milestone 4 Wave 0 freezes the harness contract. The surface targets come in Wave 1
//! (`syntax`, `expr`, `regex`, `json`) and Wave 2 (`genex`, `presets`, `cps`,
//! `archive`, `path`).

#![forbid(unsafe_code)]

pub mod harness;
pub mod options;
pub mod rng;
pub mod surface;

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
    /// Failing inputs (panic or timeout), with their iteration seed.
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
        }
    }
}

impl core::error::Error for FuzzError {}

/// Run `options.iterations` inputs on `options.surface`.
///
/// # Errors
///
/// Returns [`FuzzError::Unimplemented`] for a surface without a target. In Milestone 4
/// Wave 0, no surface has a target.
pub fn run(options: &FuzzOptions) -> Result<FuzzReport, FuzzError> {
    Err(FuzzError::Unimplemented(options.surface))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wave_zero_surfaces_are_unimplemented() {
        for surface in Surface::ALL {
            let options = FuzzOptions {
                surface,
                seed: 1,
                iterations: 1,
                budget: options::DEFAULT_BUDGET,
            };
            assert_eq!(run(&options), Err(FuzzError::Unimplemented(surface)));
        }
    }

    #[test]
    fn fuzz_limits_are_the_harness_limits() {
        let limits = rcmake_base::Limits::fuzz();
        assert_eq!(limits.wall_budget, Some(options::DEFAULT_BUDGET));
    }
}
