use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use jiff::tz::TimeZone;

use rcmake_base::{BString, Limits};
use rcmake_diag::{DiagOptions, Diagnostic};
use rcmake_eval::{CacheDefine, CancelToken, LedgerLevel, Services};
use rcmake_host::{
    CopyOptions, DirEntry, FileLockGuard, FileTime, Follow, GracefulTimeout, Host, HostError,
    HostResult, Metadata, ReadRange, RunOutput, RunRequest, Stream, SystemInfo,
    WriteMode as HostWriteMode,
    no_process::{DeniedCommand, NoProcessHost},
    overlay::OverlayHost,
    path,
};
use rcmake_model::{Cache, GeneratorKind};

use crate::{CompletionState, ConfigureResult, OverlayDiff};

/// Policy for subprocess requests during configure.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ProcessPolicy {
    /// Permit subprocesses where the host and write mode support them.
    #[default]
    Allow,
    /// Reject subprocesses and record the requests.
    Deny,
}

/// Destination of writes made through the session host.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum WriteMode {
    /// Write through the supplied host.
    #[default]
    Direct,
    /// Keep changes in memory. Subprocesses are denied in this mode.
    Overlay,
}

/// An error at the configure API boundary.
#[derive(Debug, thiserror::Error)]
pub enum ApiError {
    /// A request field has an invalid value.
    #[error("Invalid {field}. {message}")]
    InvalidConfig {
        /// Field that caused the error.
        field: &'static str,
        /// Description of the required value.
        message: String,
    },
    /// The caller cancelled the evaluation.
    #[error("Configure was cancelled.")]
    Cancelled,
    /// The evaluator exhausted its command budget.
    #[error("Configure reached its limit of {limit} commands.")]
    StepBudgetExceeded {
        /// Configured command limit.
        limit: u64,
    },
    /// A subprocess was denied and a partial result was unavailable.
    #[error("Configure could not continue after a subprocess request was denied.")]
    DeniedProcess {
        /// Subprocess requests retained by the host.
        commands: Vec<DeniedCommand>,
    },
    /// Evaluation failed.
    #[error("Configure failed with exit code {exit_code}.")]
    ConfigureFailed {
        /// Exit code from the evaluator.
        exit_code: i32,
        /// Diagnostics emitted during evaluation.
        diagnostics: Vec<Diagnostic>,
        /// Source map for the diagnostic annotations, if available.
        sources: Option<Arc<rcmake_diag::SourceMap>>,
    },
    /// Resolution or generation failed after evaluation.
    #[error("Generation failed. {message}")]
    GenerateFailed {
        /// Resolver or generator error.
        message: String,
        /// Diagnostics emitted during evaluation and generation.
        diagnostics: Vec<Diagnostic>,
        /// Source map for the diagnostic annotations, if available.
        sources: Option<Arc<rcmake_diag::SourceMap>>,
    },
    /// A host operation failed.
    #[error(transparent)]
    Io(#[from] HostError),
}

/// Settings for one project configure.
#[derive(Clone)]
pub struct ConfigureRequest {
    /// Directory that contains the top-level CMakeLists.txt.
    pub source_dir: PathBuf,
    /// Directory that receives configure and generation output.
    pub build_dir: PathBuf,
    /// Generator selected by the caller. An absent value permits automatic selection.
    pub generator: Option<GeneratorKind>,
    /// Saved cache entries, with their help text and properties.
    pub initial_cache: Option<Cache>,
    /// Cache definitions in application order.
    pub defines: Vec<CacheDefine>,
    /// Directory that contains the CMake Modules and Templates directories.
    pub cmake_root: PathBuf,
    /// Host supplied by the caller.
    pub host: Arc<dyn Host>,
    /// Policy for subprocess requests.
    pub process_policy: ProcessPolicy,
    /// Destination of writes.
    pub write_mode: WriteMode,
    /// Evaluator services and their default settings.
    pub services: Services,
    /// Build an evaluated workspace index.
    pub collect_index: bool,
    /// Cancellation token that overrides the token in services, if supplied.
    pub cancel: Option<CancelToken>,
    /// Command budget that overrides the budget in services, if supplied.
    pub step_budget: Option<u64>,
    /// Evaluator safety limits.
    pub limits: Limits,
    /// Initial diagnostic settings.
    pub diag: DiagOptions,
}

impl ConfigureRequest {
    /// Create a request with direct writes and the default evaluator settings.
    pub fn new(
        source_dir: impl Into<PathBuf>,
        build_dir: impl Into<PathBuf>,
        cmake_root: impl Into<PathBuf>,
        host: Arc<dyn Host>,
    ) -> Self {
        Self {
            source_dir: source_dir.into(),
            build_dir: build_dir.into(),
            generator: None,
            initial_cache: None,
            defines: Vec::new(),
            cmake_root: cmake_root.into(),
            host,
            process_policy: ProcessPolicy::default(),
            write_mode: WriteMode::default(),
            services: Services::default(),
            collect_index: false,
            cancel: None,
            step_budget: None,
            limits: Limits::default(),
            diag: DiagOptions::default(),
        }
    }

