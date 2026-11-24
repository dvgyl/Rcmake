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
    Host, HostError, HostResult, ReadRange, Stream, WriteMode as HostWriteMode,
    overlay::OverlayHost, path::to_native,
};

use crate::{
    SemanticContext, SemanticLocation, SemanticProvider, SemanticUpdate, analysis::Document, uri,
};

const DEFAULT_STEP_BUDGET: u64 = 250_000;
const MAX_TRACKED_FILES: usize = 4096;
const MAX_TRACKED_BYTES: usize = 32 * 1024 * 1024;
const MAX_CAPTURED_OUTPUT: usize = 64 * 1024;

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
    path: PathBuf,
    order: u64,
    document: Document,
}

#[derive(Clone, Default)]
struct Buffers {
    by_uri: BTreeMap<String, Arc<Buffer>>,
    selected: BTreeMap<PathBuf, Arc<Buffer>>,
}

impl Buffers {
    fn at(&self, host: &dyn Host, path: &Path) -> Option<&Buffer> {
        let path = buffer_path(host, path);
        let buffer = self.selected.get(&path)?;
        (buffer_path(host, &uri::path(&buffer.uri)?) == path).then_some(buffer.as_ref())
    }

    fn select(&mut self, path: &Path) {
        let active = self
            .by_uri
            .values()
            .filter(|buffer| buffer.path == path)
            .max_by_key(|buffer| buffer.order);
        if let Some(active) = active {
            self.selected.insert(path.to_owned(), Arc::clone(active));
        } else {
            self.selected.remove(path);
        }
    }
}

type Evaluate = dyn Fn(&PreparedConfigure) -> Result<ConfigureResult, ApiError> + Send + Sync;

#[derive(Default)]
struct InputReads {
    files: BTreeMap<PathBuf, Option<Box<[u8]>>>,
    bytes: usize,
}

impl InputReads {
    fn record(&mut self, path: &Path, bytes: &[u8]) {
        if let Some(previous) = self.files.get_mut(path) {
            if previous
                .as_deref()
                .is_some_and(|previous| previous != bytes)
            {
                *previous = None;
            }
        } else if self.files.len() < MAX_TRACKED_FILES
            && self.bytes.saturating_add(bytes.len()) <= MAX_TRACKED_BYTES
        {
            self.files.insert(path.to_owned(), Some(bytes.into()));
            self.bytes += bytes.len();
        }
    }
}

#[derive(Default)]
struct CaptureOutputHost {
    output: Mutex<Vec<u8>>,
}

impl CaptureOutputHost {
    fn capture(&self, _stream: Stream, bytes: &[u8]) -> bool {
        let mut output = self
            .output
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let available = MAX_CAPTURED_OUTPUT.saturating_sub(output.len());
        output.extend(bytes.iter().take(available).copied());
        true
    }
}

struct Snapshot {
    result: ConfigureResult,
    paths: BTreeMap<PathBuf, BTreeSet<BString>>,
    inputs: Arc<Buffers>,
    reads: InputReads,
}

impl Snapshot {
    fn new(
        result: ConfigureResult,
        inputs: Arc<Buffers>,
        reads: InputReads,
        host: &dyn Host,
    ) -> Self {
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
        Self {
            result,
            paths,
            inputs,
            reads,
        }
    }

    fn matches(&self, path: &Path, bytes: &[u8]) -> bool {
        self.reads
            .files
            .get(path)
            .and_then(Option::as_deref)
            .is_some_and(|evaluated| evaluated == bytes)
    }
}

fn read_file(host: &dyn Host, path: &Path) -> HostResult<Vec<u8>> {
    host.read(
        path,
        ReadRange::All {
            limit: crate::rpc::MAX_MESSAGE_BYTES as u64,
        },
    )
}

struct State {
    buffers: Arc<Buffers>,
    generation: u64,
    revision: Arc<()>,
    dirty: bool,
    running: Option<CancelToken>,
    draining: bool,
    snapshot: Option<Arc<Snapshot>>,
}

/// Result of a refresh request that did not fail.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RefreshStatus {
    /// The current buffers already have a refresh result.
    UpToDate,
    /// A worker will process the pending edit before it returns.
    Busy,
    /// Evaluation produced a snapshot for the current buffers.
    Completed,
    /// The evaluator reached its command budget before it could return a snapshot.
    BudgetExceeded {
        /// Configured command limit.
        limit: u64,
    },
}

/// An evaluated reply tied to the editor state used to compute it.
pub struct SnapshotReply<T> {
    revision: Arc<()>,
    value: T,
}

impl<T> SnapshotReply<T> {
    /// Deliver a reply only if its editor state is still current.
    ///
    /// The callback runs under the state lock. It must not call the provider.
    pub fn publish(self, provider: &SnapshotProvider, send: impl FnOnce(T)) -> bool {
        let state = provider.lock();
        if !Arc::ptr_eq(&self.revision, &state.revision) {
            return false;
        }
        send(self.value);
        true
    }

    fn current(self, provider: &SnapshotProvider) -> Option<T> {
        Arc::ptr_eq(&self.revision, &provider.lock().revision).then_some(self.value)
    }
}

