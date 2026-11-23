//! Generator registry and the macOS generators.

pub mod bundle;
pub mod dmg;

use crate::PackageGenerator;
use crate::archive_gen::{ArchiveGenerator, ArchiveKind};

/// The generator named `name` (a `CPACK_GENERATOR` entry), or `None` when Rcmake does
/// not have it.
#[must_use]
pub fn create(name: &[u8]) -> Option<Box<dyn PackageGenerator>> {
    if let Some(kind) = ArchiveKind::ALL
        .into_iter()
        .find(|kind| kind.name().as_bytes() == name)
    {
        return Some(Box::new(ArchiveGenerator { kind }));
    }
    match name {
        b"DragNDrop" => Some(Box::new(dmg::DragNDropGenerator)),
        b"Bundle" => Some(Box::new(bundle::BundleGenerator)),
        _ => None,
    }
}
