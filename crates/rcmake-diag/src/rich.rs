//! Terminal diagnostics with source excerpts, annotations, and suggested edits.

#![forbid(unsafe_code)]

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::ops::Range;

use rcmake_base::{BString, ByteSlice as _};

use crate::annotate::{Annotations, Confidence, Edit, Fix, Label, SourceSpan};
use crate::{
    DiagCategory, Diagnostic, Effect, Frame, FrameKind, Location, MessageKind, SourceFile,
    SourceId, SourceMap,
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
    let (severity, color) = match diag.kind {
        MessageKind::Error | MessageKind::InternalError => ("error", 31),
        MessageKind::Warning => ("warning", 33),
        MessageKind::AuthorWarning => ("warning", 33),
        MessageKind::AuthorError => ("error", 31),
        MessageKind::DeprecationWarning => ("warning", 33),
        MessageKind::DeprecationError => ("error", 31),
        MessageKind::Log => ("note", 36),
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
    let rule = rich_code(diag);
    let has_specific_code = matches!(
        rule,
        "parse" | "unknown-command" | "policy-version" | "missing-target" | "send-error"
    );
    let header = match diag.policy {
        Some(policy) => format!("{severity}[{rule} {policy}]:"),
        None if diag.category.is_some()
            || has_specific_code
            || fallback.is_some()
            || !groups.is_empty() =>
        {
            format!("{severity}[{rule}]:")
        }
        None => format!("{severity}:"),
    };
    let body = diag.body.to_str_lossy();
    let mut lines = body.lines().peekable();
    let headline = collect_headline(&mut lines);
    let policy_help = if rule == "policy-version" {
        let anchor = primary.and_then(|id| {
            groups.get(&id).and_then(|(file, group_labels)| {
                group_labels
                    .iter()
                    .find(|label| label.primary)
                    .or_else(|| group_labels.first())
                    .map(|label| (id, *file, label.span.start as usize))
            })
        });
        let fix = build_policy_version_fix(anchor);
        // Replace upstream VERSION_ADVICE paragraph with structured help and note.
        while lines.next().is_some() {}
        Some(fix)
    } else {
        None
    };
    renderer.paint(color, &header);
    renderer.wrap(&headline, header.len(), "  ", true, false);

    let has_location = fallback.is_some() || !groups.is_empty();
    if let Some((file, labels)) = primary.and_then(|id| groups.remove(&id)) {
        let site = fallback.filter(|site| site.path == file.path.as_slice() && !site.deferred);
        if site.is_none()
            && let Some(fallback) = fallback.filter(|site| site.name.is_some())
        {
            renderer.fallback(fallback, lookup, diag.body.as_slice());
        }
        renderer.snippet(file, &labels, site.and_then(|site| site.name));
    } else if let Some(site) = fallback {
        renderer.fallback(site, lookup, diag.body.as_slice());
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
    if let Some(fix) = &policy_help {
        renderer.fix(fix, lookup);
        renderer.footer(
            "note",
            b"or pass -DCMAKE_POLICY_VERSION_MINIMUM=3.5 to configure without changes",
        );
    }
    renderer.callers(&diag.location, lookup);
    renderer.suppression(diag);
    renderer.out.push('\n');
    BString::from(renderer.out.into_bytes())
}

fn rich_code(diag: &Diagnostic) -> &'static str {
    if let Some(category) = diag.category {
        return category.sarif_rule_id();
    }
    let body = diag.body.as_slice();
    if body.starts_with(b"Parse error.") || body.starts_with(b"Syntax Error in cmake code") {
        return "parse";
    }
    if body.starts_with(b"Unknown CMake command \"") {
        return "unknown-command";
    }
    if body.starts_with(b"Compatibility with CMake < ")
        && body.find(b"has been removed from CMake.").is_some()
    {
        return "policy-version";
    }
    if body.find(b"but the target was not found.").is_some() {
        return "missing-target";
    }
    if diag.kind == MessageKind::Error
        && diag.effect == Effect::SetError
        && matches!(
            &diag.location,
            Location::Backtrace(trace)
                if trace
                    .top()
                    .and_then(|frame| frame.name.as_deref())
                    .is_some_and(|name| name.eq_ignore_ascii_case(b"message"))
        )
    {
        return "send-error";
    }
    match diag.kind {
        MessageKind::Error | MessageKind::InternalError => match diag.effect {
            Effect::SetError => "CMake.Error",
            _ => "CMake.FatalError",
        },
        MessageKind::Warning => "CMake.Warning",
        MessageKind::AuthorWarning | MessageKind::AuthorError => "CMake.Author",
        MessageKind::DeprecationWarning | MessageKind::DeprecationError => "CMake.Deprecated",
        MessageKind::Log => "CMake.Note",
    }
}

fn collect_headline<'a>(lines: &mut std::iter::Peekable<std::str::Lines<'a>>) -> String {
    let Some(first) = lines.next_if(|line| !line.is_empty() && !line.starts_with([' ', '\t']))
    else {
        return String::new();
    };
    let mut headline = first.to_owned();
    let mut quotes = first.bytes().filter(|&b| b == b'"').count();
    let mut joined = 0;
    while quotes % 2 == 1 && joined < 16 {
        let Some(next) = lines.next() else {
            break;
        };
        joined += 1;
        quotes += next.bytes().filter(|&b| b == b'"').count();
        if !next.is_empty() && !next.starts_with('"') {
            headline.push(' ');
        }
        headline.push_str(next);
    }
    headline
}

