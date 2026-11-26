//! Clean-room CMake 4.5.1 byte regular expression engine and glob-to-regex
//! converter (ADR-005).
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

use rcmake_base::{BString, Limits};
use thiserror::Error;

pub mod glob;
mod parse;
mod program;

#[cfg(test)]
mod tests;

/// Maximum number of capture slots (`0` = full match, `1..=31` = capturing groups).
pub const MAX_CAPTURE_SLOTS: usize = 32;

/// Number of `CMAKE_MATCH_<n>` variables exposed by CMake (`0..=9`).
pub const MATCH_VAR_SLOTS: usize = 10;

/// Error from compiling a pattern or expanding a replacement.
#[derive(Clone, Debug, PartialEq, Eq, Error)]
#[error("{message}")]
pub struct RegexError {
    /// CMake 4.5.1-compatible diagnostic text.
    pub message: String,
}

/// Error returned when matching exceeds the configured step budget.
#[derive(Clone, Debug, PartialEq, Eq, Error)]
#[error("Regular expression matching exceeded step limit")]
pub struct RegexLimit;

/// Matching options controlling `^` anchoring and zero-length match acceptance.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MatchOptions {
    /// Use `start` as the beginning of the input for `^` (`CMP0186` OLD/unset).
    /// Set to `false` for `CMP0186` NEW, which uses byte offset `0`.
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

/// Reusable storage for backtracking and capture changes during a regex search.
#[derive(Default)]
pub struct Scratch(program::Scratch);

/// Compiled CMake 4.5.1 regular expression.
#[derive(Clone, Debug)]
pub struct Regex {
    group_count: usize,
    program: Arc<[program::Instruction]>,
}

impl Regex {
    /// Compile a CMake regular expression pattern.
    pub fn new(pattern: &[u8]) -> Result<Self, RegexError> {
        let (expression, group_count) = parse::parse(pattern)?;
        Ok(Self {
            group_count,
            program: program::compile(expression).into(),
        })
    }

    /// Number of capturing parentheses in this compiled pattern (`0..=31`).
    #[must_use]
    pub const fn group_count(&self) -> usize {
        self.group_count
    }

    /// Search `hay` from byte offset `start`. Subtract each match step from `budget`.
    ///
    /// Return [`RegexLimit`] if the search needs a step after the budget reaches zero.
    pub fn find_at(
        &self,
        hay: &[u8],
        start: usize,
        options: MatchOptions,
        budget: &mut u64,
    ) -> Result<Option<Captures>, RegexLimit> {
        self.find_at_with_scratch(hay, start, options, budget, &mut Scratch::default())
    }

    /// Search like [`Self::find_at`] and reuse the storage in `scratch`.
    pub fn find_at_with_scratch(
        &self,
        hay: &[u8],
        start: usize,
        options: MatchOptions,
        budget: &mut u64,
        scratch: &mut Scratch,
    ) -> Result<Option<Captures>, RegexLimit> {
        program::find(&self.program, hay, start, options, budget, &mut scratch.0)
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

/// Expand a CMake replacement template for a successful match.
///
/// Use `\0` for the full match, `\1` through `\9` for capture groups, `\\` for a
/// backslash, and `\n` for a newline. A group that did not participate gives an
/// empty string. Return an error for an invalid escape, an undefined group, or a
/// result larger than the default value size limit.
pub fn expand_replacement(
    template: &[u8],
    hay: &[u8],
    caps: &Captures,
    group_count: usize,
) -> Result<BString, RegexError> {
    let mut out = Vec::new();
    expand_parts(
        template,
        replacement_parts(template),
        hay,
        caps,
        group_count,
        &mut out,
    )?;
    Ok(BString::from(out))
}

#[derive(Clone, Copy)]
enum Part<'a> {
    Literal(&'a [u8]),
    Group(usize),
}

fn replacement_parts(mut remaining: &[u8]) -> impl Iterator<Item = Result<Part<'_>, RegexError>> {
    std::iter::from_fn(move || {
        if remaining.is_empty() {
            return None;
        }
        let literal_end = remaining
            .iter()
            .position(|&byte| byte == b'\\')
            .unwrap_or(remaining.len());
        if literal_end != 0 {
            let (literal, tail) = remaining.split_at(literal_end);
            remaining = tail;
            return Some(Ok(Part::Literal(literal)));
        }
        let [_, escape, rest @ ..] = remaining else {
            remaining = b"";
            return Some(Err(replacement_error(format_args!(
                "replace-expression ends in a backslash."
            ))));
        };
        remaining = rest;
        Some(match escape {
            b'0'..=b'9' => Ok(Part::Group(usize::from(*escape - b'0'))),
            b'\\' => Ok(Part::Literal(b"\\")),
            b'n' => Ok(Part::Literal(b"\n")),
            escape => Err(replacement_error(format_args!(
                "Unknown escape \"\\{}\" in replace-expression.",
                String::from_utf8_lossy(std::slice::from_ref(escape))
            ))),
        })
    })
}

/// A parsed CMake regex replacement template. Its literals borrow the template.
pub struct Replacement<'a> {
    template: &'a [u8],
    parts: Vec<Part<'a>>,
}

impl<'a> Replacement<'a> {
    /// Parse literals and escapes. Check capture group numbers during expansion.
    pub fn parse(template: &'a [u8]) -> Result<Self, RegexError> {
        let mut parts = Vec::new();
        let mut literal_size = 0;
        for part in replacement_parts(template) {
            let part = part?;
            if let Part::Literal(bytes) = part {
                literal_size += bytes.len();
                if literal_size > Limits::default().max_value_bytes {
                    return Err(replacement_size_error());
                }
            }
            parts.push(part);
        }
        Ok(Self { template, parts })
    }

    /// Clear `out` and write the replacement for a match. Return an error if a
    /// group number is invalid or the result exceeds the default value limit.
    pub fn expand_into(
        &self,
        hay: &[u8],
        caps: &Captures,
        group_count: usize,
        out: &mut Vec<u8>,
    ) -> Result<(), RegexError> {
        out.clear();
        expand_parts(
            self.template,
            self.parts.iter().copied().map(Ok),
            hay,
            caps,
            group_count,
            out,
        )
    }
}

fn expand_parts<'a>(
    template: &[u8],
    parts: impl Iterator<Item = Result<Part<'a>, RegexError>>,
    hay: &[u8],
    caps: &Captures,
    group_count: usize,
    out: &mut Vec<u8>,
) -> Result<(), RegexError> {
    let limit = Limits::default().max_value_bytes;
    for part in parts {
        let bytes = match part? {
            Part::Literal(bytes) => bytes,
            Part::Group(group) => {
                if group > group_count {
                    return Err(replacement_error(format_args!(
                        "replace expression \"{}\" contains an out-of-range escape for group {group}.",
                        String::from_utf8_lossy(template)
                    )));
                }
                caps.groups
                    .get(group)
                    .and_then(|&span| span)
                    .and_then(|(start, end)| hay.get(start..end))
                    .unwrap_or_default()
            }
        };
        if bytes.len() > limit - out.len() {
            return Err(replacement_size_error());
        }
        out.extend_from_slice(bytes);
    }
    Ok(())
}

fn replacement_size_error() -> RegexError {
    replacement_error(format_args!("Replacement exceeds the value size limit."))
}

fn replacement_error(detail: std::fmt::Arguments<'_>) -> RegexError {
    RegexError {
        message: format!("string sub-command REGEX, mode REPLACE: {detail}"),
    }
}
