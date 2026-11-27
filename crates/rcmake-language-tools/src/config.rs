//! Bounded `.rcmake.toml` configuration parser, schema validator, and nearest-ancestor loader.

#![forbid(unsafe_code)]

use core::fmt;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use rcmake_host::{FileKind, Follow, Host, ReadRange};

use crate::rules_meta::{RuleId, Severity};

/// Name of the project configuration file.
pub const CONFIG_FILE_NAME: &str = ".rcmake.toml";

/// Maximum accepted byte size for a `.rcmake.toml` file (64 KiB).
pub const MAX_CONFIG_BYTES: u64 = 64 * 1024;
const MAX_CONFIG_LINES: usize = 2048;
const MAX_CONFIG_KEYS: usize = 256;
const MAX_ANCESTOR_DEPTH: usize = 64;

/// Minimum allowed `indent_width`.
pub const MIN_INDENT_WIDTH: usize = 1;
/// Maximum allowed `indent_width`.
pub const MAX_INDENT_WIDTH: usize = 16;
/// Default `indent_width`.
pub const DEFAULT_INDENT_WIDTH: usize = 2;

/// Minimum allowed `line_width`.
pub const MIN_LINE_WIDTH: usize = 20;
/// Maximum allowed `line_width`.
pub const MAX_LINE_WIDTH: usize = 1000;
/// Default `line_width`.
pub const DEFAULT_LINE_WIDTH: usize = 100;

/// Formatting policy for ASCII command identifier case.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum CommandCase {
    /// Keep original spelling as written in source.
    #[default]
    Preserve,
    /// Normalize ASCII command names to lowercase.
    Lower,
    /// Normalize ASCII command names to uppercase.
    Upper,
}

impl CommandCase {
    /// Parse `"preserve"`, `"lower"`, or `"upper"`.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "preserve" => Some(Self::Preserve),
            "lower" => Some(Self::Lower),
            "upper" => Some(Self::Upper),
            _ => None,
        }
    }

    /// Canonical string representation.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Preserve => "preserve",
            Self::Lower => "lower",
            Self::Upper => "upper",
        }
    }
}

/// Formatting policy for editable physical line endings.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum EndOfLine {
    /// Keep each existing physical newline (`\n` or `\r\n`) as written.
    #[default]
    Preserve,
    /// Normalize editable physical newlines to LF (`\n`).
    Lf,
    /// Normalize editable physical newlines to CRLF (`\r\n`).
    Crlf,
}

impl EndOfLine {
    /// Parse `"preserve"`, `"lf"`, or `"crlf"`.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "preserve" => Some(Self::Preserve),
            "lf" => Some(Self::Lf),
            "crlf" => Some(Self::Crlf),
            _ => None,
        }
    }

    /// Canonical string representation.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Preserve => "preserve",
            Self::Lf => "lf",
            Self::Crlf => "crlf",
        }
    }
}

/// `[fmt]` table settings from `.rcmake.toml`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FmtConfig {
    /// Optional explicit indentation width in spaces (`1..=16`).
    pub indent_width: Option<usize>,
    /// Optional explicit target line width (`20..=1000`).
    pub line_width: Option<usize>,
    /// Optional command identifier case normalization.
    pub command_case: Option<CommandCase>,
    /// Optional editable line ending normalization.
    pub end_of_line: Option<EndOfLine>,
}

impl FmtConfig {
    /// Effective indent width in spaces (defaults to `2`).
    #[must_use]
    pub fn effective_indent_width(&self) -> usize {
        self.indent_width.unwrap_or(DEFAULT_INDENT_WIDTH)
    }

    /// Effective line width in display columns (defaults to `100`).
    #[must_use]
    pub fn effective_line_width(&self) -> usize {
        self.line_width.unwrap_or(DEFAULT_LINE_WIDTH)
    }

    /// Effective command identifier case policy (defaults to [`CommandCase::Preserve`]).
    #[must_use]
    pub fn effective_command_case(&self) -> CommandCase {
        self.command_case.unwrap_or_default()
    }

    /// Effective line ending policy (defaults to [`EndOfLine::Preserve`]).
    #[must_use]
    pub fn effective_end_of_line(&self) -> EndOfLine {
        self.end_of_line.unwrap_or_default()
    }
}

