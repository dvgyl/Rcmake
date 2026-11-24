//! Synchronous language server and the interface for semantic analysis.

use std::{cell::RefCell, collections::BTreeMap, path::PathBuf};

use rcmake_host::{Host, discover_cmake_root};
use rcmake_json::Value;

use crate::{
    analysis::Document,
    features,
    json::{array, get, int, object, string},
    position::{Lines, Position, PositionEncoding, range},
    rpc::{MAX_MESSAGE_BYTES, RpcMessage},
    uri,
    workspace::{DiskCache, Workspace},
};

const MAX_DOCUMENTS: usize = 512;
const MAX_BUFFER_BYTES: usize = MAX_MESSAGE_BYTES;
const MAX_OPEN_BYTES: usize = MAX_BUFFER_BYTES * 4;
const MAX_CHANGES: usize = 1024;
// Limit the estimated bytes that a batch scans or moves.
const MAX_CHANGE_WORK: usize = 2 * 1024 * 1024 * 1024;

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

/// An optional source of evaluated hover and definition results.
///
/// Return `None` when no result is available. The server then uses lexical
/// analysis. Supply the snapshot version if hover can use an earlier document version.
pub trait SemanticProvider {
    /// Update the provider's document snapshot after the server accepts a change.
    fn update(&self, _update: SemanticUpdate<'_>) {}

    /// Return hover text for the current site.
    fn hover(&self, _context: &SemanticContext<'_>) -> Option<String> {
        None
    }

    /// Return hover text and the document version that supplied the result, if known.
    fn hover_with_version(&self, context: &SemanticContext<'_>) -> Option<(String, Option<i64>)> {
        self.hover(context).map(|text| (text, None))
    }

    /// Return the evaluated definition of the current site.
    fn definition(&self, _context: &SemanticContext<'_>) -> Option<SemanticLocation> {
        None
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum State {
    New,
    AwaitInitialized,
    Running,
    Shutdown,
    Exited(i32),
}

enum OpenDocument {
    Synced(Document),
    Desynced(i64),
}

enum SyncFailure {
    InvalidChange,
    ChangeLimit,
    WorkLimit,
    BufferLimit,
    OpenBufferLimit,
    AwaitingText,
}

impl SyncFailure {
    fn recovery(&self) -> &'static str {
        match self {
            Self::InvalidChange | Self::AwaitingText => {
                "Save the file with full text or reopen it to restore language features."
            }
            Self::ChangeLimit => {
                "The batch exceeds 1024 changes. Save the file with full text or reopen it."
            }
            Self::WorkLimit => {
                "The batch exceeds the edit work limit. Save the file with full text or reopen it."
            }
            Self::BufferLimit => {
                "The file exceeds 16 MiB. Reduce it to the limit before you save it with full text or reopen it."
            }
            Self::OpenBufferLimit => {
                "The open files exceed 64 MiB. Close other files before you save this file with full text or reopen it."
            }
        }
    }
}

impl OpenDocument {
    fn current(&self) -> Option<&Document> {
        match self {
            Self::Synced(doc) => Some(doc),
            Self::Desynced(_) => None,
        }
    }

    fn version(&self) -> i64 {
        match self {
            Self::Synced(doc) => doc.version,
            Self::Desynced(version) => *version,
        }
    }

    fn len(&self) -> usize {
        self.current().map_or(0, |doc| doc.text.len())
    }
}

/// Language Server Protocol state for one client. Use [Self::handle] to dispatch messages.
pub struct Server<'a> {
    host: &'a dyn Host,
    semantic: Option<&'a dyn SemanticProvider>,
    state: State,
    encoding: PositionEncoding,
    markdown_hover: bool,
    workspace_roots: Vec<PathBuf>,
    bundled_modules: Option<PathBuf>,
    disk_cache: RefCell<DiskCache>,
    documents: BTreeMap<String, OpenDocument>,
    open_bytes: usize,
}

impl<'a> Server<'a> {
    /// Start a server with lexical analysis and no semantic provider.
    pub fn new(host: &'a dyn Host) -> Self {
        Self {
            host,
            semantic: None,
            state: State::New,
            encoding: PositionEncoding::Utf16,
            markdown_hover: false,
            workspace_roots: Vec::new(),
            bundled_modules: None,
            disk_cache: RefCell::default(),
            documents: BTreeMap::new(),
            open_bytes: 0,
        }
    }

