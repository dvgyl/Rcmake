use std::io::{self, ErrorKind, Read};
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use rcmake_base::Limits;
use rcmake_diag::PolicyStatus;
use rcmake_host::{FileKind, Follow, Host, HostError, HostResult, Metadata, ReadRange};
use rcmake_json::{FieldError, FromJson, Object, Value, opt, req};
use sha2::{Digest as _, Sha256};

use super::codec::{
    self, decode_variant, field, fields, unknown_variant, variant_data, variant_fields,
};
use super::frame::{EncodeError, EncodeJson, JsonSink};

pub use crate::digest::JournalDigest;

pub(super) struct JournalHasher(Sha256);

impl JournalHasher {
    pub(super) fn new(domain: &[u8]) -> Self {
        let mut hasher = Self(Sha256::new());
        hasher.0.update(b"rcmake-configure.v1\0");
        hasher.field(0, domain);
        hasher
    }

    pub(super) fn field(&mut self, tag: u8, data: &[u8]) {
        self.0.update([tag]);
        self.0.update((data.len() as u64).to_le_bytes());
        self.0.update(data);
    }

    pub(super) fn u8(&mut self, tag: u8, value: u8) {
        self.0.update([tag, value]);
    }

    pub(super) fn bool(&mut self, tag: u8, value: bool) {
        self.u8(tag, u8::from(value));
    }

    pub(super) fn u32(&mut self, tag: u8, value: u32) {
        self.0.update([tag]);
        self.0.update(value.to_le_bytes());
    }

    pub(super) fn u64(&mut self, tag: u8, value: u64) {
        self.0.update([tag]);
        self.0.update(value.to_le_bytes());
    }

    pub(super) fn usize(&mut self, tag: u8, value: usize) {
        self.u64(tag, value as u64);
    }

    pub(super) fn opt_bytes(&mut self, tag: u8, value: Option<&[u8]>) {
        match value {
            None => self.u8(tag, 0),
            Some(bytes) => {
                self.0.update([tag, 1]);
                self.0.update((bytes.len() as u64).to_le_bytes());
                self.0.update(bytes);
            }
        }
    }

    pub(super) fn file_kind(&mut self, tag: u8, kind: FileKind) {
        let code = match kind {
            FileKind::File => 1,
            FileKind::Dir => 2,
            FileKind::Symlink => 3,
            FileKind::Other => 4,
        };
        self.u8(tag, code);
    }

    pub(super) fn policy_map(&mut self, tag: u8, map: &rcmake_diag::PolicyMap) {
        const MAX_ID: usize = rcmake_diag::generated_policies::MAX_POLICY_ID as usize;
        const PACKED_LEN: usize = (MAX_ID + 1).div_ceil(4);
        let mut packed = [0_u8; PACKED_LEN];
        for id in 0..=MAX_ID {
            let code = match map.get(rcmake_diag::PolicyId::new(id as u16)) {
                PolicyStatus::Warn => 0_u8,
                PolicyStatus::Old => 1_u8,
                PolicyStatus::New => 2_u8,
            };
            if code != 0
                && let Some(slot) = packed.get_mut(id / 4)
            {
                *slot |= code << ((id % 4) * 2);
            }
        }
        self.field(tag, &packed);
    }

    pub(super) fn diag_state(&mut self, tag: u8, state: &rcmake_diag::DiagState) {
        let mut packed = [0_u8; rcmake_diag::DiagCategory::ALL.len()];
        for (slot, category) in packed.iter_mut().zip(rcmake_diag::DiagCategory::ALL) {
            *slot = state.get(category) as u8;
        }
        self.field(tag, &packed);
    }

    pub(super) fn debug_stream(&mut self, tag: u8, value: &dyn core::fmt::Debug) {
        struct StreamDigest(Sha256);
        impl core::fmt::Write for StreamDigest {
            fn write_str(&mut self, s: &str) -> core::fmt::Result {
                self.0.update(s.as_bytes());
                Ok(())
            }
        }
        let mut sub = StreamDigest(Sha256::new());
        let _ = core::fmt::write(&mut sub, format_args!("{value:?}"));
        let digest: [u8; 32] = sub.0.finalize().into();
        self.field(tag, &digest);
    }

