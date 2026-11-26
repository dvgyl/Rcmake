//! Shared aliases for the command family tests.

/// Result type of a test that reports failures through `?`.
pub(crate) type TestResult<T = ()> = std::result::Result<T, Box<dyn std::error::Error>>;
