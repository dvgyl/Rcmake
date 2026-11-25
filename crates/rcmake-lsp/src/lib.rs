//! Language server for CMake listfiles.

#![forbid(unsafe_code)]
#![deny(missing_docs)]

mod analysis;
pub use rcmake_diag::annotate;
mod catalog;
#[cfg(feature = "eval")]
pub mod eval_provider;
mod expression;
mod features;
mod generated_catalog;
mod json;
mod lexical;
mod position;
mod roles;
pub mod rpc;
pub mod server;
mod uri;
mod workspace;

#[cfg(all(any(unix, windows), feature = "eval", feature = "stdio"))]
extern crate self as rcmake_lsp;
#[cfg(all(any(unix, windows), feature = "eval", feature = "stdio"))]
mod stdio_eval;

/// Run the evaluated language server over standard input and output.
#[cfg(all(any(unix, windows), feature = "eval", feature = "stdio"))]
pub fn run_stdio() -> std::process::ExitCode {
    stdio_eval::serve().unwrap_or(std::process::ExitCode::FAILURE)
}

#[cfg(all(any(unix, windows), feature = "eval", feature = "stdio"))]
fn send(output: &mut impl std::io::Write, message: RpcMessage) -> std::io::Result<()> {
    let body = message.encode();
    if body.len() <= rpc::MAX_MESSAGE_BYTES {
        rpc::write_frame(output, &body)?;
    } else if let RpcMessage::Result { id, .. } = message {
        let error = RpcMessage::error(id, -32603, "Response exceeds the message size limit.");
        rpc::write_frame(output, &error.encode())?;
    }
    output.flush()
}

pub use position::{Position, PositionEncoding};
pub use rpc::{RequestId, RpcMessage};
pub use server::Server;

use std::ops::Range;

use annotate::Annotations;
use rcmake_diag::{Diagnostic, SourceId};
use rcmake_json::Value;

/// Current in-memory document and cursor information for a semantic provider.
pub struct SemanticContext<'a> {
    /// Document URI.
    pub uri: &'a str,
    /// Current document text.
    pub text: &'a str,
    /// Document version.
    pub version: i64,
    /// Cursor position in the negotiated encoding.
    pub position: Position,
    /// Cursor byte offset into the text.
    pub offset: usize,
    /// Encoding for the request and response positions.
    pub encoding: PositionEncoding,
}

/// A definition returned by a semantic provider.
pub struct SemanticLocation {
    /// URI of the definition document.
    pub uri: String,
    /// Start position in the negotiated encoding.
    pub start: Position,
    /// End position in the negotiated encoding.
    pub end: Position,
}

/// A change to the documents available to a semantic provider.
pub enum SemanticUpdate<'a> {
    /// A new or changed in-memory document.
    Changed {
        /// Document URI.
        uri: &'a str,
        /// Current document text.
        text: &'a str,
        /// Current document version.
        version: i64,
    },
    /// A document that is no longer open.
    Closed {
        /// Document URI.
        uri: &'a str,
    },
}

/// The document version and URI that a semantic result describes.
pub struct SemanticResult<T> {
    /// URI of the document used to answer the request.
    pub uri: String,
    /// Version of the document used to answer the request.
    pub version: i64,
    /// Data from the semantic snapshot.
    pub value: T,
}

/// A hover message from a semantic snapshot.
pub struct HoverDoc {
    /// Short heading in plain text.
    pub title: String,
    /// Main text with Markdown formatting.
    pub body_md: String,
    /// CMake source to show in a code block.
    pub code: Option<String>,
}

/// An evaluated definition and the version that supplied its positions.
pub struct SemanticDefinition {
    /// Target URI and range.
    pub location: SemanticLocation,
    /// Target document version. Use `None` for a target read from disk.
    pub target_version: Option<i64>,
}

/// Current document information for semantic diagnostics and code actions.
pub struct SemanticDocument<'a> {
    /// Document URI.
    pub uri: &'a str,
    /// Current document text.
    pub text: &'a str,
    /// Current document version.
    pub version: i64,
    /// Position encoding for protocol ranges.
    pub encoding: PositionEncoding,
}

/// A diagnostic and annotations from a semantic snapshot.
pub struct SemanticDiagnostic {
    /// Diagnostic classification, location, and message.
    pub diagnostic: Diagnostic,
    /// Source ID for the document that the provider queried.
    pub source: SourceId,
    /// Source labels and suggested fixes.
    pub annotations: Annotations,
    /// Other open documents that contain edits in these fixes.
    pub related_sources: Vec<SemanticSource>,
}

impl SemanticDiagnostic {
    /// Create a diagnostic with annotations in one document.
    pub fn new(diagnostic: Diagnostic, source: SourceId, annotations: Annotations) -> Self {
        Self {
            diagnostic,
            source,
            annotations,
            related_sources: Vec::new(),
        }
    }
}

/// An open document that contains edits from a semantic snapshot.
pub struct SemanticSource {
    /// Source ID used in the annotations.
    pub source: SourceId,
    /// Client URI of the open document.
    pub uri: String,
    /// Document version that supplied the byte offsets.
    pub version: i64,
}

/// Read semantic results from an evaluated project.
///
/// Keep query methods fast. A provider can prepare an immutable snapshot on a
/// worker and share it with these methods. Return `None` to use lexical results.
/// The server accepts older hover results with a version note. Other results
/// must match the current document version.
pub trait SemanticProvider: Send + Sync {
    /// Supply an open-document change to the provider.
    fn update(&self, _update: SemanticUpdate<'_>) {}

    /// Return legacy plain text hover content.
    fn hover(&self, _context: &SemanticContext<'_>) -> Option<String> {
        None
    }

    /// Return legacy plain text hover content and its version, if known.
    fn hover_with_version(&self, context: &SemanticContext<'_>) -> Option<(String, Option<i64>)> {
        self.hover(context).map(|text| (text, None))
    }

    /// Return a versioned hover message with Markdown and source code.
    fn hover_doc(&self, _context: &SemanticContext<'_>) -> Option<SemanticResult<HoverDoc>> {
        None
    }

    /// Return a legacy definition. Check the source and target versions before returning it.
    fn definition(&self, _context: &SemanticContext<'_>) -> Option<SemanticLocation> {
        None
    }

    /// Return a versioned definition and the version of its target, if open.
    fn definition_result(
        &self,
        _context: &SemanticContext<'_>,
    ) -> Option<SemanticResult<SemanticDefinition>> {
        None
    }

    /// Return versioned LSP completion items.
    fn completions(&self, _context: &SemanticContext<'_>) -> Option<SemanticResult<Vec<Value>>> {
        None
    }

    /// Return versioned diagnostics and their source annotations.
    fn diagnostics(
        &self,
        _document: &SemanticDocument<'_>,
    ) -> Option<SemanticResult<Vec<SemanticDiagnostic>>> {
        None
    }

    /// Return versioned LSP code actions for the given range.
    fn code_actions(
        &self,
        _document: &SemanticDocument<'_>,
        _range: Range<Position>,
    ) -> Option<SemanticResult<Vec<Value>>> {
        None
    }
}
