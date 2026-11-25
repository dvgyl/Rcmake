use std::collections::BTreeMap;
use std::io::ErrorKind;
use std::path::{Component, Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use rcmake_base::Limits;
use rcmake_host::{FileKind, Follow, Host, HostError, HostResult, Metadata, ReadRange};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

use crate::{Digest, Domain, KeyHasher};

/// The metadata observed for a regular file.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
pub struct FastStat {
    /// File size in bytes.
    pub len: u64,
    /// Modification time in whole seconds since the Unix epoch.
    pub mtime_secs: i64,
    /// Nanosecond part of the modification time.
    pub mtime_nanos: u32,
    /// File permission bits.
    pub mode: u32,
    /// Device identifier.
    #[serde(default)]
    pub dev: u64,
    /// File identifier on the device.
    #[serde(default)]
    pub ino: u64,
}

impl FastStat {
    /// Read the metadata for a regular file. Follow symbolic links.
    pub fn read(host: &dyn Host, path: &Path) -> HostResult<Self> {
        let metadata = host.metadata(path, Follow::Yes)?;
        if metadata.kind != FileKind::File {
            return Err(HostError::unsupported("Expected a regular file."));
        }
        Ok(Self::from(&metadata))
    }
}

impl From<&Metadata> for FastStat {
    fn from(metadata: &Metadata) -> Self {
        Self {
            len: metadata.len,
            mtime_secs: metadata.mtime.secs,
            mtime_nanos: metadata.mtime.nanos,
            mode: metadata.mode,
            dev: metadata.dev,
            ino: metadata.ino,
        }
    }
}

/// One input observed during a computation.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum InputStamp {
    /// A file identified by its permission bits and content digest.
    FileStat {
        /// Absolute file path.
        path: PathBuf,
        /// Observed metadata.
        stat: FastStat,
        /// Observed content digest. Old records without a digest cannot be reused.
        #[serde(default)]
        digest: Option<Digest>,
    },
    /// A file identified by SHA-256 of its contents.
    FileHash {
        /// Absolute file path.
        path: PathBuf,
        /// Observed content digest.
        digest: Digest,
    },
    /// A directory identified by its immediate entries.
    DirEntries {
        /// Absolute directory path.
        path: PathBuf,
        /// Observed digest from [directory_digest].
        digest: Digest,
    },
    /// A directory identified by its bounded recursive listing.
    TreeListing {
        /// Absolute search directory.
        path: PathBuf,
        /// Observed digest from [tree_listing_digest].
        digest: Digest,
    },
    /// A path that did not exist.
    Missing {
        /// Absolute path.
        path: PathBuf,
    },
    /// An environment variable identified by a digest of its raw value.
    EnvVar {
        /// Variable name as bytes.
        name: Vec<u8>,
        /// Observed value digest, or absence.
        digest: Option<Digest>,
    },
    /// A failed search for one candidate under a search root.
    NegativeLookup {
        /// Absolute search root.
        root: PathBuf,
        /// Candidate relative to the search root.
        relative: PathBuf,
        /// Absolute path of the last directory that existed.
        deepest: PathBuf,
        /// Observed digest of that directory's immediate entries.
        listing: Digest,
    },
}

impl InputStamp {
    /// Record a file input. Use only its digest when its timestamp is within
    /// the host filesystem's timestamp resolution of the current time.
    pub fn record_file(
        host: &dyn Host,
        path: PathBuf,
        timestamp_resolution: Duration,
    ) -> HostResult<Self> {
        let metadata = host.metadata(&path, Follow::Yes)?;
        let digest = hash_file(host, &path, &metadata)?;
        if crate::gc::older_than(host.now(), metadata.mtime, timestamp_resolution) {
            Ok(Self::FileStat {
                path,
                stat: FastStat::from(&metadata),
                digest: Some(digest),
            })
        } else {
            Ok(Self::FileHash { path, digest })
        }
    }
}

/// The input stamps required to reuse an entry.
pub type Trace = Vec<InputStamp>;

/// The result of checking a trace.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VerifyOutcome {
    /// Every stamp still matches.
    Hit,
    /// The first stamp that could not be verified.
    Miss(MissReason),
}

/// The reason a trace cannot be reused.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MissReason {
    /// Zero-based position of the stamp in the trace.
    pub stamp: usize,
}

