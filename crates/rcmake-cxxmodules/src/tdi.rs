//! `CXXDependInfo.json` (target dependency information, "TDI") reader and writer.

use rcmake_base::BString;

use crate::{ModulesError, TargetDependInfo};

/// Parse `CXXDependInfo.json`.
///
/// # Errors
///
/// Returns [`ModulesError::Unsupported`] until the reader is implemented.
pub fn read_tdi(text: &[u8]) -> Result<TargetDependInfo, ModulesError> {
    let _ = text;
    Err(ModulesError::Unsupported("CXXDependInfo.json reading"))
}

/// Write `CXXDependInfo.json`.
///
/// # Errors
///
/// Returns [`ModulesError::Unsupported`] until the writer is implemented.
pub fn write_tdi(info: &TargetDependInfo) -> Result<BString, ModulesError> {
    let _ = info;
    Err(ModulesError::Unsupported("CXXDependInfo.json writing"))
}
