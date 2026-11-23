//! macOS `DragNDrop` generator (`.dmg` through `hdiutil`).

use std::path::PathBuf;

use crate::{CpackError, PackageContext, PackageGenerator};

/// The `DragNDrop` generator.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DragNDropGenerator;

impl PackageGenerator for DragNDropGenerator {
    fn name(&self) -> &'static str {
        "DragNDrop"
    }

    fn extension(&self) -> &'static str {
        ".dmg"
    }

    fn package(&self, cx: &PackageContext<'_>) -> Result<Vec<PathBuf>, CpackError> {
        let _ = cx;
        Err(CpackError::Unsupported("the DragNDrop generator"))
    }
}
