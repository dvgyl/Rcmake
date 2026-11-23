//! `Ninja` and `Ninja Multi-Config` generators (ADR-009 Wave 3 and Wave 4).
//!
//! # Status
//!
//! Wave 0 freezes the entry points. Both return [`GenError::Unimplemented`].

#![forbid(unsafe_code)]

use std::path::Path;

use rcmake_gen_common::GenError;
use rcmake_host::Host;
use rcmake_resolve::GenerationResult;

/// Write `build.ninja` and its rule files for the single configuration of `result`.
pub fn generate_single_config(
    _result: &GenerationResult,
    _host: &dyn Host,
    _build_dir: &Path,
) -> Result<(), GenError> {
    Err(GenError::Unimplemented("the Ninja generator"))
}

/// Write `build.ninja`, `build-<Config>.ninja`, and the common rule files for every
/// configuration of `result` (`CMAKE_CONFIGURATION_TYPES`).
pub fn generate_multi_config(
    _result: &GenerationResult,
    _host: &dyn Host,
    _build_dir: &Path,
) -> Result<(), GenError> {
    Err(GenError::Unimplemented("the Ninja Multi-Config generator"))
}
