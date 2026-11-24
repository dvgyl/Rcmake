use core::fmt::Write as _;

use rcmake_base::ByteSlice as _;
use rcmake_diag::{Backtrace, Frame};
use rcmake_eval::{ValuePreview, VarOp};
use rcmake_host::{Host, Stream};
use rcmake_model::{AppendMode, PropOp, UsageField, UsageScope};
use rcmake_resolve::ChainKind;

use crate::{ExplainReport, ExplainTarget, ExplainVariable, PreviousValue, SourceLocation};

/// Options for rendering an explain report in a terminal.
#[derive(Clone, Copy, Debug, Default)]
pub struct ExplainRenderOptions<'a> {
    /// Show only this configuration, if specified.
    pub config: Option<&'a [u8]>,
    /// Use ANSI color.
    pub color: bool,
    /// Show all frames in each recorded call stack.
    pub verbose: bool,
}

impl ExplainRenderOptions<'_> {
    /// Use the host's terminal and color settings for this output stream.
    ///
    /// Nonempty `CLICOLOR_FORCE` other than `0` enables color. Then a nonempty
    /// `NO_COLOR` disables color. Otherwise color follows terminal detection.
    #[must_use]
    pub fn for_host(host: &dyn Host, stream: Stream) -> Self {
        let forced = host
            .env_get(b"CLICOLOR_FORCE")
            .is_some_and(|value| !value.is_empty() && value.as_slice() != b"0");
        let disabled = host
            .env_get(b"NO_COLOR")
            .is_some_and(|value| !value.is_empty());
        Self {
            color: forced || (!disabled && host.is_terminal(stream)),
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
    let mut renderer = Renderer {
        text: String::new(),
        options,
    };
    match report {
        ExplainReport::Target(target) => renderer.target(target),
        ExplainReport::Variable(variable) => renderer.variable(variable),
        ExplainReport::Ambiguous { name } => {
            let name = text(name);
            let _ = writeln!(
                renderer.text,
                "{name} names a target and a variable. Use --target or --var."
            );
        }
        ExplainReport::NotFound { name, suggestions } => {
            let _ = writeln!(renderer.text, "No match for {}.", text(name));
            if !suggestions.is_empty() {
                let names = suggestions
                    .iter()
                    .map(|name| text(name))
                    .collect::<Vec<_>>()
                    .join(", ");
                let _ = writeln!(renderer.text, "Similar names  {names}");
            }
        }
    }
    renderer.text
}

struct Renderer<'a> {
    text: String,
    options: ExplainRenderOptions<'a>,
}