    /// Add a semantic provider to the server.
    pub fn with_semantic_provider(mut self, provider: &'a dyn SemanticProvider) -> Self {
        self.semantic = Some(provider);
        self
    }

    /// Return the process status after the client sends `exit`.
    pub fn exit_code(&self) -> Option<i32> {
        match self.state {
            State::Exited(code) => Some(code),
            _ => None,
        }
    }

    /// Process one message and return all outgoing messages in order.
    pub fn handle(&mut self, message: RpcMessage) -> Vec<RpcMessage> {
        let RpcMessage::Call { id, method, params } = message else {
            return Vec::new();
        };
        if self.exit_code().is_some() {
            return Vec::new();
        }
        if let Some(id) = id {
            let response = match self.request(&method, &params) {
                Ok(result) => RpcMessage::Result { id, result },
                Err((code, message)) => RpcMessage::error(id, code, message),
            };
            vec![response]
        } else {
            self.notification(&method, &params).unwrap_or_default()
        }
    }

    fn request(&mut self, method: &str, params: &Value) -> Result<Value, (i64, &'static str)> {
        if self.state == State::New {
            if method != "initialize" {
                return Err((-32002, "Server has not been initialized."));
            }
            if !matches!(params, Value::Object(_)) {
                return Err(INVALID_PARAMS);
            }
            self.encoding = get(params, "capabilities")
                .and_then(|caps| get(caps, "general"))
                .and_then(|general| get(general, "positionEncodings"))
                .and_then(array)
                .filter(|encodings| {
                    encodings
                        .iter()
                        .filter_map(string)
                        .any(|value| value == "utf-8")
                })
                .map_or(PositionEncoding::Utf16, |_| PositionEncoding::Utf8);
            self.markdown_hover = get(params, "capabilities")
                .and_then(|caps| get(caps, "textDocument"))
                .and_then(|document| get(document, "hover"))
                .and_then(|hover| get(hover, "contentFormat"))
                .and_then(array)
                .into_iter()
                .flatten()
                .filter_map(string)
                .find(|format| matches!(*format, "markdown" | "plaintext"))
                == Some("markdown");
            self.workspace_roots = workspace_roots(params);
            self.bundled_modules = discover_cmake_root(self.host, None)
                .1
                .map(|root| root.join("Modules"));
            self.state = State::AwaitInitialized;
            return Ok(capabilities(self.encoding));
        }
        if method == "initialize" || self.state == State::Shutdown {
            return Err((-32600, "Request is invalid in the current server state."));
        }
        if method == "shutdown" {
            self.state = State::Shutdown;
            return Ok(Value::Null);
        }
        if self.state != State::Running {
            return Err((-32002, "Server has not received initialized."));
        }
        match method {
            "textDocument/documentSymbol" | "textDocument/semanticTokens/full" => {
                let uri = document_uri(params).ok_or(INVALID_PARAMS)?;
                let Some(doc) = self.documents.get(uri).and_then(OpenDocument::current) else {
                    return Ok(Value::Null);
                };
                Ok(if method == "textDocument/documentSymbol" {
                    features::symbols(doc, self.encoding)
                } else {
                    features::semantic_tokens(doc, self.encoding)
                })
            }
            "textDocument/formatting" => {
                let uri = document_uri(params).ok_or(INVALID_PARAMS)?;
                let Some(doc) = self.documents.get(uri).and_then(OpenDocument::current) else {
                    return Ok(Value::Null);
                };
                let options = get(params, "options").ok_or(INVALID_PARAMS)?;
                let tab_size = get(options, "tabSize")
                    .and_then(int)
                    .filter(|&size| (1..=32).contains(&size))
                    .ok_or(INVALID_PARAMS)?;
                let indent = match get(options, "insertSpaces") {
                    Some(Value::Bool(true)) => " ".repeat(tab_size as usize),
                    Some(Value::Bool(false)) => "\t".into(),
                    _ => return Err(INVALID_PARAMS),
                };
                doc.formatting(self.encoding, &indent).ok_or((
                    -32803,
                    "Formatting exceeds the 16 MiB response limit. Format a smaller file.",
                ))
            }
            "textDocument/hover" | "textDocument/definition" | "textDocument/completion" => {
                let uri = document_uri(params).ok_or(INVALID_PARAMS)?;
                let position = get(params, "position")
                    .and_then(Position::parse)
                    .ok_or(INVALID_PARAMS)?;
                let Some(doc) = self.documents.get(uri).and_then(OpenDocument::current) else {
                    return Ok(Value::Null);
                };
                let offset = doc
                    .lines
                    .offset(&doc.text, position, self.encoding)
                    .ok_or(INVALID_PARAMS)?;
                let context = SemanticContext {
                    uri,
                    text: &doc.text,
                    version: doc.version,
                    position,
                    offset,
                    encoding: self.encoding,
                };
                Ok(match method {
                    "textDocument/hover" => self.hover(doc, &context),
                    "textDocument/definition" => self.definition(doc, &context),
                    _ => features::completion(doc, || self.workspace(uri), offset, self.encoding),
                })
            }
            _ => Err((-32601, "Method not found.")),
        }
    }

