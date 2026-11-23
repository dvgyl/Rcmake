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