/// `[lint]` and `[lint.severity]` settings from `.rcmake.toml`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LintConfig {
    /// Disabled rule identifiers.
    pub disable: BTreeSet<RuleId>,
    /// Explicit per-rule severity overrides.
    pub severity: BTreeMap<RuleId, Severity>,
}

impl LintConfig {
    /// Return `true` if `rule` is enabled under this configuration.
    #[must_use]
    pub fn is_enabled(&self, rule: RuleId) -> bool {
        !self.disable.contains(&rule)
    }

    /// Effective severity for `rule` (configured override or rule default).
    #[must_use]
    pub fn effective_severity(&self, rule: RuleId) -> Severity {
        self.severity
            .get(&rule)
            .copied()
            .unwrap_or_else(|| rule.default_severity())
    }
}

/// Complete parsed `.rcmake.toml` configuration.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Config {
    /// Glob/path patterns excluded during directory discovery (`fmt` and `lint`).
    pub exclude: Vec<String>,
    /// Formatter settings (`[fmt]`).
    pub fmt: FmtConfig,
    /// Linter settings (`[lint]` and `[lint.severity]`).
    pub lint: LintConfig,
}

/// Located error produced while reading or parsing `.rcmake.toml`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConfigError {
    /// Configuration file path, if known.
    pub path: Option<PathBuf>,
    /// 1-based line number (`0` for file-level I/O or size errors).
    pub line: u32,
    /// 1-based column number (`0` if line-only).
    pub column: u32,
    /// Actionable error message.
    pub message: String,
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(path) = &self.path {
            if self.line > 0 {
                if self.column > 0 {
                    write!(
                        f,
                        "{}:{}:{}: {}",
                        path.display(),
                        self.line,
                        self.column,
                        self.message
                    )
                } else {
                    write!(f, "{}:{}: {}", path.display(), self.line, self.message)
                }
            } else {
                write!(f, "{}: {}", path.display(), self.message)
            }
        } else if self.line > 0 {
            write!(f, "{}:{}: {}", self.line, self.column, self.message)
        } else {
            f.write_str(&self.message)
        }
    }
}

impl std::error::Error for ConfigError {}

/// Result of resolving `.rcmake.toml` for a directory, including watched ancestor directories.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolvedConfig {
    /// Effective configuration (defaults when no `.rcmake.toml` exists).
    pub config: Config,
    /// Path to the loaded `.rcmake.toml`, if one was found.
    pub config_path: Option<PathBuf>,
    /// All ancestor directories probed up to the chosen config file or workspace boundary.
    pub checked_dirs: Vec<PathBuf>,
}

/// Parse a `.rcmake.toml` document from raw bytes using a bounded TOML subset parser.
///
/// Rejects unknown tables, duplicate tables, duplicate keys, unknown keys, wrong
/// value types, invalid enums, unknown rule IDs, and out-of-range integers.
pub fn parse_config(bytes: &[u8], path: Option<&Path>) -> Result<Config, ConfigError> {
    if bytes.len() as u64 > MAX_CONFIG_BYTES {
        return Err(ConfigError {
            path: path.map(Path::to_owned),
            line: 0,
            column: 0,
            message: format!("Configuration file exceeds the {MAX_CONFIG_BYTES} byte limit."),
        });
    }
    let body = bytes.strip_prefix(b"\xef\xbb\xbf").unwrap_or(bytes);
    let text = core::str::from_utf8(body).map_err(|_| ConfigError {
        path: path.map(Path::to_owned),
        line: 1,
        column: 1,
        message: "Configuration file must be valid UTF-8.".into(),
    })?;
    let mut parser = TomlParser::new(text, path);
    parser.parse()
}

