//! Import collation of one target: resolve each import to a module file, detect cycles,
//! and build the target's `CXXModules.json`.

use rcmake_base::BString;

use crate::{CxxModulesJson, ModmapFormat, ModulesError, P1689File, TargetDependInfo};

/// Input of [`collate`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CollationInput {
    /// The target's `CXXDependInfo.json`.
    pub tdi: TargetDependInfo,
    /// The target's `.ddi` files, in command-line order.
    pub ddis: Vec<P1689File>,
    /// `CXXModules.json` of each linked target directory, in TDI order.
    pub linked: Vec<CxxModulesJson>,
    /// `--modmapfmt=` value (`None` writes no module maps).
    pub modmap_format: Option<ModmapFormat>,
}

/// Output of [`collate`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CollationOutput {
    /// Content of the Ninja dyndep (`.dd`) file.
    pub dyndep: BString,
    /// Module map files: path and content.
    pub modmaps: Vec<(BString, BString)>,
    /// The target's `CXXModules.json`.
    pub modules: CxxModulesJson,
}

/// Collate the imports of one target.
///
/// # Errors
///
/// Returns [`ModulesError::Unsupported`] until collation is implemented.
pub fn collate(input: &CollationInput) -> Result<CollationOutput, ModulesError> {
    let _ = input;
    Err(ModulesError::Unsupported("C++ module collation"))
}

/// Parse `CXXModules.json`.
///
/// # Errors
///
/// Returns [`ModulesError::Unsupported`] until the reader is implemented.
pub fn read_modules_json(text: &[u8]) -> Result<CxxModulesJson, ModulesError> {
    let _ = text;
    Err(ModulesError::Unsupported("CXXModules.json reading"))
}

/// Write `CXXModules.json`.
///
/// # Errors
///
/// Returns [`ModulesError::Unsupported`] until the writer is implemented.
pub fn write_modules_json(modules: &CxxModulesJson) -> Result<BString, ModulesError> {
    let _ = modules;
    Err(ModulesError::Unsupported("CXXModules.json writing"))
}
