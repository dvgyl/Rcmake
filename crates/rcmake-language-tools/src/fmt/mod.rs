//! Lossless byte-edit formatter for CMake listfiles.
//!
//! Only edits whitespace (`Space`, editable `Newline`, and safe separator insertions)
//! and optional ASCII case on command identifiers. Every other token is copied
//! verbatim, and every formatted result passes an independent pre-write verification
//! checking AST equivalence, warning identity/attachment, protected token bytes,
//! and comment attachment.

#![forbid(unsafe_code)]

mod diff;
mod verify;

use core::fmt;
use core::ops::Range;
use std::sync::Arc;

use rcmake_base::{BString, Limits};
use rcmake_diag::source::{SourceKind, SourceMap};
use rcmake_syntax::concrete::{
    ConcreteCommand, ConcreteListFile, Token, TokenKind, parse_concrete_with_budget,
};

use crate::config::{CommandCase, DEFAULT_INDENT_WIDTH, DEFAULT_LINE_WIDTH, EndOfLine, FmtConfig};
use crate::line_index::display_width;

pub use diff::{DEFAULT_MAX_DIFF_BYTES, unified_diff};
pub use verify::{VerifyError, verify_formatted};

/// Formatting style options.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FormatStyle {
    /// Spaces per indentation level (or tab display width when `use_tabs` is `true`).
    pub indent_width: usize,
    /// Use a single `'\t'` per indentation level instead of `indent_width` spaces.
    ///
    /// CLI and `.rcmake.toml` default to spaces (`false`); LSP sets this to `true`
    /// only when `.rcmake.toml` has no `indent_width` and the client sends `insertSpaces = false`.
    pub use_tabs: bool,
    /// Target maximum display column width for wrapping at proven argument separators.
    pub line_width: usize,
    /// Optional ASCII case normalization for command names.
    pub command_case: CommandCase,
    /// Line-ending normalization policy for editable physical newlines.
    pub end_of_line: EndOfLine,
}

impl Default for FormatStyle {
    fn default() -> Self {
        Self {
            indent_width: DEFAULT_INDENT_WIDTH,
            use_tabs: false,
            line_width: DEFAULT_LINE_WIDTH,
            command_case: CommandCase::Preserve,
            end_of_line: EndOfLine::Preserve,
        }
    }
}

impl FormatStyle {
    /// Build a [`FormatStyle`] from a parsed `[fmt]` configuration section.
    #[must_use]
    pub fn from_config(cfg: &FmtConfig) -> Self {
        Self {
            indent_width: cfg.effective_indent_width(),
            use_tabs: false,
            line_width: cfg.effective_line_width(),
            command_case: cfg.effective_command_case(),
            end_of_line: cfg.effective_end_of_line(),
        }
    }

    /// Build an LSP [`FormatStyle`] where project `.rcmake.toml` indentation overrides client
    /// options, and client `tab_size` / `insert_spaces` apply when the project sets no `indent_width`.
    #[must_use]
    pub fn from_config_with_client_fallback(
        cfg: &FmtConfig,
        client_tab_size: usize,
        client_insert_spaces: bool,
    ) -> Self {
        let (indent_width, use_tabs) = match cfg.indent_width {
            Some(width) => (width, false),
            None => (client_tab_size.clamp(1, 32), !client_insert_spaces),
        };
        Self {
            indent_width,
            use_tabs,
            line_width: cfg.effective_line_width(),
            command_case: cfg.effective_command_case(),
            end_of_line: cfg.effective_end_of_line(),
        }
    }

    fn indent_bytes(&self, depth: usize) -> Vec<u8> {
        if depth == 0 {
            return Vec::new();
        }
        if self.use_tabs {
            vec![b'\t'; depth]
        } else {
            vec![b' '; depth.saturating_mul(self.indent_width)]
        }
    }

    fn indent_cols(&self, depth: usize) -> usize {
        depth.saturating_mul(self.indent_width)
    }
}

/// Resource budgets enforced during lossless formatting.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FormatLimits {
    /// Maximum input size in bytes.
    pub max_input_bytes: usize,
    /// Maximum lossless token count.
    pub max_tokens: usize,
    /// Maximum formatted output size in bytes.
    pub max_output_bytes: usize,
    /// Maximum flow-control block nesting depth.
    pub max_nesting: usize,
    /// Maximum emitted byte edits.
    pub max_edits: usize,
}

impl Default for FormatLimits {
    fn default() -> Self {
        Self {
            max_input_bytes: 16 * 1024 * 1024,
            max_tokens: 500_000,
            max_output_bytes: 32 * 1024 * 1024,
            max_nesting: 4096,
            max_edits: 250_000,
        }
    }
}

/// One byte-range replacement in the original input buffer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TextEdit {
    /// Half-open byte range in the original buffer.
    pub range: Range<usize>,
    /// Exact replacement bytes.
    pub replacement: BString,
}

/// Verified output of formatting one CMake listfile.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FormatResult {
    /// Ordered, non-overlapping byte edits against the original buffer.
    pub edits: Vec<TextEdit>,
    /// Complete formatted bytes (equal to original bytes when `edits.is_empty()`).
    pub output: Vec<u8>,
}

impl FormatResult {
    /// Return `true` if formatting changed any bytes.
    #[must_use]
    pub fn changed(&self) -> bool {
        !self.edits.is_empty()
    }
}