    /// Select a generator.
    #[must_use]
    pub fn generator(mut self, generator: GeneratorKind) -> Self {
        self.generator = Some(generator);
        self
    }

    /// Seed the saved cache before applying caller definitions.
    #[must_use]
    pub fn initial_cache(mut self, cache: Cache) -> Self {
        self.initial_cache = Some(cache);
        self
    }

    /// Append a cache definition.
    #[must_use]
    pub fn define(mut self, definition: CacheDefine) -> Self {
        self.defines.push(definition);
        self
    }

    /// Set the subprocess policy.
    #[must_use]
    pub fn process_policy(mut self, policy: ProcessPolicy) -> Self {
        self.process_policy = policy;
        self
    }

    /// Set the write mode.
    #[must_use]
    pub fn write_mode(mut self, mode: WriteMode) -> Self {
        self.write_mode = mode;
        self
    }

    /// Set the evaluator services.
    #[must_use]
    pub fn services(mut self, services: Services) -> Self {
        self.services = services;
        self
    }

    /// Select whether to build an evaluated index.
    #[must_use]
    pub fn collect_index(mut self, collect: bool) -> Self {
        self.collect_index = collect;
        self
    }

    /// Set the cancellation token.
    #[must_use]
    pub fn cancel(mut self, cancel: CancelToken) -> Self {
        self.cancel = Some(cancel);
        self
    }

    /// Set the command budget.
    #[must_use]
    pub fn step_budget(mut self, limit: u64) -> Self {
        self.step_budget = Some(limit);
        self
    }

    /// Set the evaluator safety limits.
    #[must_use]
    pub fn limits(mut self, limits: Limits) -> Self {
        self.limits = limits;
        self
    }

    /// Set the initial diagnostic options.
    #[must_use]
    pub fn diag(mut self, diag: DiagOptions) -> Self {
        self.diag = diag;
        self
    }

    /// Validate the request before any filesystem operation.
    ///
    /// Relative paths resolve against the host's initial working directory.
    ///
    /// # Errors
    ///
    /// Return an error for an invalid path or cache name, a cancelled session,
    /// or a zero command budget.
    pub fn validate(&self) -> Result<(), ApiError> {
        self.validate_at(&self.host.initial_cwd())
    }

    fn validate_at(&self, cwd: &Path) -> Result<(), ApiError> {
        for (field, value) in [
            ("source_dir", &self.source_dir),
            ("build_dir", &self.build_dir),
            ("cmake_root", &self.cmake_root),
        ] {
            if value.as_os_str().is_empty() || path::from_native(value).contains(&0) {
                return Err(ApiError::InvalidConfig {
                    field,
                    message: "Supply a nonempty path without null bytes.".into(),
                });
            }
            if !cwd.join(value).is_absolute() {
                return Err(ApiError::InvalidConfig {
                    field,
                    message: "Supply an absolute path or an absolute host working directory."
                        .into(),
                });
            }
        }
        for entry in &self.defines {
            if entry.name.is_empty()
                || entry
                    .name
                    .iter()
                    .any(|byte| matches!(*byte, 0 | b'\n' | b'\r' | b'"'))
            {
                return Err(ApiError::InvalidConfig {
                    field: "defines",
                    message: format!(
                        "Cannot write cache name {:?}. Use a nonempty name without null bytes, line breaks, or double quotes.",
                        entry.name
                    ),
                });
            }
        }
        if self
            .cancel
            .as_ref()
            .or(self.services.cancel.as_ref())
            .is_some_and(CancelToken::is_cancelled)
        {
            return Err(ApiError::Cancelled);
        }
        if self.step_budget.or(self.services.step_budget) == Some(0) {
            return Err(ApiError::StepBudgetExceeded { limit: 0 });
        }
        Ok(())
    }