struct Evaluation {
    snapshot: Snapshot,
    error: Option<ApiError>,
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
    /// A concurrent edit cancels the current generation. The active worker
    /// evaluates the latest generation before it returns. An explicit
    /// cancellation stops the worker and leaves the edit pending.
    ///
    /// # Errors
    ///
    /// Return a preparation, evaluation, or cancellation error. Evaluation
    /// errors with configure diagnostics also publish a partial snapshot.
    pub fn refresh(&self) -> Result<RefreshStatus, ApiError> {
        let mut state = self.lock();
        if state.running.is_some() {
            state.draining = true;
            return Ok(RefreshStatus::Busy);
        }
        while state.dirty {
            state.dirty = false;
            state.draining = true;
            let token = CancelToken::default();
            state.running = Some(token.clone());
            let buffers = Arc::clone(&state.buffers);
            drop(state);

            let result = self.evaluate(buffers, &token);
            state = self.lock();
            state.running = None;
            if token.is_cancelled() {
                if state.draining && state.dirty {
                    continue;
                }
                return Err(ApiError::Cancelled);
            }
            let evaluation = result?;
            state.revision = Arc::new(());
            state.snapshot = Some(Arc::new(evaluation.snapshot));
            return match evaluation.error {
                None => Ok(RefreshStatus::Completed),
                Some(ApiError::StepBudgetExceeded { limit }) => {
                    Ok(RefreshStatus::BudgetExceeded { limit })
                }
                Some(error) => Err(error),
            };
        }
        Ok(RefreshStatus::UpToDate)
    }

    /// Cancel an active configure and leave the current edit pending.
    pub fn cancel(&self) {
        let mut state = self.lock();
        if let Some(token) = &state.running {
            token.cancel();
            state.draining = false;
            state.dirty = true;
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
        let mut state = self.lock();
        let prior = state.buffers.by_uri.get(uri);
        let update = if let Some((text, version)) = change {
            let Some(path) = uri::path(uri) else {
                return;
            };
            if prior.is_some_and(|buffer| {
                buffer.document.version > version
                    || (buffer.document.version == version && buffer.document.text == text)
            }) || i32::try_from(version).is_err()
            {
                return;
            }
            let path = buffer_path(self.request.host.as_ref(), &path);
            Some(Arc::new(Buffer {
                uri: uri.to_owned(),
                path,
                order: state.generation + 1,
                document: Document::new(text.to_owned(), version),
            }))
        } else if prior.is_some() {
            None
        } else {
            return;
        };
        if let Some(token) = &state.running {
            token.cancel();
        }
        let buffers = Arc::make_mut(&mut state.buffers);
        let prior = buffers.by_uri.remove(uri);
        if let Some(buffer) = &update {
            buffers.by_uri.insert(uri.to_owned(), Arc::clone(buffer));
            buffers.select(&buffer.path);
        }
        if let Some(prior) = prior
            && update
                .as_ref()
                .is_none_or(|buffer| buffer.path != prior.path)
        {
            buffers.select(&prior.path);
        }
        state.generation += 1;
        state.revision = Arc::new(());
        state.dirty = true;
    }

    fn evaluate(&self, buffers: Arc<Buffers>, token: &CancelToken) -> Result<Evaluation, ApiError> {
        let overlay = BufferOverlayHost::new(Arc::clone(&self.request.host));
        for (path, buffer) in &buffers.selected {
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
        let mut session = ConfigureRequest {
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
        let reads = Arc::new(Mutex::new(InputReads::default()));
        let recorded = Arc::clone(&reads);
        let capture = CaptureOutputHost::default();
        session.observe_host(
            move |path, bytes| {
                recorded
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .record(path, bytes);
            },
            move |stream, bytes| capture.capture(stream, bytes),
        );
        let (result, error) = match (self.evaluate)(&session) {
            Ok(result) => (result, None),
            Err(error @ (ApiError::ConfigureFailed { .. } | ApiError::GenerateFailed { .. })) => {
                let diagnostics = match &error {
                    ApiError::ConfigureFailed { diagnostics, .. }
                    | ApiError::GenerateFailed { diagnostics, .. } => diagnostics.clone(),
                    _ => Vec::new(),
                };
                (
                    ConfigureResult {
                        diagnostics,
                        completion: partial(Vec::new()),
                        ..ConfigureResult::default()
                    },
                    Some(error),
                )
            }
            Err(ApiError::DeniedProcess { commands }) => (
                ConfigureResult {
                    completion: partial(commands),
                    ..ConfigureResult::default()
                },
                None,
            ),
            Err(error @ ApiError::StepBudgetExceeded { .. }) => (
                ConfigureResult {
                    diagnostics: vec![rcmake_diag::Diagnostic {
                        kind: rcmake_diag::MessageKind::Error,
                        effect: rcmake_diag::Effect::Fatal,
                        category: None,
                        policy: None,
                        location: rcmake_diag::Location::None,
                        body: error.to_string().into(),
                        annotations: None,
                    }],
                    completion: partial(Vec::new()),
                    ..ConfigureResult::default()
                },
                Some(error),
            ),
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
        let result = session.finish(result);
        let reads = std::mem::take(
            &mut *reads
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        );
        Ok(Evaluation {
            snapshot: Snapshot::new(result, buffers, reads, overlay.overlay.as_ref()),
            error,
        })
    }

    fn query<T>(&self, query: impl FnOnce(&Snapshot, &Buffers) -> T) -> Option<SnapshotReply<T>> {
        if self.refresh_on_query {
            let _ = self.refresh();
        }
        let (snapshot, buffers, revision) = {
            let state = self.lock();
            (
                state.snapshot.clone()?,
                Arc::clone(&state.buffers),
                Arc::clone(&state.revision),
            )
        };
        let value = query(&snapshot, &buffers);
        Arc::ptr_eq(&revision, &self.lock().revision).then_some(SnapshotReply { revision, value })
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
                    generation: 0,
                    revision: Arc::new(()),
                    dirty: true,
                    running: None,
                    draining: false,
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
