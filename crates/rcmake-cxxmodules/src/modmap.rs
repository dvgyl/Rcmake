//! Compiler module map writers (LLVM Clang, GCC, MSVC).

use rcmake_base::BString;

use crate::{CxxModulesJson, ModmapFormat, ModulesError, P1689Rule};

/// Write the module map of one scanned source in `format`. `modules` holds the
/// resolved module files of the target.
///
/// # Errors
///
/// Returns [`ModulesError::Unsupported`] until the writers are implemented.
pub fn write_modmap(
    format: ModmapFormat,
    rule: &P1689Rule,
    modules: &CxxModulesJson,
) -> Result<BString, ModulesError> {
    let _ = (format, rule, modules);
    Err(ModulesError::Unsupported("C++ module map writing"))
}