    /// Apply the request's host policies and merge its evaluator settings.
    ///
    /// An overlay denies subprocesses because it cannot apply their writes in
    /// memory. A subprocess request in this mode makes the result partial.
    ///
    /// # Errors
    ///
    /// Return any error from [Self::validate].
    pub fn prepare(&self) -> Result<PreparedConfigure, ApiError> {
        let cwd = self.host.initial_cwd();
        self.validate_at(&cwd)?;
        let absolute = |path: &Path| cwd.join(path);
        let overlay = (self.write_mode == WriteMode::Overlay)
            .then(|| Arc::new(OverlayHost::from_shared(Arc::clone(&self.host))));
        let mut host: Arc<dyn Host> = match &overlay {
            Some(overlay) => Arc::clone(overlay) as Arc<dyn Host>,
            None => Arc::clone(&self.host),
        };
        let denied = if self.process_policy == ProcessPolicy::Deny || overlay.is_some() {
            let adapter = Arc::new(NoProcessHost::from_shared(host));
            host = Arc::clone(&adapter) as Arc<dyn Host>;
            Some(adapter)
        } else {
            None
        };
        let mut services = self.services.clone();
        services.cancel = self.cancel.clone().or(services.cancel);
        services.step_budget = self.step_budget.or(services.step_budget);
        if self.collect_index && matches!(services.provenance, LedgerLevel::Off) {
            services.provenance = LedgerLevel::Full { per_name_cap: 16 };
        }
        let source_dir = absolute(&self.source_dir);
        let build_dir = absolute(&self.build_dir);
        let cmake_root = absolute(&self.cmake_root);
        Ok(PreparedConfigure {
            source_dir,
            build_dir,
            cmake_root,
            cwd,
            generator: self.generator,
            initial_cache: self.initial_cache.clone(),
            defines: self.defines.clone(),
            limits: self.limits.clone(),
            diag: self.diag.clone(),
            services: Arc::new(services),
            host,
            base: Arc::clone(&self.host),
            overlay,
            denied,
            collect_index: self.collect_index,
        })
    }
}

/// Host policies and resolved paths for one configure session.
pub struct PreparedConfigure {
    /// Absolute source directory.
    pub source_dir: PathBuf,
    /// Absolute build directory.
    pub build_dir: PathBuf,
    /// Absolute root of the CMake module library.
    pub cmake_root: PathBuf,
    /// Initial working directory used to resolve relative paths.
    pub cwd: PathBuf,
    /// Generator selected by the caller, if specified.
    pub generator: Option<GeneratorKind>,
    /// Saved cache entries, with their help text and properties.
    pub initial_cache: Option<Cache>,
    /// Cache definitions in application order.
    pub defines: Vec<CacheDefine>,
    /// Evaluator safety limits.
    pub limits: Limits,
    /// Initial diagnostic settings.
    pub diag: DiagOptions,
    /// Combined evaluator settings for this session.
    pub services: Arc<Services>,
    host: Arc<dyn Host>,
    base: Arc<dyn Host>,
    overlay: Option<Arc<OverlayHost<dyn Host>>>,
    denied: Option<Arc<NoProcessHost<dyn Host>>>,
    collect_index: bool,
}

impl PreparedConfigure {
    pub(crate) fn allows_persistence(&self) -> bool {
        cfg!(feature = "native") && self.overlay.is_none() && self.denied.is_none()
    }

    pub(crate) fn wants_index(&self) -> bool {
        self.collect_index
    }

    /// Get the host to use for evaluation, resolution, and generation.
    #[must_use]
    pub fn host(&self) -> Arc<dyn Host> {
        Arc::clone(&self.host)
    }

