//! Executable command sequences with precomputed block structure.

use std::sync::Arc;

use rcmake_base::Limits;
use rcmake_diag::{Diagnostic, SourceId, source::SourceKind};
use rcmake_syntax::{CommandInvocation, ParseError};

#[derive(Clone)]
pub(crate) struct ParsedSource {
    pub(crate) code: Result<Code, ParseError>,
    pub(crate) warnings: Vec<Diagnostic>,
}

impl ParsedSource {
    pub(crate) fn parse(text: &[u8], id: SourceId, kind: SourceKind, limits: &Limits) -> Self {
        match rcmake_syntax::parse_list_file_with_warnings(text, id, kind, limits) {
            Ok(list) => Self {
                code: Ok(Code::new(list.commands, Arc::from([]))),
                warnings: list.warnings,
            },
            Err((error, warnings)) => Self {
                code: Err(error),
                warnings,
            },
        }
    }
}

#[derive(Default)]
pub(crate) struct ParseCache {
    entries: Vec<ParseEntry>,
    bytes: usize,
}

struct ParseEntry {
    text: Arc<[u8]>,
    kind: SourceKind,
    limits: Limits,
    parsed: ParsedSource,
}

impl ParseCache {
    pub(crate) fn parse(
        &mut self,
        text: Arc<[u8]>,
        id: SourceId,
        kind: SourceKind,
        limits: &Limits,
    ) -> ParsedSource {
        if let Some(entry) = self
            .entries
            .iter()
            .find(|entry| entry.text == text && entry.kind == kind && &entry.limits == limits)
        {
            return entry.parsed.clone();
        }
        let parsed = ParsedSource::parse(&text, id, kind, limits);
        let size = text.len();
        if self.entries.len() < 256
            && size <= (1024 * 1024 - self.bytes)
            && parsed.code.as_ref().map_or(0, Code::len) <= 4096
            && parsed.warnings.len() <= 1024
        {
            self.bytes += size;
            self.entries.push(ParseEntry {
                text,
                kind,
                limits: limits.clone(),
                parsed: parsed.clone(),
            });
        }
        parsed
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
