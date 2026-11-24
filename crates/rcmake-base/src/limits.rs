//! Safety and compatibility execution limits for Rcmake.

use core::time::Duration;

/// Execution limits enforced by the Rcmake parser, regex engine, and evaluator.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Limits {
    /// Maximum command/file/function/macro recursion depth (`CMAKE_MAXIMUM_RECURSION_DEPTH`).
    pub max_recursion: u32,
    /// Maximum byte size of a single variable or expanded value.
    pub max_value_bytes: usize,
    /// Maximum bytes read by a single file read operation.
    pub max_file_read: u64,
    /// Step budget for the backtracking CMake regex VM.
    pub regex_steps: u64,
    /// Optional wall-clock execution budget (used by fuzzers and conformance harnesses).
    pub wall_budget: Option<Duration>,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_recursion: 1000,
            max_value_bytes: 256 * 1024 * 1024,
            max_file_read: 512 * 1024 * 1024,
            regex_steps: 50_000_000,
            wall_budget: None,
        }
    }
}

impl Limits {
    /// Stricter limits for the adversarial fuzz harness (`rcmake-fuzz`, ADR-016).
    ///
    /// Production code must use [`Limits::default`]. The production limits never
    /// reject input that CMake 4.5.1 accepts. The fuzz limits are smaller, so that
    /// each fuzz input finishes fast and with little memory:
    ///
    /// - value and file-read size: 16 MiB
    /// - regex step budget: 1,000,000
    /// - wall-clock budget per input: 2 s
    #[must_use]
    pub fn fuzz() -> Self {
        Self {
            max_recursion: 1000,
            max_value_bytes: 16 * 1024 * 1024,
            max_file_read: 16 * 1024 * 1024,
            regex_steps: 1_000_000,
            wall_budget: Some(Duration::from_secs(2)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Limits;

    #[test]
    fn fuzz_limits_are_never_larger_than_production_limits() {
        let production = Limits::default();
        let fuzz = Limits::fuzz();
        assert!(fuzz.max_recursion <= production.max_recursion);
        assert!(fuzz.max_value_bytes < production.max_value_bytes);
        assert!(fuzz.max_file_read < production.max_file_read);
        assert!(fuzz.regex_steps < production.regex_steps);
        assert!(fuzz.wall_budget.is_some());
        assert_eq!(production.wall_budget, None);
    }
}
