//! Archive generators: `TGZ`, `TXZ`, `TZST`, `TBZ2`, `TZ`, `TAR`, and `ZIP`.

use std::path::PathBuf;

use crate::{CpackError, PackageContext, PackageGenerator};

/// Archive format of an [`ArchiveGenerator`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ArchiveKind {
    /// `TGZ`: gzip-compressed tar.
    Tgz,
    /// `TXZ`: xz-compressed tar.
    Txz,
    /// `TZST`: zstd-compressed tar.
    Tzst,
    /// `TBZ2`: bzip2-compressed tar.
    Tbz2,
    /// `TZ`: LZW-compressed tar (`compress`).
    Tz,
    /// `TAR`: uncompressed tar.
    Tar,
    /// `ZIP`.
    Zip,
}

impl ArchiveKind {
    /// All archive generators.
    pub const ALL: [Self; 7] = [
        Self::Tgz,
        Self::Txz,
        Self::Tzst,
        Self::Tbz2,
        Self::Tz,
        Self::Tar,
        Self::Zip,
    ];

    /// Generator name.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Tgz => "TGZ",
            Self::Txz => "TXZ",
            Self::Tzst => "TZST",
            Self::Tbz2 => "TBZ2",
            Self::Tz => "TZ",
            Self::Tar => "TAR",
            Self::Zip => "ZIP",
        }
    }

    /// Package file extension.
    #[must_use]
    pub const fn extension(self) -> &'static str {
        match self {
            Self::Tgz => ".tar.gz",
            Self::Txz => ".tar.xz",
            Self::Tzst => ".tar.zst",
            Self::Tbz2 => ".tar.bz2",
            Self::Tz => ".tar.Z",
            Self::Tar => ".tar",
            Self::Zip => ".zip",
        }
    }
}

/// A CPack archive generator.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ArchiveGenerator {
    /// Archive format.
    pub kind: ArchiveKind,
}

impl PackageGenerator for ArchiveGenerator {
    fn name(&self) -> &'static str {
        self.kind.name()
    }

    fn extension(&self) -> &'static str {
        self.kind.extension()
    }

    fn package(&self, cx: &PackageContext<'_>) -> Result<Vec<PathBuf>, CpackError> {
        let _ = cx;
        Err(CpackError::Unsupported("CPack archive packaging"))
    }
}
