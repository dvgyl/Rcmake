use core::fmt::Write as _;

use rcmake_base::{BString, path_lex::Lexical};
use rcmake_diag::{Backtrace, Frame};
use rcmake_eval::{ValuePreview, VarOp};
use rcmake_host::{Host, Stream, discover_cmake_root, path::from_native};
use rcmake_model::{AppendMode, PropOp, UsageField, UsageScope};
use rcmake_resolve::{ChainKind, LinkFragmentRole};

use crate::{
    ExplainEntry, ExplainReport, ExplainRoots, ExplainTarget, ExplainVariable, ExplainWrite,
    PreviousValue, SourceLocation, VariableHistory,
};

const LINE_WIDTH: usize = 96;
const VALUE_BYTES: usize = 64;

/// Options for rendering an explanation in a terminal.
#[derive(Clone, Debug, Default)]
pub struct ExplainRenderOptions<'a> {
    /// Show only this configuration, if specified.
    pub config: Option<&'a [u8]>,
    /// Use ANSI color.
    pub color: bool,
    /// Emit OSC 8 links for absolute source paths.
    pub hyperlinks: bool,
    /// Absolute directory for the bundled modules, if known.
    pub bundled_modules: Option<BString>,
    /// Show sources, all stack frames, and full effective and cache values.
    pub verbose: bool,
}

impl ExplainRenderOptions<'_> {
    /// Use the host's terminal and color settings for this output stream.
    ///
    /// `CLICOLOR_FORCE=1` enables color. Otherwise, color requires a terminal
    /// and an unset `NO_COLOR`.
    /// Links require a terminal with a known hyperlink capability.
    #[must_use]
    pub fn for_host(host: &dyn Host, stream: Stream) -> Self {
        let terminal = host.is_terminal(stream);
        let forced = host
            .env_get(b"CLICOLOR_FORCE")
            .is_some_and(|value| value == "1");
        let disabled = host.env_get(b"NO_COLOR").is_some();
        let hyperlinks = terminal
            && (host.env_get(b"VTE_VERSION").is_some_and(|value| {
                core::str::from_utf8(&value)
                    .ok()
                    .and_then(|version| version.parse::<u32>().ok())
                    .is_some_and(|version| version >= 5000)
            }) || host
                .env_get(b"WT_SESSION")
                .is_some_and(|value| !value.is_empty())
                || host.env_get(b"TERM_PROGRAM").is_some_and(|value| {
                    matches!(
                        value.as_slice(),
                        b"iTerm.app" | b"WezTerm" | b"vscode" | b"ghostty"
                    )
                }));
        Self {
            color: forced || (!disabled && terminal),
            hyperlinks,
            bundled_modules: discover_cmake_root(host, None)
                .1
                .map(|root| from_native(&root.join("Modules"))),
            ..Self::default()
        }
    }
}

/// Render an explanation as terminal text.
#[must_use]
pub fn render_explain_terminal(
    report: &ExplainReport,
    options: ExplainRenderOptions<'_>,
) -> String {
    let default_roots = ExplainRoots::default();
    let roots = match report {
        ExplainReport::Target(target) => &target.roots,
        ExplainReport::Variable(variable) => &variable.roots,
        _ => &default_roots,
    };
    let mut out = Renderer {
        text: String::new(),
        options,
        roots,
    };
    match report {
        ExplainReport::Target(target) => out.target(target),
        ExplainReport::Variable(variable) => out.variable(variable),
        ExplainReport::Ambiguous { name, .. } => out.row(
            0,
            &format!(
                "{} names a target and a variable. Use --target or --var.",
                text(name)
            ),
        ),
        ExplainReport::NotFound { name, suggestions } => {
            out.row(0, &format!("No match for {}.", text(name)));
            if !suggestions.is_empty() {
                out.row(2, "Similar names");
                for suggestion in suggestions {
                    let flag = match suggestion.kind {
                        crate::ExplainItemKind::Target => "--target",
                        crate::ExplainItemKind::Variable => "--var",
                    };
                    out.row(4, &format!("{flag} {}", text(&suggestion.name)));
                }
            }
        }
        ExplainReport::InvalidDirectory { name, directory } => out.row(
            0,
            &format!(
                "Cannot explain {}. Directory ID {} is invalid for this snapshot.",
                text(name),
                directory.index()
            ),
        ),
    }
    out.text
}

