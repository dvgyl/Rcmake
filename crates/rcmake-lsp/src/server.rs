//! Synchronous language server and the interface for semantic analysis.

use std::collections::BTreeMap;

use rcmake_host::{FileKind, Follow, Host, discover_cmake_root};
use rcmake_json::Value;

use crate::{
    analysis::Document,
    features,
    json::{array, get, int, object, string},
    position::{Lines, Position, PositionEncoding, range},
    rpc::{MAX_MESSAGE_BYTES, RpcMessage},
    uri,
};

const MAX_DOCUMENTS: usize = 512;
const MAX_BUFFER_BYTES: usize = MAX_MESSAGE_BYTES;
const MAX_OPEN_BYTES: usize = MAX_BUFFER_BYTES * 4;

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
/// analysis. Implementations can return results from their last completed snapshot.
pub trait SemanticProvider {
    /// Update the provider's document snapshot after the server accepts a change.
    fn update(&self, _update: SemanticUpdate<'_>) {}

    /// Return hover text for the current site.
    fn hover(&self, _context: &SemanticContext<'_>) -> Option<String> {
        None
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
            self.notification(&method, &params).into_iter().collect()
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
            "textDocument/documentSymbol"
            | "textDocument/formatting"
            | "textDocument/semanticTokens/full" => {
                let uri = document_uri(params).ok_or(INVALID_PARAMS)?;
                let Some(doc) = self.documents.get(uri).and_then(OpenDocument::current) else {
                    return Ok(Value::Null);
                };
                Ok(match method {
                    "textDocument/documentSymbol" => features::symbols(doc, self.encoding),
                    "textDocument/semanticTokens/full" => {
                        features::semantic_tokens(doc, self.encoding)
                    }
                    _ => {
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
                        doc.formatting(self.encoding, &indent)
                    }
                })
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
                    _ => features::completion(doc, offset),
                })
            }
            _ => Err((-32601, "Method not found.")),
        }
    }

    fn notification(&mut self, method: &str, params: &Value) -> Option<RpcMessage> {
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
                match self.documents.get(uri) {
                    Some(OpenDocument::Synced(_)) => return None,
                    None if self.documents.len() >= MAX_DOCUMENTS => return None,
                    _ => {}
                }
                self.replace(uri, text, version)
            }
            "textDocument/didChange" => {
                let uri = document_uri(params)?;
                let doc = self.documents.get(uri)?;
                let Some(version) = get(get(params, "textDocument")?, "version").and_then(int)
                else {
                    return self.desync(uri, doc.version());
                };
                if version <= doc.version() {
                    return None;
                }
                let text = get(params, "contentChanges")
                    .and_then(array)
                    .and_then(|changes| self.apply_changes(doc, changes));
                if let Some(text) = text
                    && let Some(notification) = self.replace(uri, &text, version)
                {
                    return Some(notification);
                }
                self.desync(uri, version)
            }
            "textDocument/didSave" => {
                let uri = document_uri(params)?;
                let doc = self.documents.get(uri)?;
                if let Some(text) = get(params, "text") {
                    let version = doc.version();
                    self.replace(uri, string(text)?, version)
                        .or_else(|| self.desync(uri, version))
                } else {
                    Some(self.publish(uri))
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
                Some(self.publish(uri))
            }
            "$/cancelRequest" => {
                // Each synchronous request is complete before this notification arrives.
                None
            }
            _ => None,
        }
    }

    fn desync(&mut self, uri: &str, version: i64) -> Option<RpcMessage> {
        let previous = self
            .documents
            .insert(uri.to_owned(), OpenDocument::Desynced(version))?;
        self.open_bytes -= previous.len();
        if previous.current().is_some()
            && let Some(provider) = self.semantic
        {
            provider.update(SemanticUpdate::Closed { uri });
        }
        Some(self.publish(uri))
    }

    fn apply_changes(&self, doc: &OpenDocument, mut changes: &[Value]) -> Option<String> {
        if changes.is_empty() || changes.len() > 1024 {
            return None;
        }
        let limit = MAX_BUFFER_BYTES.min(MAX_OPEN_BYTES - (self.open_bytes - doc.len()));
        let mut text = if let Some(full) = changes
            .iter()
            .rposition(|change| get(change, "range").is_none())
        {
            changes = changes.get(full..)?;
            String::new()
        } else {
            doc.current()?.text.clone()
        };
        let mut work = 0usize;
        for change in changes {
            let replacement = get(change, "text").and_then(string)?;
            if let Some(span) = get(change, "range") {
                // Bound repeated line indexing and editing work for one notification.
                work = work.checked_add(text.len())?;
                if work > MAX_OPEN_BYTES {
                    return None;
                }
                let lines = Lines::new(&text);
                let start = get(span, "start").and_then(Position::parse)?;
                let end = get(span, "end").and_then(Position::parse)?;
                if (end.line, end.character) < (start.line, start.character) {
                    return None;
                }
                let start = lines.offset(&text, start, self.encoding)?;
                let end = lines.offset(&text, end, self.encoding)?;
                let old = text.get(start..end)?;
                if text.len() - old.len() + replacement.len() > limit {
                    return None;
                }
                text.replace_range(start..end, replacement);
            } else {
                if replacement.len() > limit {
                    return None;
                }
                text.clear();
                text.push_str(replacement);
            }
        }
        Some(text)
    }

    fn replace(&mut self, uri: &str, text: &str, version: i64) -> Option<RpcMessage> {
        if uri.is_empty() || uri.len() > 8192 || text.len() > MAX_BUFFER_BYTES {
            return None;
        }
        let old_len = self.documents.get(uri).map_or(0, OpenDocument::len);
        let total = self.open_bytes - old_len + text.len();
        if total > MAX_OPEN_BYTES {
            return None;
        }
        self.documents.insert(
            uri.to_owned(),
            OpenDocument::Synced(Document::new(text.to_owned(), version)),
        );
        self.open_bytes = total;
        if let Some(provider) = self.semantic {
            provider.update(SemanticUpdate::Changed { uri, text, version });
        }
        Some(self.publish(uri))
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
        if let Some(text) = self.semantic.and_then(|provider| provider.hover(context)) {
            return object([(
                "contents",
                object([
                    ("kind", Value::string("plaintext")),
                    ("value", Value::string(text)),
                ]),
            )]);
        }
        features::hover(doc, context.offset, self.encoding)
    }

    fn definition(&self, doc: &Document, context: &SemanticContext<'_>) -> Value {
        if let Some(target) = self
            .semantic
            .and_then(|provider| provider.definition(context))
        {
            return features::location(&target.uri, range(target.start, target.end));
        }
        if let Some(uri) = self.file_definition(doc, context) {
            return features::location(&uri, range(Position::default(), Position::default()));
        }
        features::definition(doc, context.uri, context.offset, self.encoding)
    }

    fn file_definition(&self, doc: &Document, context: &SemanticContext<'_>) -> Option<String> {
        let path = uri::path(context.uri)?;
        let mut paths = features::file_reference(doc, context.offset, path.parent()?.to_owned());
        if let Some(module) = features::module_name(doc, context.offset)
            && let (_, Some(root)) = discover_cmake_root(self.host, None)
        {
            paths.push(root.join("Modules").join(format!("{module}.cmake")));
        }
        for path in paths {
            let candidate = uri::file(&path);
            if self.documents.contains_key(&candidate) {
                return Some(candidate);
            }
            if self
                .host
                .metadata(&path, Follow::Yes)
                .is_ok_and(|metadata| metadata.kind == FileKind::File)
            {
                let destination = self.host.canonicalize(&path).unwrap_or(path);
                return Some(uri::file(&destination));
            }
        }
        None
    }
}

const INVALID_PARAMS: (i64, &str) = (-32602, "Invalid method parameters.");

fn document_uri(params: &Value) -> Option<&str> {
    string(get(get(params, "textDocument")?, "uri")?)
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
