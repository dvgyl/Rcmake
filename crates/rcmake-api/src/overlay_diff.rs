use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use rcmake_host::{
    FileKind, Follow, Host, Metadata,
    overlay::{OverlayAudit, OverlayHost},
};

const MAX_CAPTURE_BYTES: usize = 1024 * 1024;

/// Kind of path affected by an overlay change.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OverlayEntryKind {
    /// Regular file. Metadata identifies paths that share an inode.
    File,
    /// Directory.
    Directory,
    /// Symbolic link.
    Symlink,
    /// Tombstone that hides a base path or subtree.
    Removed,
    /// Special file, such as a device or socket.
    Other,
    /// The host could not read the path metadata.
    Unavailable,
}

/// Content state of a changed path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OverlayBytes {
    /// The path does not have regular file contents.
    NotApplicable,
    /// File contents retained in the snapshot.
    Captured(Arc<[u8]>),
    /// File contents still come from the base host.
    FromBase,
    /// The file exceeds the remaining byte budget.
    OmittedByLimit,
    /// The file metadata was unavailable.
    Unavailable,
}

/// One path in the overlay snapshot.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OverlayChange {
    /// Entry kind, determined without following symbolic links.
    pub kind: OverlayEntryKind,
    /// Metadata after the change. Device and inode identify hard links.
    pub metadata: Option<Metadata>,
    /// Metadata at this path in the base host. Compare inode identities to
    /// associate a removed path with a rename destination.
    pub previous_metadata: Option<Metadata>,
    /// Link destination exactly as recorded by the host, if available.
    pub link_target: Option<PathBuf>,
    /// Whether file bytes were available and retained.
    pub bytes: OverlayBytes,
}

/// Filesystem changes retained by a session's in-memory overlay.
///
/// Capture uses at most 1 MiB of file contents in total. Paths that share an
/// inode share captured bytes. Use the metadata for the full file length.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct OverlayDiff {
    /// Paths added, changed, moved, or removed, in path order.
    pub changes: BTreeMap<PathBuf, OverlayChange>,
    /// Host counts, including the number of changed inodes and environment keys.
    pub audit: OverlayAudit,
}

impl OverlayDiff {
    // Capture after all users of the session host have stopped writing.
    pub(crate) fn from_host<H: Host + ?Sized>(host: &OverlayHost<H>, base: &H) -> Self {
        let audit = host.audit();
        let mut remaining = MAX_CAPTURE_BYTES;
        let mut inodes = BTreeMap::<(u64, u64), OverlayBytes>::new();
        let mut changes = BTreeMap::new();
        for path in host.written_paths() {
            let change = match host.metadata(&path, Follow::No) {
                Ok(metadata) => {
                    let (kind, bytes) = match metadata.kind {
                        FileKind::Dir => (OverlayEntryKind::Directory, OverlayBytes::NotApplicable),
                        FileKind::Symlink => {
                            (OverlayEntryKind::Symlink, OverlayBytes::NotApplicable)
                        }
                        FileKind::Other => (OverlayEntryKind::Other, OverlayBytes::NotApplicable),
                        FileKind::File => {
                            let mut capture = || {
                                if metadata.len > remaining as u64 {
                                    return OverlayBytes::OmittedByLimit;
                                }
                                match host.read_overlay_file(&path) {
                                    Some(bytes) => {
                                        if let Some(left) = remaining.checked_sub(bytes.len()) {
                                            remaining = left;
                                            OverlayBytes::Captured(bytes.into())
                                        } else {
                                            OverlayBytes::OmittedByLimit
                                        }
                                    }
                                    None => OverlayBytes::FromBase,
                                }
                            };
                            let bytes = if metadata.ino == 0 {
                                capture()
                            } else {
                                inodes
                                    .entry((metadata.dev, metadata.ino))
                                    .or_insert_with(capture)
                                    .clone()
                            };
                            (OverlayEntryKind::File, bytes)
                        }
                    };
                    let link_target = (kind == OverlayEntryKind::Symlink)
                        .then(|| host.read_link(&path).ok())
                        .flatten();
                    OverlayChange {
                        kind,
                        metadata: Some(metadata),
                        previous_metadata: base.metadata(&path, Follow::No).ok(),
                        link_target,
                        bytes,
                    }
                }
                Err(_) => OverlayChange {
                    kind: OverlayEntryKind::Unavailable,
                    metadata: None,
                    previous_metadata: base.metadata(&path, Follow::No).ok(),
                    link_target: None,
                    bytes: OverlayBytes::Unavailable,
                },
            };
            changes.insert(path, change);
        }
        for path in host.deleted_paths() {
            let previous_metadata = base.metadata(&path, Follow::No).ok();
            changes.insert(
                path,
                OverlayChange {
                    kind: OverlayEntryKind::Removed,
                    metadata: None,
                    previous_metadata,
                    link_target: None,
                    bytes: OverlayBytes::NotApplicable,
                },
            );
        }
        Self { changes, audit }
    }

    /// Get the captured bytes of a changed file.
    #[must_use]
    pub fn read_file(&self, path: &Path) -> Option<&[u8]> {
        match &self.changes.get(path)?.bytes {
            OverlayBytes::Captured(bytes) => Some(bytes),
            _ => None,
        }
    }
}