struct Renderer<'a> {
    text: String,
    options: ExplainRenderOptions<'a>,
    roots: &'a ExplainRoots,
}

impl Renderer<'_> {
    fn target(&mut self, target: &ExplainTarget) {
        let name = text(&target.name);
        if let Some(filter) = &target.filter {
            self.filter_headline(target, filter);
        }
        self.styled_row(0, &format!("{name} ({})", target.kind.as_str()), 1, None);
        if target.name != target.target {
            self.row(2, &format!("Alias of {}", text(&target.target)));
            self.location(4, target.alias_definition.as_ref());
        }
        self.trace(2, &target.definition);
        self.provisional(target.provisional);
        if !target.configurations.is_empty() {
            let available = target
                .configurations
                .iter()
                .map(|config| config_name(&config.name))
                .collect::<Vec<_>>()
                .join(", ");
            self.row(2, &format!("Available configurations  {available}"));
        } else {
            self.row(2, "No resolved configurations.");
        }
        if let Some(requested) = self.options.config {
            let status = if target.configurations_for(Some(requested)).next().is_some() {
                "matched"
            } else {
                "unavailable"
            };
            self.row(
                2,
                &format!(
                    "Requested configuration {} {status}.",
                    config_name(requested)
                ),
            );
        }
        for config in target.configurations_for(self.options.config) {
            self.row(2, &format!("Configuration {}", config_name(&config.name)));
            if target.filter.is_some() && config.entries.is_empty() {
                self.row(4, "No entry matches the filter.");
            }
            for entry in &config.entries {
                self.entry(&target.target, entry, config.compile_groups.len());
            }
            if self.options.verbose && target.filter.is_none() {
                for source in &config.sources {
                    self.row(4, &format!("Source {}", self.display_path(&source.value)));
                    self.trace(6, &source.backtrace);
                }
            }
            for (index, group) in config.compile_groups.iter().enumerate() {
                if config.entries.iter().any(|entry| {
                    !entry.compile_positions.is_empty()
                        && entry.compile_positions.len() < config.compile_groups.len()
                        && entry
                            .compile_positions
                            .iter()
                            .any(|position| position.group == index)
                }) {
                    self.row(
                        4,
                        &format!("Compile group {} ({})", index + 1, text(&group.language)),
                    );
                    if self.options.verbose {
                        for source in &group.sources {
                            self.row(6, &self.display_path(source));
                        }
                    }
                }
            }
        }

        if self.options.verbose && target.filter.is_none() && !target.sources.is_empty() {
            self.row(2, "Declared sources");
            for source in &target.sources {
                self.row(4, &self.display_path(&source.value));
                self.trace(6, &source.backtrace);
            }
        }
        let mut properties = target.properties_for(self.options.config).peekable();
        if properties.peek().is_some() {
            self.row(2, "Property history (writes at configure time)");
        }
        for property in properties {
            self.row(
                4,
                &format!("{}  {}", text(&property.target), text(&property.name)),
            );
            if property.configurations.len() > 1 {
                let names = property
                    .configurations
                    .iter()
                    .map(|name| config_name(name))
                    .collect::<Vec<_>>()
                    .join(", ");
                self.row(6, &format!("Shared by {names}"));
            }
            if property.dropped_writes != 0 {
                self.row(
                    6,
                    &format!("{} earlier writes discarded", property.dropped_writes),
                );
            }
            for event in &property.writes {
                self.row(6, &format!("#{} {}", event.seq, prop_op(event.op)));
                self.trace(8, &event.origin);
            }
        }
        match target.property_history_incomplete {
            Some(false) => {}
            Some(true) => self.warning("The session discarded some property writes."),
            None => self.row(2, "Property history is unavailable."),
        }
    }

    fn filter_headline(&mut self, target: &ExplainTarget, filter: &[u8]) {
        let entry = target
            .configurations_for(self.options.config)
            .find_map(|config| config.entries.first());
        let Some(entry) = entry else {
            self.row(
                0,
                &format!("No match for {} in {}.", quoted(filter), text(&target.name)),
            );
            return;
        };
        let value = text(entry.spelling.as_ref().unwrap_or(&entry.value));
        let origin = entry.origin.top().map_or_else(
            || usage_label(entry.kind).to_owned(),
            |frame| {
                let command = frame
                    .name
                    .as_deref()
                    .map_or_else(|| usage_label(entry.kind).to_owned(), text);
                format!(
                    "{command} at {}",
                    self.display_site(&frame.file, frame.line)
                )
            },
        );
        let mut path = text(&target.name);
        for hop in entry.chain.iter().flatten() {
            let _ = write!(path, " -> {} {}", usage_scope(hop.via), hop_name(hop));
        }
        self.styled_row(0, &format!("{value} <- {origin} via {path}"), 1, None);
    }

    fn entry(&mut self, target: &[u8], entry: &ExplainEntry, group_count: usize) {
        let label = match entry.link_role {
            Some(LinkFragmentRole::LibraryPath) => "Library search path",
            Some(LinkFragmentRole::FrameworkPath) => "Framework search path",
            _ => usage_label(entry.kind),
        };
        let value = entry.spelling.as_ref().unwrap_or(&entry.value);
        let mut header = format!("{label}  {}", text(value));
        match &entry.chain {
            None => header.push_str("  [chain unavailable]"),
            Some(chain) if chain.is_empty() => header.push_str("  [direct]"),
            Some(_) => {}
        }
        self.row(4, &header);
        if !entry.compile_positions.is_empty() && entry.compile_positions.len() < group_count {
            let group_ids = entry
                .compile_positions
                .iter()
                .map(|p| (p.group + 1).to_string())
                .collect::<Vec<_>>()
                .join(", ");
            self.row(6, &format!("Compile groups {group_ids}"));
        }
        if let Some(input) = &entry.input {
            self.row(6, &format!("From variable {}", text(&input.name)));
            self.location(8, input.location.as_ref());
        }
        if let Some(chain) = &entry.chain
            && !chain.is_empty()
        {
            let hops: Vec<_> = chain.iter().map(hop_name).collect();
            let mut path = text(target);
            for (hop, name) in chain.iter().zip(&hops) {
                let _ = write!(path, " -> {} {name}", usage_scope(hop.via));
            }
            self.row(6, &path);
            let mut from = text(target);
            for (hop, to) in chain.iter().zip(hops) {
                self.row(8, &format!("{from} -> {} {to}", usage_scope(hop.via)));
                self.trace(10, &hop.origin);
                from = to;
            }
        }
        self.trace(6, &entry.origin);
    }

    fn variable(&mut self, variable: &ExplainVariable) {
        let name = text(&variable.name);
        self.styled_row(0, &name, 1, None);
        self.provisional(variable.provisional);
        let label = match (variable.normal_binding, &variable.cache) {
            (true, Some(_)) => "Effective value (normal variable shadows cache)",
            (true, None) => "Effective value (normal variable)",
            (false, Some(_)) => "Effective value (cache)",
            (false, None) => "Effective value",
        };
        self.row(2, label);
        if let Some(value) = &variable.value {
            self.full_value(4, value);
            self.location(4, variable.location.as_ref());
        } else {
            self.row(4, "(undefined)");
        }
        if !variable.directory.is_empty() {
            self.row(
                2,
                &format!(
                    "Source directory {}",
                    self.display_path(&variable.directory)
                ),
            );
        }
        if let Some(cache) = &variable.cache {
            let force = match cache.force {
                Some(true) => "  FORCE",
                Some(false) => "",
                None => "  [FORCE status unavailable]",
            };
            self.row(2, &format!("Cache entry {}{force}", cache.kind.as_str()));
            self.full_value(4, &cache.value);
            self.location(4, cache.location.as_ref());
            if !cache.help.is_empty() {
                self.row(4, &format!("Help  {}", text(&cache.help)));
            }
        }
        if !variable.writes.is_empty() {
            self.row(2, "Write history");
            for write in &variable.writes {
                self.variable_write(write);
            }
        }
        match variable.history {
            VariableHistory::Unavailable => self.row(2, "Variable history is unavailable."),
            VariableHistory::Excluded => {
                self.row(2, "The recording policy excluded this variable.")
            }
            VariableHistory::Complete if variable.writes.is_empty() => {
                self.row(2, "No writes recorded for this variable.")
            }
            VariableHistory::Complete => {}
            VariableHistory::Truncated => self.warning("Variable history reached its limit."),
            VariableHistory::Unknown => {
                self.warning("The ledger cannot determine whether this variable lost writes.")
            }
        }
        if variable.session_history_truncated == Some(true) {
            self.warning("Variable history reached a session-wide limit.");
        }
    }

    fn variable_write(&mut self, event: &ExplainWrite) {
        let command = event.command.as_ref().map_or_else(
            || {
                event
                    .origin
                    .top()
                    .and_then(|frame| frame.name.as_deref())
                    .map_or_else(|| var_op(event.operation).to_owned(), text)
            },
            |command| text(command),
        );
        let scope = match event.operation {
            VarOp::CacheSet | VarOp::CacheUnset => "cache",
            _ if event.in_queried_scope => "queried directory binding",
            _ if event.binding_directory.is_some() => "other directory binding",
            _ => "local or unknown binding",
        };
        self.row(
            4,
            &format!(
                "#{} {command} ({scope}, scope depth {})",
                event.sequence, event.scope_depth
            ),
        );
        if let Some(dir) = &event.directory_path {
            self.row(6, &format!("Ran in {}", self.display_path(dir)));
        }
        let previous = match &event.previous {
            PreviousValue::Unknown => "(unknown)".to_owned(),
            PreviousValue::Unset => "(unset)".to_owned(),
            PreviousValue::Value(value) => self.preview(value),
        };
        let next = event
            .value
            .as_ref()
            .map_or_else(|| "(unset)".into(), |value| self.preview(value));
        self.row(6, &format!("{previous} -> {next}"));
        self.trace(6, &event.origin);
    }

    fn full_value(&mut self, indent: usize, value: &[u8]) {
        let visible = self.value_prefix(value);
        self.row(indent, &quoted(visible));
        if visible.len() != value.len() {
            self.row(
                indent,
                &format!(
                    "(truncated, {} bytes total; use --verbose or --json for full value)",
                    value.len()
                ),
            );
        }
    }

    fn preview(&self, value: &ValuePreview) -> String {
        let visible = self.value_prefix(&value.prefix);
        if value.len <= visible.len() {
            quoted(visible)
        } else {
            format!(
                "{}… ({} bytes, hash {:016x})",
                quoted(visible),
                value.len,
                value.hash
            )
        }
    }

    fn value_prefix<'a>(&self, value: &'a [u8]) -> &'a [u8] {
        if self.options.verbose || value.len() <= VALUE_BYTES {
            return value;
        }
        let end = core::str::from_utf8(value)
            .map_or(VALUE_BYTES, |value| value.floor_char_boundary(VALUE_BYTES));
        value.get(..end).unwrap_or_default()
    }

    fn warning(&mut self, message: &str) {
        self.styled_row(2, &format!("Warning  {message}"), 33, None);
    }

    fn provisional(&mut self, provisional: bool) {
        if provisional {
            self.styled_row(2, "Partial configure", 33, None);
        }
    }

    fn location(&mut self, indent: usize, location: Option<&SourceLocation>) {
        if let Some(location) = location {
            self.path_line(indent, "At", &location.path, location.line, None);
        } else {
            self.row(indent, "Source location unavailable.");
        }
    }

    fn trace(&mut self, indent: usize, trace: &Backtrace) {
        let mut frames = trace.frames();
        if let Some(frame) = frames.next() {
            self.trace_frame(indent, "At", frame);
        }
        if self.options.verbose {
            for frame in frames {
                self.trace_frame(indent, "Called from", frame);
            }
        } else if let Some(frame) = frames.find(|frame| self.is_project_frame(frame)) {
            self.trace_frame(indent, "Called from", frame);
        }
    }

    fn trace_frame(&mut self, indent: usize, label: &str, frame: &Frame) {
        self.path_line(
            indent,
            label,
            &frame.file,
            frame.line,
            frame.name.as_deref(),
        );
    }

    fn is_project_frame(&self, frame: &Frame) -> bool {
        let path = Lexical::new(&frame.file).normalize();
        relative_inside(&path, &self.roots.source).is_some()
            && !self
                .options
                .bundled_modules
                .as_ref()
                .is_some_and(|modules| relative_inside(&path, modules).is_some())
    }

    fn display_path(&self, path: &[u8]) -> String {
        if path.starts_with(b"/") || Lexical::new(path).absolute() {
            let normalized = Lexical::new(path).normalize();
            for (root, prefix) in [(&self.roots.source, ""), (&self.roots.build, "<build>/")] {
                if let Some(relative) = relative_inside(&normalized, root) {
                    return format!("{prefix}{}", text(&relative));
                }
            }
        }
        text(path)
    }

    fn display_site(&self, path: &[u8], line: u32) -> String {
        let mut display = self.display_path(path);
        if line != 0 {
            let _ = write!(display, ":{line}");
        }
        display
    }

    fn path_line(
        &mut self,
        indent: usize,
        label: &str,
        path: &[u8],
        line: u32,
        command: Option<&[u8]>,
    ) {
        let mut display = format!("{label} {}", self.display_site(path, line));
        if let Some(command) = command {
            let _ = write!(display, " ({})", text(command));
        }
        let uri = self
            .options
            .hyperlinks
            .then(|| file_uri(path, line))
            .flatten();
        self.styled_row(indent, &display, 36, uri.as_deref());
    }

    fn row(&mut self, indent: usize, value: &str) {
        self.styled_row(indent, value, 0, None);
    }

    fn styled_row(&mut self, indent: usize, value: &str, color: u8, link: Option<&str>) {
        let mut remaining = value;
        let mut pad = indent;
        loop {
            let split = line_end(remaining, LINE_WIDTH.saturating_sub(pad));
            let Some((line, rest)) = remaining.split_at_checked(split) else {
                break;
            };
            let _ = write!(self.text, "{:pad$}", "");
            if let Some(link) = link {
                let _ = write!(self.text, "\x1b]8;;{link}\x1b\\");
            }
            if color != 0 && self.options.color {
                let _ = write!(self.text, "\x1b[{color}m{line}\x1b[0m");
            } else {
                self.text.push_str(line);
            }
            if link.is_some() {
                self.text.push_str("\x1b]8;;\x1b\\");
            }
            self.text.push('\n');
            remaining = rest;
            if remaining.is_empty() {
                break;
            }
            pad = indent + 2;
        }
    }
}