/// Error returned when formatting cannot produce a verified lossless result.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FormatError {
    /// Strict syntax or flow-control parse failure on the actual file bytes.
    Parse(rcmake_syntax::ParseError),
    /// A formatting resource budget was exceeded.
    BudgetExceeded(String),
    /// Independent pre-write verification detected an AST, warning, token, or comment mismatch.
    VerificationFailed(VerifyError),
}

impl fmt::Display for FormatError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Parse(err) => write!(f, "{err}"),
            Self::BudgetExceeded(msg) => f.write_str(msg),
            Self::VerificationFailed(err) => write!(f, "Formatting verification failed: {err}"),
        }
    }
}

impl std::error::Error for FormatError {}

/// Strictly parse `src` as a CMake file (`SourceKind::File`) and format it losslessly.
///
/// Only a successful strict parse of the raw file bytes can authorize edits.
/// Runs mandatory pre-write verification before returning.
pub fn format_bytes(
    src: &[u8],
    style: &FormatStyle,
    limits: &FormatLimits,
) -> Result<FormatResult, FormatError> {
    if src.len() > limits.max_input_bytes {
        return Err(FormatError::BudgetExceeded(format!(
            "Input size {} exceeds the {} byte formatting limit.",
            src.len(),
            limits.max_input_bytes
        )));
    }
    let mut sources = SourceMap::default();
    let Some(source) = sources.add("".into(), SourceKind::File, Arc::from(b"".as_slice())) else {
        return Err(FormatError::BudgetExceeded(
            "Unable to register formatting source.".into(),
        ));
    };
    let parse_limits = Limits {
        max_file_read: limits.max_input_bytes as u64,
        ..Limits::default()
    };
    let concrete = parse_concrete_with_budget(
        src,
        source,
        SourceKind::File,
        &parse_limits,
        limits.max_tokens,
    )
    .map_err(|(err, _)| FormatError::Parse(err))?;
    format_concrete(src, &concrete, style, limits)
}

/// Format an already strictly-parsed [`ConcreteListFile`] and verify equivalence before returning.
pub fn format_concrete(
    src: &[u8],
    concrete: &ConcreteListFile,
    style: &FormatStyle,
    limits: &FormatLimits,
) -> Result<FormatResult, FormatError> {
    if src.len() > limits.max_input_bytes {
        return Err(FormatError::BudgetExceeded(format!(
            "Input size {} exceeds the {} byte formatting limit.",
            src.len(),
            limits.max_input_bytes
        )));
    }
    if concrete.tokens.len() > limits.max_tokens {
        return Err(FormatError::BudgetExceeded(format!(
            "Token count {} exceeds the {} token formatting limit.",
            concrete.tokens.len(),
            limits.max_tokens
        )));
    }

    let mut planner = Planner::new(src, concrete, style, limits);
    planner.plan()?;
    let edits = normalize_edits(src, planner.edits, limits.max_edits)?;
    let output = apply_edits(src, &edits, limits.max_output_bytes)?;

    if !edits.is_empty() {
        verify_formatted(src, concrete, &output, style, limits)
            .map_err(FormatError::VerificationFailed)?;
    }

    Ok(FormatResult { edits, output })
}

/// Apply ordered, non-overlapping [`TextEdit`]s to `src`, enforcing `max_output_bytes`.
pub fn apply_edits(
    src: &[u8],
    edits: &[TextEdit],
    max_output_bytes: usize,
) -> Result<Vec<u8>, FormatError> {
    if edits.is_empty() {
        return Ok(src.to_vec());
    }
    let mut out = Vec::with_capacity(src.len().min(max_output_bytes));
    let mut cursor = 0usize;
    for edit in edits {
        if edit.range.start < cursor
            || edit.range.start > edit.range.end
            || edit.range.end > src.len()
        {
            return Err(FormatError::BudgetExceeded(
                "Internal formatter edit ordering violation.".into(),
            ));
        }
        let unchanged = src.get(cursor..edit.range.start).unwrap_or_default();
        let next_len = out
            .len()
            .saturating_add(unchanged.len())
            .saturating_add(edit.replacement.len());
        if next_len > max_output_bytes {
            return Err(FormatError::BudgetExceeded(format!(
                "Formatted output exceeds the {max_output_bytes} byte limit."
            )));
        }
        out.extend_from_slice(unchanged);
        out.extend_from_slice(edit.replacement.as_slice());
        cursor = edit.range.end;
    }
    let tail = src.get(cursor..).unwrap_or_default();
    if out.len().saturating_add(tail.len()) > max_output_bytes {
        return Err(FormatError::BudgetExceeded(format!(
            "Formatted output exceeds the {max_output_bytes} byte limit."
        )));
    }
    out.extend_from_slice(tail);
    Ok(out)
}

struct Planner<'a> {
    src: &'a [u8],
    concrete: &'a ConcreteListFile,
    style: &'a FormatStyle,
    limits: &'a FormatLimits,
    edits: Vec<TextEdit>,
    newline_positions: Vec<(usize, &'static [u8])>,
}

