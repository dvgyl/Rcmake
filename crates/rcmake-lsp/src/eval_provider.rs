//! Configure snapshots for editor queries over unsaved CMake listfiles.

#![forbid(unsafe_code)]

mod presentation;
mod queries;

#[cfg(test)]
mod tests;

use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Component, Path, PathBuf},
    sync::{Arc, Mutex},
};

use rcmake_api::{
    ApiError, CancelToken, CompletionState, ConfigureRequest, ConfigureResult, PreparedConfigure,
    ProcessPolicy, WriteMode,
};
use rcmake_base::BString;
use rcmake_host::{
    Host, HostError, HostResult, WriteMode as HostWriteMode,
    overlay::OverlayHost,
    path::{from_native_cmake, to_native},
};

use crate::{
    SemanticContext, SemanticLocation, SemanticProvider, SemanticUpdate, analysis::Document, uri,
};

const DEFAULT_STEP_BUDGET: u64 = 250_000;

/// An in-memory input layer over a project host.
///
/// Start one layer per configure. Use a separate write overlay for configure
/// output so an evaluation cannot change the input layer.
pub struct BufferOverlayHost {
    overlay: Arc<OverlayHost<dyn Host>>,
}

impl BufferOverlayHost {
    /// Create an empty buffer layer over a shared host.
    #[must_use]
    pub fn new(host: Arc<dyn Host>) -> Self {
        Self {
            overlay: Arc::new(OverlayHost::from_shared(host)),
        }
    }

    /// Set the text read from a file URI and any symbolic links to that file.
    ///
    /// # Errors
    ///
    /// Return a host error if the URI is invalid or the overlay cannot store the file.
    pub fn set_buffer(&self, uri: &str, text: &str) -> HostResult<()> {
        let path = uri::path(uri).ok_or_else(|| {
            HostError::from_io(
                std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "Supply an absolute file URI.",
                ),
                None,
            )
        })?;
        self.write(&buffer_path(self.overlay.as_ref(), &path), text)
    }

    /// Get the host that reads from this buffer layer.
    #[must_use]
    pub fn host(&self) -> Arc<dyn Host> {
        self.overlay.clone()
    }

    fn write(&self, path: &Path, text: &str) -> HostResult<()> {
        if let Some(parent) = path.parent() {
            self.overlay.create_dir_all(parent)?;
        }
        self.overlay
            .write(path, text.as_bytes(), HostWriteMode::Truncate)
    }
}

fn buffer_path(host: &dyn Host, path: &Path) -> PathBuf {
    if let Ok(path) = host.canonicalize(path) {
        return path;
    }
    if let Some((parent, name)) = path.parent().zip(path.file_name())
        && let Ok(parent) = host.canonicalize(parent)
    {
        return parent.join(name);
    }
    let mut normalized = PathBuf::new();
    for part in path.components() {
        match part {
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            _ => normalized.push(part),
        }
    }
    normalized
}

struct Buffer {
    uri: String,
    document: Document,
}

type Buffers = BTreeMap<PathBuf, Arc<Buffer>>;
type Evaluate = dyn Fn(&PreparedConfigure) -> Result<ConfigureResult, ApiError> + Send + Sync;

struct Snapshot {
    result: ConfigureResult,
    paths: BTreeMap<PathBuf, BTreeSet<BString>>,
    inputs: Arc<Buffers>,
    document_versions: BTreeMap<PathBuf, i32>,
}

impl Snapshot {
    fn new(result: ConfigureResult, inputs: Arc<Buffers>, host: &dyn Host) -> Self {
        let mut paths = BTreeMap::<_, BTreeSet<_>>::new();
        for file in result
            .model
            .dirs()
            .flat_map(|(_, directory)| &directory.listfiles)
            .chain(result.facts.file_edges.iter().map(|edge| &edge.site.path))
        {
            let path = to_native(file);
            if path.is_absolute() {
                paths
                    .entry(buffer_path(host, &path))
                    .or_default()
                    .insert(file.clone());
            }
        }
        let document_versions = inputs
            .iter()
            .filter_map(|(path, buffer)| {
                Some((path.clone(), i32::try_from(buffer.document.version).ok()?))
            })
            .collect();
        Self {
            result,
            paths,
            inputs,
            document_versions,
        }
    }
}

