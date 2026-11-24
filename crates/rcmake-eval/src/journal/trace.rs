use std::io::{self, ErrorKind};
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use rcmake_base::Limits;
use rcmake_base::hash::{HashAlgo, IncrementalHasher};
use rcmake_diag::PolicyStatus;
use rcmake_host::{FileKind, FileTime, Follow, Host, HostError, HostResult, Metadata, ReadRange};
use serde::{Deserialize, Serialize};

/// SHA-256 digest used by the configure journal.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct JournalDigest([u8; 32]);

impl JournalDigest {
    /// Construct a digest from raw SHA-256 bytes.
    #[must_use]
    pub const fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    /// Get the raw SHA-256 bytes.
    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    /// Hash bytes with SHA-256.
    #[must_use]
    pub fn hash(bytes: &[u8]) -> Self {
        Self::from_sha256_hex(&HashAlgo::Sha256.hex_digest(bytes))
    }

    fn from_sha256_hex(hex: &str) -> Self {
        // HashAlgo returns exactly 64 lowercase ASCII hexadecimal digits for SHA-256.
        let nibble = |byte| match byte {
            b'0'..=b'9' => byte - b'0',
            _ => byte - b'a' + 10,
        };
        let mut bytes = [0; 32];
        let (pairs, _) = hex.as_bytes().as_chunks::<2>();
        for (byte, &[high, low]) in bytes.iter_mut().zip(pairs) {
            *byte = (nibble(high) << 4) | nibble(low);
        }
        Self(bytes)
    }
}

pub(super) struct JournalHasher(IncrementalHasher);

impl JournalHasher {
    pub(super) fn new(domain: &[u8]) -> Self {
        let mut hasher = Self(HashAlgo::Sha256.hasher());
        hasher.0.update(b"rcmake-configure.v1\0");
        hasher.field(0, domain);
        hasher
    }

    pub(super) fn field(&mut self, tag: u8, data: &[u8]) {
        self.0.update(&[tag]);
        self.0.update(&(data.len() as u64).to_le_bytes());
        self.0.update(data);
    }

    pub(super) fn finish(self) -> JournalDigest {
        JournalDigest::from_sha256_hex(&self.0.finish_hex())
    }
}

/// File metadata used to check whether a content digest is still valid.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct JournalFileStat {
    len: u64,
    mtime_secs: i64,
    mtime_nanos: u32,
    mode: u32,
    dev: u64,
    ino: u64,
}

impl JournalFileStat {
    /// Read the metadata of a regular file. Follow symbolic links.
    pub fn read(host: &dyn Host, path: &Path) -> HostResult<Self> {
        let metadata = host.metadata(path, Follow::Yes)?;
        if metadata.kind != FileKind::File {
            return Err(HostError::unsupported("Expected a regular journal file."));
        }
        Ok(Self::from(&metadata))
    }
}

impl From<&Metadata> for JournalFileStat {
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

/// Recorded file-system or environment input to a region.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum JournalStamp {
    /// A regular file identified by metadata.
    FileStat {
        /// Absolute file path.
        path: PathBuf,
        /// Observed metadata.
        stat: JournalFileStat,
        /// Content digest to check when the metadata changes, if recorded.
        digest: Option<JournalDigest>,
    },
    /// A regular file that must be checked by content.
    FileHash {
        /// Absolute file path.
        path: PathBuf,
        /// Observed content digest.
        digest: JournalDigest,
    },
    /// Immediate names and types in a directory.
    DirEntries {
        /// Absolute directory path.
        path: PathBuf,
        /// Digest returned by [`directory_digest`].
        digest: JournalDigest,
    },
    /// A path that did not exist.
    Missing {
        /// Absolute path.
        path: PathBuf,
    },
    /// Environment variable value or absence.
    EnvVar {
        /// Variable name as bytes.
        name: Vec<u8>,
        /// Digest of the value, or `None` when the variable was absent.
        digest: Option<JournalDigest>,
    },
    /// Failed search for a candidate under a search root.
    NegativeLookup {
        /// Absolute search root.
        root: PathBuf,
        /// Candidate path relative to the search root.
        relative: PathBuf,
        /// Absolute path of the last directory that existed in the search.
        deepest: PathBuf,
        /// Digest of the last directory's immediate names and types.
        listing: JournalDigest,
    },
}

