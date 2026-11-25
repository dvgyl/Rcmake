//! CMake 4.5.1 policy registry (`CMP0000`–`CMP0226`) and compact policy state map.

use core::fmt;

use rcmake_base::version::Version;
use thiserror::Error;

use crate::generated_policies::{MAX_POLICY_ID, MAX_REMOVED_OLD_POLICY_ID, POLICIES};
use crate::policy_warnings::WARNING_SUMMARIES;

/// Identifier for a CMake policy (`CMP0000` through `CMP0226`).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct PolicyId(u16);

impl PolicyId {
    /// Construct a `PolicyId` from its numeric value.
    #[must_use]
    pub const fn new(num: u16) -> Self {
        Self(num)
    }

    /// Numeric value (e.g., `219` for `CMP0219`).
    #[must_use]
    pub const fn number(self) -> u16 {
        self.0
    }

    /// Parse a policy name such as `b"CMP0219"` if it is known to CMake 4.5.1.
    #[must_use]
    pub fn parse(name: &[u8]) -> Option<Self> {
        if name.len() != 7 || !name.starts_with(b"CMP") {
            return None;
        }
        let digits = core::str::from_utf8(name.get(3..7)?).ok()?;
        if !digits.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        let num: u16 = digits.parse().ok()?;
        if num <= MAX_POLICY_ID {
            Some(Self(num))
        } else {
            None
        }
    }

    /// Look up metadata for this policy in the CMake 4.5.1 table.
    #[must_use]
    pub fn info(self) -> Option<&'static PolicyInfo> {
        POLICIES.get(usize::from(self.0))
    }

    /// Summary text that CMake 4.5.1 prints in policy warnings. This is the
    /// `cmPolicies.h` description, which differs from the help title
    /// ([`PolicyInfo::summary`]) for many policies.
    #[must_use]
    pub fn warning_summary(self) -> Option<&'static str> {
        WARNING_SUMMARIES
            .binary_search_by_key(&self.0, |&(number, _)| number)
            .ok()
            .and_then(|index| WARNING_SUMMARIES.get(index))
            .map(|&(_, summary)| summary)
    }

    /// Full `Policy CMPxxxx is not set: ...` text (`cmPolicies::GetPolicyWarning`).
    #[must_use]
    pub fn warning_text(self) -> String {
        let summary = self
            .warning_summary()
            .map(str::to_owned)
            .or_else(|| self.info().map(|info| info.summary.replace("``", "")))
            .unwrap_or_default();
        format!(
            "Policy {self} is not set: {summary}  Run \"cmake --help-policy {self}\" for policy details.  Use the cmake_policy command to set the policy and suppress this warning."
        )
    }
}

impl fmt::Display for PolicyId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "CMP{:04}", self.0)
    }
}

/// Static metadata for one CMake 4.5.1 policy.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PolicyInfo {
    /// Policy identifier.
    pub id: PolicyId,
    /// CMake `(major, minor, patch)` version that introduced this policy.
    pub introduced: (u16, u16, u16),
    /// `true` for `CMP0000`–`CMP0065` whose `OLD` behaviors were removed in CMake 4.0+.
    pub removed_old: bool,
    /// One-line summary from CMake 4.5.1's policy manual.
    pub summary: &'static str,
}

/// Active status of a single policy in a scope.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PolicyStatus {
    /// Policy has not been explicitly set (`WARN`).
    Warn,
    /// Policy explicitly set to `OLD`.
    Old,
    /// Policy explicitly set to `NEW`.
    New,
}

/// Errors produced when querying or updating a [`PolicyMap`].
#[derive(Clone, Debug, PartialEq, Eq, Error)]
pub enum PolicyError {
    /// Policy ID is outside `CMP0000`..=`CMP0226`.
    #[error("Policy \"{0}\" is not known to this version of CMake.")]
    UnknownPolicy(String),
    /// Attempted to set `CMP0000`–`CMP0065` to `OLD` (removed in CMake 4.0+).
    #[error("The OLD behavior for policy {0} will be removed from a future version of CMake.")]
    RemovedOldBehavior(PolicyId),
    /// Effective policy version is below `3.5`.
    #[error("Compatibility with CMake < 3.5 has been removed from CMake.")]
    VersionBelowMinimum,
}

/// Compact bitset storing `Warn`/`Old`/`New` for up to 512 policy IDs.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PolicyMap {
    is_set: [u64; 8],
    is_new: [u64; 8],
}

