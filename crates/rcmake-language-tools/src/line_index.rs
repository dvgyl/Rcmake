//! Physical line and column index over raw CMake listfile bytes.
//!
//! Follows CMake's `SourceKind::File` line rules: `\n` and `\r\n` terminate lines,
//! while bare `\r` is horizontal space (not a line break). Columns count 1-based
//! Unicode scalar values on valid UTF-8 line prefixes, falling back to 1-based
//! raw byte offsets on non-UTF-8 slices.

#![forbid(unsafe_code)]

use core::ops::Range;

/// 1-based line and column coordinate.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LineCol {
    /// 1-based line number.
    pub line: u32,
    /// 1-based column number (`unicodeCodePoints` on UTF-8, raw bytes otherwise).
    pub column: u32,
}

/// Precomputed table of physical line start offsets in raw file bytes.
#[derive(Clone, Debug)]
pub struct LineIndex {
    starts: Vec<usize>,
    bom_len: usize,
}

impl LineIndex {
    /// Build a line index over raw file bytes (`SourceKind::File` semantics).
    #[must_use]
    pub fn new(src: &[u8]) -> Self {
        let bom_len = if src.starts_with(b"\xef\xbb\xbf") {
            3
        } else {
            0
        };
        let mut starts = vec![0usize];
        for (idx, &byte) in src.iter().enumerate() {
            if byte == b'\n' {
                starts.push(idx + 1);
            }
        }
        Self { starts, bom_len }
    }

    /// Convert a raw byte offset to a 1-based [`LineCol`].
    #[must_use]
    pub fn line_col(&self, src: &[u8], offset: usize) -> LineCol {
        let clamped = offset.min(src.len());
        let line_idx = self
            .starts
            .partition_point(|&start| start <= clamped)
            .saturating_sub(1);
        let raw_line_start = self.starts.get(line_idx).copied().unwrap_or(0);
        let content_start = if line_idx == 0 && clamped >= self.bom_len {
            self.bom_len
        } else {
            raw_line_start
        };
        let prefix = src.get(content_start..clamped).unwrap_or_default();
        let col_offset = match core::str::from_utf8(prefix) {
            Ok(text) => text.chars().count(),
            Err(_) => prefix.len(),
        };
        LineCol {
            line: u32::try_from(line_idx.saturating_add(1)).unwrap_or(u32::MAX),
            column: u32::try_from(col_offset.saturating_add(1)).unwrap_or(u32::MAX),
        }
    }

    /// Convert a half-open byte span to inclusive-start, exclusive-end 1-based coordinates.
    #[must_use]
    pub fn span_coords(&self, src: &[u8], span: Range<usize>) -> (LineCol, LineCol) {
        let start = self.line_col(src, span.start);
        let end = self.line_col(src, span.end.max(span.start));
        (start, end)
    }
}

/// Measure display column width for formatting line-width calculations.
///
/// Uses Unicode scalar count on valid UTF-8 with tabs expanded to `tab_width`,
/// and falls back to raw byte length on non-UTF-8 slices.
#[must_use]
pub fn display_width(bytes: &[u8], tab_width: usize) -> usize {
    let tab = tab_width.max(1);
    match core::str::from_utf8(bytes) {
        Ok(text) => {
            let mut width = 0usize;
            for ch in text.chars() {
                if ch == '\t' {
                    width = width.saturating_add(tab);
                } else {
                    width = width.saturating_add(1);
                }
            }
            width
        }
        Err(_) => {
            let mut width = 0usize;
            for &byte in bytes {
                if byte == b'\t' {
                    width = width.saturating_add(tab);
                } else {
                    width = width.saturating_add(1);
                }
            }
            width
        }
    }
}