impl JournalStamp {
    /// Record a file input. Use a metadata check only when the time since the
    /// last modification reaches the supplied file-system timestamp resolution.
    pub fn record_file(
        host: &dyn Host,
        path: PathBuf,
        timestamp_resolution: Duration,
    ) -> HostResult<Self> {
        let (stat, digest) = hash_file(host, &path)?;
        let now = host.now();
        let mtime = FileTime {
            secs: stat.mtime_secs,
            nanos: stat.mtime_nanos,
        };
        let nanos = |time: FileTime| i128::from(time.secs) * 1_000_000_000 + i128::from(time.nanos);
        let age = nanos(now) - nanos(mtime);
        if age >= timestamp_resolution.as_nanos() as i128 {
            Ok(Self::FileStat {
                path,
                stat,
                digest: Some(digest),
            })
        } else {
            Ok(Self::FileHash { path, digest })
        }
    }

    fn matches(&self, host: &dyn Host) -> bool {
        match self {
            Self::FileStat { path, stat, digest } => {
                if !path.is_absolute() {
                    return false;
                }
                let Ok(current) = JournalFileStat::read(host, path) else {
                    return false;
                };
                if current == *stat {
                    return host
                        .read(
                            path,
                            ReadRange::Slice {
                                offset: 0,
                                len: Some(0),
                            },
                        )
                        .is_ok();
                }
                current.mode == stat.mode
                    && digest.is_some_and(|digest| file_digest(host, path) == Ok(digest))
            }
            Self::FileHash { path, digest } => file_digest(host, path).as_ref() == Ok(digest),
            Self::DirEntries { path, digest } => {
                directory_digest(host, path).as_ref() == Ok(digest)
            }
            Self::Missing { path } => is_missing(host, path),
            Self::EnvVar { name, digest } => {
                !name.is_empty()
                    && !name.iter().any(|byte| matches!(byte, 0 | b'='))
                    && host.env_get(name).map(|value| JournalDigest::hash(&value)) == *digest
            }
            Self::NegativeLookup {
                root,
                relative,
                deepest,
                listing,
            } => {
                let Some(actual) = deepest_directory(host, root, relative) else {
                    return false;
                };
                actual == *deepest && directory_digest(host, &actual).as_ref() == Ok(listing)
            }
        }
    }
}

fn is_missing(host: &dyn Host, path: &Path) -> bool {
    path.is_absolute()
        && host
            .metadata(path, Follow::No)
            .is_err_and(|error| error.kind == ErrorKind::NotFound)
}

fn deepest_directory(host: &dyn Host, root: &Path, relative: &Path) -> Option<PathBuf> {
    let mut components = relative.components();
    if !root.is_absolute()
        || !components
            .clone()
            .all(|part| matches!(part, Component::Normal(_)))
    {
        return None;
    }
    if !is_missing(host, &root.join(relative)) {
        return None;
    }
    let mut parent = root.to_owned();
    while let Some(Component::Normal(name)) = components.next() {
        let candidate = parent.join(name);
        if is_missing(host, &candidate) {
            return Some(parent);
        }
        if !host
            .metadata(&candidate, Follow::Yes)
            .is_ok_and(|meta| meta.kind == FileKind::Dir)
        {
            return None;
        }
        parent = candidate;
    }
    None
}

