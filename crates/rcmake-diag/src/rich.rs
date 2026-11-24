//! Terminal diagnostics with source excerpts, annotations, and suggested edits.

#![forbid(unsafe_code)]

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::ops::Range;

use rcmake_base::{BString, ByteSlice as _};

use crate::annotate::{Annotations, Edit, Label, SourceSpan};
use crate::{Diagnostic, FrameKind, Location, MessageKind, SourceFile, SourceId, SourceMap};

const TAB_WIDTH: usize = 4;

/// Provides the registered source text for a diagnostic.
pub trait SourceLookup {
    /// Get the source that contains the byte offsets in a label or an edit.
    fn source(&self, id: SourceId) -> Option<&SourceFile>;

    /// Get the source for a stack frame that has a path and no source identifier.
    fn source_by_path(&self, _path: &[u8]) -> Option<&SourceFile> {
        None
    }

    /// Return true if the path belongs to the bundled CMake module library.
    fn is_bundled_module(&self, _path: &[u8]) -> bool {
        false
    }
}

impl SourceLookup for SourceMap {
    fn source(&self, id: SourceId) -> Option<&SourceFile> {
        self.get(id)
    }
}

/// Controls the appearance of a rich diagnostic.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RichOptions {
    /// Enable ANSI colors.
    pub color: bool,
    /// Target display width. The renderer uses a range of 20 to 4096 columns.
    pub terminal_width: usize,
    /// Use Unicode gutters and ellipses.
    pub unicode: bool,
}

impl Default for RichOptions {
    fn default() -> Self {
        Self {
            color: false,
            terminal_width: 100,
            unicode: true,
        }
    }
}

/// Render a diagnostic and its annotations as UTF-8 terminal text.
///
/// Span offsets refer to the bytes returned by the source lookup. The renderer
/// clamps offsets to the source and uses a point marker for an empty range.
#[must_use]
pub fn render_rich(
    diag: &Diagnostic,
    annotations: Option<&Annotations>,
    lookup: &dyn SourceLookup,
    opts: RichOptions,
) -> BString {
    let (severity, color, default_rule) = match diag.kind {
        MessageKind::Error | MessageKind::InternalError => ("error", 31, "CMake.FatalError"),
        MessageKind::Warning => ("warning", 33, "CMake.Warning"),
        MessageKind::AuthorWarning => ("warning", 33, "CMake.Author"),
        MessageKind::AuthorError => ("error", 31, "CMake.Author"),
        MessageKind::DeprecationWarning => ("warning", 33, "CMake.Deprecated"),
        MessageKind::DeprecationError => ("error", 31, "CMake.Deprecated"),
        MessageKind::Log => ("note", 36, "CMake.Note"),
    };
    let mut renderer = Renderer {
        out: String::new(),
        opts,
        width: opts.terminal_width.clamp(20, 4096),
        primary_color: color,
    };
    let labels = annotations.map_or(&[][..], |a| a.labels.as_slice());
    let mut groups: BTreeMap<SourceId, (&SourceFile, Vec<&Label>)> = BTreeMap::new();
    let mut primary = None;
    let mut missing = Vec::new();
    for label in labels {
        let id = label.span.source;
        if let Some(file) = lookup.source(id) {
            groups
                .entry(id)
                .or_insert_with(|| (file, Vec::new()))
                .1
                .push(label);
            if label.primary {
                primary.get_or_insert(id);
            }
        } else if !label.message.is_empty() {
            missing.push(label.message.as_slice());
        }
    }

    let fallback = Site::from_location(&diag.location);
    let rule = diag
        .category
        .map_or(default_rule, |category| category.sarif_rule_id());
    let header = if fallback.is_some() || !groups.is_empty() {
        format!("{severity}[{rule}]:")
    } else {
        format!("{severity}:")
    };
    let body = diag.body.to_str_lossy();
    let mut lines = body
        .lines()
        .skip_while(|line| line.trim().is_empty())
        .peekable();
    let mut headline = String::new();
    while let Some(line) = lines.next_if(|line| !line.starts_with(' ') && !line.trim().is_empty()) {
        if !headline.is_empty() {
            headline.push(' ');
        }
        headline.push_str(line);
    }
    renderer.paint(color, &header);
    renderer.wrap(&headline, header.len(), 2);

    if let Some((file, labels)) = primary.and_then(|id| groups.remove(&id)) {
        renderer.snippet(file, &labels);
    } else if let Some(site) = fallback {
        renderer.fallback(site, lookup);
    }
    for (file, labels) in groups.into_values() {
        renderer.snippet(file, &labels);
    }

    renderer.body(lines);
    for message in missing {
        renderer.footer("note", message);
    }
    if let Some(annotations) = annotations {
        for fix in &annotations.fixes {
            renderer.footer("help", fix.message.as_slice());
            for edit in &fix.edits {
                if let Some(file) = lookup.source(edit.span.source) {
                    renderer.edit(file, edit);
                }
            }
        }
        for note in &annotations.notes {
            renderer.footer("note", note.as_slice());
        }
    }
    renderer.callers(&diag.location, lookup);
    BString::from(renderer.out.into_bytes())
}