fn relative_inside(path: &[u8], root: &[u8]) -> Option<BString> {
    if root.is_empty() {
        return None;
    }
    let root = Lexical::new(root).normalize();
    Lexical::new(path)
        .relative(Lexical::new(&root))
        .filter(|relative| relative != ".." && !relative.starts_with(b"../"))
}

fn hop_name(hop: &crate::ExplainHop) -> String {
    hop.target
        .as_ref()
        .map_or_else(|| format!("target #{}", hop.id.index()), |name| text(name))
}

fn line_end(value: &str, width: usize) -> usize {
    let mut columns = 0;
    let mut space = 0;
    for (offset, ch) in value.char_indices() {
        columns += if ch.is_ascii() { 1 } else { 2 };
        if columns > width {
            return if space == 0 { offset } else { space };
        }
        if ch == ' ' {
            space = offset + 1;
        }
    }
    value.len()
}

fn file_uri(path: &[u8], line: u32) -> Option<String> {
    if !path.starts_with(b"/") && !Lexical::new(path).absolute() {
        return None;
    }
    let path = Lexical::new(path).normalize();
    let mut uri = String::from("file://");
    let mut path = path.as_slice();
    if let Some(unc) = path.strip_prefix(b"//") {
        path = unc;
    } else if !path.starts_with(b"/") {
        uri.push('/');
    }
    for &byte in path {
        if byte.is_ascii_alphanumeric() || b"-._~/:".contains(&byte) {
            uri.push(char::from(byte));
        } else {
            let _ = write!(uri, "%{byte:02X}");
        }
    }
    if line != 0 {
        let _ = write!(uri, "#L{line}");
    }
    Some(uri)
}