    /// Return a session host that reports complete file reads and can capture output.
    ///
    /// The read callback sees the bytes returned to the evaluator, including
    /// overlay reads. Return true from the output callback to consume a write.
    #[must_use]
    pub fn observed_host(
        &self,
        read: impl Fn(&Path, &[u8]) + Send + Sync + 'static,
        output: impl Fn(Stream, &[u8]) -> bool + Send + Sync + 'static,
    ) -> Arc<dyn Host> {
        observe_reads(
            self.host(),
            move |path, _, range, result| {
                if let (ReadRange::All { .. }, Ok(bytes)) = (range, result) {
                    read(path, bytes);
                }
            },
            output,
        )
    }

    /// Observe the host used by this session without changing its policies.
    pub fn observe_host(
        &mut self,
        read: impl Fn(&Path, &[u8]) + Send + Sync + 'static,
        output: impl Fn(Stream, &[u8]) -> bool + Send + Sync + 'static,
    ) {
        self.host = self.observed_host(read, output);
    }

    /// Get the retained subprocess requests denied during this session.
    #[must_use]
    pub fn denied_processes(&self) -> Vec<DeniedCommand> {
        self.denied
            .as_ref()
            .map_or_else(Vec::new, |host| host.denied_commands())
    }

    /// Check if the prepared host recorded or omitted any denied requests.
    #[must_use]
    pub fn has_denied_processes(&self) -> bool {
        self.denied
            .as_ref()
            .is_some_and(|host| host.has_denied_commands())
    }

    /// Attach the host's observations and build the requested index.
    ///
    /// Call this after the evaluator and generator have stopped using the host.
    /// An evaluator cancellation should return [ApiError::Cancelled] before this
    /// step if the caller requires a complete snapshot.
    #[must_use]
    pub fn finish(self, mut result: ConfigureResult) -> ConfigureResult {
        if let Some(adapter) = self.denied {
            let log = adapter.take_denied();
            if !log.commands.is_empty() || log.omitted_entries != 0 {
                if matches!(result.completion, CompletionState::Complete) {
                    result.completion = CompletionState::Partial {
                        denied_processes: Vec::new(),
                        omitted_denials: 0,
                        omitted_denial_bytes: 0,
                        cancelled: false,
                    };
                }
                if let CompletionState::Partial {
                    denied_processes,
                    omitted_denials,
                    omitted_denial_bytes,
                    ..
                } = &mut result.completion
                {
                    denied_processes.extend(log.commands);
                    *omitted_denials = omitted_denials.saturating_add(log.omitted_entries);
                    *omitted_denial_bytes = omitted_denial_bytes.saturating_add(log.omitted_bytes);
                }
            }
        }
        if let Some(overlay) = self.overlay {
            result.overlay_diff =
                Some(OverlayDiff::from_host(overlay.as_ref(), self.base.as_ref()));
        }
        if self.collect_index {
            result.build_index();
        }
        result
    }
}

/// Report the result and exact range of each read from a host.
///
/// Wrap the disk host before adding overlays to observe disk inputs. The second
/// path is the original spelling, before an overlay resolved it.
#[must_use]
pub fn observe_reads(
    base: Arc<dyn Host>,
    read: impl Fn(&Path, &Path, ReadRange, &HostResult<Vec<u8>>) + Send + Sync + 'static,
    output: impl Fn(Stream, &[u8]) -> bool + Send + Sync + 'static,
) -> Arc<dyn Host> {
    Arc::new(ObservedHost { base, read, output })
}

struct ObservedHost<R, O> {
    base: Arc<dyn Host>,
    read: R,
    output: O,
}

struct ObservedRead<'a, R> {
    inner: Box<dyn rcmake_host::ReadSeek + 'a>,
    observe: &'a R,
    path: PathBuf,
    pos: u64,
}

impl<R> Read for ObservedRead<'_, R>
where
    R: Fn(&Path, &Path, ReadRange, &HostResult<Vec<u8>>),
{
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        let capacity = output.len().min(64 * 1024);
        let chunk = output
            .get_mut(..capacity)
            .ok_or_else(|| io::Error::other("Invalid stream read length."))?;
        let size = self.inner.read(chunk)?;
        if size != 0 {
            let bytes = chunk
                .get(..size)
                .ok_or_else(|| io::Error::other("Invalid host read length."))?;
            let range = ReadRange::Slice {
                offset: self.pos,
                len: Some(size as u64),
            };
            (self.observe)(&self.path, &self.path, range, &Ok(bytes.to_vec()));
            self.pos = self.pos.saturating_add(size as u64);
        }
        Ok(size)
    }
}

