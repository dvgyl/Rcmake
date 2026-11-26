//! Executable command sequences with precomputed block structure.

use std::collections::HashMap;
#[cfg(feature = "replay")]
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};

use indexmap::IndexMap;
use rcmake_base::Limits;
use rcmake_diag::{Diagnostic, SourceId, source::SourceKind};
use rcmake_syntax::{CommandInvocation, ParseError};

#[cfg(feature = "replay")]
use crate::digest::JournalDigest;
use crate::util::FxBuildHasher;

#[derive(Clone)]
pub(crate) struct ParsedSource {
    pub(crate) code: Result<Code, ParseError>,
    pub(crate) warnings: Arc<[Diagnostic]>,
}

impl ParsedSource {
    pub(crate) fn parse(text: &[u8], id: SourceId, kind: SourceKind, limits: &Limits) -> Self {
        match rcmake_syntax::parse_list_file_with_warnings(text, id, kind, limits) {
            Ok(list) => Self {
                code: Ok(Code::new(list.commands, Arc::from([]))),
                warnings: list.warnings.into(),
            },
            Err((error, warnings)) => Self {
                code: Err(error),
                warnings: warnings.into(),
            },
        }
    }
}

const MAX_ENTRIES: usize = 2048;
const MAX_TEXT_BYTES: usize = 32 * 1024 * 1024;
pub(crate) const MAX_CACHE_FILE_BYTES: usize = 4 * 1024 * 1024;

/// Cached source bytes with a lazily computed SHA-256 digest for configure replay.
#[derive(Clone)]
pub(crate) struct SourceText {
    pub(crate) bytes: Arc<[u8]>,
    #[cfg(feature = "replay")]
    digest: Arc<OnceLock<JournalDigest>>,
}

impl SourceText {
    pub(crate) fn new(bytes: Arc<[u8]>) -> Self {
        Self {
            bytes,
            #[cfg(feature = "replay")]
            digest: Arc::new(OnceLock::new()),
        }
    }

    #[cfg(feature = "replay")]
    pub(crate) fn digest(&self) -> JournalDigest {
        *self.digest.get_or_init(|| JournalDigest::hash(&self.bytes))
    }
}

#[derive(Clone, Copy, Hash, PartialEq, Eq)]
pub(crate) enum LimitsKey {
    File(u64),
    Eval(usize),
}

impl LimitsKey {
    pub(crate) fn new(kind: SourceKind, limits: &Limits) -> Self {
        match kind {
            SourceKind::File => Self::File(limits.max_file_read),
            SourceKind::VirtualEval => Self::Eval(limits.max_value_bytes),
        }
    }
}

/// Fast 128-bit content hash for in-memory AST cache lookup. Every hit verifies exact
/// byte equality against the cached `Arc<[u8]>`, so collisions only fall back to parsing.
fn fast_content_hash(mut data: &[u8]) -> (u64, u64) {
    const K1: u64 = 0x517c_c1b7_2722_0a95;
    const K2: u64 = 0x9e37_79b9_7f4a_7c15;
    let len = data.len() as u64;
    let mut h1 = len.wrapping_mul(K1);
    let mut h2 = len.rotate_left(17).wrapping_mul(K2);
    while let Some((chunk, rest)) = data.split_first_chunk::<16>() {
        let [
            b0,
            b1,
            b2,
            b3,
            b4,
            b5,
            b6,
            b7,
            b8,
            b9,
            b10,
            b11,
            b12,
            b13,
            b14,
            b15,
        ] = *chunk;
        let a = u64::from_le_bytes([b0, b1, b2, b3, b4, b5, b6, b7]);
        let b = u64::from_le_bytes([b8, b9, b10, b11, b12, b13, b14, b15]);
        h1 = (h1.rotate_left(5) ^ a).wrapping_mul(K1);
        h2 = (h2.rotate_left(7) ^ b).wrapping_mul(K2);
        data = rest;
    }
    if let Some((chunk, rest)) = data.split_first_chunk::<8>() {
        let a = u64::from_le_bytes(*chunk);
        h1 = (h1.rotate_left(5) ^ a).wrapping_mul(K1);
        data = rest;
    }
    for &byte in data {
        h2 = (h2.rotate_left(7) ^ u64::from(byte)).wrapping_mul(K2);
    }
    h1 ^= h2.rotate_right(29);
    h2 ^= h1.rotate_right(31);
    (h1, h2)
}

pub(crate) type ParseKey = ((u64, u64, usize), LimitsKey);

/// A bounded cache of parsed listfiles. Each result is independent of its path.
#[derive(Default)]
pub struct SharedParseCache {
    table: RwLock<ParseTable>,
    clock: AtomicU64,
}