fn config_name(name: &[u8]) -> String {
    if name.is_empty() {
        "(default)".to_owned()
    } else {
        text(name)
    }
}

fn text(bytes: &[u8]) -> String {
    escape(bytes, false)
}

fn quoted(bytes: &[u8]) -> String {
    format!("\"{}\"", escape(bytes, true))
}

fn escape(bytes: &[u8], quoted: bool) -> String {
    let mut escaped = String::new();
    for chunk in bytes.utf8_chunks() {
        for ch in chunk.valid().chars() {
            if is_format(ch) {
                let _ = write!(escaped, "<U+{:04X}>", u32::from(ch));
            } else if ch.is_control() || (quoted && matches!(ch, '"' | '\\')) {
                escaped.extend(ch.escape_default());
            } else {
                escaped.push(ch);
            }
        }
        for byte in chunk.invalid() {
            let _ = write!(escaped, "\\x{byte:02x}");
        }
    }
    escaped
}

fn usage_label(kind: ChainKind) -> &'static str {
    match kind {
        ChainKind::Usage(UsageField::CompileDefinitions) => "Compile definition",
        ChainKind::Usage(UsageField::IncludeDirectories) => "Include directory",
        ChainKind::Usage(UsageField::CompileOptions) => "Compile option",
        ChainKind::Usage(UsageField::LinkOptions) => "Link option",
        ChainKind::LinkItem => "Link library",
        ChainKind::Usage(field) => field.name(),
    }
}