impl<R> Seek for ObservedRead<'_, R> {
    fn seek(&mut self, from: SeekFrom) -> io::Result<u64> {
        self.pos = self.inner.seek(from)?;
        Ok(self.pos)
    }
}

impl<R, O> Host for ObservedHost<R, O>
where
    R: Fn(&Path, &Path, ReadRange, &HostResult<Vec<u8>>) + Send + Sync,
    O: Fn(Stream, &[u8]) -> bool + Send + Sync,
{
    fn metadata(&self, p: &Path, follow: Follow) -> HostResult<Metadata> {
        self.base.metadata(p, follow)
    }

    fn read(&self, p: &Path, range: ReadRange) -> HostResult<Vec<u8>> {
        self.read_with_origin(p, range, p)
    }

    fn read_with_origin(&self, p: &Path, range: ReadRange, origin: &Path) -> HostResult<Vec<u8>> {
        let result = self.base.read_with_origin(p, range, origin);
        (self.read)(p, origin, range, &result);
        result
    }

    fn read_chunks(&self, p: &Path, consume: &mut dyn FnMut(&[u8])) -> HostResult<()> {
        self.base.read_chunks(p, consume)
    }

    fn write(&self, p: &Path, data: &[u8], mode: HostWriteMode) -> HostResult<()> {
        self.base.write(p, data, mode)
    }

    fn read_dir(&self, p: &Path) -> HostResult<Vec<DirEntry>> {
        self.base.read_dir(p)
    }

    fn create_dir_all(&self, p: &Path) -> HostResult<()> {
        self.base.create_dir_all(p)
    }

    fn remove_file(&self, p: &Path) -> HostResult<()> {
        self.base.remove_file(p)
    }

    fn remove_dir(&self, p: &Path) -> HostResult<()> {
        self.base.remove_dir(p)
    }

    fn remove_dir_all(&self, p: &Path) -> HostResult<()> {
        self.base.remove_dir_all(p)
    }

    fn rename(&self, from: &Path, to: &Path) -> HostResult<()> {
        self.base.rename(from, to)
    }

    fn rename_noreplace(&self, from: &Path, to: &Path) -> HostResult<()> {
        self.base.rename_noreplace(from, to)
    }

    fn lock_file(&self, p: &Path, timeout: Option<Duration>) -> HostResult<Box<dyn FileLockGuard>> {
        self.base.lock_file(p, timeout)
    }

    fn copy_file(&self, from: &Path, to: &Path, opts: CopyOptions) -> HostResult<()> {
        self.base.copy_file(from, to, opts)
    }

    fn symlink(&self, target: &Path, link: &Path) -> HostResult<()> {
        self.base.symlink(target, link)
    }

    fn hard_link(&self, src: &Path, dst: &Path) -> HostResult<()> {
        self.base.hard_link(src, dst)
    }

    fn read_link(&self, p: &Path) -> HostResult<PathBuf> {
        self.base.read_link(p)
    }

    fn canonicalize(&self, p: &Path) -> HostResult<PathBuf> {
        self.base.canonicalize(p)
    }

    fn set_permissions(&self, p: &Path, mode: u32) -> HostResult<()> {
        self.base.set_permissions(p, mode)
    }

    fn set_times(
        &self,
        p: &Path,
        atime: Option<FileTime>,
        mtime: Option<FileTime>,
    ) -> HostResult<()> {
        self.base.set_times(p, atime, mtime)
    }

    fn set_symlink_times(
        &self,
        p: &Path,
        atime: Option<FileTime>,
        mtime: Option<FileTime>,
    ) -> HostResult<()> {
        self.base.set_symlink_times(p, atime, mtime)
    }

    fn supports_process_execution(&self) -> bool {
        self.base.supports_process_execution()
    }

    fn env_get(&self, key: &[u8]) -> Option<BString> {
        self.base.env_get(key)
    }

    fn env_set(&self, key: &[u8], value: Option<&[u8]>) {
        self.base.env_set(key, value);
    }

    fn env_all(&self) -> Vec<(BString, BString)> {
        self.base.env_all()
    }

    fn run(
        &self,
        req: &RunRequest,
        on_output: &mut dyn FnMut(Stream, &[u8]),
    ) -> HostResult<RunOutput> {
        self.base.run(req, on_output)
    }

    fn run_with_timeout_signal(
        &self,
        req: &RunRequest,
        timeout: GracefulTimeout,
        on_output: &mut dyn FnMut(Stream, &[u8]),
    ) -> HostResult<RunOutput> {
        self.base.run_with_timeout_signal(req, timeout, on_output)
    }

    fn registry_host_view(&self) -> Option<rcmake_host::RegistryView> {
        self.base.registry_host_view()
    }

    fn registry_value(
        &self,
        root: rcmake_host::RegistryRoot,
        subkey: &[u8],
        name: Option<&[u8]>,
        view: rcmake_host::RegistryView,
    ) -> HostResult<Option<BString>> {
        self.base.registry_value(root, subkey, name, view)
    }

    fn open_read(&self, p: &Path) -> HostResult<Box<dyn rcmake_host::ReadSeek + '_>> {
        Ok(Box::new(ObservedRead {
            inner: self.base.open_read(p)?,
            observe: &self.read,
            path: p.to_owned(),
            pos: 0,
        }))
    }

    fn scratch_file(&self) -> HostResult<Box<dyn rcmake_host::ScratchFile + '_>> {
        self.base.scratch_file()
    }

    fn scratch_file_in(&self, dir: &Path) -> HostResult<Box<dyn rcmake_host::ScratchFile + '_>> {
        self.base.scratch_file_in(dir)
    }

    fn write_from(
        &self,
        p: &Path,
        mode: HostWriteMode,
        input: &mut dyn std::io::Read,
        len: u64,
    ) -> HostResult<()> {
        self.base.write_from(p, mode, input, len)
    }

    fn run_controlled(
        &self,
        req: &RunRequest,
        graceful: Option<GracefulTimeout>,
        hard_limit: Option<Duration>,
        update: &mut rcmake_host::DeadlineUpdate<'_>,
    ) -> HostResult<rcmake_host::ControlledOutput> {
        self.base.run_controlled(req, graceful, hard_limit, update)
    }

    fn run_confined(
        &self,
        req: &RunRequest,
        scope: rcmake_host::WriteScope<'_>,
        on_output: &mut dyn FnMut(Stream, &[u8]),
    ) -> HostResult<RunOutput> {
        self.base.run_confined(req, scope, on_output)
    }

    fn run_bounded_confined(
        &self,
        req: &RunRequest,
        limit: usize,
        scope: rcmake_host::WriteScope<'_>,
    ) -> HostResult<RunOutput> {
        self.base.run_bounded_confined(req, limit, scope)
    }

    fn run_bounded(&self, req: &RunRequest, limit: usize) -> HostResult<RunOutput> {
        self.base.run_bounded(req, limit)
    }

    fn write_out(&self, stream: Stream, bytes: &[u8]) {
        if !(self.output)(stream, bytes) {
            self.base.write_out(stream, bytes);
        }
    }

    fn read_stdin(&self, limit: u64) -> HostResult<Vec<u8>> {
        self.base.read_stdin(limit)
    }

    fn is_terminal(&self, stream: Stream) -> bool {
        self.base.is_terminal(stream)
    }

    fn now(&self) -> FileTime {
        self.base.now()
    }

    fn monotonic(&self) -> Duration {
        self.base.monotonic()
    }

    fn sleep(&self, duration: Duration) {
        self.base.sleep(duration);
    }

    fn time_zone(&self) -> TimeZone {
        self.base.time_zone()
    }

    fn current_exe(&self) -> HostResult<PathBuf> {
        self.base.current_exe()
    }

    fn initial_cwd(&self) -> PathBuf {
        self.base.initial_cwd()
    }

    fn system(&self) -> SystemInfo {
        self.base.system()
    }
}

#[cfg(test)]
mod tests;
