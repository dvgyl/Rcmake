//! Source file registry and byte-offset-to-line/column mapping.

use core::num::NonZeroU32;
use std::collections::BTreeMap;
use std::sync::Arc;

use rcmake_base::BString;

/// Opaque identifier for a loaded CMake listfile or virtual `EVAL` buffer.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct SourceId(NonZeroU32);

impl SourceId {
    /// Underlying 1-based numeric index.
    #[must_use]
    pub const fn get(self) -> u32 {
        self.0.get()
    }
}

/// Whether a source came from a disk file or an in-memory `cmake_language(EVAL)` string.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SourceKind {
    /// Read from a file (CRLF normalized to LF).
    File,
    /// In-memory `cmake_language(EVAL CODE ...)` buffer (bytes passed directly).
    VirtualEval,
}

/// A registered source file with raw bytes and precomputed line start offsets.
#[derive(Clone, Debug)]
pub struct SourceFile {
    /// Absolute path (or `<file>:<line>:EVAL` for virtual buffers) with `/` separators.
    pub path: BString,
    /// Origin kind.
    pub kind: SourceKind,
    /// Raw source bytes.
    pub text: Arc<[u8]>,
    line_starts: Vec<usize>,
}

impl SourceFile {
    /// Compute 1-based `(line, byte_column)` for a raw byte offset.
    #[must_use]
    pub fn line_col(&self, offset: usize) -> (u32, u32) {
        let clamped = offset.min(self.text.len());
        let line_idx = match self.line_starts.binary_search(&clamped) {
            Ok(i) => i,
            Err(i) => i.saturating_sub(1),
        };
        let line_start = self.line_starts.get(line_idx).copied().unwrap_or(0);
        let line = u32::try_from(line_idx + 1).unwrap_or(u32::MAX);
        let col = u32::try_from(clamped.saturating_sub(line_start) + 1).unwrap_or(u32::MAX);
        (line, col)
    }
}

/// Arena storing all parsed listfiles for a session.
#[derive(Default, Debug)]
pub struct SourceMap {
    files: Vec<SourceFile>,
    latest_files: BTreeMap<BString, SourceId>,
}

impl SourceMap {
    /// Register a new source file and return its [`SourceId`].
    pub fn add(&mut self, path: BString, kind: SourceKind, text: Arc<[u8]>) -> Option<SourceId> {
        let mut line_starts = vec![0usize];
        for (idx, &b) in text.iter().enumerate() {
            if b == b'\n' {
                line_starts.push(idx + 1);
            }
        }
        let next_idx = u32::try_from(self.files.len() + 1).ok()?;
        let id = SourceId(NonZeroU32::new(next_idx)?);
        if kind == SourceKind::File {
            self.latest_files.insert(path.clone(), id);
        }
        self.files.push(SourceFile {
            path,
            kind,
            text,
            line_starts,
        });
        Some(id)
    }

    /// Return the identifier of the most recently parsed file with this path.
    #[must_use]
    pub fn latest_file(&self, path: &[u8]) -> Option<SourceId> {
        self.latest_files.get(path).copied()
    }

    /// Look up a source file by [`SourceId`].
    #[must_use]
    pub fn get(&self, id: SourceId) -> Option<&SourceFile> {
        let idx = usize::try_from(id.0.get().checked_sub(1)?).ok()?;
        self.files.get(idx)
    }
}