#[derive(Default)]
struct ParseTable {
    entries: IndexMap<ParseKey, ParseEntry, FxBuildHasher>,
    bytes: usize,
}

struct ParseEntry {
    text: SourceText,
    parsed: ParsedSource,
    last_use: AtomicU64,
}

impl SharedParseCache {
    pub(crate) fn key(data: &[u8], limits: LimitsKey) -> ParseKey {
        let (h1, h2) = fast_content_hash(data);
        ((h1, h2, data.len()), limits)
    }

    pub(crate) fn get(&self, key: ParseKey, data: &[u8]) -> Option<(SourceText, ParsedSource)> {
        let table = self.table.read().ok()?;
        let entry = table.entries.get(&key)?;
        if !std::ptr::eq(entry.text.bytes.as_ref(), data) && entry.text.bytes.as_ref() != data {
            return None;
        }
        let tick = self.clock.fetch_add(1, Ordering::Relaxed).wrapping_add(1);
        entry.last_use.fetch_max(tick, Ordering::Relaxed);
        Some((entry.text.clone(), entry.parsed.clone()))
    }

    pub(crate) fn insert(&self, key: ParseKey, text: SourceText, parsed: ParsedSource) {
        let size = text.bytes.len();
        if parsed.warnings.len() > 1024 || size > MAX_TEXT_BYTES {
            return;
        }
        let Ok(mut table) = self.table.write() else {
            return;
        };
        if table.entries.contains_key(&key) {
            return;
        }
        while table.entries.len() >= MAX_ENTRIES || table.bytes + size > MAX_TEXT_BYTES {
            let Some(oldest) = table
                .entries
                .values()
                .enumerate()
                .min_by_key(|(_, entry)| entry.last_use.load(Ordering::Relaxed))
                .map(|(index, _)| index)
            else {
                return;
            };
            if let Some((_, entry)) = table.entries.swap_remove_index(oldest) {
                table.bytes = table.bytes.saturating_sub(entry.text.bytes.len());
            }
        }
        let tick = self.clock.fetch_add(1, Ordering::Relaxed).wrapping_add(1);
        table.entries.insert(
            key,
            ParseEntry {
                text,
                parsed,
                last_use: AtomicU64::new(tick),
            },
        );
        table.bytes += size;
    }
}

/// A flat command sequence from one listfile (or a function/macro body) together with
/// precomputed block ends, `if`/`elseif`/`else` links, and interned command names.
#[derive(Clone, Debug)]
pub(crate) struct Code {
    /// Commands in source order.
    pub(crate) cmds: Arc<[CommandInvocation]>,
    /// Pre-interned command names (preserving original case) for zero-alloc backtrace frames.
    names: Arc<[Arc<[u8]>]>,
    /// For each block opener at index `i`, `ends[i]` is the index of its end command.
    /// For other commands, `ends[i] == i`.
    ends: Arc<[usize]>,
    /// For each `if` or `elseif` at index `i`, the index of the next matching `elseif`,
    /// `else`, or `endif` of the same block. For `else` at `i`, the index of `endif`.
    next_branch: Arc<[usize]>,
    /// Absolute path of the listfile that contains these commands.
    pub(crate) file: Arc<[u8]>,
}

/// Block opener kinds handled directly by the executor.
pub(crate) fn opener_end(lower: &[u8]) -> Option<&'static [u8]> {
    match lower {
        b"if" => Some(b"endif"),
        b"foreach" => Some(b"endforeach"),
        b"while" => Some(b"endwhile"),
        b"function" => Some(b"endfunction"),
        b"macro" => Some(b"endmacro"),
        b"block" => Some(b"endblock"),
        _ => None,
    }
}

