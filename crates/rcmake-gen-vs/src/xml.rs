//! XML output for Visual Studio solutions and MSBuild projects.

use std::fmt::Write;

/// Write UTF-8 XML with two-space indentation and CRLF line endings.
pub struct Writer {
    output: String,
    elements: Vec<String>,
    empty_end: &'static str,
}

impl Writer {
    /// Start a document with the specified UTF-8 declaration and optional byte order mark.
    #[must_use]
    pub fn new(bom: bool, encoding: &str) -> Self {
        let mark = if bom { "\u{feff}" } else { "" };
        let mut output = String::with_capacity(4096);
        let _ = write!(
            output,
            "{mark}<?xml version=\"1.0\" encoding=\"{encoding}\"?>\r\n"
        );
        Self {
            output,
            elements: Vec::new(),
            empty_end: " />",
        }
    }

    /// Close empty elements with `/>` and no space before it, as CMake
    /// writes `.slnx` solutions.
    #[must_use]
    pub fn compact(mut self) -> Self {
        self.empty_end = "/>";
        self
    }

    /// Start a document without an XML declaration, as a .NET SDK-style
    /// project file starts, with an optional byte order mark.
    #[must_use]
    pub fn without_declaration(bom: bool) -> Self {
        Self {
            output: if bom {
                "\u{feff}".into()
            } else {
                String::new()
            },
            elements: Vec::new(),
            empty_end: " />",
        }
    }

    fn indent(&mut self) {
        for _ in &self.elements {
            self.output.push_str("  ");
        }
    }

    fn tag(&mut self, name: &str, attrs: &[(&str, &str)]) {
        self.indent();
        let _ = write!(self.output, "<{name}");
        for (name, value) in attrs {
            let _ = write!(self.output, " {name}=\"");
            escape(&mut self.output, value, true);
            self.output.push('"');
        }
    }

    /// Open an element.
    pub fn start(&mut self, name: &str, attrs: &[(&str, &str)]) {
        self.tag(name, attrs);
        self.output.push_str(">\r\n");
        self.elements.push(name.to_owned());
    }

    /// Close the most recent open element.
    pub fn end(&mut self) {
        if let Some(name) = self.elements.pop() {
            self.indent();
            let _ = write!(self.output, "</{name}>\r\n");
        }
    }

    /// Write an empty element.
    pub fn empty(&mut self, name: &str, attrs: &[(&str, &str)]) {
        self.tag(name, attrs);
        self.output.push_str(self.empty_end);
        self.output.push_str("\r\n");
    }

    /// Write an element with text content.
    pub fn text(&mut self, name: &str, attrs: &[(&str, &str)], value: &str) {
        self.tag(name, attrs);
        self.output.push('>');
        escape(&mut self.output, value, false);
        let _ = write!(self.output, "</{name}>\r\n");
    }

    /// Return the XML document and close any open elements.
    #[must_use]
    pub fn finish(mut self) -> String {
        while !self.elements.is_empty() {
            self.end();
        }
        self.output
    }
}

pub(crate) fn escape(output: &mut String, value: &str, attribute: bool) {
    let mut start = 0;
    for (i, ch) in value.char_indices() {
        let replacement = match ch {
            '&' => "&amp;",
            '<' => "&lt;",
            '>' => "&gt;",
            '"' if attribute => "&quot;",
            '\r' => "&#xD;",
            '\n' => "&#xA;",
            '\t' if attribute => "&#x9;",
            '\u{0}'..='\u{8}'
            | '\u{b}'..='\u{c}'
            | '\u{e}'..='\u{1f}'
            | '\u{fffe}'
            | '\u{ffff}' => "\u{fffd}",
            _ => continue,
        };
        if start < i {
            output.push_str(value.get(start..i).unwrap_or_default());
        }
        output.push_str(replacement);
        start = i + ch.len_utf8();
    }
    if start < value.len() {
        output.push_str(value.get(start..).unwrap_or_default());
    }
}

/// Return `true` when `value` can be an element name.
///
/// The rule is the ASCII part of the XML `NCName` rule: a letter or `_`
/// first, then letters, digits, `_`, `.`, or `-`. A user value that fails,
/// such as `My Tool` or `a:b`, must not become an element name.
pub(crate) fn is_name(value: &str) -> bool {
    let mut chars = value.chars();
    chars
        .next()
        .is_some_and(|ch| ch.is_ascii_alphabetic() || ch == '_')
        && chars.all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '.' | '-'))
}

pub(crate) fn unescape_attribute(mut value: &str) -> String {
    let mut out = String::new();
    while let Some((prefix, rest)) = value.split_once('&') {
        out.push_str(prefix);
        let Some((entity, next)) = rest.split_once(';') else {
            out.push('&');
            out.push_str(rest);
            return out;
        };
        let decoded = match entity {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" => Some('\''),
            _ => entity
                .strip_prefix("#x")
                .and_then(|hex| u32::from_str_radix(hex, 16).ok())
                .or_else(|| {
                    entity
                        .strip_prefix('#')
                        .and_then(|decimal| decimal.parse().ok())
                })
                .and_then(char::from_u32),
        };
        if let Some(decoded) = decoded {
            out.push(decoded);
        } else {
            out.push('&');
            out.push_str(entity);
            out.push(';');
        }
        value = next;
    }
    out.push_str(value);
    out
}
