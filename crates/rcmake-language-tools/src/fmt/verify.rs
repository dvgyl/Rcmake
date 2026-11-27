//! Independent pre-write verification of formatted CMake listfiles.
//!
//! Reparses the formatted bytes strictly as `SourceKind::File` and verifies:
//! 1. Identical ordered commands (lowercase name, case rule, argument kinds/values, nested parens).
//! 2. Identical syntax warnings by identity and `(command_index, arg_index)` token attachment.
//! 3. Byte-for-byte equality of every protected (non-whitespace, non-command-name) token.
//! 4. Identical comment order, trailing-vs-standalone attachment, and line-comment termination.

#![forbid(unsafe_code)]

use core::fmt;
use std::sync::Arc;

use rcmake_base::Limits;
use rcmake_diag::source::{SourceKind, SourceMap};
use rcmake_syntax::concrete::{ConcreteListFile, Token, TokenKind, parse_concrete_with_budget};

use crate::config::CommandCase;

use super::{FormatLimits, FormatStyle};

/// Reason independent pre-write verification rejected formatted output.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VerifyError {
    /// Human-readable description of the verification mismatch.
    pub message: String,
}

impl fmt::Display for VerifyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for VerifyError {}

impl VerifyError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

/// Independently reparse `formatted` and verify full syntactic and lossless equivalence
/// against `original` and `original_concrete`.
pub fn verify_formatted(
    original: &[u8],
    original_concrete: &ConcreteListFile,
    formatted: &[u8],
    style: &FormatStyle,
    limits: &FormatLimits,
) -> Result<(), VerifyError> {
    let mut sources = SourceMap::default();
    let Some(source) = sources.add("".into(), SourceKind::File, Arc::from(b"".as_slice())) else {
        return Err(VerifyError::new(
            "Unable to allocate verification source ID.",
        ));
    };
    let parse_limits = Limits {
        max_file_read: limits.max_output_bytes as u64,
        ..Limits::default()
    };
    let formatted_concrete = parse_concrete_with_budget(
        formatted,
        source,
        SourceKind::File,
        &parse_limits,
        limits.max_tokens.saturating_mul(2),
    )
    .map_err(|(err, _)| VerifyError::new(format!("Formatted output failed to parse: {err}")))?;

    verify_commands(original_concrete, &formatted_concrete, style)?;
    verify_warnings(original_concrete, &formatted_concrete)?;
    verify_protected_tokens(
        original,
        original_concrete,
        formatted,
        &formatted_concrete,
        style,
    )?;
    verify_comment_attachment(original_concrete, &formatted_concrete)?;

    Ok(())
}

fn verify_commands(
    orig: &ConcreteListFile,
    fmt: &ConcreteListFile,
    style: &FormatStyle,
) -> Result<(), VerifyError> {
    let orig_cmds = orig.list_file.commands.as_ref();
    let fmt_cmds = fmt.list_file.commands.as_ref();
    if orig_cmds.len() != fmt_cmds.len() {
        return Err(VerifyError::new(format!(
            "Command count changed from {} to {}.",
            orig_cmds.len(),
            fmt_cmds.len()
        )));
    }
    for (idx, ((oc, fc), (occ, fcc))) in orig_cmds
        .iter()
        .zip(fmt_cmds.iter())
        .zip(orig.commands.iter().zip(fmt.commands.iter()))
        .enumerate()
    {
        if oc.lower != fc.lower {
            return Err(VerifyError::new(format!(
                "Command #{idx} lowercase name changed."
            )));
        }
        let expected_name: Vec<u8> = match style.command_case {
            CommandCase::Preserve => oc.name.as_slice().to_vec(),
            CommandCase::Lower => oc.name.as_slice().to_ascii_lowercase(),
            CommandCase::Upper => oc.name.as_slice().to_ascii_uppercase(),
        };
        if fc.name.as_slice() != expected_name.as_slice() {
            return Err(VerifyError::new(format!(
                "Command #{idx} identifier spelling did not match command_case policy."
            )));
        }
        if oc.args.len() != fc.args.len() || occ.args.len() != fcc.args.len() {
            return Err(VerifyError::new(format!(
                "Command #{idx} argument count changed from {} to {}.",
                oc.args.len(),
                fc.args.len()
            )));
        }
        for (arg_idx, ((oa, fa), (oca, fca))) in oc
            .args
            .iter()
            .zip(&fc.args)
            .zip(occ.args.iter().zip(&fcc.args))
            .enumerate()
        {
            if oa.kind != fa.kind || oa.text != fa.text || oca.paren_depth != fca.paren_depth {
                return Err(VerifyError::new(format!(
                    "Command #{idx} argument #{arg_idx} kind, bytes, or parenthesis depth changed."
                )));
            }
        }
    }
    Ok(())
}

