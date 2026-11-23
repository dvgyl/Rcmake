//! `Unix Makefiles` generator (ADR-009 Wave 4).
//!
//! # Status
//!
//! Wave 0 freezes the entry point. It returns [`GenError::Unimplemented`].

#![forbid(unsafe_code)]

use std::path::Path;

use rcmake_gen_common::GenError;
use rcmake_host::Host;
use rcmake_resolve::GenerationResult;

/// Write the top-level `Makefile`, `CMakeFiles/Makefile2`, and per-target build files.
pub fn generate_unix_makefiles(
    _result: &GenerationResult,
    _host: &dyn Host,
    _build_dir: &Path,
) -> Result<(), GenError> {
    Err(GenError::Unimplemented("the Unix Makefiles generator"))
}