impl Renderer<'_> {
    fn target(&mut self, target: &ExplainTarget) {
        let name = self.paint(1, &text(&target.name));
        let _ = write!(self.text, "{name} ({})", target.kind.as_str());
        if target.name != target.target {
            let _ = write!(self.text, "  alias of {}", text(&target.target));
            if let Some(location) = &target.alias_definition {
                self.location(location);
            }
        }
        self.end_trace(&target.definition, 2);
        self.provisional(target.provisional);
        if !target.sources.is_empty() {
            self.text.push_str("  Declared sources\n");
            for source in &target.sources {
                let _ = write!(self.text, "    {}", text(&source.value));
                self.end_trace(&source.backtrace, 6);
            }
        }
        let mut matched = false;
        for config in &target.configurations {
            if self
                .options
                .config
                .is_some_and(|name| !name.eq_ignore_ascii_case(&config.name))
            {
                continue;
            }
            matched = true;
            let name = if config.name.is_empty() {
                "(default)".to_owned()
            } else {
                text(&config.name)
            };
            let _ = writeln!(self.text, "  Configuration {name}");
            for source in &config.sources {
                let _ = write!(self.text, "    {:<21} {}", "Source", text(&source.value));
                self.end_trace(&source.backtrace, 6);
            }
            for entry in &config.entries {
                let _ = write!(
                    self.text,
                    "    {:<21} {}",
                    usage_label(entry.kind),
                    text(&entry.value)
                );
                if entry.chain.is_none() {
                    self.text.push_str("  [chain unavailable]");
                }
                self.end_trace(&entry.origin, 6);
                if let Some(chain) = &entry.chain {
                    for hop in chain {
                        let target = hop.target.as_ref().map_or_else(
                            || format!("target #{}", hop.id.index()),
                            |name| text(name),
                        );
                        let _ = write!(self.text, "      via {:<9} {target}", usage_scope(hop.via));
                        self.end_trace(&hop.origin, 8);
                    }
                }
            }
        }
        if let Some(config) = self.options.config.filter(|_| !matched) {
            let config = text(config);
            let _ = writeln!(self.text, "  Configuration {config} is unavailable.");
        } else if !matched {
            self.text.push_str("  No resolved configurations.\n");
        }
        if !target.properties.is_empty() {
            self.text.push_str("  Property history\n");
            for property in &target.properties {
                let _ = writeln!(
                    self.text,
                    "    {}  {}",
                    text(&property.target),
                    text(&property.name)
                );
                for event in &property.writes {
                    let _ = write!(self.text, "      #{} {}", event.seq, prop_op(event.op));
                    self.end_trace(&event.origin, 8);
                }
            }
        }
        self.history_notice("Property", target.property_history_incomplete);
    }

    fn variable(&mut self, variable: &ExplainVariable) {
        let name = self.paint(1, &text(&variable.name));
        let value = variable
            .value
            .as_ref()
            .map_or_else(|| "(undefined)".into(), |value| quoted(value));
        let _ = writeln!(self.text, "{name} = {value}");
        self.provisional(variable.provisional);
        if let Some(cache) = &variable.cache {
            let force = match cache.force {
                Some(true) => "  FORCE",
                Some(false) => "",
                None => "  [FORCE status unavailable]",
            };
            let _ = writeln!(
                self.text,
                "  Cache {} = {}{force}",
                cache.kind.as_str(),
                quoted(&cache.value)
            );
            if !cache.help.is_empty() {
                let _ = writeln!(self.text, "    Help  {}", text(&cache.help));
            }
        }
        if !variable.writes.is_empty() {
            self.text.push_str("  Write history\n");
        }
        for event in &variable.writes {
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
            let _ = write!(
                self.text,
                "    #{} {command}  scope {}",
                event.sequence, event.scope_depth
            );
            match event.operation {
                VarOp::CacheSet | VarOp::CacheUnset => self.text.push_str("  cache"),
                VarOp::ParentScope => self.text.push_str("  parent scope"),
                VarOp::Set | VarOp::Unset => {}
            }
            if let Some(dir) = event.directory {
                let _ = write!(self.text, "  directory {}", dir.index());
            }
            let previous = match &event.previous {
                PreviousValue::Unknown => "(unknown)".to_owned(),
                PreviousValue::Unset => "(unset)".to_owned(),
                PreviousValue::Value(previous) => preview(previous),
            };
            let next = event
                .value
                .as_ref()
                .map_or_else(|| "(unset)".into(), preview);
            let _ = write!(self.text, "  {previous} -> {next}");
            self.end_trace(&event.origin, 6);
        }
        self.history_notice("Variable", variable.history_incomplete);
    }

    fn history_notice(&mut self, kind: &str, incomplete: Option<bool>) {
        match incomplete {
            Some(false) => {}
            Some(true) => {
                let warning = self.paint(33, "Warning");
                let _ = writeln!(self.text, "  {warning}  {kind} history reached its limit.");
            }
            None => {
                let _ = writeln!(self.text, "  {kind} history is unavailable.");
            }
        }
    }

    fn provisional(&mut self, provisional: bool) {
        if provisional {
            let warning = self.paint(33, "Partial configure");
            let _ = writeln!(self.text, "  [{warning}]");
        }
    }

    fn end_trace(&mut self, trace: &Backtrace, indent: usize) {
        let mut frames = trace.frames();
        if let Some(frame) = frames.next() {
            self.frame(frame);
        }
        self.text.push('\n');
        if self.options.verbose {
            for frame in frames {
                let _ = write!(self.text, "{:indent$}called from", "");
                self.frame(frame);
                self.text.push('\n');
            }
        }
    }

    fn frame(&mut self, frame: &Frame) {
        self.path_line(&frame.file, frame.line);
        if let Some(command) = &frame.name {
            let _ = write!(self.text, " ({})", text(command));
        }
    }

    fn location(&mut self, location: &SourceLocation) {
        self.path_line(&location.path, location.line);
    }

    fn path_line(&mut self, path: &[u8], line: u32) {
        let location = if line == 0 {
            text(path)
        } else {
            format!("{}:{line}", text(path))
        };
        let location = self.paint(36, &location);
        let _ = write!(self.text, "  {location}");
    }

    fn paint(&self, color: u8, label: &str) -> String {
        if self.options.color {
            format!("\x1b[{color}m{label}\x1b[0m")
        } else {
            label.to_owned()
        }
    }
}

fn text(bytes: &[u8]) -> String {
    let mut escaped = String::new();
    for ch in bytes.to_str_lossy().chars() {
        if ch.is_control() {
            escaped.extend(ch.escape_default());
        } else {
            escaped.push(ch);
        }
    }
    escaped
}

fn quoted(bytes: &[u8]) -> String {
    format!("{:?}", bytes.to_str_lossy())
}

fn preview(value: &ValuePreview) -> String {
    let prefix = quoted(&value.prefix);
    if value.len <= value.prefix.len() {
        prefix
    } else {
        format!("{prefix}… ({} bytes, hash {:016x})", value.len, value.hash)
    }
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