pub(crate) fn usage_scope(scope: UsageScope) -> &'static str {
    match scope {
        UsageScope::Private => "PRIVATE",
        UsageScope::Public => "PUBLIC",
        UsageScope::Interface => "INTERFACE",
    }
}

pub(crate) fn var_op(op: VarOp) -> &'static str {
    match op {
        VarOp::Set => "set",
        VarOp::Unset => "unset",
        VarOp::CacheSet => "cache_set",
        VarOp::CacheUnset => "cache_unset",
        VarOp::ParentScope => "parent_scope",
    }
}

pub(crate) fn prop_op(op: PropOp) -> &'static str {
    match op {
        PropOp::Set => "set",
        PropOp::Append(AppendMode::List) => "append",
        PropOp::Append(AppendMode::String) => "append_string",
        PropOp::Unset => "unset",
    }
}

pub(crate) fn chain_kind(kind: ChainKind) -> &'static str {
    match kind {
        ChainKind::LinkItem => "LINK_LIBRARIES",
        ChainKind::Usage(field) => field.name(),
    }
}

fn is_format(ch: char) -> bool {
    // Unicode 17.0 General_Category=Cf.
    matches!(ch,
        '\u{00ad}' | '\u{0600}'..='\u{0605}' | '\u{061c}' | '\u{06dd}' | '\u{070f}' |
        '\u{0890}'..='\u{0891}' | '\u{08e2}' | '\u{180e}' | '\u{200b}'..='\u{200f}' |
        '\u{202a}'..='\u{202e}' | '\u{2060}'..='\u{2064}' | '\u{2066}'..='\u{206f}' |
        '\u{feff}' | '\u{fff9}'..='\u{fffb}' | '\u{110bd}' | '\u{110cd}' |
        '\u{13430}'..='\u{1343f}' | '\u{1bca0}'..='\u{1bca3}' | '\u{1d173}'..='\u{1d17a}' |
        '\u{e0001}' | '\u{e0020}'..='\u{e007f}'
    )
}
