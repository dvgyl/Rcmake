use std::fmt::Write;

use rcmake_json::{ReadOptions, Value, parse};

use crate::{InstrumentError, invalid};

pub(super) enum Location<'a> {
    Absent,
    Root,
    Field(&'a [u8]),
    VersionMember(&'a [u8]),
    Item(&'a [u8], usize),
}

impl Location<'_> {
    fn offset(self, text: &[u8]) -> Option<usize> {
        let mut tokens = Tokens { text, end: 0 };
        let (root, _) = tokens.next()?;
        match self {
            Self::Absent => None,
            Self::Root => Some(root),
            Self::Field(key) => tokens.member(key),
            Self::VersionMember(key) => {
                tokens.end = tokens.member(b"version")? + 1;
                tokens.member(key)
            }
            Self::Item(key, index) => {
                tokens.end = tokens.member(key)? + 1;
                for _ in 0..index {
                    let (_, token) = tokens.next()?;
                    tokens.skip_value(token)?;
                    tokens.next()?;
                }
                tokens.next().map(|(offset, _)| offset)
            }
        }
    }
}

pub(super) struct QueryDiagnostics<'a> {
    text: &'a [u8],
    filename: &'a str,
    output: String,
    count: usize,
}

impl<'a> QueryDiagnostics<'a> {
    pub(super) fn new(text: &'a [u8], filename: &'a str) -> Self {
        Self {
            text,
            filename,
            output: String::new(),
            count: 0,
        }
    }

    pub(super) fn is_empty(&self) -> bool {
        self.output.is_empty()
    }

    pub(super) fn into_error(self) -> InstrumentError {
        invalid(self.output)
    }

    pub(super) fn add(&mut self, location: Location<'_>, message: impl AsRef<str>) {
        // Bound the amount of source text that invalid input can repeat.
        self.count += 1;
        if self.count > 32 || self.output.len() >= 32 * 1024 {
            return;
        }
        let message = message.as_ref();
        if let Some(offset) = location.offset(self.text) {
            let before = self.text.get(..offset).unwrap_or_default();
            let line = before.iter().filter(|&&byte| byte == b'\n').count() + 1;
            let column = before
                .iter()
                .rev()
                .take_while(|&&byte| byte != b'\n')
                .count()
                + 1;
            self.add_line(line, column, message);
        } else {
            self.separate();
            self.output.push_str(message);
        }
    }

    fn separate(&mut self) {
        if !self.output.is_empty() {
            self.output.push('\n');
        }
    }

    pub(super) fn add_line(&mut self, line: usize, column: usize, message: &str) {
        self.separate();
        if !self.filename.is_empty() {
            let _ = write!(self.output, "{}:", self.filename);
        }
        let _ = write!(self.output, "{line}: {message}");
        let source = self
            .text
            .split(|&byte| byte == b'\n')
            .nth(line.saturating_sub(1))
            .unwrap_or_default();
        let source = source.strip_suffix(b"\r").unwrap_or(source);
        let column = column.saturating_sub(1);
        let start = column.saturating_sub(512).min(source.len());
        let end = source.len().min(start + 1024);
        let padding = source
            .get(start..column.min(end))
            .unwrap_or_default()
            .iter()
            .map(|&byte| if byte == b'\t' { '\t' } else { ' ' })
            .collect::<String>();
        let _ = write!(
            self.output,
            "\n{}\n{padding}^",
            String::from_utf8_lossy(source.get(start..end).unwrap_or_default())
        );
    }
}

// Use the shared parser to validate JSON. Walk its tokens only to locate a
// field or array entry in a validation error.
struct Tokens<'a> {
    text: &'a [u8],
    end: usize,
}

impl<'a> Tokens<'a> {
    fn next(&mut self) -> Option<(usize, &'a [u8])> {
        let start = loop {
            let rest = self.text.get(self.end..)?;
            let start = self.end + rest.iter().position(|byte| !byte.is_ascii_whitespace())?;
            let rest = self.text.get(start..)?;
            self.end = if let Some(comment) = rest.strip_prefix(b"//") {
                start
                    + 2
                    + comment
                        .iter()
                        .position(|&byte| matches!(byte, b'\r' | b'\n'))?
            } else if let Some(comment) = rest.strip_prefix(b"/*") {
                start + 4 + comment.windows(2).position(|pair| pair == b"*/")?
            } else {
                break start;
            };
        };
        let rest = self.text.get(start..)?;
        let len = match rest.first()? {
            b'"' => {
                let mut bytes = rest.iter().enumerate().skip(1);
                loop {
                    let (index, byte) = bytes.next()?;
                    match byte {
                        b'"' => break index + 1,
                        b'\\' => {
                            bytes.next()?;
                        }
                        _ => {}
                    }
                }
            }
            b'{' | b'}' | b'[' | b']' | b':' | b',' => 1,
            _ => rest
                .iter()
                .take_while(|byte| !byte.is_ascii_whitespace() && !b",]}/".contains(byte))
                .count(),
        };
        if len == 0 {
            return None;
        }
        self.end = start + len;
        Some((start, self.text.get(start..self.end)?))
    }

    fn skip_value(&mut self, mut token: &'a [u8]) -> Option<()> {
        let mut depth: usize = 0;
        loop {
            match token {
                b"{" | b"[" => depth += 1,
                b"}" | b"]" => depth = depth.saturating_sub(1),
                _ => {}
            }
            if depth == 0 {
                return Some(());
            }
            (_, token) = self.next()?;
        }
    }

    fn member(&mut self, key: &[u8]) -> Option<usize> {
        let mut result = None;
        while let Some((_, name)) = self.next() {
            if name == b"}" {
                break;
            }
            self.next()?;
            let (offset, token) = self.next()?;
            if matches!(parse(name, &ReadOptions::default()), Ok(Value::String(name)) if name.as_slice() == key)
            {
                result = Some(offset);
            }
            self.skip_value(token)?;
            if self.next()?.1 == b"}" {
                break;
            }
        }
        result
    }
}