struct State {
    buffers: Arc<Buffers>,
    pending: bool,
    running: Option<CancelToken>,
    snapshot: Option<Arc<Snapshot>>,
}

/// Manages one project's buffers and configure snapshots.
///
/// Evaluation runs on the caller's thread. A caller can use a worker to call
/// [Self::refresh] while the editor supplies document updates.
pub struct SnapshotProvider {
    project_root: PathBuf,
    request: ConfigureRequest,
    evaluate: Box<Evaluate>,
    state: Mutex<State>,
    refresh_on_query: bool,
}

impl SnapshotProvider {
    /// Get the absolute source directory for this provider.
    #[must_use]
    pub fn project_root(&self) -> &Path {
        &self.project_root
    }

    /// Evaluate the latest buffers if an update is pending.
    ///
    /// A concurrent caller can supply new buffers during evaluation. That update
    /// cancels this run and makes the new buffers available for the next run.
    /// The evaluator must use the prepared host and honor its cancellation token
    /// and command budget. Send any output through diagnostic capture to keep
    /// configure output out of an editor's protocol stream.
    ///
    /// # Errors
    ///
    /// Return an error if preparation or evaluation fails, or if a later update
    /// cancels the run. Failures with configure diagnostics are published as partial results.
    pub fn refresh(&self) -> Result<(), ApiError> {
        let (buffers, token) = {
            let mut state = self.lock();
            if !state.pending || state.running.is_some() {
                return Ok(());
            }
            state.pending = false;
            let token = CancelToken::default();
            state.running = Some(token.clone());
            (Arc::clone(&state.buffers), token)
        };

        let result = self.evaluate(buffers, &token);
        let mut state = self.lock();
        state.running = None;
        if token.is_cancelled() {
            return Err(ApiError::Cancelled);
        }
        state.snapshot = Some(Arc::new(result?));
        Ok(())
    }

