//! Synchronous language server and the interface for semantic analysis.

use std::{
    cell::RefCell,
    collections::{BTreeMap, BTreeSet},
    ops::Range,
    path::{Path, PathBuf},
    sync::Arc,
};

use rcmake_host::{Host, discover_cmake_root};
use rcmake_json::Value;
use rcmake_language_tools::{
    Config, ConfigCache, Finding, FormatStyle, HostWorkspaceReader, LintDependencies, LintLimits,
    ResolvedConfig,
    discovery::{infer_project_root, normalize_lexical},
    lint_bytes,
};

use crate::{
    analysis::{self, Document},
    catalog::{code_block, code_span, escape_markdown},
    features,
    json::{array, get, int, object, string},
    position::{Lines, Position, PositionEncoding, range},
    rpc::{MAX_MESSAGE_BYTES, RequestId, RpcMessage},
    uri,
    workspace::{Workspace, WorkspaceCache},
};

#[cfg(all(feature = "eval", feature = "stdio"))]
mod evaluated_stdio;

mod capabilities;
use capabilities::capabilities;

const MAX_DOCUMENTS: usize = 512;
const MAX_BUFFER_BYTES: usize = MAX_MESSAGE_BYTES;
const MAX_OPEN_BYTES: usize = MAX_BUFFER_BYTES * 4;
const MAX_CHANGES: usize = 1024;
// Limit the estimated bytes that a batch scans or moves.
const MAX_CHANGE_WORK: usize = 2 * 1024 * 1024 * 1024;

pub use crate::{
    HoverDoc, SemanticContext, SemanticDefinition, SemanticDiagnostic, SemanticDocument,
    SemanticLocation, SemanticProvider, SemanticResult, SemanticSource, SemanticUpdate,
};

#[derive(Clone, Copy, PartialEq, Eq)]
enum State {
    New,
    AwaitInitialized,
    Running,
    Shutdown,
    Exited(i32),
}

enum OpenDocument {
    Synced {
        doc: Arc<Document>,
        show_parse_error: bool,
    },
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
    fn current(&self) -> Option<&Arc<Document>> {
        match self {
            Self::Synced { doc, .. } => Some(doc),
            Self::Desynced(_) => None,
        }
    }

    fn version(&self) -> i64 {
        match self {
            Self::Synced { doc, .. } => doc.version,
            Self::Desynced(version) => *version,
        }
    }

    fn len(&self) -> usize {
        self.current().map_or(0, |doc| doc.text.len())
    }
}

enum Provider<'a> {
    Borrowed(&'a dyn SemanticProvider),
    #[cfg(all(feature = "eval", feature = "stdio"))]
    Shared(Arc<dyn SemanticProvider>),
}