/// Resolve the nearest ancestor `.rcmake.toml` starting at `start_dir`, bounded by
/// `boundary_root` (inclusive) if provided.
pub fn resolve_config(
    host: &dyn Host,
    start_dir: &Path,
    boundary_root: Option<&Path>,
) -> Result<ResolvedConfig, ConfigError> {
    let mut checked_dirs = Vec::new();
    let mut current = Some(start_dir);
    let mut depth = 0usize;

    while let Some(dir) = current {
        if depth >= MAX_ANCESTOR_DEPTH {
            break;
        }
        depth += 1;
        checked_dirs.push(dir.to_owned());

        let candidate = dir.join(CONFIG_FILE_NAME);
        if let Ok(meta) = host.metadata(&candidate, Follow::Yes)
            && meta.kind == FileKind::File
        {
            if meta.len > MAX_CONFIG_BYTES {
                return Err(ConfigError {
                    path: Some(candidate),
                    line: 0,
                    column: 0,
                    message: format!(
                        "Configuration file exceeds the {MAX_CONFIG_BYTES} byte limit."
                    ),
                });
            }
            let bytes = host
                .read(
                    &candidate,
                    ReadRange::All {
                        limit: MAX_CONFIG_BYTES,
                    },
                )
                .map_err(|err| ConfigError {
                    path: Some(candidate.clone()),
                    line: 0,
                    column: 0,
                    message: format!("Failed to read configuration: {err}"),
                })?;
            let config = parse_config(&bytes, Some(&candidate))?;
            return Ok(ResolvedConfig {
                config,
                config_path: Some(candidate),
                checked_dirs,
            });
        }

        if let Some(root) = boundary_root
            && dir == root
        {
            break;
        }
        current = dir.parent();
        if let Some(next) = current
            && let Some(root) = boundary_root
            && !next.starts_with(root)
        {
            break;
        }
    }

    Ok(ResolvedConfig {
        config: Config::default(),
        config_path: None,
        checked_dirs,
    })
}

/// Directory-keyed cache of resolved `.rcmake.toml` configurations for one CLI run or LSP session.
#[derive(Clone, Debug, Default)]
pub struct ConfigCache {
    by_dir: BTreeMap<PathBuf, Result<ResolvedConfig, ConfigError>>,
}

impl ConfigCache {
    /// Resolve and cache configuration for `dir` bounded by `boundary_root`.
    pub fn resolve(
        &mut self,
        host: &dyn Host,
        dir: &Path,
        boundary_root: Option<&Path>,
    ) -> Result<ResolvedConfig, ConfigError> {
        if let Some(cached) = self.by_dir.get(dir) {
            return cached.clone();
        }
        let result = resolve_config(host, dir, boundary_root);
        self.by_dir.insert(dir.to_owned(), result.clone());
        result
    }

    /// Clear all cached configuration entries (for example after `.rcmake.toml` changes).
    pub fn clear(&mut self) {
        self.by_dir.clear();
    }
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Table {
    Root,
    Fmt,
    Lint,
    LintSeverity,
}

enum TomlValue {
    String(String),
    Int(i64),
    Bool,
    StringArray(Vec<(String, u32, u32)>),
}

struct TomlParser<'a> {
    src: &'a str,
    bytes: &'a [u8],
    pos: usize,
    line: u32,
    col: u32,
    path: Option<PathBuf>,
}

impl<'a> TomlParser<'a> {
    fn new(src: &'a str, path: Option<&Path>) -> Self {
        Self {
            src,
            bytes: src.as_bytes(),
            pos: 0,
            line: 1,
            col: 1,
            path: path.map(Path::to_owned),
        }
    }