fn verify_warnings(orig: &ConcreteListFile, fmt: &ConcreteListFile) -> Result<(), VerifyError> {
    if orig.list_file.warnings.len() != fmt.list_file.warnings.len()
        || orig.attached_warnings.len() != fmt.attached_warnings.len()
    {
        return Err(VerifyError::new(format!(
            "Syntax warning count changed from {} to {}.",
            orig.list_file.warnings.len(),
            fmt.list_file.warnings.len()
        )));
    }
    for (idx, ((ow, fw), (oa, fa))) in orig
        .list_file
        .warnings
        .iter()
        .zip(&fmt.list_file.warnings)
        .zip(orig.attached_warnings.iter().zip(&fmt.attached_warnings))
        .enumerate()
    {
        if ow.kind != fw.kind
            || ow.category != fw.category
            || warning_identity(&ow.body) != warning_identity(&fw.body)
            || oa.command_index != fa.command_index
            || oa.arg_index != fa.arg_index
        {
            return Err(VerifyError::new(format!(
                "Syntax warning #{idx} identity or token attachment changed."
            )));
        }
    }
    Ok(())
}

/// Strip the column number prefix from a CMake syntax warning so identity ignores relocation.
fn warning_identity(body: &[u8]) -> &[u8] {
    if let Some(after) = body.strip_prefix(b"Syntax Warning in cmake code at column ")
        && let Some(pos) = after.iter().position(|&b| b == b'\n')
    {
        return after.get(pos..).unwrap_or(body);
    }
    body
}

fn verify_protected_tokens(
    original: &[u8],
    orig: &ConcreteListFile,
    formatted: &[u8],
    fmt: &ConcreteListFile,
    style: &FormatStyle,
) -> Result<(), VerifyError> {
    let orig_sig: Vec<(usize, &Token)> = orig
        .tokens
        .iter()
        .enumerate()
        .filter(|(_, t)| !t.kind.is_whitespace())
        .collect();
    let fmt_sig: Vec<(usize, &Token)> = fmt
        .tokens
        .iter()
        .enumerate()
        .filter(|(_, t)| !t.kind.is_whitespace())
        .collect();

    if orig_sig.len() != fmt_sig.len() {
        return Err(VerifyError::new(format!(
            "Non-whitespace token count changed from {} to {}.",
            orig_sig.len(),
            fmt_sig.len()
        )));
    }

    let mut next_cmd = 0usize;
    for (sig_idx, ((oi, ot), (fi, ft))) in orig_sig.iter().zip(&fmt_sig).enumerate() {
        if ot.kind != ft.kind {
            return Err(VerifyError::new(format!(
                "Significant token #{sig_idx} kind changed from {:?} to {:?}.",
                ot.kind, ft.kind
            )));
        }
        let is_cmd_name = orig
            .commands
            .get(next_cmd)
            .is_some_and(|c| c.name_token == *oi)
            && fmt
                .commands
                .get(next_cmd)
                .is_some_and(|c| c.name_token == *fi);
        if is_cmd_name {
            next_cmd += 1;
            let expected: Vec<u8> = match style.command_case {
                CommandCase::Preserve => ot.slice(original).to_vec(),
                CommandCase::Lower => ot.slice(original).to_ascii_lowercase(),
                CommandCase::Upper => ot.slice(original).to_ascii_uppercase(),
            };
            if ft.slice(formatted) != expected.as_slice() {
                return Err(VerifyError::new(format!(
                    "Command name token #{sig_idx} did not match case policy."
                )));
            }
        } else if ot.slice(original) != ft.slice(formatted) {
            return Err(VerifyError::new(format!(
                "Protected token #{sig_idx} ({:?}) bytes were modified.",
                ot.kind
            )));
        }
    }
    Ok(())
}

fn verify_comment_attachment(
    orig: &ConcreteListFile,
    fmt: &ConcreteListFile,
) -> Result<(), VerifyError> {
    let orig_comments = collect_comment_attachments(&orig.tokens);
    let fmt_comments = collect_comment_attachments(&fmt.tokens);

    if orig_comments != fmt_comments {
        return Err(VerifyError::new(
            "Comment attachment (trailing vs standalone or line termination) changed.",
        ));
    }
    Ok(())
}

#[derive(Debug, PartialEq, Eq)]
struct CommentAttachment {
    kind: TokenKind,
    /// `true` if a non-whitespace token preceded this comment on the same physical line.
    trailing: bool,
    /// `true` if a `LineComment` had a following token (requiring line termination).
    followed_by_token: bool,
}

fn collect_comment_attachments(tokens: &[Token]) -> Vec<CommentAttachment> {
    let mut out = Vec::new();
    let mut seen_code_on_line = false;

    for (idx, tok) in tokens.iter().enumerate() {
        match tok.kind {
            TokenKind::Bom | TokenKind::Space => {}
            TokenKind::Newline => {
                seen_code_on_line = false;
            }
            TokenKind::LineComment | TokenKind::BracketComment => {
                let followed_by_token = tokens
                    .get(idx + 1..)
                    .unwrap_or_default()
                    .iter()
                    .any(|t| !t.kind.is_whitespace());
                out.push(CommentAttachment {
                    kind: tok.kind,
                    trailing: seen_code_on_line,
                    followed_by_token,
                });
                seen_code_on_line = true;
            }
            _ => {
                seen_code_on_line = true;
            }
        }
    }
    out
}
