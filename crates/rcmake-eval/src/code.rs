//! Executable command sequences with precomputed block structure.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};

use rcmake_base::Limits;
use rcmake_diag::{Diagnostic, SourceId, source::SourceKind};
use rcmake_syntax::{CommandInvocation, ParseError};

use crate::journal::JournalDigest;

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

#[derive(Clone)]
pub(crate) struct SourceText {
    pub(crate) bytes: Arc<[u8]>,
    pub(crate) digest: JournalDigest,
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

type ParseKey = (JournalDigest, LimitsKey);

/// A bounded cache of parsed listfiles. Each result is independent of its path.
#[derive(Default)]
pub(crate) struct SharedParseCache(Mutex<ParseTable>);

#[derive(Default)]
struct ParseTable {
    entries: HashMap<ParseKey, ParseEntry>,
    lru: BTreeMap<u128, ParseKey>,
    bytes: usize,
    clock: u128,
}

struct ParseEntry {
    text: SourceText,
    parsed: ParsedSource,
    last_use: u128,
}

impl SharedParseCache {
    pub(crate) fn get(&self, key: ParseKey, data: &[u8]) -> Option<(SourceText, ParsedSource)> {
        let mut table = self.0.lock().ok()?;
        let ParseTable {
            entries,
            lru,
            clock,
            ..
        } = &mut *table;
        let entry = entries.get_mut(&key)?;
        if !std::ptr::eq(entry.text.bytes.as_ref(), data) && entry.text.bytes.as_ref() != data {
            return None;
        }
        lru.remove(&entry.last_use);
        *clock += 1;
        entry.last_use = *clock;
        lru.insert(entry.last_use, key);
        Some((entry.text.clone(), entry.parsed.clone()))
    }

    pub(crate) fn insert(&self, limits: LimitsKey, text: SourceText, parsed: ParsedSource) {
        if parsed.warnings.len() > 1024 {
            return;
        }
        let Ok(mut table) = self.0.lock() else {
            return;
        };
        let key = (text.digest, limits);
        if table.entries.contains_key(&key) {
            return;
        }
        let size = text.bytes.len();
        while table.entries.len() >= MAX_ENTRIES || table.bytes + size > MAX_TEXT_BYTES {
            let Some((_, oldest)) = table.lru.pop_first() else {
                return;
            };
            if let Some(entry) = table.entries.remove(&oldest) {
                table.bytes -= entry.text.bytes.len();
            }
        }
        table.clock += 1;
        let last_use = table.clock;
        table.lru.insert(last_use, key);
        table.entries.insert(
            key,
            ParseEntry {
                text,
                parsed,
                last_use,
            },
        );
        table.bytes += size;
    }
}

/// A flat command sequence from one listfile (or a function/macro body) together with
/// the index of the matching end command for every block opener.
#[derive(Clone, Debug)]
pub(crate) struct Code {
    /// Commands in source order.
    pub(crate) cmds: Arc<[CommandInvocation]>,
    /// For each block opener at index `i`, `ends[i]` is the index of its end command.
    /// For other commands, `ends[i] == i`.
    ends: Arc<[usize]>,
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
        let mut stack: Vec<usize> = Vec::new();
        for (idx, cmd) in cmds.iter().enumerate() {
            let lower = cmd.lower.as_slice();
            if opener_end(lower).is_some() {
                stack.push(idx);
            } else if lower.starts_with(b"end") {
                let matches = stack.last().is_some_and(|&open| {
                    cmds.get(open)
                        .and_then(|c| opener_end(c.lower.as_slice()))
                        .is_some_and(|end| end == lower)
                });
                if matches
                    && let Some(open) = stack.pop()
                    && let Some(slot) = ends.get_mut(open)
                {
                    *slot = idx;
                }
            }
        }
        for open in stack {
            if let Some(slot) = ends.get_mut(open) {
                *slot = len;
            }
        }
        Self {
            cmds,
            ends: ends.into(),
            file,
        }
    }

    /// Number of commands.
    pub(crate) fn len(&self) -> usize {
        self.cmds.len()
    }

    /// Index of the end command that matches the opener at `idx` (or `idx` itself).
    pub(crate) fn end_of(&self, idx: usize) -> usize {
        self.ends.get(idx).copied().unwrap_or(idx)
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
                ends: Arc::clone(&self.ends),
                file: Arc::clone(&self.file),
            }
        } else {
            Self::new(cmds.into(), Arc::clone(&self.file))
        }
    }

    /// Indices of the `elseif` and `else` commands of the `if` block that starts at `idx`.
    pub(crate) fn if_branches(&self, idx: usize) -> Vec<usize> {
        let end = self.end_of(idx);
        let mut out = Vec::new();
        let mut j = idx + 1;
        while j < end {
            let Some(cmd) = self.cmds.get(j) else { break };
            let lower = cmd.lower.as_slice();
            if opener_end(lower).is_some() {
                j = self.end_of(j).max(j) + 1;
                continue;
            }
            if lower == b"elseif" || lower == b"else" {
                out.push(j);
            }
            j += 1;
        }
        out
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
        let digest = JournalDigest::hash(data);
        let key = LimitsKey::new(kind, limits);
        let bytes: Arc<[u8]> = Arc::from(data);
        let id = SourceMap::default().add("fixture.cmake".into(), kind, bytes.clone())?;
        let parsed = ParsedSource::parse(data, id, kind, limits);
        cache.insert(
            key,
            SourceText {
                bytes: bytes.clone(),
                digest,
            },
            parsed,
        );
        Some(bytes)
    }

    #[test]
    fn cache_reuses_text_and_checks_content_and_limits() {
        let cache = SharedParseCache::default();
        let kind = SourceKind::File;
        let limits = Limits::default();
        let source = b"set(V one)\n";
        let text = parse(&cache, source, kind, &limits).expect("source");
        let key = (JournalDigest::hash(source), LimitsKey::new(kind, &limits));
        let (hit, parsed) = cache.get(key, source).expect("cache hit");
        assert!(Arc::ptr_eq(&hit.bytes, &text));
        assert!(parsed.code.is_ok());
        assert!(cache.get(key, b"set(V two)\n").is_none());
        assert!(
            cache
                .get(
                    (JournalDigest::hash(b"set(V two)\n"), key.1),
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
                (JournalDigest::hash(text), LimitsKey::new(kind, &limits)),
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
    fn cache_keeps_listfiles_with_more_than_4096_commands() {
        let cache = SharedParseCache::default();
        let kind = SourceKind::File;
        let limits = Limits::default();
        let source = b"set(V value)\n".repeat(4097);
        assert!(parse(&cache, &source, kind, &limits).is_some());
        let key = (JournalDigest::hash(&source), LimitsKey::new(kind, &limits));
        let (_, parsed) = cache.get(key, &source).expect("cache hit");
        assert_eq!(parsed.code.expect("valid listfile").len(), 4097);
    }
}