/// Language Server Protocol state for one client. Use [Self::handle] to dispatch messages.
pub struct Server<'a> {
    host: &'a dyn Host,
    semantic: Option<Provider<'a>>,
    state: State,
    encoding: PositionEncoding,
    markdown_hover: bool,
    workspace_roots: Vec<uri::Root>,
    watch_files: bool,
    #[cfg(all(feature = "eval", feature = "stdio"))]
    disk_watches: evaluated_stdio::DiskWatches,
    versioned_edits: bool,
    bundled_modules: Option<PathBuf>,
    workspace_cache: RefCell<WorkspaceCache>,
    config_cache: RefCell<ConfigCache>,
    lint_deps: RefCell<BTreeMap<String, LintDependencies>>,
    documents: BTreeMap<String, OpenDocument>,
    open_bytes: usize,
    open_limit_reported: bool,
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
            watch_files: false,
            #[cfg(all(feature = "eval", feature = "stdio"))]
            disk_watches: evaluated_stdio::DiskWatches::default(),
            versioned_edits: false,
            bundled_modules: None,
            workspace_cache: RefCell::default(),
            config_cache: RefCell::default(),
            lint_deps: RefCell::default(),
            documents: BTreeMap::new(),
            open_bytes: 0,
            open_limit_reported: false,
        }
    }

    /// Add a semantic provider to the server.
    pub fn with_semantic_provider(mut self, provider: &'a dyn SemanticProvider) -> Self {
        self.semantic = Some(Provider::Borrowed(provider));
        self
    }

    fn semantic(&self) -> Option<&dyn SemanticProvider> {
        match self.semantic.as_ref()? {
            Provider::Borrowed(provider) => Some(*provider),
            #[cfg(all(feature = "eval", feature = "stdio"))]
            Provider::Shared(provider) => Some(provider.as_ref()),
        }
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
                .and_then(|encodings| {
                    encodings
                        .iter()
                        .filter_map(string)
                        .find_map(PositionEncoding::from_name)
                })
                .unwrap_or(PositionEncoding::Utf16);
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
            let workspace = get(params, "capabilities").and_then(|caps| get(caps, "workspace"));
            self.watch_files = workspace
                .and_then(|caps| get(caps, "didChangeWatchedFiles"))
                .and_then(|caps| get(caps, "dynamicRegistration"))
                == Some(&Value::Bool(true));
            #[cfg(all(feature = "eval", feature = "stdio"))]
            {
                self.disk_watches.relative = workspace
                    .and_then(|caps| get(caps, "didChangeWatchedFiles"))
                    .and_then(|caps| get(caps, "relativePatternSupport"))
                    == Some(&Value::Bool(true));
            }
            self.versioned_edits = workspace
                .and_then(|caps| get(caps, "workspaceEdit"))
                .and_then(|caps| get(caps, "documentChanges"))
                == Some(&Value::Bool(true));
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
                    analysis::semantic_tokens(doc, self.encoding)
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
                let insert_spaces = match get(options, "insertSpaces") {
                    Some(Value::Bool(flag)) => *flag,
                    _ => return Err(INVALID_PARAMS),
                };
                let resolved = self.resolve_doc_config(uri);
                let style = FormatStyle::from_config_with_client_fallback(
                    &resolved.config.fmt,
                    tab_size as usize,
                    insert_spaces,
                );
                doc.formatting(self.encoding, &style).ok_or((
                    -32803,
                    "Formatting exceeds the 16 MiB response limit. Format a smaller file.",
                ))
            }
            "textDocument/codeAction" => self.code_actions(params),
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
                    _ => self
                        .semantic()
                        .and_then(|provider| provider.completions(&context))
                        .filter(|result| self.current_result(result, uri, doc.version))
                        .map_or_else(
                            || {
                                features::completion(
                                    doc,
                                    || self.workspace(uri),
                                    offset,
                                    self.encoding,
                                )
                            },
                            |result| features::completion_items(result.value),
                        ),
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
            return self.watch_files.then(|| vec![register_watchers()]);
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
                    Some(OpenDocument::Synced { .. }) => return None,
                    None if self.documents.len() >= MAX_DOCUMENTS => {
                        if std::mem::replace(&mut self.open_limit_reported, true) {
                            return None;
                        }
                        return Some(vec![show_warning(format!(
                            "rcmake supports up to {MAX_DOCUMENTS} open files. Close other files, then reopen this file."
                        ))]);
                    }
                    _ => {}
                }
                Some(self.accept_text(uri, Ok(text.to_owned()), version, true))
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
                let changes = get(params, "contentChanges").and_then(array);
                let full_text = changes.is_some_and(|changes| {
                    changes.iter().any(|change| get(change, "range").is_none())
                });
                let text = changes
                    .ok_or(SyncFailure::InvalidChange)
                    .and_then(|changes| self.apply_changes(doc, changes));
                Some(self.accept_text(uri, text, version, full_text))
            }
            "textDocument/didSave" => {
                let uri = document_uri(params)?;
                let doc = self.documents.get_mut(uri)?;
                if let Some(text) = get(params, "text") {
                    let text = string(text)
                        .map(str::to_owned)
                        .ok_or(SyncFailure::InvalidChange);
                    let version = doc.version();
                    Some(self.accept_text(uri, text, version, true))
                } else {
                    if let OpenDocument::Synced {
                        show_parse_error, ..
                    } = doc
                    {
                        *show_parse_error = true;
                    }
                    Some(vec![self.publish(uri)])
                }
            }
            "textDocument/didClose" => {
                let uri = document_uri(params)?;
                let doc = self.documents.remove(uri)?;
                self.open_bytes -= doc.len();
                self.workspace_cache.get_mut().invalidate_documents();
                self.lint_deps.borrow_mut().remove(uri);
                if doc.current().is_some()
                    && let Some(provider) = self.semantic()
                {
                    provider.update(SemanticUpdate::Closed { uri });
                }
                let mut messages = vec![self.publish(uri)];
                if let Some(path) = uri::path(uri) {
                    for caller in self.affected_callers_for_file(&path, Some(uri)) {
                        messages.push(self.publish(&caller));
                    }
                }
                Some(messages)
            }
            "workspace/didChangeWatchedFiles" => {
                let changes: Vec<PathBuf> = get(params, "changes")
                    .and_then(array)?
                    .iter()
                    .take(4096)
                    .filter_map(|change| get(change, "uri").and_then(string).and_then(uri::path))
                    .collect();
                let mut affected_uris = BTreeSet::new();
                for path in changes {
                    self.workspace_cache
                        .get_mut()
                        .invalidate_file(self.host, &path);
                    let is_config = path.file_name().is_some_and(|name| {
                        name == rcmake_language_tools::config::CONFIG_FILE_NAME
                    });
                    if is_config {
                        self.config_cache.borrow_mut().clear();
                        affected_uris.extend(self.affected_docs_for_config(&path));
                    } else {
                        affected_uris.extend(self.affected_callers_for_file(&path, None));
                    }
                }
                if affected_uris.is_empty() {
                    None
                } else {
                    Some(
                        affected_uris
                            .into_iter()
                            .map(|uri| self.publish(&uri))
                            .collect(),
                    )
                }
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
        self.workspace_cache.get_mut().invalidate_documents();
        if matches!(previous, Some(OpenDocument::Synced { .. }))
            && let Some(provider) = self.semantic()
        {
            provider.update(SemanticUpdate::Closed { uri });
        }
        let mut messages = vec![self.publish(uri)];
        if !matches!(previous, Some(OpenDocument::Desynced(_))) {
            messages.push(show_warning(format!(
                "rcmake lost synchronization with {uri}. {}",
                failure.recovery()
            )));
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
        if self.encoding != PositionEncoding::Utf8 {
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
        show_parse_error: bool,
    ) -> Vec<RpcMessage> {
        match text.and_then(|text| self.replace(uri, text, version, show_parse_error)) {
            Ok(()) => {
                let mut messages = vec![self.publish(uri)];
                if let Some(path) = uri::path(uri) {
                    for caller in self.affected_callers_for_file(&path, Some(uri)) {
                        messages.push(self.publish(&caller));
                    }
                }
                messages
            }
            Err(failure) => self.desync(uri, version, failure),
        }
    }

    fn replace(
        &mut self,
        uri: &str,
        text: String,
        version: i64,
        show_parse_error: bool,
    ) -> Result<(), SyncFailure> {
        let old_len = self.documents.get(uri).map_or(0, OpenDocument::len);
        let other_bytes = self.open_bytes - old_len;
        check_buffer_size(text.len(), other_bytes)?;
        self.open_bytes = other_bytes + text.len();
        let doc = Arc::new(Document::new(text, version));
        if let Some(provider) = self.semantic() {
            provider.update(SemanticUpdate::Changed {
                uri,
                text: &doc.text,
                version,
            });
        }
        self.documents.insert(
            uri.to_owned(),
            OpenDocument::Synced {
                doc,
                show_parse_error,
            },
        );
        self.workspace_cache.get_mut().invalidate_documents();
        Ok(())
    }

    /// Publish updated diagnostics for an open document after a snapshot becomes available.
    ///
    /// A caller can use this method when its semantic worker finishes.
    pub fn refresh_diagnostics(&self, uri: &str) -> Option<RpcMessage> {
        self.documents.get(uri)?.current()?;
        Some(self.publish(uri))
    }

    fn publish(&self, uri: &str) -> RpcMessage {
        let mut params = vec![("uri", Value::string(uri))];
        let mut diagnostics = Vec::new();
        if let Some(doc) = self.documents.get(uri) {
            params.push(("version", Value::Int(doc.version())));
            if let OpenDocument::Synced {
                doc,
                show_parse_error,
            } = doc
            {
                let semantic = self.semantic_diagnostics(&self.semantic_document(uri, doc));
                let lint_findings = self.lint_document(uri, doc);
                diagnostics =
                    doc.diagnostics(self.encoding, *show_parse_error, &semantic, &lint_findings);
            } else {
                self.lint_deps.borrow_mut().remove(uri);
            }
        } else {
            self.lint_deps.borrow_mut().remove(uri);
        }
        params.push(("diagnostics", Value::Array(diagnostics)));
        RpcMessage::notification("textDocument/publishDiagnostics", object(params))
    }

    fn root_for_path(&self, path: &Path) -> Option<PathBuf> {
        let matched = self
            .workspace_roots
            .iter()
            .map(|r| &r.path)
            .filter(|root| path.starts_with(root))
            .max_by_key(|root| root.components().count())
            .cloned();
        matched.or_else(|| {
            let parent = path.parent().unwrap_or(path);
            let cwd = self.host.initial_cwd();
            Some(infer_project_root(self.host, parent, &cwd))
        })
    }

    fn resolve_doc_config(&self, uri: &str) -> ResolvedConfig {
        let Some(path) = uri::path(uri) else {
            return ResolvedConfig {
                config: Config::default(),
                config_path: None,
                checked_dirs: Vec::new(),
            };
        };
        let dir = path.parent().unwrap_or(&path);
        let boundary = self.root_for_path(&path);
        match self
            .config_cache
            .borrow_mut()
            .resolve(self.host, dir, boundary.as_deref())
        {
            Ok(resolved) => resolved,
            Err(_) => {
                // Keep checked directories tracked even when a config file has an edit error so
                // fixing it republishes diagnostics immediately.
                let mut checked_dirs = Vec::new();
                let mut cur = Some(dir);
                while let Some(d) = cur {
                    checked_dirs.push(d.to_owned());
                    if boundary.as_deref() == Some(d) {
                        break;
                    }
                    cur = d.parent();
                }
                ResolvedConfig {
                    config: Config::default(),
                    config_path: None,
                    checked_dirs,
                }
            }
        }
    }

    fn lint_document(&self, uri: &str, doc: &Document) -> Vec<Finding> {
        let path = uri::path(uri).unwrap_or_else(|| PathBuf::from(uri));
        let root = self.root_for_path(&path);
        let resolved = self.resolve_doc_config(uri);

        let mut readable_roots: Vec<PathBuf> = self
            .workspace_roots
            .iter()
            .map(|r| r.path.clone())
            .collect();
        if let Some(root_dir) = &root
            && !readable_roots.contains(root_dir)
        {
            readable_roots.push(root_dir.clone());
        }

        let mut reader =
            HostWorkspaceReader::new(self.host, readable_roots, self.bundled_modules.clone());
        // Prefer ALL open synced LSP buffers over disk state.
        for (open_uri, open_doc) in &self.documents {
            if let Some(synced) = open_doc.current()
                && let Some(open_path) = uri::path(open_uri)
            {
                reader.insert_open_buffer(open_path, Arc::from(synced.text.as_bytes()));
            }
        }

        let mut deps = LintDependencies {
            included_files: BTreeSet::new(),
            config_dirs: resolved
                .checked_dirs
                .into_iter()
                .map(|d| normalize_lexical(&d))
                .collect(),
            config_file: resolved.config_path.map(|p| normalize_lexical(&p)),
        };

        let findings = match lint_bytes(
            &path,
            doc.text.as_bytes(),
            &resolved.config.lint,
            &LintLimits::default(),
            &reader,
            root.as_deref(),
        ) {
            Ok(result) => {
                deps.included_files = result.dependencies.included_files;
                result.findings
            }
            Err(_) => Vec::new(),
        };

        self.lint_deps.borrow_mut().insert(uri.to_owned(), deps);
        findings
    }

    fn affected_callers_for_file(
        &self,
        changed_path: &Path,
        exclude_uri: Option<&str>,
    ) -> Vec<String> {
        let norm = normalize_lexical(changed_path);
        let canonical = self.host.canonicalize(&norm).ok();
        let deps = self.lint_deps.borrow();
        let mut affected = Vec::new();
        for (uri, doc_deps) in deps.iter() {
            if exclude_uri == Some(uri.as_str()) {
                continue;
            }
            if !self
                .documents
                .get(uri)
                .is_some_and(|doc| doc.current().is_some())
            {
                continue;
            }
            let matches_include = doc_deps.included_files.iter().any(|inc| {
                inc == &norm
                    || canonical.as_ref().is_some_and(|c| inc == c)
                    || canonical
                        .as_ref()
                        .is_some_and(|c| self.host.canonicalize(inc).ok().as_ref() == Some(c))
            });
            if matches_include {
                affected.push(uri.clone());
            }
        }
        affected
    }

    fn affected_docs_for_config(&self, config_path: &Path) -> Vec<String> {
        let norm_cfg = normalize_lexical(config_path);
        let cfg_dir = norm_cfg.parent().map(normalize_lexical);
        let canonical_dir = cfg_dir
            .as_deref()
            .and_then(|d| self.host.canonicalize(d).ok());
        let deps = self.lint_deps.borrow();
        let mut affected = Vec::new();
        for (uri, doc_deps) in deps.iter() {
            if !self
                .documents
                .get(uri)
                .is_some_and(|doc| doc.current().is_some())
            {
                continue;
            }
            let same_file = doc_deps.config_file.as_ref() == Some(&norm_cfg);
            let in_checked_dir = cfg_dir.as_ref().is_some_and(|dir| {
                doc_deps.config_dirs.iter().any(|checked| {
                    checked == dir
                        || canonical_dir.as_ref().is_some_and(|c| checked == c)
                        || canonical_dir.as_ref().is_some_and(|c| {
                            self.host.canonicalize(checked).ok().as_ref() == Some(c)
                        })
                })
            });
            if same_file || in_checked_dir {
                affected.push(uri.clone());
            }
        }
        affected
    }

    fn semantic_document<'b>(&self, uri: &'b str, doc: &'b Document) -> SemanticDocument<'b> {
        SemanticDocument {
            uri,
            text: &doc.text,
            version: doc.version,
            encoding: self.encoding,
        }
    }

    fn semantic_diagnostics(&self, document: &SemanticDocument<'_>) -> Vec<SemanticDiagnostic> {
        self.semantic()
            .and_then(|provider| provider.diagnostics(document))
            .filter(|result| self.current_result(result, document.uri, document.version))
            .map_or_else(Vec::new, |result| result.value)
    }

    fn code_actions(&self, params: &Value) -> Result<Value, (i64, &'static str)> {
        let uri = document_uri(params).ok_or(INVALID_PARAMS)?;
        let span = get(params, "range").ok_or(INVALID_PARAMS)?;
        let start = edit_position(span, "start").map_err(|_| INVALID_PARAMS)?;
        let end = edit_position(span, "end").map_err(|_| INVALID_PARAMS)?;
        if (start.line, start.character) > (end.line, end.character) {
            return Err(INVALID_PARAMS);
        }
        let Some(doc) = self.documents.get(uri).and_then(OpenDocument::current) else {
            return Ok(Value::Array(Vec::new()));
        };
        let document = self.semantic_document(uri, doc);
        let only = get(params, "context")
            .and_then(|context| get(context, "only"))
            .and_then(array);
        let allows = |kind: &str| {
            only.is_none_or(|only| {
                only.iter().filter_map(string).any(|prefix| {
                    kind == prefix
                        || kind
                            .strip_prefix(prefix)
                            .is_some_and(|tail| tail.starts_with('.'))
                })
            })
        };
        let selection: Range<Position> = start..end;
        let mut actions = if allows("quickfix") {
            doc.quick_fixes(
                uri,
                selection.clone(),
                self.encoding,
                self.versioned_edits,
                &self.semantic_diagnostics(&document),
                |uri, version| {
                    let doc = self.documents.get(uri)?.current()?;
                    (doc.version == version).then_some(doc.as_ref())
                },
            )
        } else {
            Vec::new()
        };
        if let Some(result) = self
            .semantic()
            .and_then(|provider| provider.code_actions(&document, selection))
            .filter(|result| self.current_result(result, uri, doc.version))
        {
            actions.extend(
                result
                    .value
                    .into_iter()
                    .filter(|action| {
                        get(action, "kind")
                            .and_then(string)
                            .map_or(only.is_none(), allows)
                    })
                    .take(256 - actions.len()),
            );
        }
        Ok(Value::Array(actions))
    }

    fn hover(&self, doc: &Document, context: &SemanticContext<'_>) -> Value {
        if let Some(provider) = self.semantic() {
            let content = match provider.hover_doc(context) {
                Some(result)
                    if result.version <= context.version
                        && self.same_uri(&result.uri, context.uri) =>
                {
                    Some((
                        render_hover_doc(result.value, self.markdown_hover),
                        Some(result.version),
                    ))
                }
                Some(_) => None,
                None => provider.hover_with_version(context).map(|(text, version)| {
                    (
                        if self.markdown_hover {
                            escape_markdown(&text)
                        } else {
                            text
                        },
                        version,
                    )
                }),
            };
            if let Some((text, version)) = content
                && version.is_none_or(|version| version <= context.version)
            {
                let text = if let Some(version) =
                    version.filter(|&version| version < context.version)
                {
                    format!("Provisional. This result uses document version {version}.\n\n{text}")
                } else {
                    text
                };
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
        if let Some(provider) = self.semantic() {
            let target = match provider.definition_result(context) {
                Some(result) if self.current_result(&result, context.uri, doc.version) => {
                    let definition = result.value;
                    self.semantic_location(&definition.location, Some(definition.target_version))
                }
                Some(_) => None,
                None => provider
                    .definition(context)
                    .and_then(|location| self.semantic_location(&location, None)),
            };
            if let Some(target) = target {
                return target;
            }
        }
        features::definition(
            doc,
            context.uri,
            || self.workspace(context.uri),
            context.offset,
            self.encoding,
        )
    }

    fn semantic_location(
        &self,
        target: &SemanticLocation,
        version: Option<Option<i64>>,
    ) -> Option<Value> {
        let open = self.documents.get_key_value(&target.uri).or_else(|| {
            self.documents
                .iter()
                .find(|(uri, _)| self.same_uri(uri, &target.uri))
        });
        let uri = if let Some((uri, doc)) = open {
            let doc = doc.current()?;
            if version.is_some_and(|version| version != Some(doc.version)) {
                return None;
            }
            uri.clone()
        } else {
            if matches!(version, Some(Some(_))) {
                return None;
            }
            uri::path(&target.uri).map_or_else(
                || target.uri.clone(),
                |path| uri::client_file(&path, &self.workspace_roots),
            )
        };
        Some(features::location(&uri, range(target.start, target.end)))
    }

    fn same_uri(&self, left: &str, right: &str) -> bool {
        if left == right {
            return true;
        }
        let (Some(left), Some(right)) = (uri::path(left), uri::path(right)) else {
            return false;
        };
        uri::file(&left) == uri::file(&right)
            || self.host.canonicalize(&left).ok().is_some_and(|left| {
                self.host
                    .canonicalize(&right)
                    .is_ok_and(|right| left == right)
            })
    }

    fn current_result<T>(&self, result: &SemanticResult<T>, uri: &str, version: i64) -> bool {
        result.version == version && self.same_uri(&result.uri, uri)
    }

    fn workspace(&self, current: &str) -> Workspace {
        self.workspace_cache.borrow_mut().workspace(
            self.host,
            current,
            &self.workspace_roots,
            self.bundled_modules.as_deref(),
            self.documents
                .iter()
                .map(|(uri, doc)| (uri.as_str(), doc.current())),
        )
    }
}

const INVALID_PARAMS: (i64, &str) = (-32602, "Invalid method parameters.");

fn document_uri(params: &Value) -> Option<&str> {
    string(get(get(params, "textDocument")?, "uri")?)
}

fn workspace_roots(params: &Value) -> Vec<uri::Root> {
    let mut roots: Vec<_> = get(params, "workspaceFolders")
        .and_then(array)
        .into_iter()
        .flatten()
        .filter_map(|folder| get(folder, "uri").and_then(string).and_then(uri::Root::new))
        .take(MAX_DOCUMENTS)
        .collect();
    if roots.is_empty() {
        roots.extend(
            get(params, "rootUri")
                .and_then(string)
                .and_then(uri::Root::new),
        );
    }
    roots
}

fn show_warning(message: String) -> RpcMessage {
    RpcMessage::notification(
        "window/showMessage",
        object([("type", Value::Int(2)), ("message", Value::string(message))]),
    )
}

fn register_watchers() -> RpcMessage {
    let watchers = ["**/CMakeLists.txt", "**/*.cmake", "**/.rcmake.toml"]
        .map(|pattern| object([("globPattern", Value::string(pattern))]));
    RpcMessage::request(
        RequestId::String("rcmake-watch-listfiles".into()),
        "client/registerCapability",
        object([(
            "registrations",
            Value::Array(vec![object([
                ("id", Value::string("rcmake-watch-listfiles")),
                ("method", Value::string("workspace/didChangeWatchedFiles")),
                (
                    "registerOptions",
                    object([("watchers", Value::Array(watchers.into()))]),
                ),
            ])]),
        )]),
    )
}

fn render_hover_doc(doc: HoverDoc, markdown: bool) -> String {
    let mut parts = Vec::new();
    if !doc.title.is_empty() {
        parts.push(if markdown {
            code_span(&doc.title)
        } else {
            doc.title
        });
    }
    if !doc.body_md.is_empty() {
        parts.push(doc.body_md);
    }
    if let Some(code) = doc.code {
        parts.push(if markdown { code_block(&code) } else { code });
    }
    parts.join(
        "

",
    )
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