fn build_policy_version_fix(anchor: Option<(SourceId, &SourceFile, usize)>) -> Fix {
    let max_ver = format!(
        "{}.{}",
        rcmake_base::CMAKE_VERSION_MAJOR,
        rcmake_base::CMAKE_VERSION_MINOR
    );
    let range = format!("3.5...{max_ver}");
    if let Some((source, file, start)) = anchor
        && let Some((arg_start, arg_end)) = find_version_argument_span(&file.text, start)
        && let (Ok(start_u32), Ok(end_u32)) = (u32::try_from(arg_start), u32::try_from(arg_end))
    {
        return Fix {
            message: "use a version range".into(),
            edits: vec![Edit {
                span: SourceSpan {
                    source,
                    start: start_u32,
                    end: end_u32,
                },
                replacement: range.into(),
            }],
            confidence: Confidence::MachineApplicable,
        };
    }
    Fix {
        message: format!("use a version range (for example {range})").into(),
        edits: Vec::new(),
        confidence: Confidence::MachineApplicable,
    }
}

fn find_version_argument_span(text: &[u8], start: usize) -> Option<(usize, usize)> {
    let slice = text.get(start..)?;
    let open = slice.iter().position(|&b| b == b'(')?;
    let mut pos = start + open + 1;
    let limit = text.len().min(pos + 512);
    let mut saw_version = false;
    while pos < limit {
        while pos < limit && text.get(pos).is_some_and(u8::is_ascii_whitespace) {
            pos += 1;
        }
        let &byte = text.get(pos)?;
        if byte == b')' {
            break;
        }
        if byte == b'#' {
            while pos < limit && text.get(pos) != Some(&b'\n') {
                pos += 1;
            }
            continue;
        }
        let (tok_start, tok_end, next_pos) = if byte == b'"' {
            let content_start = pos + 1;
            let mut end = content_start;
            while end < limit {
                match text.get(end) {
                    Some(b'\\') => end += 2,
                    Some(b'"') => break,
                    Some(_) => end += 1,
                    None => break,
                }
            }
            if text.get(end) != Some(&b'"') {
                return None;
            }
            (content_start, end, end + 1)
        } else {
            let token_start = pos;
            let mut end = pos;
            while end < limit
                && text
                    .get(end)
                    .is_some_and(|&b| !b.is_ascii_whitespace() && !matches!(b, b')' | b'#' | b'"'))
            {
                end += 1;
            }
            (token_start, end, end)
        };
        let token = text.get(tok_start..tok_end)?;
        if saw_version {
            if !token.is_empty() && token.iter().all(|b| b.is_ascii_digit() || *b == b'.') {
                return Some((tok_start, tok_end));
            }
            return None;
        }
        if token.eq_ignore_ascii_case(b"VERSION") {
            saw_version = true;
        }
        pos = next_pos;
    }
    None
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
        let indent = if self.opts.unicode {
            indent + 1
        } else {
            indent
        };
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
        let mut anchor: Option<(_, _, bool)> = None;
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

    fn fallback(&mut self, site: Site<'_>, lookup: &dyn SourceLookup, body: &[u8]) {
        if !site.deferred
            && site.line != 0
            && let Some(file) = lookup.source_by_path(site.path)
        {
            let file_lines: Vec<&[u8]> = file
                .text
                .split_inclusive(|&byte| byte == b'\n')
                .map(|raw| {
                    raw.strip_suffix(b"\n")
                        .map_or(raw, |line| line.strip_suffix(b"\r").unwrap_or(line))
                })
                .collect();

            if body.starts_with(b"Parse error.")
                && body.find(b"unterminated string").is_some()
                && let Some((line_num, text, quote_pos)) =
                    find_unclosed_quote(&file_lines, site.line)
            {
                let mut quote_site = site;
                quote_site.line = line_num;
                let number_width = line_num.to_string().len();
                let mark = Mark {
                    bytes: quote_pos..text.len().max(quote_pos + 1),
                    primary: true,
                    message: b"the string that starts here has no closing quote",
                };
                self.location(number_width, quote_site, editor_column(text, quote_pos));
                self.gap(number_width);
                self.marked_line(number_width, line_num, text, &[mark]);
                self.gap(number_width);
                return;
            }

            let target_line = usize::try_from(site.line.saturating_sub(1))
                .ok()
                .and_then(|idx| file_lines.get(idx).copied().map(|text| (site.line, text)))
                .or_else(|| {
                    body.starts_with(b"Parse error.")
                        .then(|| {
                            file_lines
                                .iter()
                                .enumerate()
                                .rev()
                                .find(|(_, line)| !line.is_empty())
                                .and_then(|(idx, line)| {
                                    u32::try_from(idx + 1).ok().map(|n| (n, *line))
                                })
                        })
                        .flatten()
                });

            if let Some((line_num, text)) = target_line {
                let start = text
                    .iter()
                    .take_while(|byte| byte.is_ascii_whitespace())
                    .count();
                if let Some(name) = site.name.filter(|name| !name.is_empty()) {
                    let end = start.saturating_add(name.len());
                    if text
                        .get(start..end)
                        .is_some_and(|command| command.eq_ignore_ascii_case(name))
                        && text
                            .get(end)
                            .is_none_or(|&byte| byte.is_ascii_whitespace() || byte == b'(')
                    {
                        let number_width = line_num.to_string().len();
                        let mark = Mark {
                            bytes: start..end,
                            primary: true,
                            message: b"",
                        };
                        self.location(number_width, site, editor_column(text, start));
                        self.gap(number_width);
                        self.marked_line(number_width, line_num, text, &[mark]);
                        self.gap(number_width);
                        return;
                    }
                } else {
                    let ident_len = if text
                        .get(start)
                        .is_some_and(|b| b.is_ascii_alphabetic() || *b == b'_')
                    {
                        text.get(start..)
                            .unwrap_or_default()
                            .iter()
                            .take_while(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'))
                            .count()
                    } else {
                        0
                    };
                    let end = start.saturating_add(ident_len);
                    let label = if body.starts_with(b"Parse error.")
                        && body
                            .find(b"Function missing ending \")\".  End of file reached.")
                            .is_some()
                    {
                        b"this command has no closing \")\"".as_slice()
                    } else {
                        b""
                    };
                    let mut render_site = site;
                    render_site.line = line_num;
                    let number_width = line_num.to_string().len();
                    let mark = Mark {
                        bytes: start..end,
                        primary: true,
                        message: label,
                    };
                    self.location(number_width, render_site, editor_column(text, start));
                    self.gap(number_width);
                    self.marked_line(number_width, line_num, text, &[mark]);
                    self.gap(number_width);
                    return;
                }
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

fn find_unclosed_quote<'a>(lines: &[&'a [u8]], site_line: u32) -> Option<(u32, &'a [u8], usize)> {
    let max_idx = usize::try_from(site_line)
        .ok()
        .unwrap_or(lines.len())
        .min(lines.len());
    for idx in (0..max_idx).rev() {
        let line = *lines.get(idx)?;
        let mut open = None;
        let mut pos = 0;
        while pos < line.len() {
            match line.get(pos) {
                Some(b'#') if open.is_none() => break,
                Some(b'\\') if open.is_some() => pos += 2,
                Some(b'"') => {
                    if open.is_some() {
                        open = None;
                    } else {
                        open = Some(pos);
                    }
                    pos += 1;
                }
                _ => pos += 1,
            }
        }
        if let Some(quote_pos) = open {
            return u32::try_from(idx + 1).ok().map(|n| (n, line, quote_pos));
        }
    }
    None
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