    /// Cancel an active configure and allow the next refresh to use the current buffers.
    pub fn cancel(&self) {
        let mut state = self.lock();
        if let Some(token) = &state.running {
            token.cancel();
            state.pending = true;
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn update(&self, update: SemanticUpdate<'_>) {
        let (uri, change) = match update {
            SemanticUpdate::Changed { uri, text, version } => (uri, Some((text, version))),
            SemanticUpdate::Closed { uri } => (uri, None),
        };
        let Some(path) = uri::path(uri) else {
            return;
        };
        let key = buffer_path(self.request.host.as_ref(), &path);
        let mut state = self.lock();
        let prior = state.buffers.iter().find(|(_, buffer)| buffer.uri == uri);
        let change = change.filter(|&(_, version)| i32::try_from(version).is_ok());
        let prior_key = prior.map(|(path, _)| path.clone());
        if let Some((text, version)) = change {
            if prior.is_some_and(|(_, buffer)| {
                buffer.document.version > version
                    || (buffer.document.version == version && buffer.document.text == text)
            }) {
                return;
            }
        } else if prior_key.is_none() {
            return;
        }
        if let Some(token) = &state.running {
            token.cancel();
        }
        let buffers = Arc::make_mut(&mut state.buffers);
        if let Some(prior_key) = prior_key.filter(|prior| change.is_none() || prior != &key) {
            buffers.remove(&prior_key);
        }
        if let Some((text, version)) = change {
            buffers.insert(
                key,
                Arc::new(Buffer {
                    uri: uri.to_owned(),
                    document: Document::new(text.to_owned(), version),
                }),
            );
        }
        state.pending = true;
    }

    fn evaluate(&self, buffers: Arc<Buffers>, token: &CancelToken) -> Result<Snapshot, ApiError> {
        let overlay = BufferOverlayHost::new(Arc::clone(&self.request.host));
        for (path, buffer) in buffers.iter() {
            if token.is_cancelled() {
                return Err(ApiError::Cancelled);
            }
            overlay.write(path, &buffer.document.text)?;
        }
        let request = self
            .request
            .clone()
            .process_policy(ProcessPolicy::Deny)
            .write_mode(WriteMode::Overlay)
            .collect_index(true)
            .cancel(token.clone())
            .step_budget(
                self.request
                    .step_budget
                    .or(self.request.services.step_budget)
                    .unwrap_or(DEFAULT_STEP_BUDGET),
            );
        let session = ConfigureRequest {
            host: overlay.host(),
            ..request
        }
        .prepare()?;
        let partial = |denied_processes| CompletionState::Partial {
            denied_processes,
            omitted_denials: 0,
            omitted_denial_bytes: 0,
            cancelled: false,
        };
        let result = match (self.evaluate)(&session) {
            Ok(result) => result,
            Err(
                ApiError::ConfigureFailed { diagnostics, .. }
                | ApiError::GenerateFailed { diagnostics, .. },
            ) => ConfigureResult {
                diagnostics,
                completion: partial(Vec::new()),
                ..ConfigureResult::default()
            },
            Err(ApiError::DeniedProcess { commands }) => ConfigureResult {
                completion: partial(commands),
                ..ConfigureResult::default()
            },
            Err(error) => return Err(error),
        };
        if token.is_cancelled()
            || matches!(
                result.completion,
                CompletionState::Partial {
                    cancelled: true,
                    ..
                }
            )
        {
            return Err(ApiError::Cancelled);
        }
        Ok(Snapshot::new(
            session.finish(result),
            buffers,
            overlay.overlay.as_ref(),
        ))
    }

    fn view(&self) -> Option<(Arc<Snapshot>, Arc<Buffers>)> {
        if self.refresh_on_query {
            let _ = self.refresh();
        }
        let state = self.lock();
        Some((state.snapshot.clone()?, Arc::clone(&state.buffers)))
    }
}

/// Evaluated hover, definitions, completion, and diagnostics for one project.
pub struct EvalSemanticProvider {
    snapshots: SnapshotProvider,
}

impl EvalSemanticProvider {
    /// Create a provider with a project request and an evaluator.
    ///
    /// The provider uses in-memory writes, denies processes, and collects an index.
    /// The evaluator runs the prepared session and returns the result before
    /// [PreparedConfigure::finish]. Use the host in that session for every input and output.
    #[must_use]
    pub fn new(
        request: ConfigureRequest,
        evaluate: impl Fn(&PreparedConfigure) -> Result<ConfigureResult, ApiError>
        + Send
        + Sync
        + 'static,
    ) -> Self {
        let project_root = request.host.initial_cwd().join(&request.source_dir);
        Self {
            snapshots: SnapshotProvider {
                project_root,
                request,
                evaluate: Box::new(evaluate),
                refresh_on_query: true,
                state: Mutex::new(State {
                    buffers: Arc::default(),
                    pending: true,
                    running: None,
                    snapshot: None,
                }),
            },
        }
    }

    /// Leave configure scheduling to the caller.
    ///
    /// Call [SnapshotProvider::refresh] after a group of edits. Queries can use
    /// the last snapshot while a worker evaluates the new buffers.
    #[must_use]
    pub fn with_explicit_refresh(mut self) -> Self {
        self.snapshots.refresh_on_query = false;
        self
    }

    /// Get the snapshot provider for explicit refresh or cancellation.
    #[must_use]
    pub fn snapshots(&self) -> &SnapshotProvider {
        &self.snapshots
    }
}

impl SemanticProvider for EvalSemanticProvider {
    fn update(&self, update: SemanticUpdate<'_>) {
        self.snapshots.update(update);
    }

    fn hover(&self, context: &SemanticContext<'_>) -> Option<String> {
        self.hover_with_version(context).map(|(text, _)| text)
    }

    fn hover_with_version(&self, context: &SemanticContext<'_>) -> Option<(String, Option<i64>)> {
        self.render_hover(context, false)
    }

    fn definition(&self, context: &SemanticContext<'_>) -> Option<SemanticLocation> {
        self.definitions(context).into_iter().next()
    }
}
