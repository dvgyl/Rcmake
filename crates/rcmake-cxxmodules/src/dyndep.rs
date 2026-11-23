//! Ninja `ninja_dyndep_version = 1.0` writer and the `cmake -E cmake_ninja_dyndep`
//! request.

use rcmake_base::BString;

use crate::{ModmapFormat, ModulesError, P1689File};

/// Arguments of `cmake -E cmake_ninja_dyndep --tdi=<file> --lang=<lang> --dd=<file>
/// [--modmapfmt=<fmt>] [<file.ddi>...]`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DyndepRequest {
    /// `--tdi=`: path of `CXXDependInfo.json`.
    pub tdi: BString,
    /// `--lang=`: language (`CXX`).
    pub lang: BString,
    /// `--dd=`: path of the dyndep file to write.
    pub dd: BString,
    /// `--modmapfmt=`.
    pub modmap_format: Option<ModmapFormat>,
    /// `.ddi` file paths.
    pub ddis: Vec<BString>,
}

/// Write the dyndep file content for the scanned sources of one target. `module_dir`
/// is the target's module file directory.
///
/// # Errors
///
/// Returns [`ModulesError::Unsupported`] until the writer is implemented.
pub fn write_dyndep(ddis: &[P1689File], module_dir: &[u8]) -> Result<BString, ModulesError> {
    let _ = (ddis, module_dir);
    Err(ModulesError::Unsupported("Ninja dyndep writing"))
}