impl Code {
    /// Build a [`Code`] and compute its block structure.
    ///
    /// The parser validates nesting before execution. If the nesting is not valid, an
    /// unmatched opener extends to the end of the sequence.
    pub(crate) fn new(cmds: Arc<[CommandInvocation]>, file: Arc<[u8]>) -> Self {
        let len = cmds.len();
        let mut ends: Vec<usize> = (0..len).collect();
        let mut next_branch: Vec<usize> = (0..len).collect();
        let mut stack: Vec<(usize, usize)> = Vec::new();
        let mut interned: HashMap<&[u8], Arc<[u8]>, FxBuildHasher> =
            HashMap::with_hasher(FxBuildHasher);
        let mut names = Vec::with_capacity(len);
        for (idx, cmd) in cmds.iter().enumerate() {
            let name_arc = if let Some(existing) = interned.get(cmd.name.as_slice()) {
                Arc::clone(existing)
            } else {
                let arc: Arc<[u8]> = Arc::from(cmd.name.as_slice());
                interned.insert(cmd.name.as_slice(), Arc::clone(&arc));
                arc
            };
            names.push(name_arc);
            let lower = cmd.lower.as_slice();
            if opener_end(lower).is_some() {
                stack.push((idx, idx));
            } else if matches!(lower, b"elseif" | b"else") {
                if let Some(&mut (open, ref mut prev)) = stack.last_mut()
                    && cmds.get(open).is_some_and(|c| c.lower.as_slice() == b"if")
                {
                    if let Some(slot) = next_branch.get_mut(*prev) {
                        *slot = idx;
                    }
                    *prev = idx;
                }
            } else if lower.starts_with(b"end") {
                let matches = stack.last().is_some_and(|&(open, _)| {
                    cmds.get(open)
                        .and_then(|c| opener_end(c.lower.as_slice()))
                        .is_some_and(|end| end == lower)
                });
                if matches && let Some((open, prev)) = stack.pop() {
                    if let Some(slot) = ends.get_mut(open) {
                        *slot = idx;
                    }
                    if cmds.get(open).is_some_and(|c| c.lower.as_slice() == b"if")
                        && let Some(slot) = next_branch.get_mut(prev)
                    {
                        *slot = idx;
                    }
                }
            }
        }
        for (open, prev) in stack {
            if let Some(slot) = ends.get_mut(open) {
                *slot = len;
            }
            if cmds.get(open).is_some_and(|c| c.lower.as_slice() == b"if")
                && let Some(slot) = next_branch.get_mut(prev)
            {
                *slot = len;
            }
        }
        Self {
            cmds,
            names: names.into(),
            ends: ends.into(),
            next_branch: next_branch.into(),
            file,
        }
    }

    /// Number of commands.
    pub(crate) fn len(&self) -> usize {
        self.cmds.len()
    }

    /// Pre-interned command name at `idx`.
    pub(crate) fn name_arc(&self, idx: usize) -> Arc<[u8]> {
        self.names
            .get(idx)
            .map_or_else(|| Arc::from(&b""[..]), Arc::clone)
    }

    /// Index of the end command that matches the opener at `idx` (or `idx` itself).
    pub(crate) fn end_of(&self, idx: usize) -> usize {
        self.ends.get(idx).copied().unwrap_or(idx)
    }

    /// Index of the next `elseif`, `else`, or `endif` after the `if`/`elseif`/`else` at `idx`.
    pub(crate) fn next_branch(&self, idx: usize) -> usize {
        self.next_branch.get(idx).copied().unwrap_or(idx)
    }

    /// Copy the commands in `start..end` into a new [`Code`] (function and macro bodies).
    pub(crate) fn slice(&self, start: usize, end: usize) -> Self {
        let cmds: Vec<CommandInvocation> = self
            .cmds
            .get(start..end.min(self.cmds.len()))
            .map(<[CommandInvocation]>::to_vec)
            .unwrap_or_default();
        Self::new(cmds.into(), Arc::clone(&self.file))
    }

    /// Replace the commands while keeping the same structure (macro substitution keeps
    /// command names and the command count).
    pub(crate) fn with_cmds(&self, cmds: Vec<CommandInvocation>) -> Self {
        if cmds.len() == self.cmds.len() {
            Self {
                cmds: cmds.into(),
                names: Arc::clone(&self.names),
                ends: Arc::clone(&self.ends),
                next_branch: Arc::clone(&self.next_branch),
                file: Arc::clone(&self.file),
            }
        } else {
            Self::new(cmds.into(), Arc::clone(&self.file))
        }
    }
}

#[cfg(test)]
mod parse_cache_tests {
    use rcmake_diag::SourceMap;

    use super::*;

    fn parse(
        cache: &SharedParseCache,
        data: &[u8],
        kind: SourceKind,
        limits: &Limits,
    ) -> Option<Arc<[u8]>> {
        let key = SharedParseCache::key(data, LimitsKey::new(kind, limits));
        let bytes: Arc<[u8]> = Arc::from(data);
        let id = SourceMap::default().add("fixture.cmake".into(), kind, bytes.clone())?;
        let parsed = ParsedSource::parse(data, id, kind, limits);
        cache.insert(key, SourceText::new(bytes.clone()), parsed);
        Some(bytes)
    }

