//! macOS `Bundle` generator (application bundle inside a `.dmg`).

use std::path::PathBuf;

use crate::{CpackError, PackageContext, PackageGenerator};

/// The `Bundle` generator.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct BundleGenerator;

impl PackageGenerator for BundleGenerator {
    fn name(&self) -> &'static str {
        "Bundle"
    }

    fn extension(&self) -> &'static str {
        ".dmg"
    }

    fn package(&self, cx: &PackageContext<'_>) -> Result<Vec<PathBuf>, CpackError> {
        let _ = cx;
        Err(CpackError::Unsupported("the Bundle generator"))
    }
}