impl<'a> Planner<'a> {
    fn new(
        src: &'a [u8],
        concrete: &'a ConcreteListFile,
        style: &'a FormatStyle,
        limits: &'a FormatLimits,
    ) -> Self {
        let mut newline_positions = Vec::new();
        for tok in &concrete.tokens {
            if tok.kind == TokenKind::Newline {
                let bytes: &'static [u8] = if tok.slice(src) == b"\r\n" {
                    b"\r\n"
                } else {
                    b"\n"
                };
                newline_positions.push((tok.span.start, bytes));
            }
        }
        Self {
            src,
            concrete,
            style,
            limits,
            edits: Vec::new(),
            newline_positions,
        }
    }

    fn inserted_newline(&self, near_offset: usize) -> &'static [u8] {
        match self.style.end_of_line {
            EndOfLine::Lf => b"\n",
            EndOfLine::Crlf => b"\r\n",
            EndOfLine::Preserve => {
                if self.newline_positions.is_empty() {
                    return b"\n";
                }
                let idx = self
                    .newline_positions
                    .partition_point(|&(off, _)| off < near_offset);
                let before = idx
                    .checked_sub(1)
                    .and_then(|i| self.newline_positions.get(i));
                let after = self.newline_positions.get(idx);
                match (before, after) {
                    (Some(&(bo, bb)), Some(&(ao, ab))) => {
                        if near_offset.saturating_sub(bo) <= ao.saturating_sub(near_offset) {
                            bb
                        } else {
                            ab
                        }
                    }
                    (Some(&(_, bb)), None) => bb,
                    (None, Some(&(_, ab))) => ab,
                    (None, None) => b"\n",
                }
            }
        }
    }

    fn push_edit(&mut self, range: Range<usize>, replacement: &[u8]) -> Result<(), FormatError> {
        let original = self.src.get(range.clone()).unwrap_or_default();
        if original == replacement {
            return Ok(());
        }
        if self.edits.len() >= self.limits.max_edits {
            return Err(FormatError::BudgetExceeded(format!(
                "Formatter edit count exceeds the {} limit.",
                self.limits.max_edits
            )));
        }
        self.edits.push(TextEdit {
            range,
            replacement: BString::from(replacement),
        });
        Ok(())
    }

    fn emit_newline_token(&mut self, tok: &Token) -> Result<(), FormatError> {
        match self.style.end_of_line {
            EndOfLine::Preserve => Ok(()),
            EndOfLine::Lf => self.push_edit(tok.span.clone(), b"\n"),
            EndOfLine::Crlf => self.push_edit(tok.span.clone(), b"\r\n"),
        }
    }

    fn plan(&mut self) -> Result<(), FormatError> {
        let tokens = &self.concrete.tokens;
        let commands = &self.concrete.commands;
        let mut idx = 0usize;
        let mut next_cmd = 0usize;
        let mut depth = 0usize;

        if tokens.first().is_some_and(|t| t.kind == TokenKind::Bom) {
            idx = 1;
        }

        let mut at_line_start = true;
        while idx < tokens.len() {
            let Some(tok) = tokens.get(idx) else {
                break;
            };
            if tok.kind == TokenKind::Newline {
                self.emit_newline_token(tok)?;
                at_line_start = true;
                idx += 1;
                continue;
            }

            if at_line_start {
                let line_start_offset = tok.span.start;
                let (indent_span, next_idx) = if tok.kind == TokenKind::Space {
                    (tok.span.clone(), idx + 1)
                } else {
                    (line_start_offset..line_start_offset, idx)
                };

                match tokens.get(next_idx) {
                    None => {
                        // Trailing horizontal whitespace at EOF on an otherwise empty line.
                        self.push_edit(indent_span, b"")?;
                        break;
                    }
                    Some(next_tok) if next_tok.kind == TokenKind::Newline => {
                        // Blank line: strip trailing whitespace, keep physical newline.
                        self.push_edit(indent_span, b"")?;
                        self.emit_newline_token(next_tok)?;
                        at_line_start = true;
                        idx = next_idx + 1;
                        continue;
                    }
                    Some(next_tok) => {
                        if let Some(cmd) = commands.get(next_cmd)
                            && cmd.name_token == next_idx
                        {
                            let lower = self
                                .concrete
                                .list_file
                                .commands
                                .get(cmd.command_index)
                                .map(|c| c.lower.as_slice())
                                .unwrap_or_default();
                            if is_block_end(lower) || is_block_branch(lower) {
                                depth = depth.saturating_sub(1);
                            }
                            let cmd_depth = depth;
                            let indent = self.style.indent_bytes(cmd_depth);
                            self.push_edit(indent_span, &indent)?;
                            self.format_command(cmd, cmd_depth)?;
                            if is_block_start(lower) || is_block_branch(lower) {
                                depth += 1;
                                if depth > self.limits.max_nesting {
                                    return Err(FormatError::BudgetExceeded(format!(
                                        "Flow-control block nesting exceeds the {} limit.",
                                        self.limits.max_nesting
                                    )));
                                }
                            }
                            next_cmd += 1;
                            idx = cmd.close_paren_token + 1;
                            at_line_start = false;
                            continue;
                        }

                        // Standalone comment (LineComment or BracketComment) at top level.
                        let indent = self.style.indent_bytes(depth);
                        self.push_edit(indent_span, &indent)?;
                        let _ = next_tok;
                        idx = next_idx + 1;
                        at_line_start = false;
                        continue;
                    }
                }
            }

            // Not at line start, outside any command (trailing comments / spaces after a
            // command's ')' or after a top-level bracket comment).
            if tok.kind == TokenKind::Space {
                match tokens.get(idx + 1) {
                    None => {
                        self.push_edit(tok.span.clone(), b"")?;
                    }
                    Some(next_tok) if next_tok.kind == TokenKind::Newline => {
                        self.push_edit(tok.span.clone(), b"")?;
                    }
                    Some(_) => {
                        // Horizontal space before a trailing comment on the same line.
                        self.push_edit(tok.span.clone(), b" ")?;
                    }
                }
            }
            idx += 1;
        }

        Ok(())
    }

    fn format_command(
        &mut self,
        cmd: &ConcreteCommand,
        cmd_depth: usize,
    ) -> Result<(), FormatError> {
        let tokens = &self.concrete.tokens;
        let Some(name_tok) = tokens.get(cmd.name_token) else {
            return Ok(());
        };
        let Some(open_tok) = tokens.get(cmd.open_paren_token) else {
            return Ok(());
        };
        if tokens.get(cmd.close_paren_token).is_none() {
            return Ok(());
        }

        // 1. Optional command name ASCII case normalization.
        let raw_name = name_tok.slice(self.src);
        let formatted_name: Vec<u8> = match self.style.command_case {
            CommandCase::Preserve => raw_name.to_vec(),
            CommandCase::Lower => raw_name.to_ascii_lowercase(),
            CommandCase::Upper => raw_name.to_ascii_uppercase(),
        };
        self.push_edit(name_tok.span.clone(), &formatted_name)?;

        // 2. Remove horizontal spaces between command name and '('.
        if name_tok.span.end < open_tok.span.start {
            self.push_edit(name_tok.span.end..open_tok.span.start, b"")?;
        }

        let inner_start = cmd.open_paren_token + 1;
        let inner_end = cmd.close_paren_token;
        let inner = tokens.get(inner_start..inner_end).unwrap_or_default();

        // Check empty call (no non-whitespace tokens inside parens).
        if inner.iter().all(|t| t.kind.is_whitespace()) {
            return self.format_empty_command_body(inner_start, inner_end, cmd_depth);
        }

        let has_newline = inner.iter().any(|t| t.kind == TokenKind::Newline);
        let has_comment = inner.iter().any(|t| t.kind.is_comment());
        let has_multiline_token = inner.iter().any(|t| t.start_line != t.end_line);
        let is_line_sensitive = self.command_uses_list_line(cmd);

        if !has_newline && !has_comment && !has_multiline_token {
            self.format_single_line_command(cmd, cmd_depth, &formatted_name, is_line_sensitive)
        } else {
            self.format_multiline_command(cmd, cmd_depth, &formatted_name, is_line_sensitive)
        }
    }

    fn command_uses_list_line(&self, cmd: &ConcreteCommand) -> bool {
        let Some(inv) = self.concrete.list_file.commands.get(cmd.command_index) else {
            return false;
        };
        inv.args.iter().any(|arg| {
            arg.kind != rcmake_syntax::ArgKind::Bracket
                && arg
                    .text
                    .as_slice()
                    .windows(b"CMAKE_CURRENT_LIST_LINE".len())
                    .any(|w| w == b"CMAKE_CURRENT_LIST_LINE")
        })
    }

    fn format_empty_command_body(
        &mut self,
        inner_start: usize,
        inner_end: usize,
        cmd_depth: usize,
    ) -> Result<(), FormatError> {
        let tokens = &self.concrete.tokens;
        let inner = tokens.get(inner_start..inner_end).unwrap_or_default();
        let has_newline = inner.iter().any(|t| t.kind == TokenKind::Newline);
        if !has_newline {
            if let (Some(first), Some(last)) = (inner.first(), inner.last()) {
                self.push_edit(first.span.start..last.span.end, b"")?;
            }
            return Ok(());
        }
        // Multiline empty call: strip trailing spaces on earlier lines, preserve newlines,
        // align closing ')' with cmd_depth.
        let mut pos = inner_start;
        while pos < inner_end {
            let Some(tok) = tokens.get(pos) else {
                break;
            };
            if tok.kind == TokenKind::Newline {
                self.emit_newline_token(tok)?;
            } else if tok.kind == TokenKind::Space {
                if pos + 1 == inner_end {
                    let indent = self.style.indent_bytes(cmd_depth);
                    self.push_edit(tok.span.clone(), &indent)?;
                } else {
                    self.push_edit(tok.span.clone(), b"")?;
                }
            }
            pos += 1;
        }
        if inner.last().is_some_and(|t| t.kind == TokenKind::Newline)
            && let Some(close_tok) = tokens.get(inner_end)
        {
            let indent = self.style.indent_bytes(cmd_depth);
            self.push_edit(close_tok.span.start..close_tok.span.start, &indent)?;
        }
        Ok(())
    }

    fn format_single_line_command(
        &mut self,
        cmd: &ConcreteCommand,
        cmd_depth: usize,
        formatted_name: &[u8],
        is_line_sensitive: bool,
    ) -> Result<(), FormatError> {
        let tokens = &self.concrete.tokens;
        let inner_start = cmd.open_paren_token + 1;
        let inner_end = cmd.close_paren_token;
        let Some(open_tok) = tokens.get(cmd.open_paren_token) else {
            return Ok(());
        };
        let Some(close_tok) = tokens.get(cmd.close_paren_token) else {
            return Ok(());
        };

        let atoms = collect_line_atoms(
            self.src,
            tokens,
            inner_start,
            inner_end,
            self.style.indent_width,
        );
        let Some(first_atom) = atoms.first() else {
            return Ok(());
        };
        let Some(last_atom) = atoms.last() else {
            return Ok(());
        };

        let base_cols = self.style.indent_cols(cmd_depth);
        let name_cols = display_width(formatted_name, self.style.indent_width);
        let line1_prefix_cols = base_cols + name_cols + 1; // `indent + name + '('`

        let body_cols: usize =
            atoms.iter().map(|a| a.display_width).sum::<usize>() + atoms.len().saturating_sub(1);
        let total_single_line_cols = line1_prefix_cols + body_cols + 1; // + ')'

        let has_safe_separator = atoms.len() > 1;
        let should_wrap = !is_line_sensitive
            && has_safe_separator
            && total_single_line_cols > self.style.line_width;

        if !should_wrap {
            // Leading space right after '('.
            if open_tok.span.end < first_atom.start_offset {
                self.push_edit(open_tok.span.end..first_atom.start_offset, b"")?;
            }
            // Internal spaces within atoms (collapsed around nested parens) and between atoms.
            self.emit_single_line_atoms(&atoms)?;
            // Trailing space right before ')'.
            if last_atom.end_offset < close_tok.span.start {
                self.push_edit(last_atom.end_offset..close_tok.span.start, b"")?;
            }
            return Ok(());
        }

        // Wrap onto multiple lines at proven safe argument separators.
        let nl = self.inserted_newline(open_tok.span.start);
        let base_indent = self.style.indent_bytes(cmd_depth);
        let cont_indent = self.style.indent_bytes(cmd_depth + 1);
        let cont_cols = self.style.indent_cols(cmd_depth + 1);

        let mut break_and_cont = Vec::with_capacity(nl.len() + cont_indent.len());
        break_and_cont.extend_from_slice(nl);
        break_and_cont.extend_from_slice(&cont_indent);

        let broke_after_open = line1_prefix_cols + first_atom.display_width > self.style.line_width;
        let mut col = if broke_after_open {
            self.push_edit(open_tok.span.end..first_atom.start_offset, &break_and_cont)?;
            cont_cols + first_atom.display_width
        } else {
            if open_tok.span.end < first_atom.start_offset {
                self.push_edit(open_tok.span.end..first_atom.start_offset, b"")?;
            }
            line1_prefix_cols + first_atom.display_width
        };

        self.emit_atom_internal_collapses(first_atom)?;

        let atom_count = atoms.len();
        for (idx, atom) in atoms.iter().enumerate().skip(1) {
            let Some(prev) = atoms.get(idx - 1) else {
                continue;
            };
            let is_last = idx + 1 == atom_count;
            let close_extra = usize::from(is_last && !broke_after_open);
            let sep_span = prev.end_offset..atom.start_offset;
            if col + 1 + atom.display_width + close_extra <= self.style.line_width {
                self.push_edit(sep_span, b" ")?;
                col += 1 + atom.display_width;
            } else {
                self.push_edit(sep_span, &break_and_cont)?;
                col = cont_cols + atom.display_width;
            }
            self.emit_atom_internal_collapses(atom)?;
        }

        if broke_after_open {
            let mut break_and_base = Vec::with_capacity(nl.len() + base_indent.len());
            break_and_base.extend_from_slice(nl);
            break_and_base.extend_from_slice(&base_indent);
            self.push_edit(last_atom.end_offset..close_tok.span.start, &break_and_base)?;
        } else if last_atom.end_offset < close_tok.span.start {
            self.push_edit(last_atom.end_offset..close_tok.span.start, b"")?;
        }

        Ok(())
    }

    fn format_multiline_command(
        &mut self,
        cmd: &ConcreteCommand,
        cmd_depth: usize,
        formatted_name: &[u8],
        is_line_sensitive: bool,
    ) -> Result<(), FormatError> {
        let tokens = &self.concrete.tokens;
        let inner_start = cmd.open_paren_token + 1;
        let inner_end = cmd.close_paren_token;
        let Some(open_tok) = tokens.get(cmd.open_paren_token) else {
            return Ok(());
        };
        let Some(close_tok) = tokens.get(cmd.close_paren_token) else {
            return Ok(());
        };

        let base_cols = self.style.indent_cols(cmd_depth);
        let name_cols = display_width(formatted_name, self.style.indent_width);
        let line1_prefix_cols = base_cols + name_cols + 1;
        let cont_indent = self.style.indent_bytes(cmd_depth + 1);
        let cont_cols = self.style.indent_cols(cmd_depth + 1);
        let base_indent = self.style.indent_bytes(cmd_depth);

        // Split `inner_start..inner_end` into physical lines separated by `TokenKind::Newline`.
        let mut seg_start = inner_start;
        let mut is_first_seg = true;

        loop {
            let mut seg_end = seg_start;
            while seg_end < inner_end {
                if tokens
                    .get(seg_end)
                    .is_some_and(|t| t.kind == TokenKind::Newline)
                {
                    break;
                }
                seg_end += 1;
            }
            let is_last_seg = seg_end == inner_end;
            let seg = tokens.get(seg_start..seg_end).unwrap_or_default();

            let seg_start_offset = if is_first_seg {
                open_tok.span.end
            } else {
                seg.first()
                    .map(|t| t.span.start)
                    .or_else(|| tokens.get(seg_end).map(|t| t.span.start))
                    .unwrap_or(close_tok.span.start)
            };
            let seg_end_offset = if is_last_seg {
                close_tok.span.start
            } else {
                tokens
                    .get(seg_end)
                    .map(|t| t.span.start)
                    .unwrap_or(close_tok.span.start)
            };

            let first_non_ws = seg
                .iter()
                .position(|t| !t.kind.is_whitespace())
                .map(|rel| seg_start + rel);

            if is_first_seg {
                match first_non_ws {
                    None => {
                        // Nothing between '(' and the first newline (or ')').
                        if seg_start_offset < seg_end_offset {
                            self.push_edit(seg_start_offset..seg_end_offset, b"")?;
                        }
                    }
                    Some(first_idx) => {
                        let Some(first_tok) = tokens.get(first_idx) else {
                            return Ok(());
                        };
                        if first_tok.kind.is_comment() {
                            if open_tok.span.end < first_tok.span.start {
                                self.push_edit(open_tok.span.end..first_tok.span.start, b" ")?;
                            }
                        } else if open_tok.span.end < first_tok.span.start {
                            self.push_edit(open_tok.span.end..first_tok.span.start, b"")?;
                        }
                        self.format_multiline_segment_body(
                            first_idx,
                            seg_end,
                            seg_end_offset,
                            line1_prefix_cols,
                            &cont_indent,
                            cont_cols,
                            is_last_seg,
                            is_line_sensitive,
                        )?;
                    }
                }
            } else {
                // Continuation line inside `(...)`.
                match first_non_ws {
                    None => {
                        if is_last_seg {
                            // Closing ')' is the first non-whitespace token on its line!
                            self.push_edit(seg_start_offset..close_tok.span.start, &base_indent)?;
                        } else if seg_start_offset < seg_end_offset {
                            // Blank line inside command: strip horizontal spaces.
                            self.push_edit(seg_start_offset..seg_end_offset, b"")?;
                        }
                    }
                    Some(first_idx) => {
                        let Some(first_tok) = tokens.get(first_idx) else {
                            return Ok(());
                        };
                        let closing_only_line = is_last_seg
                            && seg.iter().all(|t| {
                                t.kind == TokenKind::Space || t.kind == TokenKind::RightParen
                            });
                        let indent = if closing_only_line {
                            &base_indent
                        } else {
                            &cont_indent
                        };
                        let prefix_cols = if closing_only_line {
                            base_cols
                        } else {
                            cont_cols
                        };
                        self.push_edit(seg_start_offset..first_tok.span.start, indent)?;
                        self.format_multiline_segment_body(
                            first_idx,
                            seg_end,
                            seg_end_offset,
                            prefix_cols,
                            &cont_indent,
                            cont_cols,
                            is_last_seg,
                            is_line_sensitive,
                        )?;
                    }
                }
            }

            if is_last_seg {
                break;
            }
            if let Some(nl_tok) = tokens.get(seg_end) {
                self.emit_newline_token(nl_tok)?;
            }
            seg_start = seg_end + 1;
            is_first_seg = false;
        }

        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn format_multiline_segment_body(
        &mut self,
        first_idx: usize,
        seg_end: usize,
        seg_end_offset: usize,
        line_prefix_cols: usize,
        cont_indent: &[u8],
        cont_cols: usize,
        is_last_seg: bool,
        is_line_sensitive: bool,
    ) -> Result<(), FormatError> {
        let tokens = &self.concrete.tokens;
        let slice = tokens.get(first_idx..seg_end).unwrap_or_default();
        let has_comment = slice.iter().any(|t| t.kind.is_comment());
        let has_multiline_token = slice.iter().any(|t| t.start_line != t.end_line);

        let atoms = collect_line_atoms(
            self.src,
            tokens,
            first_idx,
            seg_end,
            self.style.indent_width,
        );
        let Some(last_atom) = atoms.last() else {
            return Ok(());
        };

        let body_cols: usize =
            atoms.iter().map(|a| a.display_width).sum::<usize>() + atoms.len().saturating_sub(1);
        let close_extra = usize::from(is_last_seg);
        let can_wrap = !is_line_sensitive
            && !has_comment
            && !has_multiline_token
            && atoms.len() > 1
            && line_prefix_cols + body_cols + close_extra > self.style.line_width;

        if !can_wrap {
            self.emit_single_line_atoms(&atoms)?;
        } else {
            let nl = self.inserted_newline(last_atom.start_offset);
            let mut break_and_cont = Vec::with_capacity(nl.len() + cont_indent.len());
            break_and_cont.extend_from_slice(nl);
            break_and_cont.extend_from_slice(cont_indent);

            let Some(first_atom) = atoms.first() else {
                return Ok(());
            };
            self.emit_atom_internal_collapses(first_atom)?;
            let mut col = line_prefix_cols + first_atom.display_width;
            let count = atoms.len();
            for (idx, atom) in atoms.iter().enumerate().skip(1) {
                let Some(prev) = atoms.get(idx - 1) else {
                    continue;
                };
                let last_on_line = idx + 1 == count;
                let extra = usize::from(is_last_seg && last_on_line);
                let sep_span = prev.end_offset..atom.start_offset;
                if col + 1 + atom.display_width + extra <= self.style.line_width {
                    self.push_edit(sep_span, b" ")?;
                    col += 1 + atom.display_width;
                } else {
                    self.push_edit(sep_span, &break_and_cont)?;
                    col = cont_cols + atom.display_width;
                }
                self.emit_atom_internal_collapses(atom)?;
            }
        }

        // Trailing space at end of line segment (before newline or before closing ')').
        if last_atom.end_offset < seg_end_offset {
            let last_tok_is_bracket_comment = tokens
                .get(last_atom.last_token)
                .is_some_and(|t| t.kind == TokenKind::BracketComment);
            if is_last_seg && last_tok_is_bracket_comment {
                // Keep a clean space between trailing bracket comment and ')' if one existed.
                self.push_edit(last_atom.end_offset..seg_end_offset, b" ")?;
            } else {
                self.push_edit(last_atom.end_offset..seg_end_offset, b"")?;
            }
        }

        Ok(())
    }

    fn emit_single_line_atoms(&mut self, atoms: &[Atom]) -> Result<(), FormatError> {
        for (idx, atom) in atoms.iter().enumerate() {
            if idx > 0
                && let Some(prev) = atoms.get(idx - 1)
            {
                self.push_edit(prev.end_offset..atom.start_offset, b" ")?;
            }
            self.emit_atom_internal_collapses(atom)?;
        }
        Ok(())
    }

    fn emit_atom_internal_collapses(&mut self, atom: &Atom) -> Result<(), FormatError> {
        for span in &atom.collapsed_spaces {
            self.push_edit(span.clone(), b"")?;
        }
        Ok(())
    }
}

