use std::fmt::Write as _;
use std::io::{self, ErrorKind};
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use rcmake_base::Limits;
use rcmake_diag::PolicyStatus;
use rcmake_host::{FileKind, Follow, Host, HostError, HostResult, Metadata, ReadRange};
use rcmake_json::{FieldError, FromJson, Object, ToJson, Value, opt, req};
use sha2::{Digest as _, Sha256};

use super::codec::{
    self, decode_variant, field, fields, unknown_variant, variant, variant_data, variant_fields,
};

/// SHA-256 digest used by the configure journal.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
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
        Self(Sha256::digest(bytes).into())
    }
}

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

    pub(super) fn finish(self) -> JournalDigest {
        JournalDigest(self.0.finalize().into())
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
    let stat = JournalFileStat::read(host, path)?;
    let error =
        |kind, message| HostError::from_io(io::Error::new(kind, message), Some(path.to_owned()));
    if stat.len > Limits::default().max_file_read {
        return Err(error(
            ErrorKind::FileTooLarge,
            "Journal file exceeds the read limit.",
        ));
    }
    let bytes = host.read(path, ReadRange::All { limit: stat.len })?;
    if bytes.len() as u64 != stat.len || JournalFileStat::read(host, path)? != stat {
        return Err(error(
            ErrorKind::InvalidData,
            "Journal file changed during the read.",
        ));
    }
    Ok((stat, JournalDigest::hash(&bytes)))
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

impl ToJson for JournalDigest {
    fn to_json(&self) -> Value {
        let mut hex = String::with_capacity(64);
        for byte in self.0 {
            let _ = write!(hex, "{byte:02x}");
        }
        Value::string(hex)
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
        Ok(Self(raw))
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

impl ToJson for JournalFileStat {
    fn to_json(&self) -> Value {
        Value::object_from([
            ("kind", self.kind.to_json()),
            ("len", self.len.to_json()),
            ("mtime_secs", self.mtime_secs.to_json()),
            ("mtime_nanos", self.mtime_nanos.to_json()),
            ("mode", self.mode.to_json()),
            ("dev", self.dev.to_json()),
            ("ino", self.ino.to_json()),
        ])
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

impl ToJson for JournalStamp {
    fn to_json(&self) -> Value {
        let (name, members) = match self {
            Self::FileStat { path, stat, digest } => (
                "FileStat",
                vec![
                    ("path", codec::path(path)),
                    ("stat", stat.to_json()),
                    ("digest", digest.to_json()),
                ],
            ),
            Self::FileHash { path, digest } => (
                "FileHash",
                vec![("path", codec::path(path)), ("digest", digest.to_json())],
            ),
            Self::DirEntries { path, digest } => (
                "DirEntries",
                vec![("path", codec::path(path)), ("digest", digest.to_json())],
            ),
            Self::Missing { path } => ("Missing", vec![("path", codec::path(path))]),
            Self::Link { path, target } => (
                "Link",
                vec![("path", codec::path(path)), ("target", codec::path(target))],
            ),
            Self::Canonical { path, target } => (
                "Canonical",
                vec![("path", codec::path(path)), ("target", codec::path(target))],
            ),
            Self::Metadata { path, follow, stat } => (
                "Metadata",
                vec![
                    ("path", codec::path(path)),
                    ("follow", follow.to_json()),
                    ("stat", stat.to_json()),
                ],
            ),
            Self::EnvVar { name, digest } => (
                "EnvVar",
                vec![("name", codec::bytes(name)), ("digest", digest.to_json())],
            ),
            Self::NegativeLookup {
                root,
                relative,
                deepest,
                listing,
            } => (
                "NegativeLookup",
                vec![
                    ("root", codec::path(root)),
                    ("relative", codec::path(relative)),
                    ("deepest", codec::path(deepest)),
                    ("listing", listing.to_json()),
                ],
            ),
        };
        variant(name, Value::object_from(members))
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

impl ToJson for VariableScope {
    fn to_json(&self) -> Value {
        Value::string(match self {
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

impl ToJson for TargetKey {
    fn to_json(&self) -> Value {
        Value::object_from([
            ("binary_dir", codec::path(&self.binary_dir)),
            ("name", codec::bytes(&self.name)),
        ])
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

impl ToJson for PropertyOwner {
    fn to_json(&self) -> Value {
        match self {
            Self::Global => Value::string("Global"),
            Self::Directory(path) => variant("Directory", codec::path(path)),
            Self::Target(target) => variant("Target", target.to_json()),
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

impl ToJson for PropertyKey {
    fn to_json(&self) -> Value {
        Value::object_from([
            ("owner", self.owner.to_json()),
            ("name", codec::bytes(&self.name)),
        ])
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

impl ToJson for StateKey {
    fn to_json(&self) -> Value {
        match self {
            Self::Variable { scope, name } => variant(
                "Variable",
                Value::object_from([("scope", scope.to_json()), ("name", codec::bytes(name))]),
            ),
            Self::EffectiveVariable(name) => variant("EffectiveVariable", codec::bytes(name)),
            Self::Cache(name) => variant("Cache", codec::bytes(name)),
            Self::Property(key) => variant("Property", key.to_json()),
            Self::Policy(id) => variant("Policy", id.to_json()),
            Self::Target(target) => variant("Target", target.to_json()),
            Self::Command(name) => variant("Command", codec::bytes(name)),
            Self::Probe(key) => variant("Probe", key.to_json()),
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

impl ToJson for JournalInput {
    fn to_json(&self) -> Value {
        match self {
            Self::Host(stamp) => variant("Host", stamp.to_json()),
            Self::State { key, value } => variant(
                "State",
                Value::object_from([("key", key.to_json()), ("value", value.to_json())]),
            ),
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
