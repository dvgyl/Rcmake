//! Terminal diagnostics with source excerpts, annotations, and suggested edits.

#![forbid(unsafe_code)]

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::ops::Range;

use rcmake_base::{BString, ByteSlice as _};

use crate::annotate::{Annotations, Label, SourceSpan};
use crate::{
    DiagCategory, Diagnostic, Frame, FrameKind, Location, MessageKind, SourceFile, SourceId,
    SourceMap,
};

mod cells;
mod fixes;

use cells::{Cells, Window, editor_column, visible_char};

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

    /// Return true to show the individual frames in each bundled module group.
    fn show_bundled_frames(&self) -> bool {
        true
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
/// Explicit annotations take precedence over the annotations in `diag`.
/// The renderer clamps label offsets to the source. An empty label uses a point
/// marker. Location columns count Unicode scalars from one, with one column per
/// tab. Markers and excerpt boundaries use terminal cells with four-cell tab stops.
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
    let annotations = annotations.or(diag.annotations.as_deref());
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
    let header = match diag.policy {
        Some(policy) => format!("{severity}[{rule} {policy}]:"),
        None if diag.category.is_some() || fallback.is_some() || !groups.is_empty() => {
            format!("{severity}[{rule}]:")
        }
        None => format!("{severity}:"),
    };
    let body = diag.body.to_str_lossy();
    let mut lines = body.lines().peekable();
    let headline = lines.next_if(|line| !line.is_empty() && !line.starts_with([' ', '\t']));
    renderer.paint(color, &header);
    renderer.wrap(
        headline.unwrap_or_default(),
        header.len(),
        "  ",
        true,
        false,
    );

    let has_location = fallback.is_some() || !groups.is_empty();
    if let Some((file, labels)) = primary.and_then(|id| groups.remove(&id)) {
        let site = fallback.filter(|site| site.path == file.path.as_slice() && !site.deferred);
        if site.is_none()
            && let Some(fallback) = fallback.filter(|site| site.name.is_some())
        {
            renderer.fallback(fallback, lookup);
        }
        renderer.snippet(file, &labels, site.and_then(|site| site.name));
    } else if let Some(site) = fallback {
        renderer.fallback(site, lookup);
    }
    for (file, labels) in groups.into_values() {
        renderer.snippet(file, &labels, None);
    }

    if has_location && lines.peek().is_some_and(|line| !line.is_empty()) {
        renderer.out.push('\n');
    }
    renderer.body(lines);
    for message in missing {
        renderer.footer("note", message);
    }
    if let Some(annotations) = annotations {
        for fix in &annotations.fixes {
            renderer.fix(fix, lookup);
        }
        for note in &annotations.notes {
            renderer.footer("note", note.as_slice());
        }
    }
    renderer.callers(&diag.location, lookup);
    renderer.suppression(diag);
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
            Location::Backtrace(trace) => trace.top().map(Self::from_frame),
        }
    }

    fn from_frame(frame: &'a Frame) -> Self {
        Self {
            path: &frame.file,
            line: frame.line,
            name: frame.name.as_deref(),
            deferred: frame.kind == FrameKind::Deferred,
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
    bytes: Range<usize>,
    primary: bool,
    message: &'a [u8],
}

impl Mark<'_> {
    fn columns(&self, cells: &Cells<'_>) -> Range<usize> {
        let start = cells.column(self.bytes.start, false);
        let end = if self.bytes.is_empty() {
            start
        } else {
            cells.column(self.bytes.end, true)
        };
        start..end.max(start + 1)
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

    fn location(&mut self, indent: usize, site: Site<'_>, column: usize) {
        let arrow = self.arrow();
        let _ = write!(self.out, "{:indent$}{arrow} ", "");
        self.site(site, column);
    }

    fn site(&mut self, site: Site<'_>, column: usize) {
        self.out.push_str(&inline_text(site.path));
        if site.deferred {
            self.out.push_str(":DEFERRED");
        } else if site.line != 0 {
            let _ = write!(self.out, ":{}", site.line);
            if column != 0 {
                let _ = write!(self.out, ":{column}");
            }
        }
        if let Some(name) = site.name {
            let _ = write!(self.out, " ({})", inline_text(name));
        }
        self.out.push('\n');
    }

    fn wrap(
        &mut self,
        text: &str,
        mut column: usize,
        continuation: &str,
        mut space: bool,
        hard_wrap: bool,
    ) {
        let indent = continuation.chars().count();
        for (index, line) in text.split('\n').enumerate() {
            if index != 0 {
                self.out.push_str(continuation);
                column = indent;
                space = false;
            }
            if !hard_wrap && line.contains('\t') {
                if space {
                    self.out.push(' ');
                }
                self.out.extend(line.chars().map(body_char));
            } else if !line.is_empty() {
                let mut start = 0;
                let breaks = line.match_indices(' ').filter_map(|(byte, _)| {
                    let bytes = line.as_bytes();
                    (byte > 0
                        && byte + 1 < bytes.len()
                        && bytes.get(byte - 1) != Some(&b' ')
                        && bytes.get(byte + 1) != Some(&b' '))
                    .then_some(byte)
                });
                for end in breaks.chain(std::iter::once(line.len())) {
                    let word = line.get(start..end).unwrap_or_default();
                    let cells = Cells::new(word.as_bytes());
                    if column > indent && column + usize::from(space) + cells.width() > self.width {
                        self.out.push('\n');
                        self.out.push_str(continuation);
                        column = indent;
                    } else if space {
                        self.out.push(' ');
                        column += 1;
                    }
                    if hard_wrap {
                        column = self.wrap_cells(&cells, column, continuation, indent);
                    } else {
                        self.out.extend(word.chars().map(body_char));
                        column += cells.width();
                    }
                    start = end + 1;
                    space = true;
                }
            }
            self.out.push('\n');
        }
    }

    fn wrap_cells(
        &mut self,
        cells: &Cells<'_>,
        mut column: usize,
        continuation: &str,
        indent: usize,
    ) -> usize {
        let mut start = 0;
        while start < cells.width() {
            let (piece, end) = cells.piece(start, self.width.saturating_sub(column));
            if start != end {
                self.out.push_str(&piece);
                column += end - start;
                start = end;
            }
            if start < cells.width() {
                self.out.push('\n');
                self.out.push_str(continuation);
                column = indent;
            }
        }
        column
    }

    fn body<'a>(&mut self, lines: impl Iterator<Item = &'a str>) {
        for line in lines {
            if line.is_empty() {
                self.out.push('\n');
            } else if line.starts_with([' ', '\t']) {
                self.out.push_str("  ");
                self.out.extend(line.chars().map(body_char));
                self.out.push('\n');
            } else {
                self.out.push_str("  ");
                self.wrap(line, 2, "  ", false, false);
            }
        }
    }

    fn footer(&mut self, kind: &str, bytes: &[u8]) {
        self.out.push_str("  = ");
        self.paint(if kind.starts_with("help") { 32 } else { 36 }, kind);
        self.out.push(':');
        self.wrap(&bytes.to_str_lossy(), 5 + kind.len(), "    ", true, false);
    }

    fn suppression(&mut self, diag: &Diagnostic) {
        let (flag, severity) = match diag.kind {
            MessageKind::Warning | MessageKind::AuthorWarning | MessageKind::DeprecationWarning => {
                ("-Wno-", "warning")
            }
            MessageKind::Error | MessageKind::AuthorError | MessageKind::DeprecationError => {
                ("-Wno-error=", "error")
            }
            _ => return,
        };
        let category = match diag.category {
            Some(DiagCategory::Author) => "author",
            Some(DiagCategory::Policy) => "policy",
            Some(DiagCategory::Deprecated) => "deprecated",
            Some(DiagCategory::Experimental) => "experimental",
            Some(DiagCategory::NonTargetDirective) => "non-target-directive",
            Some(DiagCategory::InstallAbsoluteDestination) => "install-absolute-destination",
            _ => match diag.kind {
                MessageKind::AuthorWarning | MessageKind::AuthorError => "author",
                MessageKind::DeprecationWarning | MessageKind::DeprecationError => "deprecated",
                _ => return,
            },
        };
        let mut message = format!("Use {flag}author");
        if category != "author" {
            let _ = write!(message, " or {flag}{category}");
        }
        let _ = write!(message, " to suppress this {severity}.");
        self.footer("note", message.as_bytes());
    }

    fn snippet(&mut self, file: &SourceFile, labels: &[&Label], name: Option<&[u8]>) {
        let mut rows: BTreeMap<u32, (&[u8], Vec<Mark<'_>>)> = BTreeMap::new();
        let mut anchor = None;
        for label in labels {
            let bytes = clamped_range(file, label.span);
            let first = Line::at(file, bytes.start);
            let last = Line::at(file, bytes.end.saturating_sub(1).max(bytes.start));
            if anchor.is_none_or(|(_, _, primary)| label.primary && !primary) {
                let column = editor_column(first.text, bytes.start - first.offset);
                anchor = Some((first.number, column, label.primary));
            }

            let mut line = first;
            loop {
                let bytes =
                    bytes.start.saturating_sub(line.offset)..bytes.end.saturating_sub(line.offset);
                let message = if line.number == last.number {
                    label.message.as_slice()
                } else {
                    b""
                };
                let mark = Mark {
                    bytes,
                    primary: label.primary,
                    message,
                };
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
        self.location(
            number_width,
            Site {
                path: file.path.as_slice(),
                line,
                name,
                deferred: false,
            },
            column,
        );
        self.gap(number_width);
        let mut previous = None;
        for (number, (text, marks)) in rows {
            if previous.is_some_and(|prev| number - prev > 1) {
                let ellipsis = self.ellipsis();
                let bar = self.bar();
                let _ = writeln!(self.out, "{:number_width$} {bar} {ellipsis}", "");
            }
            previous = Some(number);
            self.marked_line(number_width, number, text, &marks);
        }
        self.gap(number_width);
    }

    fn marked_line(&mut self, number_width: usize, number: u32, text: &[u8], marks: &[Mark<'_>]) {
        let width = self.width - (number_width + 3);
        let bar = self.bar();
        let ellipsis = if width < 10 && !self.opts.unicode {
            "."
        } else {
            self.ellipsis()
        };
        let ellipsis_width = ellipsis.chars().count();
        let cells = Cells::new(text);
        let mut marks: Vec<_> = marks
            .iter()
            .map(|mark| (mark.columns(&cells), mark))
            .collect();
        marks.sort_by_key(|(columns, mark)| (columns.start, !mark.primary));
        let mut marks = marks.as_slice();
        let continuation = format!("{:number_width$} {bar} ", "");

        while let Some((columns, _)) = marks.first() {
            let reserve = usize::from(columns.start == cells.width());
            let window = cells.window(columns.start, width - reserve, ellipsis_width);
            let head = usize::from(window.before) * ellipsis_width;
            let _ = write!(self.out, "{number:>number_width$} {bar} ");
            self.window(&window, ellipsis);
            self.out.push('\n');

            let visible = marks
                .iter()
                .take_while(|(columns, _)| {
                    columns.start < window.end
                        || (!window.after
                            && columns.start == window.end
                            && head + window.end - window.start < width)
                })
                .count()
                .max(1);
            let (visible, rest) = marks.split_at(visible);
            for (columns, mark) in visible {
                let indent = head + columns.start.saturating_sub(window.start);
                let count = columns
                    .end
                    .min(window.end)
                    .saturating_sub(columns.start)
                    .max(1);
                let (symbol, color) = if mark.primary {
                    ('^', self.primary_color)
                } else {
                    ('-', 34)
                };
                let _ = write!(self.out, "{continuation}{:indent$}", "");
                self.paint(color, &symbol.to_string().repeat(count));
                self.wrap(
                    &mark.message.to_str_lossy(),
                    number_width + 3 + indent + count,
                    &continuation,
                    true,
                    true,
                );
            }
            marks = rest;
        }
    }

    fn window(&mut self, window: &Window, ellipsis: &str) {
        if window.before {
            self.out.push_str(ellipsis);
        }
        self.out.push_str(&window.text);
        if window.after {
            self.out.push_str(ellipsis);
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
                let mark = Mark {
                    bytes: start..end,
                    primary: true,
                    message: b"",
                };
                self.location(number_width, site, editor_column(text, start));
                self.gap(number_width);
                self.marked_line(number_width, site.line, text, &[mark]);
                self.gap(number_width);
                return;
            }
        }
        self.location(1, site, 0);
    }

    fn callers(&mut self, location: &Location, lookup: &dyn SourceLookup) {
        let Location::Backtrace(trace) = location else {
            return;
        };
        let mut frames = trace.frames().skip(1).peekable();
        if frames.peek().is_none() {
            return;
        }
        self.out
            .push_str("  Call stack (most recent call first):\n");
        let show_bundled = lookup.show_bundled_frames();
        while let Some(frame) = frames.next() {
            if !lookup.is_bundled_module(&frame.file) {
                self.out.push_str("    ");
                self.site(Site::from_frame(frame), 0);
                continue;
            }
            let mut bundled = vec![frame];
            while let Some(frame) = frames.next_if(|frame| lookup.is_bundled_module(&frame.file)) {
                bundled.push(frame);
            }
            let ellipsis = self.ellipsis();
            let count = bundled.len();
            let noun = if count == 1 { "frame" } else { "frames" };
            let _ = writeln!(self.out, "    {ellipsis} {count} {noun} in CMake modules");
            if show_bundled {
                for frame in bundled {
                    self.out.push_str("      ");
                    self.site(Site::from_frame(frame), 0);
                }
            }
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

fn body_char(ch: char) -> char {
    if ch == '\t' { ch } else { visible_char(ch) }
}

fn inline_text(bytes: &[u8]) -> String {
    bytes.chars().map(visible_char).collect()
}

#[cfg(test)]
mod tests;