/// An inseparable horizontal sequence of tokens (e.g. a single argument, adjacent
/// warning-producing tokens, or space-collapsed nested parens like `(a`).
struct Atom {
    start_offset: usize,
    end_offset: usize,
    last_token: usize,
    display_width: usize,
    collapsed_spaces: Vec<Range<usize>>,
}

/// Group non-whitespace tokens in `start..end` (within a single line segment) into
/// [`Atom`]s separated ONLY at proven safe horizontal separators.
fn collect_line_atoms(
    src: &[u8],
    tokens: &[Token],
    start: usize,
    end: usize,
    tab_width: usize,
) -> Vec<Atom> {
    let mut atoms: Vec<Atom> = Vec::new();
    let mut idx = start;
    let mut pending_space: Option<Range<usize>> = None;

    while idx < end {
        let Some(tok) = tokens.get(idx) else {
            break;
        };
        if tok.kind == TokenKind::Space {
            pending_space = Some(tok.span.clone());
            idx += 1;
            continue;
        }
        if tok.kind == TokenKind::Newline {
            break;
        }

        let tok_width = display_width(tok.slice(src), tab_width);
        let prev_tok = atoms.last().and_then(|a| tokens.get(a.last_token));

        let can_glue = match (prev_tok, &pending_space) {
            (None, _) => false,
            // No whitespace between previous token and this token: MUST stay glued!
            (Some(_), None) => true,
            // Space right after nested '(' (unless next is a comment): safe to collapse.
            (Some(prev), Some(_))
                if prev.kind == TokenKind::LeftParen && !tok.kind.is_comment() =>
            {
                true
            }
            // Space right before nested ')' (unless prev is a bracket comment): safe to collapse.
            (Some(prev), Some(_))
                if tok.kind == TokenKind::RightParen && !prev.kind.is_comment() =>
            {
                true
            }
            // Otherwise, this space is a proven safe argument separator.
            (Some(_), Some(_)) => false,
        };

        if can_glue && let Some(current) = atoms.last_mut() {
            if let Some(space_span) = pending_space.take() {
                current.collapsed_spaces.push(space_span);
            }
            current.end_offset = tok.span.end;
            current.last_token = idx;
            current.display_width = current.display_width.saturating_add(tok_width);
        } else {
            pending_space = None;
            atoms.push(Atom {
                start_offset: tok.span.start,
                end_offset: tok.span.end,
                last_token: idx,
                display_width: tok_width,
                collapsed_spaces: Vec::new(),
            });
        }
        idx += 1;
    }

    atoms
}

