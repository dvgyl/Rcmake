//! Validation and rendering for suggested edits.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::ops::Range;

use crate::annotate::{Confidence, Edit, Fix};
use crate::{SourceFile, SourceId};

use super::{Cells, Line, Renderer, Site, SourceLookup, editor_column};

struct CheckedEdit<'source, 'edit> {
    bytes: Range<usize>,
    replacement: &'edit str,
    first: Line<'source>,
    last: Line<'source>,
}

struct CheckedFile<'source, 'edit> {
    file: &'source SourceFile,
    text: &'source str,
    edits: Vec<CheckedEdit<'source, 'edit>>,
}

fn check<'source, 'edit>(
    fix: &'edit Fix,
    lookup: &'source dyn SourceLookup,
) -> Option<Vec<CheckedFile<'source, 'edit>>> {
    let mut files: BTreeMap<SourceId, Vec<&Edit>> = BTreeMap::new();
    for edit in &fix.edits {
        files.entry(edit.span.source).or_default().push(edit);
    }
    files
        .into_iter()
        .map(|(id, mut edits)| {
            let file = lookup.source(id)?;
            let text = std::str::from_utf8(&file.text).ok()?;
            edits.sort_unstable_by_key(|edit| (edit.span.start, edit.span.end));
            let mut previous: Option<Range<usize>> = None;
            let mut checked = Vec::with_capacity(edits.len());
            for edit in edits {
                let (bytes, replacement) = edit.validate(text)?;
                if previous
                    .is_some_and(|prior| prior.start == bytes.start || prior.end > bytes.start)
                {
                    return None;
                }
                previous = Some(bytes.clone());
                let original = text.get(bytes.clone())?;
                if original == replacement {
                    continue;
                }
                let prefix = shared_bytes(original.chars(), replacement.chars());
                let original = original.get(prefix..)?;
                let replacement = replacement.get(prefix..)?;
                let suffix = shared_bytes(original.chars().rev(), replacement.chars().rev());
                let bytes = bytes.start + prefix..bytes.end - suffix;
                let replacement = replacement.get(..replacement.len() - suffix)?;
                let first = Line::at(file, bytes.start);
                let last = Line::at(file, bytes.end);
                checked.push(CheckedEdit {
                    bytes,
                    replacement,
                    first,
                    last,
                });
            }
            Some(CheckedFile {
                file,
                text,
                edits: checked,
            })
        })
        .collect()
}

fn shared_bytes(left: impl Iterator<Item = char>, right: impl Iterator<Item = char>) -> usize {
    left.zip(right)
        .take_while(|(left, right)| left == right)
        .map(|(ch, _)| ch.len_utf8())
        .sum()
}

impl Renderer {
    /// Render the help text and a preview if all edits are valid.
    pub(super) fn fix(&mut self, fix: &Fix, lookup: &dyn SourceLookup) {
        let confidence = match fix.confidence {
            Confidence::MachineApplicable => "help",
            Confidence::MaybeIncorrect => "help (review required)",
        };
        self.footer(confidence, fix.message.as_slice());
        let Some(files) = check(fix, lookup) else {
            self.footer("note", b"Cannot show the edit preview.");
            return;
        };
        for file in files {
            let mut remaining = file.edits.as_slice();
            while let Some(first) = remaining.first() {
                let mut last_line = first.last.number;
                let count = remaining
                    .iter()
                    .take_while(|edit| {
                        if edit.first.number > last_line.saturating_add(1) {
                            return false;
                        }
                        last_line = last_line.max(edit.last.number);
                        true
                    })
                    .count();
                let (hunk, rest) = remaining.split_at(count);
                remaining = rest;
                self.hunk(&file, hunk);
                if !remaining.is_empty() {
                    let ellipsis = self.ellipsis();
                    let _ = writeln!(self.out, "    {ellipsis}");
                }
            }
        }
    }

    fn hunk(&mut self, file: &CheckedFile<'_, '_>, edits: &[CheckedEdit<'_, '_>]) {
        let (Some(first), Some(last)) = (edits.first(), edits.last()) else {
            return;
        };
        let start = first.first.offset;
        let end = last
            .last
            .next_offset
            .map_or(file.text.len(), |next| next - 1);
        let Some(before) = file.text.get(start..end) else {
            return;
        };
        let mut after = String::new();
        let mut prior = start;
        let mut removed = Vec::with_capacity(edits.len());
        let mut inserted = Vec::with_capacity(edits.len());
        for edit in edits {
            let Some(context) = file.text.get(prior..edit.bytes.start) else {
                return;
            };
            after.push_str(context);
            let replacement_start = after.len();
            after.push_str(edit.replacement);
            inserted.push(replacement_start..after.len());
            removed.push(edit.bytes.start - start..edit.bytes.end - start);
            prior = edit.bytes.end;
        }
        let Some(tail) = file.text.get(prior..end) else {
            return;
        };
        after.push_str(tail);
        self.location(
            4,
            Site {
                path: file.file.path.as_slice(),
                line: first.first.number,
                name: None,
                deferred: false,
            },
            editor_column(first.first.text, first.bytes.start - start),
        );
        let has_next_line = last.last.next_offset.is_some();
        self.diff('-', before, &removed, has_next_line);
        self.diff('+', &after, &inserted, has_next_line);
    }

    fn diff(&mut self, sign: char, text: &str, changes: &[Range<usize>], has_next_line: bool) {
        let mut lines = text.split('\n');
        let mut offset = 0;
        for line in lines.by_ref().take(3) {
            let has_newline = has_next_line || offset + line.len() < text.len();
            self.diff_line(sign, line, offset, changes, has_newline);
            offset += line.len() + 1;
        }
        if let Some(last) = lines.next_back() {
            let last_offset = text.len() - last.len();
            if last_offset > offset {
                self.diff_omission(sign);
            }
            self.diff_line(sign, last, last_offset, changes, has_next_line);
        }
    }

    fn diff_line(
        &mut self,
        sign: char,
        line: &str,
        offset: usize,
        changes: &[Range<usize>],
        has_newline: bool,
    ) {
        let end = offset + line.len();
        let first_change = changes.partition_point(|change| change.end < offset);
        let changes = changes.get(first_change..).unwrap_or_default();
        let text = if has_newline
            && !changes
                .iter()
                .take_while(|change| change.start < end)
                .any(|change| change.contains(&end.saturating_sub(1)))
        {
            line.strip_suffix('\r').unwrap_or(line)
        } else {
            line
        };
        let cells = Cells::new(text.as_bytes());
        let ellipsis = self.ellipsis();
        let ellipsis_width = ellipsis.chars().count();
        let mut focus = changes
            .iter()
            .take_while(|change| change.start <= end)
            .map(|change| cells.column(change.start.saturating_sub(offset), false))
            .peekable();
        let mut column = focus.next().unwrap_or(0);
        for _ in 0..3 {
            let window = cells.window(column, self.width - 6, ellipsis_width);
            let color = if sign == '+' { 32 } else { 31 };
            let _ = write!(self.out, "    {sign} ");
            if window.before {
                self.out.push_str(ellipsis);
            }
            self.paint(color, &window.text);
            if window.after {
                self.out.push_str(ellipsis);
            }
            self.out.push('\n');
            while focus
                .next_if(|&point| point < window.end || !window.after)
                .is_some()
            {}
            let Some(next) = focus.next() else {
                return;
            };
            column = next;
        }
        self.diff_omission(sign);
    }

    fn diff_omission(&mut self, sign: char) {
        let ellipsis = self.ellipsis();
        let _ = writeln!(self.out, "    {sign} {ellipsis}");
    }
}