impl PolicyMap {
    /// Query the status of `id`.
    #[must_use]
    pub fn get(&self, id: PolicyId) -> PolicyStatus {
        let Some((word, mask)) = bit_pos(id) else {
            return PolicyStatus::Warn;
        };
        let set = self.is_set.get(word).copied().unwrap_or(0) & mask != 0;
        if !set {
            return PolicyStatus::Warn;
        }
        let new = self.is_new.get(word).copied().unwrap_or(0) & mask != 0;
        if new {
            PolicyStatus::New
        } else {
            PolicyStatus::Old
        }
    }

    /// Explicitly set `id` to `status`. Rejects setting `CMP0000`–`CMP0065` to `Old`.
    pub fn set(&mut self, id: PolicyId, status: PolicyStatus) -> Result<(), PolicyError> {
        if id.number() > MAX_POLICY_ID {
            return Err(PolicyError::UnknownPolicy(id.to_string()));
        }
        if status == PolicyStatus::Old && id.number() <= MAX_REMOVED_OLD_POLICY_ID {
            return Err(PolicyError::RemovedOldBehavior(id));
        }
        let Some((word, mask)) = bit_pos(id) else {
            return Err(PolicyError::UnknownPolicy(id.to_string()));
        };
        if let (Some(set_slot), Some(new_slot)) =
            (self.is_set.get_mut(word), self.is_new.get_mut(word))
        {
            match status {
                PolicyStatus::Warn => {
                    *set_slot &= !mask;
                    *new_slot &= !mask;
                }
                PolicyStatus::Old => {
                    *set_slot |= mask;
                    *new_slot &= !mask;
                }
                PolicyStatus::New => {
                    *set_slot |= mask;
                    *new_slot |= mask;
                }
            }
        }
        Ok(())
    }

    /// Apply `cmake_policy(VERSION <v>)`: sets all policies introduced at or before `v`
    /// to `NEW` and resets newer policies to `Warn`. Rejects versions below `3.5`.
    pub fn apply_version(&mut self, version: &Version) -> Result<(), PolicyError> {
        let min_supported = Version::from_tuple(3, 5, 0);
        if version < &min_supported {
            return Err(PolicyError::VersionBelowMinimum);
        }
        *self = Self::default();
        for info in POLICIES {
            let intro =
                Version::from_tuple(info.introduced.0, info.introduced.1, info.introduced.2);
            if &intro <= version {
                self.set(info.id, PolicyStatus::New)?;
            }
        }
        Ok(())
    }
}

fn bit_pos(id: PolicyId) -> Option<(usize, u64)> {
    let idx = usize::from(id.number());
    let word = idx / 64;
    let bit = idx % 64;
    if word < 8 {
        Some((word, 1u64 << bit))
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn warning_text_uses_runtime_summary() {
        assert_eq!(
            PolicyId::new(76).warning_text(),
            "Policy CMP0076 is not set: target_sources() command converts relative paths to absolute.  Run \"cmake --help-policy CMP0076\" for policy details.  Use the cmake_policy command to set the policy and suppress this warning."
        );
        assert_eq!(
            PolicyId::new(211).warning_summary(),
            Some("A file may belong to at most one non-HEADERS file set in a target.")
        );
        assert!(
            WARNING_SUMMARIES
                .windows(2)
                .all(|w| matches!(w, [a, b] if a.0 < b.0))
        );
        assert_eq!(WARNING_SUMMARIES.len(), usize::from(MAX_POLICY_ID) + 1);
    }

    #[test]
    fn policy_range_and_removed_old() {
        assert_eq!(PolicyId::parse(b"CMP0226"), Some(PolicyId::new(226)));
        assert_eq!(PolicyId::parse(b"CMP0227"), None);

        let mut map = PolicyMap::default();
        assert!(map.set(PolicyId::new(54), PolicyStatus::Old).is_err());
        assert!(map.set(PolicyId::new(66), PolicyStatus::Old).is_ok());
        assert_eq!(map.get(PolicyId::new(66)), PolicyStatus::Old);
    }

    #[test]
    fn apply_version_enforces_3_5_floor() {
        let mut map = PolicyMap::default();
        assert_eq!(
            map.apply_version(&Version::from_tuple(3, 4, 0)),
            Err(PolicyError::VersionBelowMinimum)
        );
        map.apply_version(&Version::from_tuple(4, 5, 1)).unwrap();
        assert_eq!(map.get(PolicyId::new(219)), PolicyStatus::New);
        assert_eq!(map.get(PolicyId::new(226)), PolicyStatus::New);
    }
}
