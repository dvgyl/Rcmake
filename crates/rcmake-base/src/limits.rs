//! Safety and compatibility execution limits for Rcmake.

use core::time::Duration;

/// Execution limits enforced by the Rcmake parser, regex engine, and evaluator.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Limits {
    /// Maximum command/file/function/macro recursion depth (`CMAKE_MAXIMUM_RECURSION_DEPTH`).
    pub max_recursion: u32,
    /// Highest recursion depth that `CMAKE_MAXIMUM_RECURSION_DEPTH` can select. The
    /// evaluator also applies its own hard ceiling of 20,000. A host with a small native
    /// stack, such as WebAssembly, sets a lower value.
    pub max_recursion_ceiling: u32,
    /// Maximum byte size of a single variable or expanded value.
    pub max_value_bytes: usize,
    /// Maximum bytes read by a single file read operation.
    pub max_file_read: u64,
    /// Step budget for the backtracking CMake regex VM.
    pub regex_steps: u64,
    /// Optional wall-clock execution budget (used by fuzzers and conformance harnesses).
    pub wall_budget: Option<Duration>,
    /// Maximum number of arguments of one command call after list expansion. The same cap
    /// limits the elements of the lists that one command splits.
    pub max_command_arguments: usize,
    /// Maximum total bytes of the expanded arguments of one command call. The same cap
    /// limits the bytes of the lists that one command splits.
    pub max_command_argument_bytes: usize,
    /// Maximum estimated bytes of all cache entries that `set(CACHE)`, `option()`, and
    /// the other cache writes of the evaluator can create.
    pub max_cache_bytes: usize,
    /// Maximum total bytes of all live normal variable bindings in all scopes.
    ///
    /// Each binding costs its name length, its value length, and 64 bytes of overhead.
    pub max_live_variable_bytes: usize,
}

/// Default for [`Limits::max_live_variable_bytes`] on 64-bit targets: 4 GiB.
#[cfg(target_pointer_width = "64")]
const LIVE_VARIABLE_BYTES: usize = 1 << 32;
/// Default for [`Limits::max_live_variable_bytes`] on smaller targets, such as wasm32: 1 GiB.
#[cfg(not(target_pointer_width = "64"))]
const LIVE_VARIABLE_BYTES: usize = 1 << 30;

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_recursion: 1000,
            max_recursion_ceiling: u32::MAX,
            max_value_bytes: 256 * 1024 * 1024,
            max_file_read: 512 * 1024 * 1024,
            regex_steps: 50_000_000,
            wall_budget: None,
            max_command_arguments: 16 * 1024 * 1024,
            max_command_argument_bytes: 512 * 1024 * 1024,
            max_cache_bytes: 1 << 30,
            max_live_variable_bytes: LIVE_VARIABLE_BYTES,
        }
    }
}

impl Limits {
    /// Stricter limits for the adversarial fuzz harness (`rcmake-fuzz`, ADR-016).
    ///
    /// Production code must use [`Limits::default`]. The production limits reject
    /// only input that exhausts memory or time. Each memory cap is more than 10 times
    /// larger than the largest known real project needs:
    ///
    /// - command arguments after list expansion: 16,777,216 elements (LLVM and Qt
    ///   source globs have about 200,000 files)
    /// - command argument bytes: 512 MiB (two values of the 256 MiB value limit; such a
    ///   glob is about 20 MB)
    /// - live variable bytes: 4 GiB, 1 GiB on 32-bit targets (a large configure
    ///   holds about 20 MB)
    /// - cache bytes: 1 GiB (a large `CMakeCache.txt` is a few MB)
    ///
    /// The fuzz limits are smaller, so that each fuzz input finishes fast and with
    /// little memory:
    ///
    /// - value and file-read size: 16 MiB
    /// - regex step budget: 1,000,000
    /// - command arguments: 1,048,576
    /// - command argument bytes: 32 MiB
    /// - live variable bytes: 256 MiB
    /// - cache bytes: 64 MiB
    /// - wall-clock budget per input: 2 s
    #[must_use]
    pub fn fuzz() -> Self {
        Self {
            max_recursion: 1000,
            max_recursion_ceiling: u32::MAX,
            max_value_bytes: 16 * 1024 * 1024,
            max_file_read: 16 * 1024 * 1024,
            regex_steps: 1_000_000,
            wall_budget: Some(Duration::from_secs(2)),
            max_command_arguments: 1 << 20,
            max_command_argument_bytes: 32 * 1024 * 1024,
            max_cache_bytes: 64 * 1024 * 1024,
            max_live_variable_bytes: 256 * 1024 * 1024,
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
        assert!(fuzz.max_command_arguments < production.max_command_arguments);
        assert!(fuzz.max_command_argument_bytes < production.max_command_argument_bytes);
        assert!(production.max_command_argument_bytes >= 2 * production.max_value_bytes);
        assert!(fuzz.max_live_variable_bytes < production.max_live_variable_bytes);
        assert!(fuzz.max_cache_bytes < production.max_cache_bytes);
        assert!(fuzz.wall_budget.is_some());
        assert_eq!(production.wall_budget, None);
    }
}