/// Sort, deduplicate, and merge adjacent edits while guaranteeing no edit endpoint
/// ever splits a `\r\n` pair in `src`.
fn normalize_edits(
    src: &[u8],
    mut edits: Vec<TextEdit>,
    max_edits: usize,
) -> Result<Vec<TextEdit>, FormatError> {
    edits.retain(|e| src.get(e.range.clone()).unwrap_or_default() != e.replacement.as_slice());
    edits.sort_by_key(|e| (e.range.start, e.range.end));

    let mut merged: Vec<TextEdit> = Vec::with_capacity(edits.len());
    for edit in edits {
        if let Some(last) = merged.last_mut()
            && last.range.end == edit.range.start
        {
            last.range.end = edit.range.end;
            last.replacement
                .extend_from_slice(edit.replacement.as_slice());
            continue;
        }
        merged.push(edit);
    }

    // Remove any merged edit that ended up a no-op and verify no endpoint splits CRLF.
    let mut final_edits = Vec::with_capacity(merged.len());
    for edit in merged {
        let orig = src.get(edit.range.clone()).unwrap_or_default();
        if orig == edit.replacement.as_slice() {
            continue;
        }
        if splits_crlf(src, edit.range.start) || splits_crlf(src, edit.range.end) {
            return Err(FormatError::BudgetExceeded(
                "Internal formatter attempted to split a CRLF line ending.".into(),
            ));
        }
        if final_edits.len() >= max_edits {
            return Err(FormatError::BudgetExceeded(format!(
                "Formatter edit count exceeds the {max_edits} limit."
            )));
        }
        final_edits.push(edit);
    }
    Ok(final_edits)
}