    fn err(&self, line: u32, column: u32, message: impl Into<String>) -> ConfigError {
        ConfigError {
            path: self.path.clone(),
            line,
            column,
            message: message.into(),
        }
    }

    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.pos).copied()
    }

    fn bump_byte(&mut self) -> Option<u8> {
        let byte = self.peek()?;
        self.pos += 1;
        if byte == b'\n' {
            self.line = self.line.saturating_add(1);
            self.col = 1;
        } else {
            self.col = self.col.saturating_add(1);
        }
        Some(byte)
    }

    fn skip_horizontal_space(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\t')) {
            self.bump_byte();
        }
    }

    fn skip_comment(&mut self) {
        if self.peek() == Some(b'#') {
            while let Some(b) = self.peek() {
                if b == b'\n' || b == b'\r' {
                    break;
                }
                self.bump_byte();
            }
        }
    }

    fn skip_ws_and_comments(&mut self) -> Result<(), ConfigError> {
        loop {
            self.skip_horizontal_space();
            match self.peek() {
                Some(b'#') => self.skip_comment(),
                Some(b'\r') => {
                    self.bump_byte();
                    if self.peek() == Some(b'\n') {
                        self.bump_byte();
                    }
                }
                Some(b'\n') => {
                    self.bump_byte();
                }
                _ => break,
            }
            if self.line as usize > MAX_CONFIG_LINES {
                return Err(self.err(
                    self.line,
                    self.col,
                    format!("Configuration file exceeds the {MAX_CONFIG_LINES} line limit."),
                ));
            }
        }
        Ok(())
    }

    fn expect_line_end(&mut self) -> Result<(), ConfigError> {
        self.skip_horizontal_space();
        if self.peek() == Some(b'#') {
            self.skip_comment();
        }
        match self.peek() {
            None => Ok(()),
            Some(b'\n') => {
                self.bump_byte();
                Ok(())
            }
            Some(b'\r') => {
                self.bump_byte();
                if self.peek() == Some(b'\n') {
                    self.bump_byte();
                    Ok(())
                } else {
                    Err(self.err(self.line, self.col, "Bare carriage return is not allowed."))
                }
            }
            Some(_) => Err(self.err(
                self.line,
                self.col,
                "Unexpected trailing characters on line.",
            )),
        }
    }

    fn parse(&mut self) -> Result<Config, ConfigError> {
        let mut config = Config::default();
        let mut current_table = Table::Root;
        let mut seen_tables = BTreeSet::new();
        let mut seen_keys: BTreeSet<(Table, String)> = BTreeSet::new();
        let mut key_count = 0usize;

        loop {
            self.skip_ws_and_comments()?;
            let Some(byte) = self.peek() else {
                break;
            };
            let start_line = self.line;
            let start_col = self.col;

            if byte == b'[' {
                let table = self.parse_table_header()?;
                if !seen_tables.insert(table) {
                    return Err(self.err(start_line, start_col, "Duplicate table header."));
                }
                current_table = table;
                self.expect_line_end()?;
                continue;
            }

            key_count += 1;
            if key_count > MAX_CONFIG_KEYS {
                return Err(self.err(
                    start_line,
                    start_col,
                    format!("Configuration file exceeds the {MAX_CONFIG_KEYS} key limit."),
                ));
            }

            let key = self.parse_key()?;
            self.skip_horizontal_space();
            if self.peek() != Some(b'=') {
                return Err(self.err(self.line, self.col, "Expected '=' after configuration key."));
            }
            self.bump_byte();
            self.skip_horizontal_space();

            let val_line = self.line;
            let val_col = self.col;
            let value = self.parse_value()?;
            self.expect_line_end()?;

            if !seen_keys.insert((current_table, key.clone())) {
                return Err(self.err(start_line, start_col, format!("Duplicate key '{key}'.")));
            }

            self.apply_entry(
                &mut config,
                current_table,
                &key,
                value,
                start_line,
                start_col,
                val_line,
                val_col,
            )?;
        }

        Ok(config)
    }

    fn parse_table_header(&mut self) -> Result<Table, ConfigError> {
        let start_line = self.line;
        let start_col = self.col;
        self.bump_byte(); // '['
        if self.peek() == Some(b'[') {
            return Err(self.err(
                start_line,
                start_col,
                "Array-of-tables syntax is not supported.",
            ));
        }
        let mut parts = Vec::new();
        loop {
            self.skip_horizontal_space();
            parts.push(self.parse_key()?);
            self.skip_horizontal_space();
            match self.peek() {
                Some(b'.') => {
                    self.bump_byte();
                }
                Some(b']') => {
                    self.bump_byte();
                    break;
                }
                _ => {
                    return Err(self.err(
                        self.line,
                        self.col,
                        "Expected ']' to close table header.",
                    ));
                }
            }
        }
        match parts.as_slice() {
            [a] if a == "fmt" => Ok(Table::Fmt),
            [a] if a == "lint" => Ok(Table::Lint),
            [a, b] if a == "lint" && b == "severity" => Ok(Table::LintSeverity),
            _ => Err(self.err(
                start_line,
                start_col,
                format!("Unknown configuration table '[{}]'.", parts.join(".")),
            )),
        }
    }

    fn parse_key(&mut self) -> Result<String, ConfigError> {
        let start_line = self.line;
        let start_col = self.col;
        match self.peek() {
            Some(b'"') => self.parse_basic_string(),
            Some(b'\'') => self.parse_literal_string(),
            Some(b) if is_bare_key_byte(b) => {
                let start = self.pos;
                while self.peek().is_some_and(is_bare_key_byte) {
                    self.bump_byte();
                }
                let text = self.src.get(start..self.pos).unwrap_or_default();
                Ok(text.to_owned())
            }
            _ => Err(self.err(start_line, start_col, "Expected a configuration key.")),
        }
    }

    fn parse_value(&mut self) -> Result<TomlValue, ConfigError> {
        let start_line = self.line;
        let start_col = self.col;
        match self.peek() {
            Some(b'"') => self.parse_basic_string().map(TomlValue::String),
            Some(b'\'') => self.parse_literal_string().map(TomlValue::String),
            Some(b'[') => self.parse_string_array().map(TomlValue::StringArray),
            Some(b't' | b'f') => self.parse_bool().map(|_| TomlValue::Bool),
            Some(b'0'..=b'9' | b'+' | b'-') => self.parse_int().map(TomlValue::Int),
            _ => Err(self.err(start_line, start_col, "Expected a TOML value.")),
        }
    }

    fn parse_basic_string(&mut self) -> Result<String, ConfigError> {
        let start_line = self.line;
        let start_col = self.col;
        self.bump_byte(); // '"'
        if self.bytes.get(self.pos..self.pos + 2) == Some(b"\"\"") {
            return Err(self.err(
                start_line,
                start_col,
                "Multiline strings are not supported.",
            ));
        }
        let mut out = String::new();
        while let Some(byte) = self.peek() {
            match byte {
                b'"' => {
                    self.bump_byte();
                    return Ok(out);
                }
                b'\n' | b'\r' => {
                    return Err(self.err(start_line, start_col, "Unterminated string literal."));
                }
                b'\\' => {
                    self.bump_byte();
                    match self.bump_byte() {
                        Some(b'"') => out.push('"'),
                        Some(b'\\') => out.push('\\'),
                        Some(b'n') => out.push('\n'),
                        Some(b'r') => out.push('\r'),
                        Some(b't') => out.push('\t'),
                        _ => {
                            return Err(self.err(
                                self.line,
                                self.col,
                                "Unsupported escape sequence in string literal.",
                            ));
                        }
                    }
                }
                _ => {
                    let tail = self.src.get(self.pos..).unwrap_or_default();
                    let Some(ch) = tail.chars().next() else {
                        break;
                    };
                    if ch.is_control() && ch != '\t' {
                        return Err(self.err(
                            self.line,
                            self.col,
                            "Control character is not allowed in string literal.",
                        ));
                    }
                    out.push(ch);
                    self.pos += ch.len_utf8();
                    self.col = self.col.saturating_add(1);
                }
            }
        }
        Err(self.err(start_line, start_col, "Unterminated string literal."))
    }

    fn parse_literal_string(&mut self) -> Result<String, ConfigError> {
        let start_line = self.line;
        let start_col = self.col;
        self.bump_byte(); // '\''
        if self.bytes.get(self.pos..self.pos + 2) == Some(b"''") {
            return Err(self.err(
                start_line,
                start_col,
                "Multiline strings are not supported.",
            ));
        }
        let mut out = String::new();
        while let Some(byte) = self.peek() {
            match byte {
                b'\'' => {
                    self.bump_byte();
                    return Ok(out);
                }
                b'\n' | b'\r' => {
                    return Err(self.err(start_line, start_col, "Unterminated literal string."));
                }
                _ => {
                    let tail = self.src.get(self.pos..).unwrap_or_default();
                    let Some(ch) = tail.chars().next() else {
                        break;
                    };
                    if ch.is_control() && ch != '\t' {
                        return Err(self.err(
                            self.line,
                            self.col,
                            "Control character is not allowed in literal string.",
                        ));
                    }
                    out.push(ch);
                    self.pos += ch.len_utf8();
                    self.col = self.col.saturating_add(1);
                }
            }
        }
        Err(self.err(start_line, start_col, "Unterminated literal string."))
    }

    fn parse_bool(&mut self) -> Result<bool, ConfigError> {
        let start_line = self.line;
        let start_col = self.col;
        let tail = self.src.get(self.pos..).unwrap_or_default();
        if tail.starts_with("true")
            && !tail
                .as_bytes()
                .get(4)
                .copied()
                .is_some_and(is_bare_key_byte)
        {
            for _ in 0..4 {
                self.bump_byte();
            }
            return Ok(true);
        }
        if tail.starts_with("false")
            && !tail
                .as_bytes()
                .get(5)
                .copied()
                .is_some_and(is_bare_key_byte)
        {
            for _ in 0..5 {
                self.bump_byte();
            }
            return Ok(false);
        }
        Err(self.err(start_line, start_col, "Invalid value syntax."))
    }

    fn parse_int(&mut self) -> Result<i64, ConfigError> {
        let start_line = self.line;
        let start_col = self.col;
        let start = self.pos;
        if matches!(self.peek(), Some(b'+' | b'-')) {
            self.bump_byte();
        }
        let digits_start = self.pos;
        while let Some(b) = self.peek() {
            if b.is_ascii_digit() {
                self.bump_byte();
            } else if b == b'_' {
                return Err(self.err(
                    self.line,
                    self.col,
                    "Underscores in integer literals are not supported.",
                ));
            } else if is_bare_key_byte(b) || b == b'.' {
                return Err(self.err(start_line, start_col, "Invalid integer value."));
            } else {
                break;
            }
        }
        let digits = self.src.get(digits_start..self.pos).unwrap_or_default();
        if digits.is_empty() || (digits.len() > 1 && digits.starts_with('0')) {
            return Err(self.err(start_line, start_col, "Invalid integer value."));
        }
        let full = self.src.get(start..self.pos).unwrap_or_default();
        full.parse::<i64>()
            .map_err(|_| self.err(start_line, start_col, "Integer value is out of range."))
    }

    fn parse_string_array(&mut self) -> Result<Vec<(String, u32, u32)>, ConfigError> {
        let open_line = self.line;
        let open_col = self.col;
        self.bump_byte(); // '['
        let mut items = Vec::new();
        loop {
            self.skip_ws_and_comments()?;
            if self.peek() == Some(b']') {
                self.bump_byte();
                return Ok(items);
            }
            let item_line = self.line;
            let item_col = self.col;
            let item = match self.peek() {
                Some(b'"') => self.parse_basic_string()?,
                Some(b'\'') => self.parse_literal_string()?,
                None => {
                    return Err(self.err(open_line, open_col, "Unterminated array literal."));
                }
                _ => {
                    return Err(self.err(
                        item_line,
                        item_col,
                        "Expected a string element in array.",
                    ));
                }
            };
            if items.len() >= MAX_CONFIG_KEYS {
                return Err(self.err(item_line, item_col, "Array exceeds maximum entry limit."));
            }
            items.push((item, item_line, item_col));
            self.skip_ws_and_comments()?;
            match self.peek() {
                Some(b',') => {
                    self.bump_byte();
                }
                Some(b']') => {
                    self.bump_byte();
                    return Ok(items);
                }
                None => {
                    return Err(self.err(open_line, open_col, "Unterminated array literal."));
                }
                _ => {
                    return Err(self.err(
                        self.line,
                        self.col,
                        "Expected ',' or ']' after array element.",
                    ));
                }
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn apply_entry(
        &self,
        config: &mut Config,
        table: Table,
        key: &str,
        value: TomlValue,
        key_line: u32,
        key_col: u32,
        val_line: u32,
        val_col: u32,
    ) -> Result<(), ConfigError> {
        match table {
            Table::Root => match key {
                "exclude" => {
                    let TomlValue::StringArray(items) = value else {
                        return Err(self.err(
                            val_line,
                            val_col,
                            "Expected an array of path/glob pattern strings for exclude.",
                        ));
                    };
                    for (pattern, line, col) in items {
                        if pattern.trim().is_empty() {
                            return Err(self.err(
                                line,
                                col,
                                "Exclude pattern in 'exclude' must not be empty.",
                            ));
                        }
                        config.exclude.push(pattern);
                    }
                    Ok(())
                }
                _ => Err(self.err(
                    key_line,
                    key_col,
                    format!(
                        "Top-level key '{key}' is not supported; only 'exclude' is allowed at top level (place other settings under [fmt], [lint], or [lint.severity])."
                    ),
                )),
            },
            Table::Fmt => match key {
                "indent_width" => {
                    let TomlValue::Int(n) = value else {
                        return Err(self.err(
                            val_line,
                            val_col,
                            "Expected an integer for fmt.indent_width.",
                        ));
                    };
                    let width = usize::try_from(n).ok().filter(|w| {
                        (MIN_INDENT_WIDTH..=MAX_INDENT_WIDTH).contains(w)
                    });
                    let Some(width) = width else {
                        return Err(self.err(
                            val_line,
                            val_col,
                            format!(
                                "fmt.indent_width must be between {MIN_INDENT_WIDTH} and {MAX_INDENT_WIDTH}."
                            ),
                        ));
                    };
                    config.fmt.indent_width = Some(width);
                    Ok(())
                }
                "line_width" => {
                    let TomlValue::Int(n) = value else {
                        return Err(self.err(
                            val_line,
                            val_col,
                            "Expected an integer for fmt.line_width.",
                        ));
                    };
                    let width = usize::try_from(n).ok().filter(|w| {
                        (MIN_LINE_WIDTH..=MAX_LINE_WIDTH).contains(w)
                    });
                    let Some(width) = width else {
                        return Err(self.err(
                            val_line,
                            val_col,
                            format!(
                                "fmt.line_width must be between {MIN_LINE_WIDTH} and {MAX_LINE_WIDTH}."
                            ),
                        ));
                    };
                    config.fmt.line_width = Some(width);
                    Ok(())
                }
                "command_case" => {
                    let TomlValue::String(s) = value else {
                        return Err(self.err(
                            val_line,
                            val_col,
                            "Expected a string for fmt.command_case.",
                        ));
                    };
                    let Some(parsed) = CommandCase::parse(&s) else {
                        return Err(self.err(
                            val_line,
                            val_col,
                            format!(
                                "Invalid fmt.command_case '{s}'; expected 'preserve', 'lower', or 'upper'."
                            ),
                        ));
                    };
                    config.fmt.command_case = Some(parsed);
                    Ok(())
                }
                "end_of_line" => {
                    let TomlValue::String(s) = value else {
                        return Err(self.err(
                            val_line,
                            val_col,
                            "Expected a string for fmt.end_of_line.",
                        ));
                    };
                    let Some(parsed) = EndOfLine::parse(&s) else {
                        return Err(self.err(
                            val_line,
                            val_col,
                            format!(
                                "Invalid fmt.end_of_line '{s}'; expected 'preserve', 'lf', or 'crlf'."
                            ),
                        ));
                    };
                    config.fmt.end_of_line = Some(parsed);
                    Ok(())
                }
                _ => Err(self.err(key_line, key_col, format!("Unknown [fmt] key '{key}'."))),
            },
            Table::Lint => match key {
                "disable" => {
                    let TomlValue::StringArray(items) = value else {
                        return Err(self.err(
                            val_line,
                            val_col,
                            "Expected an array of rule ID strings for lint.disable.",
                        ));
                    };
                    for (name, line, col) in items {
                        let Some(rule) = RuleId::parse(&name) else {
                            return Err(self.err(
                                line,
                                col,
                                format!("Unknown lint rule '{name}' in lint.disable."),
                            ));
                        };
                        config.lint.disable.insert(rule);
                    }
                    Ok(())
                }
                _ => Err(self.err(key_line, key_col, format!("Unknown [lint] key '{key}'."))),
            },
            Table::LintSeverity => {
                let Some(rule) = RuleId::parse(key) else {
                    return Err(self.err(
                        key_line,
                        key_col,
                        format!("Unknown lint rule '{key}' in [lint.severity]."),
                    ));
                };
                let TomlValue::String(s) = value else {
                    return Err(self.err(
                        val_line,
                        val_col,
                        format!("Expected a severity string for lint.severity.{key}."),
                    ));
                };
                let Some(sev) = Severity::parse(&s) else {
                    return Err(self.err(
                        val_line,
                        val_col,
                        format!(
                            "Invalid severity '{s}' for '{key}'; expected 'note', 'warning', or 'error'."
                        ),
                    ));
                };
                config.lint.severity.insert(rule, sev);
                Ok(())
            }
        }
    }
}

fn is_bare_key_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-'
}

#[cfg(test)]
mod tests {
    use rcmake_host::PureHost;

    use super::*;

    #[test]
    fn parses_valid_rcmake_toml() {
        let src = br#"
# Project config
exclude = [
  "third_party/**",
  "generated/*.cmake",
]

[fmt]
indent_width = 4
line_width = 120
command_case = "lower"
end_of_line = "lf"

[lint]
disable = [
  "directory-options", # trailing comma ok
]

[lint.severity]
unguarded-build-type = "error"
old-policy = 'note'
"#;
        let cfg = parse_config(src, Some(Path::new("/work/.rcmake.toml"))).expect("valid config");
        assert_eq!(
            cfg.exclude,
            vec!["third_party/**".to_owned(), "generated/*.cmake".to_owned()]
        );
        assert_eq!(cfg.fmt.indent_width, Some(4));
        assert_eq!(cfg.fmt.line_width, Some(120));
        assert_eq!(cfg.fmt.command_case, Some(CommandCase::Lower));
        assert_eq!(cfg.fmt.end_of_line, Some(EndOfLine::Lf));
        assert!(!cfg.lint.is_enabled(RuleId::DirectoryOptions));
        assert!(cfg.lint.is_enabled(RuleId::OldPolicy));
        assert_eq!(
            cfg.lint.effective_severity(RuleId::UnguardedBuildType),
            Severity::Error
        );
        assert_eq!(
            cfg.lint.effective_severity(RuleId::OldPolicy),
            Severity::Note
        );
    }

    #[test]
    fn rejects_invalid_config_cases() {
        let cases: &[(&str, &str)] = &[
            (
                "[fmt]\nindent_width = 2\nindent_width = 4\n",
                "Duplicate key",
            ),
            ("[fmt]\n[fmt]\n", "Duplicate table"),
            ("[fmt]\nindent_width = \"2\"\n", "Expected an integer"),
            ("[fmt]\nindent_width = 0\n", "must be between"),
            ("[fmt]\nindent_width = 99\n", "must be between"),
            ("[fmt]\nline_width = 10\n", "must be between"),
            (
                "[fmt]\ncommand_case = \"camel\"\n",
                "Invalid fmt.command_case",
            ),
            ("[fmt]\nend_of_line = \"cr\"\n", "Invalid fmt.end_of_line"),
            ("[lint]\ndisable = [\"bogus-rule\"]\n", "Unknown lint rule"),
            (
                "[lint.severity]\nold-policy = \"fatal\"\n",
                "Invalid severity",
            ),
            ("[unknown]\nfoo = 1\n", "Unknown configuration table"),
            ("indent_width = 2\n", "Top-level key"),
            ("exclude = \"vendor\"\n", "Expected an array"),
            ("exclude = [\"\"]\n", "must not be empty"),
        ];
        for &(src, expected) in cases {
            let err = parse_config(src.as_bytes(), None).expect_err(src);
            assert!(
                err.message.contains(expected),
                "expected '{expected}' in '{}' for:\n{src}",
                err.message
            );
        }
    }

    #[test]
    fn nearest_ancestor_config_respects_boundary() {
        let host = PureHost::default();
        host.seed_file(
            Path::new("/outside/.rcmake.toml"),
            b"[fmt]\nindent_width = 8\n",
        )
        .unwrap();
        host.seed_file(
            Path::new("/outside/proj/.rcmake.toml"),
            b"[fmt]\nindent_width = 4\n",
        )
        .unwrap();
        host.seed_file(
            Path::new("/outside/proj/sub/.rcmake.toml"),
            b"[fmt]\nindent_width = 3\n",
        )
        .unwrap();

        let sub = resolve_config(
            &host,
            Path::new("/outside/proj/sub/deep"),
            Some(Path::new("/outside/proj")),
        )
        .unwrap();
        assert_eq!(sub.config.fmt.indent_width, Some(3));
        assert_eq!(
            sub.config_path,
            Some(PathBuf::from("/outside/proj/sub/.rcmake.toml"))
        );

        let root_only = resolve_config(
            &host,
            Path::new("/outside/proj/other"),
            Some(Path::new("/outside/proj")),
        )
        .unwrap();
        assert_eq!(root_only.config.fmt.indent_width, Some(4));

        // Bounded so /outside/.rcmake.toml is NOT read when proj has none.
        let host2 = PureHost::default();
        host2
            .seed_file(
                Path::new("/outside/.rcmake.toml"),
                b"[fmt]\nindent_width = 8\n",
            )
            .unwrap();
        let bounded = resolve_config(
            &host2,
            Path::new("/outside/proj/src"),
            Some(Path::new("/outside/proj")),
        )
        .unwrap();
        assert_eq!(bounded.config_path, None);
        assert_eq!(bounded.config.fmt.indent_width, None);
        assert_eq!(
            bounded.checked_dirs,
            vec![
                PathBuf::from("/outside/proj/src"),
                PathBuf::from("/outside/proj")
            ]
        );
    }
}