    fn notification(&mut self, method: &str, params: &Value) -> Option<Vec<RpcMessage>> {
        if method == "exit" {
            self.state = State::Exited(i32::from(self.state != State::Shutdown));
            return None;
        }
        if self.state == State::AwaitInitialized && method == "initialized" {
            self.state = State::Running;
            return None;
        }
        if self.state != State::Running {
            return None;
        }
        match method {
            "textDocument/didOpen" => {
                let doc = get(params, "textDocument")?;
                let uri = get(doc, "uri").and_then(string)?;
                let version = get(doc, "version").and_then(int)?;
                let text = get(doc, "text").and_then(string)?;
                if uri.is_empty() || uri.len() > 8192 {
                    return None;
                }
                match self.documents.get(uri) {
                    Some(OpenDocument::Synced(_)) => return None,
                    None if self.documents.len() >= MAX_DOCUMENTS => return None,
                    _ => {}
                }
                Some(self.accept_text(uri, Ok(text.to_owned()), version))
            }
            "textDocument/didChange" => {
                let uri = document_uri(params)?;
                let doc = self.documents.get(uri)?;
                let Some(version) = get(get(params, "textDocument")?, "version").and_then(int)
                else {
                    return Some(self.desync(uri, doc.version(), SyncFailure::InvalidChange));
                };
                if version <= doc.version() {
                    return None;
                }
                let text = get(params, "contentChanges")
                    .and_then(array)
                    .ok_or(SyncFailure::InvalidChange)
                    .and_then(|changes| self.apply_changes(doc, changes));
                Some(self.accept_text(uri, text, version))
            }
            "textDocument/didSave" => {
                let uri = document_uri(params)?;
                let doc = self.documents.get(uri)?;
                if let Some(text) = get(params, "text") {
                    let text = string(text)
                        .map(str::to_owned)
                        .ok_or(SyncFailure::InvalidChange);
                    Some(self.accept_text(uri, text, doc.version()))
                } else {
                    Some(vec![self.publish(uri)])
                }
            }
            "textDocument/didClose" => {
                let uri = document_uri(params)?;
                let doc = self.documents.remove(uri)?;
                self.open_bytes -= doc.len();
                if doc.current().is_some()
                    && let Some(provider) = self.semantic
                {
                    provider.update(SemanticUpdate::Closed { uri });
                }
                Some(vec![self.publish(uri)])
            }
            "$/cancelRequest" => {
                // Each synchronous request is complete before this notification arrives.
                None
            }
            _ => None,
        }
    }

    fn desync(&mut self, uri: &str, version: i64, failure: SyncFailure) -> Vec<RpcMessage> {
        let previous = self
            .documents
            .insert(uri.to_owned(), OpenDocument::Desynced(version));
        self.open_bytes -= previous.as_ref().map_or(0, OpenDocument::len);
        if matches!(previous, Some(OpenDocument::Synced(_)))
            && let Some(provider) = self.semantic
        {
            provider.update(SemanticUpdate::Closed { uri });
        }
        let mut messages = vec![self.publish(uri)];
        if !matches!(previous, Some(OpenDocument::Desynced(_))) {
            messages.push(RpcMessage::notification(
                "window/showMessage",
                object([
                    ("type", Value::Int(2)),
                    (
                        "message",
                        Value::string(format!(
                            "Rcmake lost synchronization with {uri}. {}",
                            failure.recovery()
                        )),
                    ),
                ]),
            ));
        }
        messages
    }