fn splits_crlf(src: &[u8], offset: usize) -> bool {
    offset > 0 && src.get(offset - 1..=offset) == Some(b"\r\n")
}

fn is_block_start(lower: &[u8]) -> bool {
    matches!(
        lower,
        b"if" | b"foreach" | b"while" | b"function" | b"macro" | b"block"
    )
}

fn is_block_end(lower: &[u8]) -> bool {
    matches!(
        lower,
        b"endif" | b"endforeach" | b"endwhile" | b"endfunction" | b"endmacro" | b"endblock"
    )
}

fn is_block_branch(lower: &[u8]) -> bool {
    matches!(lower, b"else" | b"elseif")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fmt(src: &[u8], style: &FormatStyle) -> FormatResult {
        let first = format_bytes(src, style, &FormatLimits::default()).expect("format succeeds");
        let second =
            format_bytes(&first.output, style, &FormatLimits::default()).expect("second pass ok");
        assert!(
            second.edits.is_empty(),
            "Formatter was not idempotent!\n--- FIRST ---\n{}\n--- SECOND ---\n{}\nEdits: {:?}",
            String::from_utf8_lossy(&first.output),
            String::from_utf8_lossy(&second.output),
            second.edits
        );
        first
    }

    #[test]
    fn preserves_bom_crlf_mixed_endings_missing_eof_newline_and_non_utf8() {
        let src = b"\xef\xbb\xbfif(ON)\r\nset(A   \"caf\xe9\r\n  keep\"  [=[\r\n  raw\r\n]=])\n  message(\xfe \"\xff\") # tail\r\nendif()";
        let res = fmt(src, &FormatStyle::default());
        assert_eq!(
            res.output.as_slice(),
            b"\xef\xbb\xbfif(ON)\r\n  set(A \"caf\xe9\r\n  keep\" [=[\r\n  raw\r\n]=])\n  message(\xfe \"\xff\") # tail\r\nendif()"
        );
    }

    #[test]
    fn preserves_warning_producing_adjacency_and_make_reference_boundaries() {
        let src = b"x(\"a\"y  a\"b\"c  $ (CC)  [[b]](c))\n";
        let res = fmt(src, &FormatStyle::default());
        assert_eq!(
            res.output.as_slice(),
            b"x(\"a\"y a\"b\"c $ (CC) [[b]](c))\n"
        );
    }

    #[test]
    fn wraps_long_commands_idempotently_and_leaves_indivisible_tokens() {
        let src = b"if(ON)\ntarget_link_libraries(my_app PRIVATE lib_alpha lib_beta lib_gamma lib_delta lib_epsilon lib_zeta lib_eta)\nset(LONG_URL https://example.com/very/long/indivisible/url/that/exceeds/one/hundred/characters/all/by/itself/indeed)\nendif()\n";
        let res = fmt(src, &FormatStyle::default());
        let text = String::from_utf8(res.output).expect("utf8");
        assert!(text.contains("  target_link_libraries("));
        assert!(text.contains("\n    lib_"));
        assert!(text.contains("  set(LONG_URL\n    https://example.com/"));
    }

    #[test]
    fn command_case_and_end_of_line_normalization_are_atomic_and_lossless() {
        let src = b"IF(ON)\r\n  SeT(A \"keep\r\nnewline\")\r\nENDIF()\r\n";
        let style = FormatStyle {
            command_case: CommandCase::Lower,
            end_of_line: EndOfLine::Lf,
            ..FormatStyle::default()
        };
        let res = fmt(src, &style);
        // Interior CRLF inside quoted argument stays untouched!
        assert_eq!(
            res.output.as_slice(),
            b"if(ON)\n  set(A \"keep\r\nnewline\")\nendif()\n"
        );
        // Verify no edit split a CRLF pair.
        for edit in &res.edits {
            assert!(!splits_crlf(src, edit.range.start));
            assert!(!splits_crlf(src, edit.range.end));
        }
    }

    #[test]
    fn rejects_invalid_syntax_including_bare_cr_between_commands() {
        let err = format_bytes(
            b"if(ON)\rmessage(hi)\rendif()\r",
            &FormatStyle::default(),
            &FormatLimits::default(),
        )
        .expect_err("bare CR between commands is a syntax error");
        assert!(matches!(err, FormatError::Parse(_)));
    }

    #[test]
    fn preserves_line_positions_for_cmake_current_list_line() {
        let src = b"message(STATUS \"at ${CMAKE_CURRENT_LIST_LINE}\" aaaaaaaaaa bbbbbbbbbb cccccccccc dddddddddd eeeeeeeeee ffffffffff)\n";
        let res = fmt(src, &FormatStyle::default());
        assert_eq!(res.output.as_slice(), src);
    }

    #[test]
    fn preserves_bracket_comment_at_eof_and_escaped_quotes_and_continuations() {
        let src = b"set(X \"a\\\"b\\\nc\" \n  \"d\") #[=[eof]=]";
        let res = fmt(src, &FormatStyle::default());
        assert_eq!(
            res.output.as_slice(),
            b"set(X \"a\\\"b\\\nc\"\n  \"d\") #[=[eof]=]"
        );
    }
}