/// Hash the sorted immediate names and types of a directory through the host.
pub fn directory_digest(host: &dyn Host, path: &Path) -> HostResult<JournalDigest> {
    if !path.is_absolute() {
        return Err(HostError::not_absolute(path.to_owned()));
    }
    let kind_tag = |kind| match kind {
        FileKind::File => 1,
        FileKind::Dir => 2,
        FileKind::Symlink => 3,
        FileKind::Other => 4,
    };
    let mut entries = host.read_dir(path)?;
    entries.sort_unstable_by(|left, right| {
        left.name
            .cmp(&right.name)
            .then_with(|| kind_tag(left.kind).cmp(&kind_tag(right.kind)))
    });
    let mut hasher = JournalHasher::new(b"directory");
    for entry in entries {
        hasher.field(kind_tag(entry.kind), &entry.name);
    }
    Ok(hasher.finish())
}

/// Hash a regular file through the host using bounded reads.
///
/// Return an error if the file exceeds the read limit or changes during the read.
pub fn file_digest(host: &dyn Host, path: &Path) -> HostResult<JournalDigest> {
    hash_file(host, path).map(|(_, digest)| digest)
}

fn hash_file(host: &dyn Host, path: &Path) -> HostResult<(JournalFileStat, JournalDigest)> {
    if !path.is_absolute() {
        return Err(HostError::not_absolute(path.to_owned()));
    }
    let stat = JournalFileStat::read(host, path)?;
    let error =
        |kind, message| HostError::from_io(io::Error::new(kind, message), Some(path.to_owned()));
    if stat.len > Limits::default().max_file_read {
        return Err(error(
            ErrorKind::FileTooLarge,
            "Journal file exceeds the read limit.",
        ));
    }
    let mut hasher = HashAlgo::Sha256.hasher();
    let mut offset = 0;
    loop {
        // Check one byte beyond the recorded size to detect a file that grew.
        let len = (256 * 1024).min(stat.len - offset + 1);
        let chunk = host.read(
            path,
            ReadRange::Slice {
                offset,
                len: Some(len),
            },
        )?;
        let read = chunk.len() as u64;
        offset += read;
        hasher.update(&chunk);
        if read < len || offset > stat.len {
            break;
        }
    }
    if offset != stat.len || JournalFileStat::read(host, path)? != stat {
        return Err(error(
            ErrorKind::InvalidData,
            "Journal file changed during the read.",
        ));
    }
    Ok((stat, JournalDigest::from_sha256_hex(&hasher.finish_hex())))
}

/// Variable scope relative to the region entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum VariableScope {
    /// The region's own variable scope.
    Current,
    /// The enclosing variable scope.
    Parent,
}

/// Persistent target identity. The binary directory identifies its defining directory.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct TargetKey {
    /// Absolute binary directory of the target definition.
    pub binary_dir: PathBuf,
    /// Target name as bytes.
    pub name: Vec<u8>,
}

/// Persistent identity of the object that owns a property.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum PropertyOwner {
    /// Global configure state.
    Global,
    /// A directory identified by its absolute binary path.
    Directory(PathBuf),
    /// A target identified by its defining directory and name.
    Target(TargetKey),
}

/// Persistent identity of a property.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct PropertyKey {
    /// Object that owns the property.
    pub owner: PropertyOwner,
    /// Property name as bytes.
    pub name: Vec<u8>,
}

/// Configure state that a region can read at entry.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum StateKey {
    /// Normal variable in a tracked scope.
    Variable {
        /// Scope that supplied the read or received the write.
        scope: VariableScope,
        /// Variable name as bytes.
        name: Vec<u8>,
    },
    /// Cache entry by name. Use the digest of the complete entry.
    Cache(Vec<u8>),
    /// Property on a global, directory, or target object.
    Property(PropertyKey),
    /// Policy status by the numeric part of its CMake policy identifier.
    Policy(u16),
    /// Target existence or state by persistent identity.
    Target(TargetKey),
    /// User command definition by its folded name.
    Command(Vec<u8>),
    /// Validated probe outcome by its store key.
    Probe(JournalDigest),
}

/// Result of checking an input against the current entry state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StateLookup {
    /// The key has a value, or is known to be absent.
    Observed(Option<JournalDigest>),
    /// The key cannot be checked, for example when the probe store is disabled.
    Unavailable,
}

