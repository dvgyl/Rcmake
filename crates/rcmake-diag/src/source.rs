//! Source file registry and byte-offset-to-line/column mapping.

use core::num::NonZeroU32;
use std::collections::BTreeMap;
use std::sync::{Arc, OnceLock};

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

/// A registered source file with raw bytes and line offsets computed on first use.
#[derive(Clone, Debug)]
pub struct SourceFile {
    /// Absolute path (or `<file>:<line>:EVAL` for virtual buffers) with `/` separators.
    pub path: BString,
    /// Origin kind.
    pub kind: SourceKind,
    /// Raw source bytes.
    pub text: Arc<[u8]>,
    line_starts: Arc<OnceLock<LineStarts>>,
}

#[derive(Debug)]
enum LineStarts {
    Compact(Box<[u32]>),
    Wide(Vec<usize>),
}

impl LineStarts {
    fn new(text: &[u8]) -> Self {
        let starts = std::iter::once(0).chain(
            text.iter()
                .enumerate()
                .filter_map(|(index, byte)| (*byte == b'\n').then_some(index + 1)),
        );
        if u32::try_from(text.len()).is_ok() {
            Self::Compact(starts.map(|index| index as u32).collect())
        } else {
            Self::Wide(starts.collect())
        }
    }

    fn line_at(&self, offset: usize) -> (usize, usize) {
        fn line_index(result: Result<usize, usize>) -> usize {
            match result {
                Ok(i) => i,
                Err(i) => i.saturating_sub(1),
            }
        }
        match self {
            Self::Compact(starts) => {
                let index = line_index(starts.binary_search(&(offset as u32)));
                (index, starts.get(index).copied().unwrap_or(0) as usize)
            }
            Self::Wide(starts) => {
                let index = line_index(starts.binary_search(&offset));
                (index, starts.get(index).copied().unwrap_or(0))
            }
        }
    }
}

impl SourceFile {
    /// Compute 1-based `(line, byte_column)` for a raw byte offset.
    #[must_use]
    pub fn line_col(&self, offset: usize) -> (u32, u32) {
        let clamped = offset.min(self.text.len());
        let starts = self.line_starts.get_or_init(|| LineStarts::new(&self.text));
        let (line_idx, line_start) = starts.line_at(clamped);
        let line = u32::try_from(line_idx.saturating_add(1)).unwrap_or(u32::MAX);
        let col =
            u32::try_from(clamped.saturating_sub(line_start).saturating_add(1)).unwrap_or(u32::MAX);
        (line, col)
    }
}

/// Arena storing all parsed listfiles for a session.
#[derive(Clone, Default, Debug)]
pub struct SourceMap {
    files: Vec<SourceFile>,
    latest_files: BTreeMap<BString, SourceId>,
}

impl SourceMap {
    /// Register a new source file and return its [`SourceId`].
    pub fn add(&mut self, path: BString, kind: SourceKind, text: Arc<[u8]>) -> Option<SourceId> {
        if kind == SourceKind::File
            && let Some(id) = self.latest_file(&path)
            && let Some(file) = self.get(id)
            && Arc::ptr_eq(&file.text, &text)
        {
            return Some(id);
        }
        let next_idx = u32::try_from(self.files.len().checked_add(1)?).ok()?;
        let id = SourceId(NonZeroU32::new(next_idx)?);
        if kind == SourceKind::File {
            self.latest_files.insert(path.clone(), id);
        }
        self.files.push(SourceFile {
            path,
            kind,
            text,
            line_starts: Arc::default(),
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

    /// Return all registered source files in insertion order.
    #[must_use]
    pub fn files(&self) -> &[SourceFile] {
        &self.files
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn line_table_is_lazy_and_shared() {
        let mut sources = SourceMap::default();
        let bytes: Arc<[u8]> = Arc::from(b"aa\r\n\nc\n".as_slice());
        let id = sources
            .add("/work/file".into(), SourceKind::File, bytes)
            .expect("source ID");
        let file = sources.get(id).expect("source file");
        let clone = file.clone();
        assert!(file.line_starts.get().is_none());
        let columns = [
            (1, 1),
            (1, 2),
            (1, 3),
            (1, 4),
            (2, 1),
            (3, 1),
            (3, 2),
            (4, 1),
            (4, 1),
        ];
        for (offset, expected) in columns.into_iter().enumerate() {
            assert_eq!(file.line_col(offset), expected);
        }
        assert!(matches!(
            clone.line_starts.get(),
            Some(LineStarts::Compact(_))
        ));
        assert!(Arc::ptr_eq(&file.line_starts, &clone.line_starts));
    }

    #[test]
    fn registry_reuses_only_the_latest_identical_file_text() {
        let mut sources = SourceMap::default();
        let text: Arc<[u8]> = Arc::from(b"first".as_slice());
        let replacement: Arc<[u8]> = Arc::from(b"second".as_slice());
        let add = |sources: &mut SourceMap, data: &Arc<[u8]>| {
            sources
                .add("/work/file".into(), SourceKind::File, data.clone())
                .expect("source ID")
        };
        let first = add(&mut sources, &text);
        assert_eq!(add(&mut sources, &text), first);
        let second = add(&mut sources, &replacement);
        assert_ne!(second, first);
        assert_eq!(sources.latest_file(b"/work/file"), Some(second));
        assert_eq!(sources.get(first).expect("original source").text, text);
        let third = add(&mut sources, &text);
        assert_ne!(third, first);
        assert_ne!(third, second);
    }

    #[test]
    fn wide_line_table_preserves_byte_offsets() {
        let offset = usize::MAX - 4;
        let wide = LineStarts::Wide(vec![0, offset]);
        assert_eq!(wide.line_at(offset - 1), (0, 0));
        assert_eq!(wide.line_at(offset), (1, offset));
        assert_eq!(wide.line_at(usize::MAX), (1, offset));
    }
}
