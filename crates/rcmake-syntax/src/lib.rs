//! Byte-preserving lexer and parser for CMake 4.5.1 listfiles.
//!
//! Contracts (ADR-005):
//! - Preserves raw bytes (stripping a leading UTF-8 BOM, rejecting UTF-16/UTF-32 BOM,
//!   converting CRLF to LF for file inputs, preserving lone CR inside quoted/bracket args).
//! - Produces a flat list of [`CommandInvocation`]s with original-case command names,
//!   [`ArgKind`] delimiters, opening/closing lines, and byte spans.
//! - Strips only quoted-argument `\<LF>` continuations (odd backslash count) and the
//!   first `\n` after a bracket opener; all other escapes stay raw for `rcmake-expr`.
//! - Validates control-flow block nesting (`if`/`foreach`/`while`/`function`/`macro`/`block`)
//!   before execution begins.

#![forbid(unsafe_code)]

use core::ops::Range;
use std::sync::Arc;

use rcmake_base::{BString, Limits};
use rcmake_diag::{Diagnostic, SourceId, source::SourceKind};
use thiserror::Error;

mod lexer;
mod parser;
mod tolerant;

pub use tolerant::{PartialCommandInvocation, TolerantParse, parse_tolerant};

#[cfg(test)]
mod tests;

/// A parsed CMake listfile consisting of a flat command sequence and non-fatal syntax warnings.
#[derive(Clone, Debug)]
pub struct ListFile {
    /// Registered source identifier.
    pub source: SourceId,
    /// Ordered top-level command invocations.
    pub commands: Arc<[CommandInvocation]>,
    /// Non-fatal syntax warnings. A warning's file path is empty because the parser
    /// receives only a source identifier. Resolve the path from [`Self::source`].
    pub warnings: Vec<Diagnostic>,
}

/// One parsed command invocation (`name(arg1 arg2 ...)`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommandInvocation {
    /// Original command spelling as written in source (preserved for diagnostics).
    pub name: BString,
    /// ASCII-lowercased command name for case-insensitive dispatch.
    pub lower: BString,
    /// 1-based opening line number.
    pub line: u32,
    /// 1-based closing `)` line number.
    pub close_line: u32,
    /// Raw byte span covering `name(...)` in the source file.
    pub span: Range<usize>,
    /// Unexpanded arguments in source order (nested unquoted `(` and `)` appear as separate
    /// [`ArgKind::Unquoted`] arguments).
    pub args: Vec<Argument>,
}

/// Delimiter classification of a parsed command argument.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ArgKind {
    /// Unquoted argument (subject to escape decoding, variable expansion, `;` splitting,
    /// and `if()` auto-deref).
    Unquoted,
    /// Double-quoted argument (subject to escape decoding and variable expansion; never
    /// split on `;` and never auto-dereferenced in `if()`).
    Quoted,
    /// Bracket argument `[=[...]=]` (verbatim bytes; never expanded or auto-dereferenced).
    Bracket,
}

/// One parsed argument belonging to a [`CommandInvocation`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Argument {
    /// Delimiter kind.
    pub kind: ArgKind,
    /// Lexed argument bytes (escapes other than quoted continuations remain undecoded).
    pub text: BString,
    /// 1-based opening line number of this argument.
    pub line: u32,
    /// Raw byte span in the source buffer.
    pub span: Range<usize>,
}

/// Fatal syntax or structural nesting error produced while parsing a listfile.
#[derive(Clone, Debug, PartialEq, Eq, Error)]
#[error("Parse error at line {line}, column {column}: {message}")]
pub struct ParseError {
    /// 1-based line number, or `0` for an error that applies to the whole file.
    pub line: u32,
    /// 1-based byte column number (`0` if line-only).
    pub column: u32,
    /// Optional command name associated with the error (e.g., `endif` on bad nesting).
    pub command: Option<BString>,
    /// Exact CMake error text.
    pub message: BString,
}

/// Parse a CMake 4.5.1 listfile from raw bytes.
///
/// Returns an error for invalid syntax, invalid flow-control nesting, or input that
/// exceeds the file size limit for files or the value size limit for `EVAL` buffers.
pub fn parse_list_file(
    src: &[u8],
    source: SourceId,
    kind: SourceKind,
    limits: &Limits,
) -> Result<ListFile, ParseError> {
    parse_list_file_with_warnings(src, source, kind, limits).map_err(|(error, _)| error)
}

/// Parse a listfile and keep any warnings that occur before a fatal error.
///
/// On success, the warnings are in the returned listfile. On failure, they are
/// in the error tuple, in source order.
pub fn parse_list_file_with_warnings(
    src: &[u8],
    source: SourceId,
    kind: SourceKind,
    limits: &Limits,
) -> Result<ListFile, (ParseError, Vec<Diagnostic>)> {
    let mut warnings = Vec::new();
    match parser::parse(src, kind, limits, &mut warnings) {
        Ok(commands) => Ok(ListFile {
            source,
            commands,
            warnings,
        }),
        Err(error) => Err((error, warnings)),
    }
}