#[derive(Clone, Copy)]
struct Site<'a> {
    path: &'a [u8],
    line: u32,
    name: Option<&'a [u8]>,
    deferred: bool,
}

impl<'a> Site<'a> {
    fn from_location(location: &'a Location) -> Option<Self> {
        match location {
            Location::None => None,
            Location::FileLine { file, line } => Some(Self {
                path: file,
                line: *line,
                name: None,
                deferred: false,
            }),
            Location::Backtrace(trace) => trace.top().map(|frame| Self {
                path: &frame.file,
                line: frame.line,
                name: frame.name.as_deref(),
                deferred: frame.kind == FrameKind::Deferred,
            }),
        }
    }
}

#[derive(Clone, Copy)]
struct Line<'a> {
    number: u32,
    offset: usize,
    text: &'a [u8],
    next_offset: Option<usize>,
}

impl<'a> Line<'a> {
    fn at(file: &'a SourceFile, offset: usize) -> Self {
        let (number, byte_column) = file.line_col(offset);
        let start = offset.saturating_sub(byte_column.saturating_sub(1) as usize);
        Self::from_start(file.text.as_ref(), number, start)
    }

    fn from_start(text: &'a [u8], number: u32, offset: usize) -> Self {
        let rest = text.get(offset..).unwrap_or_default();
        let newline = rest.find_byte(b'\n');
        let text = newline.map_or(rest, |end| {
            let line = rest.get(..end).unwrap_or_default();
            line.strip_suffix(b"\r").unwrap_or(line)
        });
        Self {
            number,
            offset,
            text,
            next_offset: newline.map(|end| offset + end + 1),
        }
    }
}

struct Mark<'a> {
    columns: Range<usize>,
    primary: bool,
    message: &'a [u8],
}

impl<'a> Mark<'a> {
    fn new(text: &[u8], bytes: Range<usize>, primary: bool, message: &'a [u8]) -> Self {
        let start = display_column(text, bytes.start, false);
        let end = if bytes.is_empty() {
            start
        } else {
            display_column(text, bytes.end, true)
        };
        Self {
            columns: start..end.max(start + 1),
            primary,
            message,
        }
    }
}

struct Renderer {
    out: String,
    opts: RichOptions,
    width: usize,
    primary_color: u8,
}

impl Renderer {
    fn paint(&mut self, color: u8, text: &str) {
        if self.opts.color {
            let _ = write!(self.out, "\x1b[{color}m{text}\x1b[0m");
        } else {
            self.out.push_str(text);
        }
    }

