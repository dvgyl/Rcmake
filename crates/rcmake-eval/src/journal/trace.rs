use std::path::PathBuf;

use rcmake_diag::PolicyStatus;
use rcmake_host::Host;
use rcmake_store::{Digest, InputStamp, VerifyOutcome, verify_trace};
use serde::{Deserialize, Serialize};

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
    Probe(Digest),
}

/// Result of checking an input against the current entry state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StateLookup {
    /// The key has a value, or is known to be absent.
    Observed(Option<Digest>),
    /// The key cannot be checked, for example when the probe store is disabled.
    Unavailable,
}

/// One entry fact that must match before replay.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum JournalInput {
    /// A file, directory, missing path, or environment variable observed through the host.
    Host(InputStamp),
    /// First read of shared evaluator state before a write to that key.
    State {
        /// State to check.
        key: StateKey,
        /// Digest of the observed value, or `None` if it was absent.
        value: Option<Digest>,
    },
}

impl JournalInput {
    pub(super) fn matches(
        &self,
        host: &dyn Host,
        lookup: &mut impl FnMut(&StateKey) -> StateLookup,
    ) -> bool {
        match self {
            Self::Host(stamp) => {
                valid_host_paths(stamp)
                    && verify_trace(host, std::slice::from_ref(stamp)) == VerifyOutcome::Hit
            }
            Self::State { key, value } => lookup(key) == StateLookup::Observed(*value),
        }
    }
}

fn valid_host_paths(stamp: &InputStamp) -> bool {
    match stamp {
        InputStamp::FileStat { path, .. }
        | InputStamp::FileHash { path, .. }
        | InputStamp::DirEntries { path, .. }
        | InputStamp::Missing { path } => path.is_absolute(),
        InputStamp::EnvVar { .. } => true,
        InputStamp::NegativeLookup { root, deepest, .. } => {
            root.is_absolute() && deepest.is_absolute()
        }
    }
}

/// Encode the active status of a CMake policy for a trace entry.
#[must_use]
pub fn policy_digest(status: PolicyStatus) -> Digest {
    Digest::hash(match status {
        PolicyStatus::Warn => b"WARN".as_slice(),
        PolicyStatus::Old => b"OLD".as_slice(),
        PolicyStatus::New => b"NEW".as_slice(),
    })
}