    #[test]
    fn cache_reuses_text_and_checks_content_and_limits() {
        let cache = SharedParseCache::default();
        let kind = SourceKind::File;
        let limits = Limits::default();
        let source = b"set(V one)\n";
        let text = parse(&cache, source, kind, &limits).expect("source");
        let key = SharedParseCache::key(source, LimitsKey::new(kind, &limits));
        let (hit, parsed) = cache.get(key, source).expect("cache hit");
        assert!(Arc::ptr_eq(&hit.bytes, &text));
        assert!(parsed.code.is_ok());
        assert!(cache.get(key, b"set(V two)\n").is_none());
        assert!(
            cache
                .get(
                    SharedParseCache::key(b"set(V two)\n", key.1),
                    b"set(V two)\n"
                )
                .is_none()
        );

        let smaller = Limits {
            max_file_read: 1,
            ..limits.clone()
        };
        for different in [
            LimitsKey::new(kind, &smaller),
            LimitsKey::new(SourceKind::VirtualEval, &limits),
        ] {
            assert!(cache.get((key.0, different), source).is_none());
        }
    }

    #[test]
    fn cache_evicts_the_least_recently_used_entry() {
        let cache = SharedParseCache::default();
        let kind = SourceKind::File;
        let limits = Limits::default();
        let get = |text: &[u8]| {
            cache.get(
                SharedParseCache::key(text, LimitsKey::new(kind, &limits)),
                text,
            )
        };
        for value in 0..MAX_ENTRIES {
            assert!(
                parse(
                    &cache,
                    format!("set(V {value})\n").as_bytes(),
                    kind,
                    &limits
                )
                .is_some()
            );
        }
        assert!(get(b"set(V 0)\n").is_some());
        assert!(parse(&cache, b"set(V extra)\n", kind, &limits).is_some());
        assert!(get(b"set(V 0)\n").is_some());
        assert!(get(b"set(V 1)\n").is_none());
    }

    #[test]
    fn cache_collision_does_not_refresh_lru_and_fetch_max_preserves_newer_tick() {
        let cache = SharedParseCache::default();
        let kind = SourceKind::File;
        let limits = Limits::default();
        let key_of = |text: &[u8]| SharedParseCache::key(text, LimitsKey::new(kind, &limits));
        for value in 0..MAX_ENTRIES {
            assert!(
                parse(
                    &cache,
                    format!("set(V {value})\n").as_bytes(),
                    kind,
                    &limits
                )
                .is_some()
            );
        }
        let key0 = key_of(b"set(V 0)\n");
        assert!(cache.get(key0, b"set(V wrong)\n").is_none());
        assert!(parse(&cache, b"set(V extra)\n", kind, &limits).is_some());
        assert!(cache.get(key0, b"set(V 0)\n").is_none());

        let key1 = key_of(b"set(V 1)\n");
        let future_tick = cache.clock.load(Ordering::Relaxed) + 100;
        {
            let table = cache.table.read().expect("read table");
            table
                .entries
                .get(&key1)
                .expect("entry 1")
                .last_use
                .store(future_tick, Ordering::Relaxed);
        }
        assert!(cache.get(key1, b"set(V 1)\n").is_some());
        let table = cache.table.read().expect("read table");
        assert_eq!(
            table
                .entries
                .get(&key1)
                .expect("entry 1")
                .last_use
                .load(Ordering::Relaxed),
            future_tick
        );
    }

    #[test]
    fn cache_keeps_listfiles_with_more_than_4096_commands() {
        let cache = SharedParseCache::default();
        let kind = SourceKind::File;
        let limits = Limits::default();
        let source = b"set(V value)\n".repeat(4097);
        assert!(parse(&cache, &source, kind, &limits).is_some());
        let key = SharedParseCache::key(&source, LimitsKey::new(kind, &limits));
        let (_, parsed) = cache.get(key, &source).expect("cache hit");
        assert_eq!(parsed.code.expect("valid listfile").len(), 4097);
    }

    #[test]
    fn code_precomputes_if_branches_and_interns_command_names() {
        let source = b"if(A)\n  if(B)\n    set(X 1)\n  else()\n    set(X 2)\n  endif()\nelseif(C)\n  set(X 3)\nelse()\n  set(X 4)\nendif()\n";
        let limits = Limits::default();
        let id = SourceMap::default()
            .add(
                "branch.cmake".into(),
                SourceKind::File,
                Arc::from(&source[..]),
            )
            .expect("source");
        let parsed = ParsedSource::parse(source, id, SourceKind::File, &limits);
        let code = parsed.code.expect("valid code");
        assert_eq!(code.end_of(0), 10);
        assert_eq!(code.next_branch(0), 6);
        assert_eq!(code.next_branch(6), 8);
        assert_eq!(code.next_branch(8), 10);
        assert_eq!(code.end_of(1), 5);
        assert_eq!(code.next_branch(1), 3);
        assert_eq!(code.next_branch(3), 5);
        // Repeated identical spellings share the same `Arc<[u8]>`.
        assert!(Arc::ptr_eq(&code.name_arc(2), &code.name_arc(4)));
        assert!(Arc::ptr_eq(&code.name_arc(0), &code.name_arc(1)));
    }
}