/// One entry fact that must match before replay.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum JournalInput {
    /// A file, directory, missing path, or environment variable observed through the host.
    Host(JournalStamp),
    /// First read of shared evaluator state before a write to that key.
    State {
        /// State to check.
        key: StateKey,
        /// Digest of the observed value, or `None` if it was absent.
        value: Option<JournalDigest>,
    },
}

impl JournalInput {
    pub(super) fn matches(
        &self,
        host: &dyn Host,
        lookup: &mut impl FnMut(&StateKey) -> StateLookup,
    ) -> bool {
        match self {
            Self::Host(stamp) => stamp.matches(host),
            Self::State { key, value } => lookup(key) == StateLookup::Observed(*value),
        }
    }
}

/// Encode the active status of a CMake policy for a trace entry.
#[must_use]
pub fn policy_digest(status: PolicyStatus) -> JournalDigest {
    JournalDigest::hash(match status {
        PolicyStatus::Warn => b"WARN".as_slice(),
        PolicyStatus::Old => b"OLD".as_slice(),
        PolicyStatus::New => b"NEW".as_slice(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use rcmake_host::PureHost;

    fn fixture() -> (PureHost, PathBuf) {
        let root = PathBuf::from(if cfg!(windows) { "C:/work" } else { "/work" });
        (PureHost::new(root.clone(), root.join("rcmake")), root)
    }

    #[test]
    fn stores_raw_sha256_bytes() {
        let raw = [
            0xba, 0x78, 0x16, 0xbf, 0x8f, 0x01, 0xcf, 0xea, 0x41, 0x41, 0x40, 0xde, 0x5d, 0xae,
            0x22, 0x23, 0xb0, 0x03, 0x61, 0xa3, 0x96, 0x17, 0x7a, 0x9c, 0xb4, 0x10, 0xff, 0x61,
            0xf2, 0x00, 0x15, 0xad,
        ];
        assert_eq!(JournalDigest::hash(b"abc").as_bytes(), &raw);
        assert_eq!(JournalDigest::from_bytes(raw), JournalDigest::hash(b"abc"));
    }

    #[test]
    fn records_recent_files_by_content_and_older_files_by_metadata() -> HostResult<()> {
        let (host, root) = fixture();
        let path = root.join("input.cmake");
        host.seed_file(&path, b"initial")?;
        let now = host.now();
        host.set_times(&path, None, Some(now))?;
        let resolution = Duration::from_secs(1);
        let recent = JournalStamp::record_file(&host, path.clone(), resolution)?;
        assert!(matches!(recent, JournalStamp::FileHash { .. }));
        assert!(recent.matches(&host));

        let old = FileTime {
            secs: now.secs - 2,
            nanos: now.nanos,
        };
        host.set_times(&path, None, Some(old))?;
        let stable = JournalStamp::record_file(&host, path.clone(), resolution)?;
        assert!(matches!(stable, JournalStamp::FileStat { .. }));
        assert!(stable.matches(&host));

        host.seed_file(&path, b"changed")?;
        assert!(!recent.matches(&host));
        assert!(!stable.matches(&host));
        Ok(())
    }

    #[test]
    fn negative_lookup_requires_the_same_missing_path_and_listing() -> HostResult<()> {
        let (host, root) = fixture();
        let deepest = root.join("include");
        host.create_dir_all(&deepest)?;
        let stamp = JournalStamp::NegativeLookup {
            root: root.clone(),
            relative: PathBuf::from("include/missing/header.h"),
            deepest: deepest.clone(),
            listing: directory_digest(&host, &deepest)?,
        };
        assert!(stamp.matches(&host));

        host.seed_file(&deepest.join("sibling.h"), b"")?;
        assert!(!stamp.matches(&host));
        host.remove_file(&deepest.join("sibling.h"))?;
        assert!(stamp.matches(&host));
        host.seed_file(&deepest.join("missing/header.h"), b"")?;
        assert!(!stamp.matches(&host));
        Ok(())
    }
}