    pub(super) fn layered_policies(
        &mut self,
        tag: u8,
        stack: &crate::LayeredStack<rcmake_diag::PolicyMap>,
    ) {
        self.usize(tag, stack.len());
        stack.for_each_entry(|map, weak| {
            self.bool(tag, weak);
            self.policy_map(tag, map);
        });
    }

    pub(super) fn layered_diags(
        &mut self,
        tag: u8,
        stack: &crate::LayeredStack<rcmake_diag::DiagState>,
    ) {
        self.usize(tag, stack.len());
        stack.for_each_entry(|state, weak| {
            self.bool(tag, weak);
            self.diag_state(tag, state);
        });
    }

    pub(super) fn finish(self) -> JournalDigest {
        JournalDigest::from_bytes(self.0.finalize().into())
    }
}

/// File type, identity, size, permissions, and modification time.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct JournalFileStat {
    kind: u8,
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
            kind: match metadata.kind {
                FileKind::File => 1,
                FileKind::Dir => 2,
                FileKind::Symlink => 3,
                FileKind::Other => 4,
            },
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
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum JournalStamp {
    /// A regular file with a content digest and optional metadata evidence.
    FileStat {
        /// Absolute file path.
        path: PathBuf,
        /// Observed metadata.
        stat: JournalFileStat,
        /// Required content digest. Records without a digest cannot replay.
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
    /// Exact target returned by `file(READ_SYMLINK)`.
    Link {
        /// Absolute symbolic link path.
        path: PathBuf,
        /// Target as stored in the link.
        target: PathBuf,
    },
    /// Exact path returned by `file(REAL_PATH)`.
    Canonical {
        /// Absolute path passed to canonicalization.
        path: PathBuf,
        /// Resolved native path.
        target: PathBuf,
    },
    /// Exact stat for timestamp and `if(IS_SYMLINK)` observations.
    Metadata {
        /// Absolute path examined by the command.
        path: PathBuf,
        /// Whether the metadata call followed symbolic links.
        follow: bool,
        /// Observed metadata fields.
        stat: JournalFileStat,
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
    /// Record a content check for a listfile, toolchain, or other file input.
    ///
    /// The resolution argument is retained for callers that also record file
    /// times. Metadata equality alone does not establish content identity.
    pub fn record_file(
        host: &dyn Host,
        path: PathBuf,
        _timestamp_resolution: Duration,
    ) -> HostResult<Self> {
        let (_, digest) = hash_file(host, &path)?;
        Ok(Self::FileHash { path, digest })
    }

    pub(super) fn path(&self) -> Option<&Path> {
        match self {
            Self::FileStat { path, .. }
            | Self::FileHash { path, .. }
            | Self::DirEntries { path, .. }
            | Self::Missing { path }
            | Self::Link { path, .. }
            | Self::Canonical { path, .. }
            | Self::Metadata { path, .. } => Some(path),
            Self::EnvVar { .. } | Self::NegativeLookup { .. } => None,
        }
    }

    fn matches(&self, host: &dyn Host) -> bool {
        match self {
            Self::FileStat { path, stat, digest } => {
                path.is_absolute()
                    && JournalFileStat::read(host, path)
                        .is_ok_and(|current| current.mode == stat.mode)
                    && digest.is_some_and(|digest| file_digest(host, path) == Ok(digest))
            }
            Self::Link { path, target } => {
                path.is_absolute() && host.read_link(path).as_ref() == Ok(target)
            }
            Self::Canonical { path, target } => {
                path.is_absolute() && host.canonicalize(path).as_ref() == Ok(target)
            }
            Self::Metadata { path, follow, stat } => {
                path.is_absolute()
                    && host
                        .metadata(path, if *follow { Follow::Yes } else { Follow::No })
                        .is_ok_and(|actual| JournalFileStat::from(&actual) == *stat)
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
    let before = host.metadata_fresh(path, Follow::Yes)?;
    if before.kind != FileKind::File {
        return Err(HostError::unsupported("Expected a regular journal file."));
    }
    let stat = JournalFileStat::from(&before);
    let error =
        |kind, message| HostError::from_io(io::Error::new(kind, message), Some(path.to_owned()));
    if stat.len > Limits::default().max_file_read {
        return Err(error(
            ErrorKind::FileTooLarge,
            "Journal file exceeds the read limit.",
        ));
    }
    let streamed = if stat.len >= 1024 * 1024 {
        stream_digest(host, path, &before)?
    } else {
        None
    };
    let (length, digest) = if let Some(streamed) = streamed {
        streamed
    } else {
        let bytes = host.read(path, ReadRange::All { limit: stat.len })?;
        (bytes.len() as u64, JournalDigest::hash(&bytes))
    };
    let after = host.metadata_fresh(path, Follow::Yes)?;
    if length != stat.len || JournalFileStat::from(&after) != stat || after.ctime != before.ctime {
        return Err(error(
            ErrorKind::InvalidData,
            "Journal file changed during the read.",
        ));
    }
    Ok((stat, digest))
}

fn stream_digest(
    host: &dyn Host,
    path: &Path,
    before: &Metadata,
) -> HostResult<Option<(u64, JournalDigest)>> {
    let (reader, opened) = match host.open_read_with_metadata(path) {
        Ok((reader, Some(opened))) => (reader, opened),
        Ok((_, None)) => return Ok(None),
        Err(error) if error.kind == ErrorKind::Unsupported => return Ok(None),
        Err(error) => return Err(error),
    };
    let error_at = |error| HostError::from_io(error, Some(path.to_owned()));
    if JournalFileStat::from(&opened) != JournalFileStat::from(before)
        || opened.ctime != before.ctime
    {
        return Err(error_at(io::Error::new(
            ErrorKind::InvalidData,
            "Journal file changed before the read.",
        )));
    }
    let mut reader = reader.take(before.len + 1);
    let mut buffer = vec![0; 1024 * 1024];
    let mut hash = Sha256::new();
    loop {
        match reader.read(&mut buffer) {
            Ok(0) => break,
            Ok(len) => {
                let bytes = buffer
                    .get(..len)
                    .ok_or_else(|| error_at(io::Error::other("Invalid journal read length.")))?;
                hash.update(bytes);
            }
            Err(error) if error.kind() == ErrorKind::Interrupted => {}
            Err(error) => return Err(error_at(error)),
        }
    }
    Ok(Some((
        before.len + 1 - reader.limit(),
        JournalDigest::from_bytes(hash.finalize().into()),
    )))
}

/// Variable scope relative to the region entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum VariableScope {
    /// The region's own variable scope.
    Current,
    /// The enclosing variable scope.
    Parent,
}

/// Persistent target identity. The binary directory identifies its defining directory.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct TargetKey {
    /// Absolute binary directory of the target definition.
    pub binary_dir: PathBuf,
    /// Target name as bytes.
    pub name: Vec<u8>,
}

/// Persistent identity of the object that owns a property.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum PropertyOwner {
    /// Global configure state.
    Global,
    /// A directory identified by its absolute binary path.
    Directory(PathBuf),
    /// A target identified by its defining directory and name.
    Target(TargetKey),
}

/// Persistent identity of a property.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct PropertyKey {
    /// Object that owns the property.
    pub owner: PropertyOwner,
    /// Property name as bytes.
    pub name: Vec<u8>,
}

/// Configure state that a region can read at entry.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum StateKey {
    /// Normal variable in a tracked scope.
    Variable {
        /// Scope that supplied the read or received the write.
        scope: VariableScope,
        /// Variable name as bytes.
        name: Vec<u8>,
    },
    /// Effective normal binding at entry, including an inherited binding.
    EffectiveVariable(Vec<u8>),
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
#[derive(Clone, Debug, PartialEq, Eq)]
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

impl EncodeJson for JournalDigest {
    fn encode(&self, sink: &mut JsonSink<'_>) -> Result<(), EncodeError> {
        sink.hex_digest(self.as_bytes())
    }
}

impl FromJson for JournalDigest {
    fn from_json(value: &Value) -> Result<Self, FieldError> {
        let invalid = || FieldError::invalid("Expected 64 lowercase hexadecimal digits.");
        let nibble = |digit: u8| match digit {
            b'0'..=b'9' => Some(digit - b'0'),
            b'a'..=b'f' => Some(digit - b'a' + 10),
            _ => None,
        };
        let (pairs, rest) = rcmake_json::bytes(value)?.as_chunks::<2>();
        if pairs.len() != 32 || !rest.is_empty() {
            return Err(invalid());
        }
        let mut raw = [0_u8; 32];
        for (slot, &[high, low]) in raw.iter_mut().zip(pairs) {
            *slot = (nibble(high).ok_or_else(invalid)? << 4) | nibble(low).ok_or_else(invalid)?;
        }
        Ok(Self::from_bytes(raw))
    }
}

const FILE_STAT_FIELDS: &[&str] = &[
    "kind",
    "len",
    "mtime_secs",
    "mtime_nanos",
    "mode",
    "dev",
    "ino",
];

impl EncodeJson for JournalFileStat {
    fn encode(&self, sink: &mut JsonSink<'_>) -> Result<(), EncodeError> {
        sink.object(|obj| {
            obj.field("dev", |s| self.dev.encode(s))?;
            obj.field("ino", |s| self.ino.encode(s))?;
            obj.field("kind", |s| self.kind.encode(s))?;
            obj.field("len", |s| self.len.encode(s))?;
            obj.field("mode", |s| self.mode.encode(s))?;
            obj.field("mtime_nanos", |s| self.mtime_nanos.encode(s))?;
            obj.field("mtime_secs", |s| self.mtime_secs.encode(s))
        })
    }
}

impl FromJson for JournalFileStat {
    fn from_json(value: &Value) -> Result<Self, FieldError> {
        let obj = fields(value, FILE_STAT_FIELDS)?;
        Ok(Self {
            kind: req(obj, "kind")?,
            len: req(obj, "len")?,
            mtime_secs: req(obj, "mtime_secs")?,
            mtime_nanos: req(obj, "mtime_nanos")?,
            mode: req(obj, "mode")?,
            dev: req(obj, "dev")?,
            ino: req(obj, "ino")?,
        })
    }
}

impl EncodeJson for JournalStamp {
    fn encode(&self, sink: &mut JsonSink<'_>) -> Result<(), EncodeError> {
        match self {
            Self::FileStat { path, stat, digest } => sink.variant("FileStat", |s| {
                s.object(|obj| {
                    obj.field("digest", |s| digest.encode(s))?;
                    obj.field("path", |s| s.path(path))?;
                    obj.field("stat", |s| stat.encode(s))
                })
            }),
            Self::FileHash { path, digest } => sink.variant("FileHash", |s| {
                s.object(|obj| {
                    obj.field("digest", |s| digest.encode(s))?;
                    obj.field("path", |s| s.path(path))
                })
            }),
            Self::DirEntries { path, digest } => sink.variant("DirEntries", |s| {
                s.object(|obj| {
                    obj.field("digest", |s| digest.encode(s))?;
                    obj.field("path", |s| s.path(path))
                })
            }),
            Self::Missing { path } => sink.variant("Missing", |s| {
                s.object(|obj| obj.field("path", |s| s.path(path)))
            }),
            Self::Link { path, target } => sink.variant("Link", |s| {
                s.object(|obj| {
                    obj.field("path", |s| s.path(path))?;
                    obj.field("target", |s| s.path(target))
                })
            }),
            Self::Canonical { path, target } => sink.variant("Canonical", |s| {
                s.object(|obj| {
                    obj.field("path", |s| s.path(path))?;
                    obj.field("target", |s| s.path(target))
                })
            }),
            Self::Metadata { path, follow, stat } => sink.variant("Metadata", |s| {
                s.object(|obj| {
                    obj.field("follow", |s| follow.encode(s))?;
                    obj.field("path", |s| s.path(path))?;
                    obj.field("stat", |s| stat.encode(s))
                })
            }),
            Self::EnvVar { name, digest } => sink.variant("EnvVar", |s| {
                s.object(|obj| {
                    obj.field("digest", |s| digest.encode(s))?;
                    obj.field("name", |s| s.bytes(name))
                })
            }),
            Self::NegativeLookup {
                root,
                relative,
                deepest,
                listing,
            } => sink.variant("NegativeLookup", |s| {
                s.object(|obj| {
                    obj.field("deepest", |s| s.path(deepest))?;
                    obj.field("listing", |s| listing.encode(s))?;
                    obj.field("relative", |s| s.path(relative))?;
                    obj.field("root", |s| s.path(root))
                })
            }),
        }
    }
}

impl FromJson for JournalStamp {
    fn from_json(value: &Value) -> Result<Self, FieldError> {
        decode_variant(value, |name, data| {
            let members = |allowed: &[&str]| variant_fields(name, data, allowed);
            let path = |obj: &Object, key: &str| field(obj, key, codec::from_path);
            Ok(match name {
                "FileStat" => {
                    let obj = members(&["path", "stat", "digest"])?;
                    Self::FileStat {
                        path: path(obj, "path")?,
                        stat: req(obj, "stat")?,
                        digest: opt(obj, "digest")?,
                    }
                }
                "FileHash" => {
                    let obj = members(&["path", "digest"])?;
                    Self::FileHash {
                        path: path(obj, "path")?,
                        digest: req(obj, "digest")?,
                    }
                }
                "DirEntries" => {
                    let obj = members(&["path", "digest"])?;
                    Self::DirEntries {
                        path: path(obj, "path")?,
                        digest: req(obj, "digest")?,
                    }
                }
                "Missing" => Self::Missing {
                    path: path(members(&["path"])?, "path")?,
                },
                "Link" => {
                    let obj = members(&["path", "target"])?;
                    Self::Link {
                        path: path(obj, "path")?,
                        target: path(obj, "target")?,
                    }
                }
                "Canonical" => {
                    let obj = members(&["path", "target"])?;
                    Self::Canonical {
                        path: path(obj, "path")?,
                        target: path(obj, "target")?,
                    }
                }
                "Metadata" => {
                    let obj = members(&["path", "follow", "stat"])?;
                    Self::Metadata {
                        path: path(obj, "path")?,
                        follow: req(obj, "follow")?,
                        stat: req(obj, "stat")?,
                    }
                }
                "EnvVar" => {
                    let obj = members(&["name", "digest"])?;
                    Self::EnvVar {
                        name: field(obj, "name", codec::from_bytes)?,
                        digest: opt(obj, "digest")?,
                    }
                }
                "NegativeLookup" => {
                    let obj = members(&["root", "relative", "deepest", "listing"])?;
                    Self::NegativeLookup {
                        root: path(obj, "root")?,
                        relative: path(obj, "relative")?,
                        deepest: path(obj, "deepest")?,
                        listing: req(obj, "listing")?,
                    }
                }
                _ => return Err(unknown_variant(name)),
            })
        })
    }
}

impl EncodeJson for VariableScope {
    fn encode(&self, sink: &mut JsonSink<'_>) -> Result<(), EncodeError> {
        sink.str(match self {
            Self::Current => "Current",
            Self::Parent => "Parent",
        })
    }
}

impl FromJson for VariableScope {
    fn from_json(value: &Value) -> Result<Self, FieldError> {
        match rcmake_json::str(value)? {
            "Current" => Ok(Self::Current),
            "Parent" => Ok(Self::Parent),
            name => Err(unknown_variant(name)),
        }
    }
}

impl EncodeJson for TargetKey {
    fn encode(&self, sink: &mut JsonSink<'_>) -> Result<(), EncodeError> {
        sink.object(|obj| {
            obj.field("binary_dir", |s| s.path(&self.binary_dir))?;
            obj.field("name", |s| s.bytes(&self.name))
        })
    }
}

impl FromJson for TargetKey {
    fn from_json(value: &Value) -> Result<Self, FieldError> {
        let obj = fields(value, &["binary_dir", "name"])?;
        Ok(Self {
            binary_dir: field(obj, "binary_dir", codec::from_path)?,
            name: field(obj, "name", codec::from_bytes)?,
        })
    }
}

impl EncodeJson for PropertyOwner {
    fn encode(&self, sink: &mut JsonSink<'_>) -> Result<(), EncodeError> {
        match self {
            Self::Global => sink.str("Global"),
            Self::Directory(path) => sink.variant("Directory", |s| s.path(path)),
            Self::Target(target) => sink.variant("Target", |s| target.encode(s)),
        }
    }
}

impl FromJson for PropertyOwner {
    fn from_json(value: &Value) -> Result<Self, FieldError> {
        decode_variant(value, |name, data| match (name, data) {
            ("Global", None) => Ok(Self::Global),
            ("Directory", Some(data)) => codec::from_path(data).map(Self::Directory),
            ("Target", Some(data)) => TargetKey::from_json(data).map(Self::Target),
            _ => Err(unknown_variant(name)),
        })
    }
}

impl EncodeJson for PropertyKey {
    fn encode(&self, sink: &mut JsonSink<'_>) -> Result<(), EncodeError> {
        sink.object(|obj| {
            obj.field("name", |s| s.bytes(&self.name))?;
            obj.field("owner", |s| self.owner.encode(s))
        })
    }
}

impl FromJson for PropertyKey {
    fn from_json(value: &Value) -> Result<Self, FieldError> {
        let obj = fields(value, &["owner", "name"])?;
        Ok(Self {
            owner: req(obj, "owner")?,
            name: field(obj, "name", codec::from_bytes)?,
        })
    }
}

impl EncodeJson for StateKey {
    fn encode(&self, sink: &mut JsonSink<'_>) -> Result<(), EncodeError> {
        match self {
            Self::Variable { scope, name } => sink.variant("Variable", |s| {
                s.object(|obj| {
                    obj.field("name", |s| s.bytes(name))?;
                    obj.field("scope", |s| scope.encode(s))
                })
            }),
            Self::EffectiveVariable(name) => sink.variant("EffectiveVariable", |s| s.bytes(name)),
            Self::Cache(name) => sink.variant("Cache", |s| s.bytes(name)),
            Self::Property(key) => sink.variant("Property", |s| key.encode(s)),
            Self::Policy(id) => sink.variant("Policy", |s| id.encode(s)),
            Self::Target(target) => sink.variant("Target", |s| target.encode(s)),
            Self::Command(name) => sink.variant("Command", |s| s.bytes(name)),
            Self::Probe(key) => sink.variant("Probe", |s| key.encode(s)),
        }
    }
}

impl FromJson for StateKey {
    fn from_json(value: &Value) -> Result<Self, FieldError> {
        decode_variant(value, |name, data| {
            if name == "Variable" {
                let obj = variant_fields(name, data, &["scope", "name"])?;
                return Ok(Self::Variable {
                    scope: req(obj, "scope")?,
                    name: field(obj, "name", codec::from_bytes)?,
                });
            }
            let data = variant_data(name, data)?;
            match name {
                "EffectiveVariable" => codec::from_bytes(data).map(Self::EffectiveVariable),
                "Cache" => codec::from_bytes(data).map(Self::Cache),
                "Property" => PropertyKey::from_json(data).map(Self::Property),
                "Policy" => u16::from_json(data).map(Self::Policy),
                "Target" => TargetKey::from_json(data).map(Self::Target),
                "Command" => codec::from_bytes(data).map(Self::Command),
                "Probe" => JournalDigest::from_json(data).map(Self::Probe),
                _ => Err(unknown_variant(name)),
            }
        })
    }
}

impl EncodeJson for JournalInput {
    fn encode(&self, sink: &mut JsonSink<'_>) -> Result<(), EncodeError> {
        match self {
            Self::Host(stamp) => sink.variant("Host", |s| stamp.encode(s)),
            Self::State { key, value } => sink.variant("State", |s| {
                s.object(|obj| {
                    obj.field("key", |s| key.encode(s))?;
                    obj.field("value", |s| value.encode(s))
                })
            }),
        }
    }
}

impl FromJson for JournalInput {
    fn from_json(value: &Value) -> Result<Self, FieldError> {
        decode_variant(value, |name, data| match name {
            "Host" => JournalStamp::from_json(variant_data(name, data)?).map(Self::Host),
            "State" => {
                let obj = variant_fields(name, data, &["key", "value"])?;
                Ok(Self::State {
                    key: req(obj, "key")?,
                    value: opt(obj, "value")?,
                })
            }
            _ => Err(unknown_variant(name)),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rcmake_host::{FileTime, PureHost, WriteMode};

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
    fn detects_same_size_rewrite_with_preserved_mtime() -> HostResult<()> {
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
        assert!(matches!(stable, JournalStamp::FileHash { .. }));
        assert!(stable.matches(&host));

        host.write(&path, b"changed", WriteMode::Truncate)?;
        host.set_times(&path, None, Some(old))?;
        assert!(!recent.matches(&host));
        assert!(!stable.matches(&host));
        Ok(())
    }

    #[test]
    fn large_file_digest_supports_hosts_without_bound_metadata() -> HostResult<()> {
        let (host, root) = fixture();
        let path = root.join("large-input");
        let bytes = vec![b'x'; 1024 * 1024 + 1];
        host.seed_file(&path, &bytes)?;
        assert_eq!(file_digest(&host, &path)?, JournalDigest::hash(&bytes));
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