    fn apply_changes(&self, doc: &OpenDocument, changes: &[Value]) -> Result<String, SyncFailure> {
        if changes.is_empty() {
            return Err(SyncFailure::InvalidChange);
        }
        if changes.len() > MAX_CHANGES {
            return Err(SyncFailure::ChangeLimit);
        }
        let other_bytes = self.open_bytes - doc.len();
        let (mut text, mut lines, changes) = if let Some(full) = changes
            .iter()
            .rposition(|change| get(change, "range").is_none())
        {
            let (change, tail) = changes
                .get(full..)
                .and_then(<[Value]>::split_first)
                .ok_or(SyncFailure::InvalidChange)?;
            let replacement = change_text(change)?;
            check_buffer_size(replacement.len(), other_bytes)?;
            if tail.is_empty() {
                return Ok(replacement.to_owned());
            }
            (replacement.to_owned(), Lines::new(replacement), tail)
        } else {
            let doc = doc.current().ok_or(SyncFailure::AwaitingText)?;
            (doc.text.clone(), doc.lines.clone(), changes)
        };
        let mut work_left = MAX_CHANGE_WORK;
        for change in changes {
            let replacement = change_text(change)?;
            let span = get(change, "range").ok_or(SyncFailure::InvalidChange)?;
            let start = edit_position(span, "start")?;
            let end = edit_position(span, "end")?;
            if (end.line, end.character) < (start.line, start.character) {
                return Err(SyncFailure::InvalidChange);
            }
            let start = self.edit_offset(&lines, &text, start, &mut work_left)?;
            let end = self.edit_offset(&lines, &text, end, &mut work_left)?;
            check_buffer_size(text.len() - (end - start) + replacement.len(), other_bytes)?;
            if !lines.replace(&mut text, start..end, replacement, &mut work_left) {
                return Err(SyncFailure::WorkLimit);
            }
        }
        Ok(text)
    }

    fn edit_offset(
        &self,
        lines: &Lines,
        text: &str,
        position: Position,
        work_left: &mut usize,
    ) -> Result<usize, SyncFailure> {
        let offset = lines
            .edit_offset(text, position, self.encoding)
            .ok_or(SyncFailure::InvalidChange)?;
        if self.encoding == PositionEncoding::Utf16 {
            let start = lines
                .start(position.line)
                .ok_or(SyncFailure::InvalidChange)?;
            *work_left = work_left
                .checked_sub(offset - start)
                .ok_or(SyncFailure::WorkLimit)?;
        }
        Ok(offset)
    }

    fn accept_text(
        &mut self,
        uri: &str,
        text: Result<String, SyncFailure>,
        version: i64,
    ) -> Vec<RpcMessage> {
        match text.and_then(|text| self.replace(uri, text, version)) {
            Ok(message) => vec![message],
            Err(failure) => self.desync(uri, version, failure),
        }
    }

    fn replace(
        &mut self,
        uri: &str,
        text: String,
        version: i64,
    ) -> Result<RpcMessage, SyncFailure> {
        let old_len = self.documents.get(uri).map_or(0, OpenDocument::len);
        let other_bytes = self.open_bytes - old_len;
        check_buffer_size(text.len(), other_bytes)?;
        self.open_bytes = other_bytes + text.len();
        let doc = Document::new(text, version);
        if let Some(provider) = self.semantic {
            provider.update(SemanticUpdate::Changed {
                uri,
                text: &doc.text,
                version,
            });
        }
        self.documents
            .insert(uri.to_owned(), OpenDocument::Synced(doc));
        Ok(self.publish(uri))
    }

    fn publish(&self, uri: &str) -> RpcMessage {
        let mut params = vec![("uri", Value::string(uri))];
        if let Some(doc) = self.documents.get(uri) {
            params.push(("version", Value::Int(doc.version())));
            let diagnostics = doc
                .current()
                .map_or_else(Vec::new, |doc| doc.diagnostics(self.encoding));
            params.push(("diagnostics", Value::Array(diagnostics)));
        } else {
            params.push(("diagnostics", Value::Array(Vec::new())));
        }
        RpcMessage::notification("textDocument/publishDiagnostics", object(params))
    }

