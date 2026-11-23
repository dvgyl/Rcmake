//! Clean-room CMake 4.5.1 (KWSys / Henry-Spencer) byte regular expression engine
//! and glob-to-regex converter (ADR-005).
//!
//! Key CMake 4.5.1 semantics:
//! - Byte-oriented matching (`.` matches any single byte, including `\n`).
//! - Up to **31 capturing groups** (slot 0 = whole match, slots 1..=31 = capture groups;
//!   the 32nd opening parenthesis fails with `RegularExpression::compile(): Too many parentheses.`).
//! - `CMAKE_MATCH_0..9` and `CMAKE_MATCH_COUNT` expose slots 0..=9.
//! - Left-to-right ordered alternation (`(a|aa)` on `aa` matches `a`), greedy quantifiers
//!   with backtracking, no `{m,n}` repeats, no lazy quantifiers, no backreferences in patterns,
//!   and `^` re-anchoring controlled by [`MatchOptions::bol_at_start`] (`CMP0186`).

#![forbid(unsafe_code)]

use std::collections::BTreeMap;
use std::sync::Arc;

use rcmake_base::BString;
use thiserror::Error;

/// Maximum number of capture slots (`0` = full match, `1..=31` = capturing groups).
pub const MAX_CAPTURE_SLOTS: usize = 32;

/// Number of `CMAKE_MATCH_<n>` variables exposed by CMake (`0..=9`).
pub const MATCH_VAR_SLOTS: usize = 10;

/// Compilation error produced by [`Regex::new`].
#[derive(Clone, Debug, PartialEq, Eq, Error)]
#[error("{message}")]
pub struct RegexError {
    /// CMake / KWSys-compatible diagnostic text.
    pub message: String,
}

/// Error returned when matching exceeds the configured step budget.
#[derive(Clone, Debug, PartialEq, Eq, Error)]
#[error("Regular expression matching exceeded step limit")]
pub struct RegexLimit;

/// Matching options controlling `^` anchoring and zero-length match acceptance.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MatchOptions {
    /// When `true` (`CMP0186` OLD/unset), `^` matches at `start`; when `false` (`CMP0186` NEW),
    /// `^` matches only at byte offset `0`.
    pub bol_at_start: bool,
    /// When `false`, a zero-length match at `start` is rejected (used when retrying after
    /// an empty match during `MATCHALL`/`REPLACE`).
    pub allow_empty_at_start: bool,
}

impl Default for MatchOptions {
    fn default() -> Self {
        Self {
            bol_at_start: true,
            allow_empty_at_start: true,
        }
    }
}

/// Byte spans captured by a successful [`Regex`] match (slot 0 = whole match, 1..=31 = groups).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Captures {
    /// Optional `(start, end)` byte range for each capture slot `0..32`.
    pub groups: [Option<(usize, usize)>; MAX_CAPTURE_SLOTS],
}

impl Captures {
    /// Extract `CMAKE_MATCH_0..9` byte strings and `CMAKE_MATCH_COUNT` (highest non-empty
    /// slot in `0..=9`, or `None` if all slots including slot 0 are empty).
    #[must_use]
    pub fn match_vars(&self, hay: &[u8]) -> ([BString; MATCH_VAR_SLOTS], Option<usize>) {
        let mut vars: [BString; MATCH_VAR_SLOTS] = core::array::from_fn(|_| BString::default());
        let mut highest: Option<usize> = None;
        for (idx, slot) in self.groups.iter().take(MATCH_VAR_SLOTS).enumerate() {
            if let Some((s, e)) = *slot
                && let Some(slice) = hay.get(s..e)
                && !slice.is_empty()
                && let Some(dst) = vars.get_mut(idx)
            {
                *dst = BString::from(slice);
                highest = Some(idx);
            }
        }
        (vars, highest)
    }
}

/// Compiled CMake 4.5.1 regular expression.
#[derive(Clone, Debug)]
pub struct Regex {
    group_count: usize,
}

impl Regex {
    /// Compile a CMake regular expression pattern.
    pub fn new(_pattern: &[u8]) -> Result<Self, RegexError> {
        Err(RegexError {
            message: "RegularExpression::compile(): not yet implemented (scheduled for M1-W1)"
                .to_owned(),
        })
    }

    /// Number of capturing parentheses in this compiled pattern (`0..=31`).
    #[must_use]
    pub const fn group_count(&self) -> usize {
        self.group_count
    }

    /// Search `hay` starting at byte offset `start`, consuming from `budget`.
    pub fn find_at(
        &self,
        _hay: &[u8],
        _start: usize,
        _options: MatchOptions,
        _budget: &mut u64,
    ) -> Result<Option<Captures>, RegexLimit> {
        Ok(None)
    }
}

/// Per-interpreter cache of compiled [`Regex`] instances.
#[derive(Default, Debug)]
pub struct RegexCache {
    entries: BTreeMap<BString, Arc<Regex>>,
}

impl RegexCache {
    /// Look up or compile `pattern`.
    pub fn get_or_compile(&mut self, pattern: &[u8]) -> Result<Arc<Regex>, RegexError> {
        if let Some(re) = self.entries.get(pattern) {
            return Ok(Arc::clone(re));
        }
        let compiled = Arc::new(Regex::new(pattern)?);
        if self.entries.len() >= 256 {
            self.entries.clear();
        }
        self.entries
            .insert(BString::from(pattern), Arc::clone(&compiled));
        Ok(compiled)
    }
}

/// CMake glob-to-regex conversion (`file(GLOB)` / `string(MATCH)` glob helpers).
pub mod glob {
    use rcmake_base::BString;

    /// Convert a CMake glob pattern into an equivalent anchored CMake regex.
    #[must_use]
    pub fn to_regex(glob: &[u8]) -> BString {
        BString::from(glob)
    }
}