/// Hash a regular file using bounded reads through the host.
///
/// Return an error if the file exceeds the read limit or its metadata changes
/// during the read.
pub fn file_digest(host: &dyn Host, path: &Path) -> HostResult<Digest> {
    let before = host.metadata(path, Follow::Yes)?;
    hash_file(host, path, &before)
}

/// Reuse file digests within one session while file identity and metadata match.
#[derive(Default)]
pub struct TraceMemo {
    files: Mutex<BTreeMap<FastStat, Digest>>,
}

impl TraceMemo {
    /// Read the current metadata, then return or calculate the file digest.
    pub fn file_digest(&self, host: &dyn Host, path: &Path) -> HostResult<Digest> {
        let metadata = host.metadata(path, Follow::Yes)?;
        if metadata.kind != FileKind::File || metadata.ino == 0 {
            return hash_file(host, path, &metadata);
        }
        let stat = FastStat::from(&metadata);
        if let Ok(files) = self.files.lock()
            && let Some(&digest) = files.get(&stat)
        {
            return Ok(digest);
        }
        let digest = hash_file(host, path, &metadata)?;
        if let Ok(mut files) = self.files.lock()
            && files.len() < 32_768
        {
            files.insert(stat, digest);
        }
        Ok(digest)
    }

    /// Verify current inputs and share file digests across checks in this session.
    pub fn verify(&self, host: &dyn Host, trace: &[InputStamp]) -> VerifyOutcome {
        verify_with(host, trace, Some(self))
    }
}

fn hash_file(host: &dyn Host, path: &Path, before: &Metadata) -> HostResult<Digest> {
    if before.kind != FileKind::File {
        return Err(HostError::unsupported("Expected a regular file."));
    }
    if before.len > Limits::default().max_file_read {
        return Err(crate::io_at(
            std::io::Error::new(
                ErrorKind::FileTooLarge,
                "Trace file exceeds the read limit.",
            ),
            path,
        ));
    }
    let mut hash = Sha256::new();
    let mut offset = 0;
    loop {
        const CHUNK: u64 = 256 * 1024;
        // Request one extra byte at EOF to detect growth.
        let len = CHUNK.min(before.len - offset + 1);
        let bytes = host.read(
            path,
            ReadRange::Slice {
                offset,
                len: Some(len),
            },
        )?;
        offset += bytes.len() as u64;
        hash.update(&bytes);
        if offset > before.len || bytes.len() as u64 != len {
            break;
        }
    }
    let after = host.metadata(path, Follow::Yes)?;
    if offset != before.len
        || after.kind != FileKind::File
        || FastStat::from(before) != FastStat::from(&after)
    {
        return Err(crate::io_at(
            std::io::Error::new(
                ErrorKind::InvalidData,
                "Trace file changed during the read.",
            ),
            path,
        ));
    }
    Ok(Digest::from_bytes(hash.finalize().into()))
}

/// Hash the sorted byte names and kinds of a directory's immediate entries.
pub fn directory_digest(host: &dyn Host, path: &Path) -> HostResult<Digest> {
    let mut entries = host.read_dir(path)?;
    entries.sort_unstable_by(|left, right| left.name.cmp(&right.name));
    let mut hasher = KeyHasher::new(Domain::BlobV1);
    hasher.field(0, b"directory");
    for entry in entries {
        let tag = match entry.kind {
            FileKind::File => 1,
            FileKind::Dir => 2,
            FileKind::Symlink => 3,
            FileKind::Other => 4,
        };
        hasher.field(tag, &entry.name);
    }
    Ok(hasher.finish())
}

