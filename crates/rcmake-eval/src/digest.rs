//! SHA-256 content digests shared between the parse cache and configure replay journal.

#![cfg_attr(not(feature = "replay"), allow(unreachable_pub, dead_code))]

use sha2::{Digest as _, Sha256};

/// SHA-256 digest used by the configure journal and shared parse cache.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct JournalDigest([u8; 32]);

impl JournalDigest {
    /// Construct a digest from raw SHA-256 bytes.
    #[cfg(feature = "replay")]
    #[must_use]
    pub const fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    /// Get the raw SHA-256 bytes.
    #[cfg(feature = "replay")]
    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    /// Hash bytes with SHA-256.
    #[must_use]
    pub fn hash(bytes: &[u8]) -> Self {
        Self(Sha256::digest(bytes).into())
    }
}