    fn bar(&self) -> &'static str {
        if self.opts.unicode { "│" } else { "|" }
    }

    fn arrow(&self) -> &'static str {
        if self.opts.unicode {
            "╭─▶"
        } else {
            "-->"
        }
    }

    fn ellipsis(&self) -> &'static str {
        if self.opts.unicode { "…" } else { "..." }
    }

    fn gap(&mut self, width: usize) {
        let bar = self.bar();
        let _ = writeln!(self.out, "{:width$} {bar}", "");
    }

    fn location(&mut self, indent: usize, path: &[u8], line: u32, column: usize, deferred: bool) {
        let path = inline_text(path);
        let arrow = self.arrow();
        let _ = write!(self.out, "{:indent$}{arrow} {path}", "");
        if deferred {
            self.out.push_str(":DEFERRED");
        } else if line != 0 {
            let _ = write!(self.out, ":{line}");
            if column != 0 {
                let _ = write!(self.out, ":{column}");
            }
        }
        self.out.push('\n');
    }

    fn wrap(&mut self, text: &str, mut column: usize, indent: usize) {
        for word in text.split_whitespace() {
            let len = word.chars().count();
            if column > indent && column + 1 + len > self.width {
                self.out.push('\n');
                self.out.extend(std::iter::repeat_n(' ', indent));
                column = indent;
            } else if column != 0 {
                self.out.push(' ');
                column += 1;
            }
            self.out.extend(word.chars().map(visible_char));
            column += len;
        }
        self.out.push('\n');
    }

    fn body<'a>(&mut self, lines: impl Iterator<Item = &'a str>) {
        let mut after_break = true;
        for line in lines {
            if line.trim().is_empty() {
                after_break = true;
                continue;
            }
            if after_break {
                self.out.push('\n');
            }
            after_break = false;
            if line.starts_with(' ') {
                self.out.push_str("  ");
                self.out.extend(display_cells(line.as_bytes()));
                self.out.push('\n');
            } else {
                self.out.push(' ');
                self.wrap(line, 1, 2);
            }
        }
    }

    fn footer(&mut self, kind: &str, bytes: &[u8]) {
        self.out.push_str("  = ");
        self.paint(if kind == "help" { 32 } else { 36 }, kind);
        self.out.push(':');
        self.wrap(&bytes.to_str_lossy(), 5 + kind.len(), 4);
    }

    fn snippet(&mut self, file: &SourceFile, labels: &[&Label]) {
        let mut rows: BTreeMap<u32, (&[u8], Vec<Mark<'_>>)> = BTreeMap::new();
        let mut anchor = None;
        for label in labels {
            let bytes = clamped_range(file, label.span);
            let first = Line::at(file, bytes.start);
            let last = Line::at(file, bytes.end.saturating_sub(1).max(bytes.start));
            if anchor.is_none_or(|(_, _, primary)| label.primary && !primary) {
                let column = display_column(first.text, bytes.start - first.offset, false);
                anchor = Some((first.number, column + 1, label.primary));
            }

            let mut line = first;
            loop {
                let columns =
                    bytes.start.saturating_sub(line.offset)..bytes.end.saturating_sub(line.offset);
                let message = if line.number == last.number {
                    label.message.as_slice()
                } else {
                    b""
                };
                let mark = Mark::new(line.text, columns, label.primary, message);
                rows.entry(line.number)
                    .or_insert_with(|| (line.text, Vec::new()))
                    .1
                    .push(mark);
                if line.number == last.number {
                    break;
                }
                if last.number - first.number > 3 {
                    line = last;
                } else if let Some(next) = line.next_offset {
                    line = Line::from_start(file.text.as_ref(), line.number + 1, next);
                } else {
                    break;
                }
            }
        }
        let Some((line, column, _)) = anchor else {
            return;
        };
        let number_width = rows
            .last_key_value()
            .map_or(1, |(number, _)| number.to_string().len());
        self.location(number_width, file.path.as_slice(), line, column, false);
        self.gap(number_width);
        let mut previous = None;
        for (number, (text, mut marks)) in rows {
            if previous.is_some_and(|prev| number - prev > 1) {
                let ellipsis = self.ellipsis();
                let bar = self.bar();
                let _ = writeln!(self.out, "{:number_width$} {bar} {ellipsis}", "");
            }
            previous = Some(number);
            marks.sort_by_key(|mark| (mark.columns.start, !mark.primary));
            self.marked_line(number_width, number, text, &marks);
        }
        self.gap(number_width);
    }

    fn marked_line(
        &mut self,
        number_width: usize,
        number: u32,
        text: &[u8],
        mut marks: &[Mark<'_>],
    ) {
        let width = self.width.saturating_sub(number_width + 3).max(7);
        let bar = self.bar();
        let ellipsis = self.ellipsis();
        let ellipsis_width = ellipsis.chars().count();

        while let Some(first) = marks.first() {
            let left = if first.columns.start < width - ellipsis_width {
                0
            } else {
                first.columns.start - (width - 2 * ellipsis_width) / 2
            };
            let head = if left == 0 { 0 } else { ellipsis_width };
            let available = width - head;
            let mut cells: Vec<char> = display_cells(text).skip(left).take(available + 1).collect();
            let cropped = cells.len() > available;
            if cropped {
                cells.truncate(available - ellipsis_width);
            }
            let right = left + cells.len();
            let _ = write!(self.out, "{number:>number_width$} {bar} ");
            if left != 0 {
                self.out.push_str(ellipsis);
            }
            self.out.extend(cells);
            if cropped {
                self.out.push_str(ellipsis);
            }
            self.out.push('\n');

            let visible = marks
                .iter()
                .take_while(|mark| {
                    mark.columns.start < right || (!cropped && mark.columns.start == right)
                })
                .count()
                .max(1);
            let (visible, rest) = marks.split_at(visible);
            for mark in visible {
                let indent = head + mark.columns.start.saturating_sub(left);
                let count = mark
                    .columns
                    .end
                    .min(right)
                    .saturating_sub(mark.columns.start)
                    .max(1);
                let (symbol, color) = if mark.primary {
                    ('^', self.primary_color)
                } else {
                    ('-', 34)
                };
                let _ = write!(self.out, "{:number_width$} {bar} {:indent$}", "", "");
                self.paint(color, &symbol.to_string().repeat(count));
                self.wrap(
                    &mark.message.to_str_lossy(),
                    number_width + 3 + indent + count,
                    number_width + 3,
                );
            }
            marks = rest;
        }
    }

    fn fallback(&mut self, site: Site<'_>, lookup: &dyn SourceLookup) {
        if !site.deferred
            && let Some(name) = site.name.filter(|name| !name.is_empty())
            && let Some(file) = lookup.source_by_path(site.path)
            && let Ok(index) = usize::try_from(site.line.saturating_sub(1))
            && site.line != 0
            && let Some(raw) = file.text.split_inclusive(|&byte| byte == b'\n').nth(index)
        {
            let text = raw
                .strip_suffix(b"\n")
                .map_or(raw, |line| line.strip_suffix(b"\r").unwrap_or(line));
            let start = text
                .iter()
                .take_while(|byte| byte.is_ascii_whitespace())
                .count();
            let end = start.saturating_add(name.len());
            if text
                .get(start..end)
                .is_some_and(|command| command.eq_ignore_ascii_case(name))
                && text
                    .get(end)
                    .is_none_or(|&byte| byte.is_ascii_whitespace() || byte == b'(')
            {
                let number_width = site.line.to_string().len();
                let mark = Mark::new(text, start..end, true, b"");
                self.location(
                    number_width,
                    site.path,
                    site.line,
                    mark.columns.start + 1,
                    false,
                );
                self.gap(number_width);
                self.marked_line(number_width, site.line, text, &[mark]);
                self.gap(number_width);
                return;
            }
        }
        self.location(1, site.path, site.line, 0, site.deferred);
    }

    fn edit(&mut self, file: &SourceFile, edit: &Edit) {
        let bytes = clamped_range(file, edit.span);
        let first = Line::at(file, bytes.start);
        let last = Line::at(file, bytes.end);
        let column = display_column(first.text, bytes.start - first.offset, false);
        let prefix = file.text.get(first.offset..bytes.start).unwrap_or_default();
        let before = file.text.get(bytes.clone()).unwrap_or_default();
        let suffix = file
            .text
            .get(bytes.end..last.offset + last.text.len())
            .unwrap_or_default();
        self.location(4, file.path.as_slice(), first.number, column + 1, false);
        self.diff('-', prefix, before, suffix);
        self.diff('+', prefix, edit.replacement.as_slice(), suffix);
    }

    fn diff(&mut self, sign: char, prefix: &[u8], change: &[u8], suffix: &[u8]) {
        let start = prefix.len().saturating_sub(self.width / 2);
        let budget = self.width * 12;
        let mut bytes = Vec::new();
        if start != 0 {
            bytes.extend_from_slice(self.ellipsis().as_bytes());
        }
        bytes.extend(
            prefix
                .get(start..)
                .unwrap_or_default()
                .iter()
                .chain(change)
                .chain(suffix)
                .copied()
                .take(budget + 1),
        );
        let clipped = bytes.len() > budget;
        bytes.truncate(budget);
        let mut lines = bytes.split(|&b| b == b'\n').peekable();
        let width = self.width - 6;
        let color = if sign == '+' { 32 } else { 31 };
        for index in 0..3 {
            let Some(text) = lines.next() else {
                break;
            };
            let text = if lines.peek().is_some() {
                text.strip_suffix(b"\r").unwrap_or(text)
            } else {
                text
            };
            let mut cells: Vec<char> = display_cells(text).take(width + 1).collect();
            let more = cells.len() > width
                || (clipped && lines.peek().is_none())
                || (index == 2 && lines.peek().is_some());
            if more {
                cells.truncate(width - self.ellipsis().chars().count());
                cells.extend(self.ellipsis().chars());
            }
            let _ = write!(self.out, "    {sign} ");
            self.paint(color, &cells.into_iter().collect::<String>());
            self.out.push('\n');
        }
    }

    fn callers(&mut self, location: &Location, lookup: &dyn SourceLookup) {
        let Location::Backtrace(trace) = location else {
            return;
        };
        let mut bundled = 0;
        for frame in trace.frames().skip(1) {
            if lookup.is_bundled_module(&frame.file) {
                bundled += 1;
                continue;
            }
            self.bundled_frames(bundled);
            bundled = 0;
            let path = inline_text(&frame.file);
            let mut text = format!("called from {path}");
            if frame.kind == FrameKind::Deferred {
                text.push_str(":DEFERRED");
            } else if frame.line != 0 {
                let _ = write!(text, ":{}", frame.line);
            }
            if let Some(name) = &frame.name {
                let _ = write!(text, " ({})", inline_text(name));
            }
            self.footer("note", text.as_bytes());
        }
        self.bundled_frames(bundled);
    }

    fn bundled_frames(&mut self, count: usize) {
        if count != 0 {
            let ellipsis = self.ellipsis();
            let noun = if count == 1 { "frame" } else { "frames" };
            self.footer(
                "note",
                format!("{ellipsis} {count} {noun} in CMake modules").as_bytes(),
            );
        }
    }
}

fn clamped_range(file: &SourceFile, span: SourceSpan) -> Range<usize> {
    let offset = |value| {
        usize::try_from(value)
            .unwrap_or(usize::MAX)
            .min(file.text.len())
    };
    let start = offset(span.start);
    start..offset(span.end).max(start)
}

fn display_column(text: &[u8], offset: usize, round_up: bool) -> usize {
    let mut column = 0;
    for (start, end, ch) in text.char_indices() {
        if start >= offset || (!round_up && end > offset) {
            break;
        }
        column += cell_width(ch, column);
    }
    column
}

fn display_cells(bytes: &[u8]) -> impl Iterator<Item = char> + '_ {
    let mut column = 0;
    bytes.chars().flat_map(move |ch| {
        let width = cell_width(ch, column);
        column += width;
        std::iter::repeat_n(if ch == '\t' { ' ' } else { visible_char(ch) }, width)
    })
}