/// Hash the names, kinds, and link targets below a search directory.
///
/// Reject trees over 20,000 entries or 16 levels deep. Reject links to directories
/// because changes below those links would not appear in this listing.
pub fn tree_listing_digest(host: &dyn Host, root: &Path) -> HostResult<Digest> {
    let mut key = KeyHasher::new(Domain::BlobV1);
    key.field(0, b"tree-listing");
    if host.metadata(root, Follow::No)?.kind == FileKind::Symlink {
        key.field(1, host.read_link(root)?.as_os_str().as_encoded_bytes());
    }
    let mut directories = vec![(root.to_path_buf(), 0)];
    let mut remaining = 20_000_usize;
    while let Some((directory, depth)) = directories.pop() {
        let mut entries = host.read_dir(&directory)?;
        remaining = remaining
            .checked_sub(entries.len())
            .filter(|_| depth <= 16)
            .ok_or_else(|| HostError::unsupported("Search directory exceeds the listing limit."))?;
        entries.sort_unstable_by(|left, right| left.name.cmp(&right.name));
        let relative = directory
            .strip_prefix(root)
            .map_err(|_| HostError::unsupported("Search directory is outside its root."))?;
        key.field(2, relative.as_os_str().as_encoded_bytes());
        for entry in entries {
            let name = rcmake_host::path::to_native(&entry.name);
            if !matches!(name.components().next(), Some(Component::Normal(_)))
                || name.components().count() != 1
            {
                return Err(HostError::unsupported("Invalid search directory entry."));
            }
            let path = directory.join(name);
            match entry.kind {
                FileKind::File => key.field(3, &entry.name),
                FileKind::Dir => {
                    key.field(4, &entry.name);
                    directories.push((path, depth + 1));
                }
                FileKind::Symlink => {
                    key.field(5, &entry.name);
                    key.field(6, host.read_link(&path)?.as_os_str().as_encoded_bytes());
                    match host.metadata(&path, Follow::Yes) {
                        Ok(stat) if stat.kind == FileKind::File => key.field(7, b"file"),
                        Err(error) if error.kind == ErrorKind::NotFound => key.field(7, b"missing"),
                        _ => {
                            return Err(HostError::unsupported(
                                "Search directory contains an unverifiable link.",
                            ));
                        }
                    }
                }
                FileKind::Other => {
                    return Err(HostError::unsupported(
                        "Search directory contains an unsupported entry.",
                    ));
                }
            }
        }
    }
    Ok(key.finish())
}

/// Check all inputs through the host. A changed, invalid, or unreadable input is a miss.
pub fn verify_trace(host: &dyn Host, trace: &[InputStamp]) -> VerifyOutcome {
    verify_with(host, trace, None)
}

fn verify_with(host: &dyn Host, trace: &[InputStamp], memo: Option<&TraceMemo>) -> VerifyOutcome {
    for (stamp, input) in trace.iter().enumerate() {
        if !matches_input(host, input, memo) {
            return VerifyOutcome::Miss(MissReason { stamp });
        }
    }
    VerifyOutcome::Hit
}

fn matches_input(host: &dyn Host, input: &InputStamp, memo: Option<&TraceMemo>) -> bool {
    match input {
        InputStamp::FileStat { path, stat, digest } => {
            let Some(digest) = digest else {
                return false;
            };
            let Ok(current) = host.metadata(path, Follow::Yes) else {
                return false;
            };
            current.mode == stat.mode && hash_file(host, path, &current).as_ref() == Ok(digest)
        }
        InputStamp::FileHash { path, digest } => {
            memo.map_or_else(
                || file_digest(host, path),
                |memo| memo.file_digest(host, path),
            )
            .as_ref()
                == Ok(digest)
        }
        InputStamp::DirEntries { path, digest } => {
            directory_digest(host, path).as_ref() == Ok(digest)
        }
        InputStamp::TreeListing { path, digest } => {
            tree_listing_digest(host, path).as_ref() == Ok(digest)
        }
        InputStamp::Missing { path } => missing(host, path),
        InputStamp::EnvVar { name, digest } => {
            !name.is_empty()
                && !name.iter().any(|byte| matches!(byte, 0 | b'='))
                && host.env_get(name).map(|value| Digest::hash(&value)) == *digest
        }
        InputStamp::NegativeLookup {
            root,
            relative,
            deepest,
            listing,
        } => {
            if !root.is_absolute() || !missing(host, &root.join(relative)) {
                return false;
            }
            let mut parent = root.to_owned();
            let mut components = relative.components().peekable();
            while let Some(Component::Normal(next)) = components.next() {
                let candidate = parent.join(next);
                if missing(host, &candidate) {
                    return components.all(|part| matches!(part, Component::Normal(_)))
                        && parent == *deepest
                        && host
                            .metadata(&parent, Follow::Yes)
                            .is_ok_and(|meta| meta.kind == FileKind::Dir)
                        && directory_digest(host, &parent).as_ref() == Ok(listing);
                }
                if components.peek().is_none()
                    || !host
                        .metadata(&candidate, Follow::Yes)
                        .is_ok_and(|meta| meta.kind == FileKind::Dir)
                {
                    return false;
                }
                parent = candidate;
            }
            false
        }
    }
}

pub(crate) fn missing(host: &dyn Host, path: &Path) -> bool {
    path.is_absolute()
        && host
            .metadata(path, Follow::No)
            .is_err_and(|error| error.kind == ErrorKind::NotFound)
}
