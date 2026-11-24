//! XML output for Visual Studio solutions and MSBuild projects.

use std::fmt::Write;

/// Write UTF-8 XML with two-space indentation and CRLF line endings.
pub struct Writer {
    output: String,
    elements: Vec<String>,
}

impl Writer {
    /// Start a document with the specified UTF-8 declaration and optional byte order mark.
    #[must_use]
    pub fn new(bom: bool, encoding: &str) -> Self {
        let mark = if bom { "\u{feff}" } else { "" };
        Self {
            output: format!("{mark}<?xml version=\"1.0\" encoding=\"{encoding}\"?>\r\n"),
            elements: Vec::new(),
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
        self.output.push_str(" />\r\n");
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

fn escape(output: &mut String, value: &str, attribute: bool) {
    for ch in value.chars() {
        match ch {
            '&' => output.push_str("&amp;"),
            '<' => output.push_str("&lt;"),
            '>' => output.push_str("&gt;"),
            '"' if attribute => output.push_str("&quot;"),
            '\r' => output.push_str("&#xD;"),
            '\n' => output.push_str("&#xA;"),
            '\t' if attribute => output.push_str("&#x9;"),
            '\u{0}'..='\u{8}'
            | '\u{b}'..='\u{c}'
            | '\u{e}'..='\u{1f}'
            | '\u{fffe}'
            | '\u{ffff}' => {
                output.push('\u{fffd}');
            }
            _ => output.push(ch),
        }
    }
}