fn cell_width(ch: char, column: usize) -> usize {
    if ch == '\t' {
        TAB_WIDTH - column % TAB_WIDTH
    } else {
        1
    }
}

fn visible_char(ch: char) -> char {
    if ch.is_control() { '�' } else { ch }
}

fn inline_text(bytes: &[u8]) -> String {
    bytes.chars().map(visible_char).collect()
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use crate::annotate::{Confidence, Fix};
    use crate::{Backtrace, DiagCategory, Effect, Frame, source::SourceKind};

    use super::*;

    fn add(map: &mut SourceMap, path: &str, text: &[u8]) -> Result<SourceId, &'static str> {
        map.add(path.into(), SourceKind::File, Arc::from(text))
            .ok_or("Cannot register source")
    }

    fn diagnostic(location: Location) -> Diagnostic {
        Diagnostic {
            kind: MessageKind::Error,
            effect: Effect::Fatal,
            category: None,
            policy: None,
            location,
            body: "Unknown command.".into(),
            annotations: None,
        }
    }

    fn span(source: SourceId, start: u32, end: u32) -> SourceSpan {
        SourceSpan { source, start, end }
    }

    fn ascii() -> RichOptions {
        RichOptions {
            unicode: false,
            ..RichOptions::default()
        }
    }

    #[test]
    fn tabs_crlf_and_invalid_utf8_align_markers() -> Result<(), &'static str> {
        let mut map = SourceMap::default();
        let source = add(&mut map, "CMakeLists.txt", b"x\tm\xffs\r\nok\r\n")?;
        let annotations = Annotations::primary(span(source, 2, 5), "first line")
            .with_secondary(span(source, 7, 9), "second line");
        let result = render_rich(
            &diagnostic(Location::None),
            Some(&annotations),
            &map,
            ascii(),
        );
        assert_eq!(
            result.as_slice(),
            concat!(
                "error[CMake.FatalError]: Unknown command.\n",
                " --> CMakeLists.txt:1:5\n",
                "  |\n",
                "1 | x   m�s\n",
                "  |     ^^^ first line\n",
                "2 | ok\n",
                "  | -- second line\n",
                "  |\n",
            )
            .as_bytes(),
        );
        Ok(())
    }

    #[test]
    fn offsets_inside_a_utf8_scalar_mark_the_scalar() -> Result<(), &'static str> {
        let mut map = SourceMap::default();
        let source = add(&mut map, "input", "aéz".as_bytes())?;
        let annotations = Annotations::primary(span(source, 2, 3), "scalar");
        let result = render_rich(
            &diagnostic(Location::None),
            Some(&annotations),
            &map,
            ascii(),
        );
        let text = result.to_str_lossy();
        assert!(text.contains(" --> input:1:2\n"));
        assert!(text.contains("1 | aéz\n  |  ^ scalar\n"));
        Ok(())
    }

    #[test]
    fn eof_empty_input_and_reverse_ranges_have_point_markers() -> Result<(), &'static str> {
        let mut map = SourceMap::default();
        let empty = add(&mut map, "empty", b"")?;
        let final_newline = add(&mut map, "newline", b"set(a)\n")?;
        let no_newline = add(&mut map, "no-newline", b"set(b)")?;
        let annotations = Annotations::primary(span(empty, 0, 0), "empty input")
            .with_secondary(span(final_newline, u32::MAX, u32::MAX), "past EOF")
            .with_secondary(span(no_newline, 6, 3), "reversed range");
        let result = render_rich(
            &diagnostic(Location::None),
            Some(&annotations),
            &map,
            ascii(),
        );
        let text = result.to_str_lossy();
        assert!(text.contains(" --> empty:1:1\n  |\n1 | \n  | ^ empty input\n"));
        assert!(text.contains(" --> newline:2:1\n  |\n2 | \n  | - past EOF\n"));
        assert!(
            text.contains(" --> no-newline:1:7\n  |\n1 | set(b)\n  |       - reversed range\n")
        );
        Ok(())
    }

    #[test]
    fn multiline_overlapping_labels_share_source_lines() -> Result<(), &'static str> {
        let mut map = SourceMap::default();
        let source = add(&mut map, "input", b"aa\nbb\ncc\n")?;
        let mut annotations = Annotations::primary(span(source, 1, 7), "multiline")
            .with_secondary(span(source, 3, 5), "whole line")
            .with_secondary(span(source, 6, 8), "overlap");
        annotations.labels.push(Label {
            span: span(source, 4, 5),
            message: "another primary".into(),
            primary: true,
        });
        let result = render_rich(
            &diagnostic(Location::None),
            Some(&annotations),
            &map,
            ascii(),
        );
        let text = result.to_str_lossy();
        assert_eq!(text.matches("2 | bb\n").count(), 1);
        assert!(text.contains("1 | aa\n  |  ^\n"));
        assert!(text.contains("2 | bb\n  | ^^\n  | -- whole line\n  |  ^ another primary\n"));
        assert!(text.contains("3 | cc\n  | ^ multiline\n  | -- overlap\n"));
        Ok(())
    }

    #[test]
    fn long_spans_skip_middle_lines_and_align_line_numbers() -> Result<(), &'static str> {
        let mut map = SourceMap::default();
        let source = add(&mut map, "input", b"a\nb\nc\nd\ne\nf\ng\nh\ni\nj\n")?;
        let annotations = Annotations::primary(span(source, 0, 19), "");
        let result = render_rich(
            &diagnostic(Location::None),
            Some(&annotations),
            &map,
            ascii(),
        );
        let text = result.to_str_lossy();
        assert!(text.contains(" 1 | a\n   | ^\n   | ...\n10 | j\n   | ^\n"));
        Ok(())
    }

    #[test]
    fn narrow_terminal_keeps_distant_labels_visible() -> Result<(), &'static str> {
        let mut map = SourceMap::default();
        let source = add(&mut map, "a", b"a012345678901234567890123456789z")?;
        let annotations = Annotations::primary(span(source, 0, 1), "one")
            .with_secondary(span(source, 31, 32), "two");
        let opts = RichOptions {
            terminal_width: 20,
            ..ascii()
        };
        let result = render_rich(&diagnostic(Location::None), Some(&annotations), &map, opts);
        let text = result.to_str_lossy();
        assert!(text.contains("1 | a012345678901...\n  | ^ one\n"));
        assert!(text.contains("1 | ..."));
        assert!(text.contains("z\n"));
        assert!(text.contains("two\n"));
        assert!(
            text.lines()
                .filter(|line| line.starts_with("1 |"))
                .all(|line| line.len() <= 20)
        );
        Ok(())
    }

    #[test]
    fn fix_preview_preserves_context_for_replacement_and_insertion() -> Result<(), &'static str> {
        let mut map = SourceMap::default();
        let source = add(&mut map, "input", b"mesage(ok)\r\n")?;
        let annotations = Annotations::default()
            .with_fix(Fix {
                message: "Use message.".into(),
                edits: vec![Edit {
                    span: span(source, 0, 6),
                    replacement: "message".into(),
                }],
                confidence: Confidence::MachineApplicable,
            })
            .with_fix(Fix {
                message: "Insert a newline.".into(),
                edits: vec![Edit {
                    span: span(source, 7, 7),
                    replacement: "\n".into(),
                }],
                confidence: Confidence::MaybeIncorrect,
            })
            .with_note("Check the spelling.");
        let result = render_rich(
            &diagnostic(Location::None),
            Some(&annotations),
            &map,
            ascii(),
        );
        let text = result.to_str_lossy();
        assert!(text.contains(
            "  = help: Use message.\n    --> input:1:1\n    - mesage(ok)\n    + message(ok)\n"
        ));
        assert!(text.contains("    --> input:1:8\n    - mesage(ok)\n    + mesage(\n    + ok)\n"));
        assert!(text.contains("  = note: Check the spelling.\n"));
        Ok(())
    }

    #[test]
    fn color_and_gutters_are_independent_and_controls_are_safe() -> Result<(), &'static str> {
        let mut map = SourceMap::default();
        let source = add(&mut map, "input", b"\x1b[0m")?;
        let annotations = Annotations::primary(span(source, 0, 1), "");
        let mut diag = diagnostic(Location::None);
        diag.kind = MessageKind::Warning;
        diag.category = Some(DiagCategory::Policy);
        let plain = render_rich(&diag, Some(&annotations), &map, RichOptions::default());
        let plain = plain.to_str_lossy();
        assert!(plain.starts_with("warning[CMake.Policy]: Unknown command.\n"));
        assert!(plain.contains(" ╭─▶ input:1:1\n"));
        assert!(plain.contains("1 │ �[0m\n"));
        assert!(!plain.contains('\x1b'));
        let colored = render_rich(
            &diag,
            Some(&annotations),
            &map,
            RichOptions {
                color: true,
                ..ascii()
            },
        );
        let colored = colored.to_str_lossy();
        assert!(colored.contains("\x1b[33mwarning[CMake.Policy]:\x1b[0m"));
        assert!(colored.contains("\x1b[33m^\x1b[0m"));
        assert!(colored.contains("1 | �[0m\n"));
        Ok(())
    }

    struct Lookup(SourceMap, SourceId);

    impl SourceLookup for Lookup {
        fn source(&self, id: SourceId) -> Option<&SourceFile> {
            self.0.get(id)
        }

        fn source_by_path(&self, path: &[u8]) -> Option<&SourceFile> {
            self.0
                .get(self.1)
                .filter(|file| file.path.as_slice() == path)
        }

        fn is_bundled_module(&self, path: &[u8]) -> bool {
            path.starts_with(b"/bundle/Modules/")
        }
    }

    #[test]
    fn fallback_underlines_command_and_stack_collapses_bundled_frames() -> Result<(), &'static str>
    {
        let mut map = SourceMap::default();
        let source = add(&mut map, "script.cmake", b"\tMeSage()\n")?;
        let lookup = Lookup(map, source);
        let frame = |file: &[u8], line, name: &[u8]| Frame {
            file: Arc::from(file),
            line,
            name: Some(Arc::from(name)),
            kind: FrameKind::Command,
        };
        let stack = Backtrace::default()
            .push(frame(b"root.cmake", 10, b"include"))
            .push(frame(b"/bundle/Modules/B.cmake", 1, b"include"))
            .push(frame(b"/bundle/Modules/A.cmake", 2, b"include"))
            .push(frame(b"script.cmake", 1, b"mesage"));
        let result = render_rich(
            &diagnostic(Location::Backtrace(stack)),
            None,
            &lookup,
            ascii(),
        );
        let text = result.to_str_lossy();
        assert!(text.contains(" --> script.cmake:1:5\n"));
        assert!(text.contains("1 |     MeSage()\n  |     ^^^^^^\n"));
        assert!(text.contains("  = note: ... 2 frames in CMake modules\n  = note: called from root.cmake:10 (include)\n"));
        Ok(())
    }

    #[test]
    fn locationless_body_keeps_preformatted_lines() {
        let mut diag = diagnostic(Location::None);
        diag.body = "First line.\nSecond line.\n\n  keep   spacing\n\nMore detail.".into();
        let result = render_rich(&diag, None, &SourceMap::default(), ascii());
        assert_eq!(
            result.as_slice(),
            b"error: First line. Second line.\n\n    keep   spacing\n\n  More detail.\n",
        );
    }

    #[test]
    fn unavailable_source_keeps_fallback_location_and_label_message() -> Result<(), &'static str> {
        let mut map = SourceMap::default();
        let source = add(&mut map, "unavailable", b"input")?;
        let annotations = Annotations::primary(span(source, 0, 1), "missing source");
        let diag = diagnostic(Location::FileLine {
            file: Arc::from(b"fallback".as_slice()),
            line: 23,
        });
        let result = render_rich(&diag, Some(&annotations), &SourceMap::default(), ascii());
        let text = result.to_str_lossy();
        assert!(text.contains(" --> fallback:23\n"));
        assert!(text.contains("  = note: missing source\n"));
        Ok(())
    }
}