    fn hover(&self, doc: &Document, context: &SemanticContext<'_>) -> Value {
        if let Some((text, version)) = self
            .semantic
            .and_then(|provider| provider.hover_with_version(context))
        {
            let mut text =
                if let Some(version) = version.filter(|&version| version < context.version) {
                    format!("Provisional. This result uses document version {version}.\n\n{text}")
                } else {
                    text
                };
            if self.markdown_hover {
                text = features::escape_markdown(&text);
            }
            return object([(
                "contents",
                object([
                    (
                        "kind",
                        Value::string(if self.markdown_hover {
                            "markdown"
                        } else {
                            "plaintext"
                        }),
                    ),
                    ("value", Value::string(text)),
                ]),
            )]);
        }
        features::hover(
            doc,
            context.uri,
            || self.workspace(context.uri),
            context.offset,
            self.encoding,
            self.markdown_hover,
        )
    }

    fn definition(&self, doc: &Document, context: &SemanticContext<'_>) -> Value {
        if let Some(target) = self
            .semantic
            .and_then(|provider| provider.definition(context))
            .filter(|target| {
                self.documents
                    .get(&target.uri)
                    .is_none_or(|doc| doc.current().is_some())
            })
        {
            return features::location(&target.uri, range(target.start, target.end));
        }
        features::definition(
            doc,
            context.uri,
            || self.workspace(context.uri),
            context.offset,
            self.encoding,
        )
    }

    fn workspace<'b>(&'b self, current: &'b str) -> Workspace<'b> {
        Workspace::new(
            self.host,
            current,
            &self.workspace_roots,
            self.bundled_modules.as_deref(),
            self.documents
                .iter()
                .map(|(uri, doc)| (uri.as_str(), doc.current())),
            &mut self.disk_cache.borrow_mut(),
        )
    }
}

const INVALID_PARAMS: (i64, &str) = (-32602, "Invalid method parameters.");

fn document_uri(params: &Value) -> Option<&str> {
    string(get(get(params, "textDocument")?, "uri")?)
}

fn workspace_roots(params: &Value) -> Vec<PathBuf> {
    let mut roots: Vec<_> = get(params, "workspaceFolders")
        .and_then(array)
        .into_iter()
        .flatten()
        .filter_map(|folder| get(folder, "uri").and_then(string).and_then(uri::path))
        .take(MAX_DOCUMENTS)
        .collect();
    if roots.is_empty() {
        roots.extend(get(params, "rootUri").and_then(string).and_then(uri::path));
    }
    roots
}

fn check_buffer_size(bytes: usize, other_bytes: usize) -> Result<(), SyncFailure> {
    if bytes > MAX_BUFFER_BYTES {
        return Err(SyncFailure::BufferLimit);
    }
    if bytes + other_bytes > MAX_OPEN_BYTES {
        return Err(SyncFailure::OpenBufferLimit);
    }
    Ok(())
}

fn change_text(change: &Value) -> Result<&str, SyncFailure> {
    get(change, "text")
        .and_then(string)
        .ok_or(SyncFailure::InvalidChange)
}

fn edit_position(span: &Value, endpoint: &str) -> Result<Position, SyncFailure> {
    get(span, endpoint)
        .and_then(Position::parse)
        .ok_or(SyncFailure::InvalidChange)
}

fn capabilities(encoding: PositionEncoding) -> Value {
    object([
        (
            "serverInfo",
            object([
                ("name", Value::string("rcmake-lsp")),
                ("version", Value::string(env!("CARGO_PKG_VERSION"))),
            ]),
        ),
        (
            "capabilities",
            object([
                ("positionEncoding", Value::string(encoding.name())),
                (
                    "textDocumentSync",
                    object([
                        ("openClose", Value::Bool(true)),
                        ("change", Value::Int(2)),
                        ("save", object([("includeText", Value::Bool(true))])),
                    ]),
                ),
                ("documentSymbolProvider", Value::Bool(true)),
                ("documentFormattingProvider", Value::Bool(true)),
                ("definitionProvider", Value::Bool(true)),
                ("hoverProvider", Value::Bool(true)),
                ("completionProvider", object([])),
                (
                    "semanticTokensProvider",
                    object([
                        ("full", Value::Bool(true)),
                        (
                            "legend",
                            object([
                                (
                                    "tokenTypes",
                                    Value::Array(features::TOKEN_TYPES.map(Value::string).into()),
                                ),
                                ("tokenModifiers", Value::Array(Vec::new())),
                            ]),
                        ),
                    ]),
                ),
            ]),
        ),
    ])
}
