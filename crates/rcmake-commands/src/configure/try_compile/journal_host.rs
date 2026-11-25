//! Host operations for a journaled probe.

#![forbid(unsafe_code)]

use super::*;

impl Host for JournalHost<'_> {
    fn metadata(&self, path: &Path, follow: Follow) -> HostResult<Metadata> {
        let result = self.inner.metadata(path, follow);
        if !self.unobserved_read(path) {
            match &result {
                Ok(meta) if is_absolute(path) && meta.kind != FileKind::Other => {
                    let mut state = self.state();
                    state.check(
                        Check::Metadata(path.to_owned(), follow == Follow::Yes),
                        metadata_digest(meta),
                    );
                }
                Err(error)
                    if follow == Follow::Yes
                        && is_absolute(path)
                        && error.kind == ErrorKind::NotFound =>
                {
                    self.state().check(
                        Check::FollowMissing(path.to_owned()),
                        Digest::hash(b"missing"),
                    );
                }
                Err(error) => self.record_error(path, error),
                _ => self.mark_impure(ImpureReason::UnverifiableInput),
            }
        }
        result
    }

    fn read(&self, path: &Path, range: ReadRange) -> HostResult<Vec<u8>> {
        let result = self.inner.read(path, range);
        if !self.unobserved_read(path) {
            match (&result, range) {
                (Ok(bytes), ReadRange::All { .. }) if is_absolute(path) => {
                    self.state().stamp(InputStamp::FileHash {
                        path: path.to_owned(),
                        digest: Digest::hash(bytes),
                    });
                }
                (Ok(bytes), ReadRange::Slice { offset, len }) if is_absolute(path) => {
                    // Bound the verification read by the observed byte count plus one.
                    let limit = (bytes.len() as u64).saturating_add(1);
                    let range = Slice {
                        offset,
                        len: Some(len.map_or(limit, |len| len.min(limit))),
                    };
                    self.state()
                        .check(Check::Slice(path.to_owned(), range), Digest::hash(bytes));
                }
                (Err(error), _) => self.record_error(path, error),
                _ => self.mark_impure(ImpureReason::UnverifiableInput),
            }
        }
        result
    }

    fn read_chunks(&self, path: &Path, consume: &mut dyn FnMut(&[u8])) -> HostResult<()> {
        if self.private_path(path) {
            return self.inner.read_chunks(path, consume);
        }
        if !bounded_file(&*self.inner, path) {
            self.guard_effect(ImpureReason::UnverifiableInput)?;
            return self.inner.read_chunks(path, consume);
        }
        let mut key = KeyHasher::new(Domain::ProbeV1);
        let result = self.inner.read_chunks(path, &mut |chunk| {
            key.field(1, chunk);
            consume(chunk);
        });
        if is_absolute(path) && result.is_ok() {
            self.state()
                .check(Check::Chunks(path.to_owned()), key.finish());
        } else {
            self.mark_impure(ImpureReason::UnverifiableInput);
        }
        result
    }

    fn write(&self, path: &Path, bytes: &[u8], mode: WriteMode) -> HostResult<()> {
        self.guard_write(path)?;
        self.inner.write(path, bytes, mode)
    }

    fn read_dir(&self, path: &Path) -> HostResult<Vec<DirEntry>> {
        let mut result = self.inner.read_dir(path);
        if self.sorted_listings
            && let Ok(entries) = &mut result
        {
            entries.sort_unstable_by(|left, right| left.name.cmp(&right.name));
        }
        if !self.private_path(path) {
            match &result {
                Ok(entries) if is_absolute(path) => {
                    let mut state = self.state();
                    state.stamp(InputStamp::DirEntries {
                        path: path.to_owned(),
                        digest: listing_digest(entries, true),
                    });
                    if !self.sorted_listings {
                        state.check(
                            Check::DirectoryOrder(path.to_owned()),
                            listing_digest(entries, false),
                        );
                    }
                }
                Err(error) if is_absolute(path) && error.kind == ErrorKind::NotFound => {
                    self.state().check(
                        Check::OrderedDirectoryMissing(path.to_owned()),
                        Digest::hash(b"missing"),
                    );
                }
                Err(error) => self.record_error(path, error),
                _ => self.mark_impure(ImpureReason::UnverifiableInput),
            }
        }
        result
    }

    fn create_dir_all(&self, path: &Path) -> HostResult<()> {
        self.guard_write(path)?;
        self.inner.create_dir_all(path)
    }

    fn remove_file(&self, path: &Path) -> HostResult<()> {
        self.guard_write(path)?;
        self.inner.remove_file(path)
    }

    fn remove_dir(&self, path: &Path) -> HostResult<()> {
        self.guard_write(path)?;
        self.inner.remove_dir(path)
    }

    fn remove_dir_all(&self, path: &Path) -> HostResult<()> {
        self.guard_write(path)?;
        self.inner.remove_dir_all(path)
    }

    fn rename(&self, from: &Path, to: &Path) -> HostResult<()> {
        self.guard_write(from)?;
        self.guard_write(to)?;
        self.inner.rename(from, to)
    }

    fn rename_noreplace(&self, from: &Path, to: &Path) -> HostResult<()> {
        self.guard_write(from)?;
        self.guard_write(to)?;
        self.inner.rename_noreplace(from, to)
    }

    fn lock_file(
        &self,
        path: &Path,
        timeout: Option<Duration>,
    ) -> HostResult<Box<dyn FileLockGuard>> {
        self.guard_effect(ImpureReason::SharedState)?;
        self.inner.lock_file(path, timeout)
    }

    fn copy_file(&self, from: &Path, to: &Path, options: CopyOptions) -> HostResult<()> {
        self.guard_write(to)?;
        let _ = self.record_dependency(from);
        if options.preserve_permissions || options.preserve_mtime {
            let _ = self.metadata(from, Follow::Yes);
        }
        let result = self.inner.copy_file(from, to, options);
        if result.is_err() {
            self.mark_impure(ImpureReason::UnverifiableInput);
        }
        result
    }

    fn symlink(&self, target: &Path, link: &Path) -> HostResult<()> {
        self.guard_effect(ImpureReason::ExternalWrite)?;
        self.inner.symlink(target, link)
    }

    fn hard_link(&self, source: &Path, destination: &Path) -> HostResult<()> {
        self.guard_effect(ImpureReason::ExternalWrite)?;
        self.inner.hard_link(source, destination)
    }

    fn read_link(&self, path: &Path) -> HostResult<PathBuf> {
        let result = self.inner.read_link(path);
        if is_absolute(path)
            && path
                .parent()
                .is_some_and(|parent| self.private_path(parent))
        {
            return result;
        }
        match &result {
            Ok(target) if is_absolute(path) => {
                self.state()
                    .check(Check::Link(path.to_owned()), path_digest(target));
            }
            Err(error) => self.record_error(path, error),
            _ => self.mark_impure(ImpureReason::UnverifiableInput),
        }
        result
    }

    fn canonicalize(&self, path: &Path) -> HostResult<PathBuf> {
        let result = self.inner.canonicalize(path);
        if !self.unobserved_read(path) {
            match &result {
                Ok(target) if is_absolute(path) => {
                    self.state()
                        .check(Check::Canonical(path.to_owned()), path_digest(target));
                }
                Err(error) => self.record_error(path, error),
                _ => self.mark_impure(ImpureReason::UnverifiableInput),
            }
        }
        result
    }

    fn set_permissions(&self, path: &Path, mode: u32) -> HostResult<()> {
        self.guard_write(path)?;
        self.inner.set_permissions(path, mode)
    }

    fn set_times(
        &self,
        path: &Path,
        atime: Option<FileTime>,
        mtime: Option<FileTime>,
    ) -> HostResult<()> {
        self.guard_write(path)?;
        self.inner.set_times(path, atime, mtime)
    }

    fn env_get(&self, name: &[u8]) -> Option<BString> {
        if let Some(spec) = &self.speculation {
            let windows = self.inner.system().sysname == b"Windows";
            return spec.environment.iter().rev().find_map(|(key, value)| {
                (key.as_slice() == name || (windows && key.eq_ignore_ascii_case(name)))
                    .then(|| value.clone())
            });
        }
        let value = self.inner.env_get(name);
        let mut state = self.state();
        if state.can_record() {
            if conflicts(&mut state.environment, name.to_vec(), value.clone()) {
                state.impure.get_or_insert(ImpureReason::ChangedInput);
            } else {
                state.stamp(InputStamp::EnvVar {
                    name: name.to_vec(),
                    digest: value.as_ref().map(|value| Digest::hash(value)),
                });
            }
        }
        value
    }

    fn env_set(&self, name: &[u8], value: Option<&[u8]>) {
        if self.guard_effect(ImpureReason::SharedState).is_ok() {
            self.inner.env_set(name, value);
        }
    }

    fn env_all(&self) -> Vec<(BString, BString)> {
        if let Some(spec) = &self.speculation {
            return spec.environment.to_vec();
        }
        let environment = self.inner.env_all();
        let mut state = self.state();
        if state.full_environment.is_none() {
            state.full_environment = Some(environment.clone());
        }
        state.check(Check::Environment, environment_fingerprint(&environment));
        environment
    }

    fn registry_host_view(&self) -> Option<rcmake_host::RegistryView> {
        self.inner.registry_host_view()
    }

    fn registry_value(
        &self,
        root: rcmake_host::RegistryRoot,
        subkey: &[u8],
        name: Option<&[u8]>,
        view: rcmake_host::RegistryView,
    ) -> HostResult<Option<BString>> {
        self.guard_effect(ImpureReason::UnverifiableInput)?;
        self.inner.registry_value(root, subkey, name, view)
    }

    fn open_read(&self, p: &Path) -> HostResult<Box<dyn rcmake_host::ReadSeek + '_>> {
        self.guard_effect(ImpureReason::UnverifiableInput)?;
        self.inner.open_read(p)
    }

    fn scratch_file(&self) -> HostResult<Box<dyn rcmake_host::ScratchFile + '_>> {
        self.inner.scratch_file()
    }

    fn scratch_file_in(&self, dir: &Path) -> HostResult<Box<dyn rcmake_host::ScratchFile + '_>> {
        self.inner.scratch_file_in(dir)
    }

    fn write_from(
        &self,
        p: &Path,
        mode: WriteMode,
        input: &mut dyn std::io::Read,
        len: u64,
    ) -> HostResult<()> {
        self.guard_write(p)?;
        self.guard_effect(ImpureReason::UnverifiableInput)?;
        self.inner.write_from(p, mode, input, len)
    }

    fn run_controlled(
        &self,
        req: &RunRequest,
        graceful: Option<GracefulTimeout>,
        hard_limit: Option<std::time::Duration>,
        update: &mut rcmake_host::DeadlineUpdate<'_>,
    ) -> HostResult<rcmake_host::ControlledOutput> {
        self.guard_effect(ImpureReason::UnverifiedProcess)?;
        self.record_request(req);
        self.inner.run_controlled(req, graceful, hard_limit, update)
    }

    fn run(
        &self,
        request: &RunRequest,
        on_output: &mut dyn FnMut(Stream, &[u8]),
    ) -> HostResult<RunOutput> {
        if let Some(result) = self.verified_platform_query(request) {
            // A process with `tee` also streams its captured output.
            if request.tee {
                on_output(Stream::Stdout, &result.stdout);
                on_output(Stream::Stderr, &result.stderr);
            }
            return Ok(result);
        }
        let (request, preflight) = self.process_request(request)?;
        let spawn = self.inner.now();
        let start = self.compile_start(&request, preflight.is_some());
        let result = if self.mode == JournalMode::Speculate {
            self.inner.run_bounded(&request, MAX_PROCESS_OUTPUT)
        } else {
            self.inner.run(&request, on_output)
        };
        self.compile_end(start, &result);
        self.after_process(&request, spawn, &result, preflight.as_deref());
        result
    }

    fn run_bounded(&self, request: &RunRequest, limit: usize) -> HostResult<RunOutput> {
        if let Some(result) = self.verified_platform_query(request)
            && result.stdout.len().saturating_add(result.stderr.len()) <= limit
        {
            return Ok(result);
        }
        let (request, preflight) = self.process_request(request)?;
        let limit = if self.mode == JournalMode::Speculate {
            limit.min(MAX_PROCESS_OUTPUT)
        } else {
            limit
        };
        let spawn = self.inner.now();
        let start = self.compile_start(&request, preflight.is_some());
        let result = self.inner.run_bounded(&request, limit);
        self.compile_end(start, &result);
        self.after_process(&request, spawn, &result, preflight.as_deref());
        result
    }

    fn run_with_timeout_signal(
        &self,
        request: &RunRequest,
        timeout: GracefulTimeout,
        on_output: &mut dyn FnMut(Stream, &[u8]),
    ) -> HostResult<RunOutput> {
        self.guard_effect(ImpureReason::UnverifiedProcess)?;
        self.record_request(request);
        self.inner
            .run_with_timeout_signal(request, timeout, on_output)
    }

    fn write_out(&self, stream: Stream, bytes: &[u8]) {
        if self.mode == JournalMode::Observe {
            self.inner.write_out(stream, bytes);
            self.mark_impure(ImpureReason::SharedState);
        } else {
            let mut state = self.state();
            if state.output_bytes.saturating_add(bytes.len()) <= MAX_PROCESS_OUTPUT
                && state.output.len() < MAX_OBSERVATIONS
            {
                state.output_bytes += bytes.len();
                state.output.push((stream, bytes.to_vec()));
            } else {
                state.impure.get_or_insert(ImpureReason::JournalLimit);
            }
        }
    }

    fn read_stdin(&self, limit: u64) -> HostResult<Vec<u8>> {
        self.guard_effect(ImpureReason::SharedState)?;
        self.inner.read_stdin(limit)
    }

    fn is_terminal(&self, stream: Stream) -> bool {
        self.mark_impure(ImpureReason::NondeterministicInput);
        self.inner.is_terminal(stream)
    }

    fn now(&self) -> FileTime {
        self.mark_impure(ImpureReason::NondeterministicInput);
        self.inner.now()
    }

    fn monotonic(&self) -> Duration {
        // This clock measures engine budgets and profiling intervals.
        self.inner.monotonic()
    }

    fn sleep(&self, duration: Duration) {
        if self
            .guard_effect(ImpureReason::NondeterministicInput)
            .is_ok()
        {
            self.inner.sleep(duration);
        }
    }

    fn time_zone(&self) -> TimeZone {
        self.mark_impure(ImpureReason::NondeterministicInput);
        self.inner.time_zone()
    }

    fn current_exe(&self) -> HostResult<PathBuf> {
        let result = self.inner.current_exe();
        match &result {
            Ok(exe) => self.state().check(Check::Executable, path_digest(exe)),
            Err(_) => self.mark_impure(ImpureReason::UnverifiableInput),
        }
        result
    }

    fn initial_cwd(&self) -> PathBuf {
        let cwd = self.inner.initial_cwd();
        if !self.private_path(&cwd) {
            self.state()
                .check(Check::WorkingDirectory, path_digest(&cwd));
        }
        cwd
    }

    fn system(&self) -> SystemInfo {
        let system = self.inner.system();
        self.state().check(Check::System, system_digest(&system));
        system
    }
}

pub(super) enum SharedOrBorrowed<'a, T: ?Sized> {
    Shared(std::sync::Arc<T>),
    Borrowed(&'a T),
}

impl<T: ?Sized> std::ops::Deref for SharedOrBorrowed<'_, T> {
    type Target = T;

    fn deref(&self) -> &T {
        match self {
            Self::Shared(value) => value,
            Self::Borrowed(value) => value,
        }
    }
}

impl JournalHost<'static> {
    pub(crate) fn from_shared(
        inner: std::sync::Arc<dyn Host>,
        scratch: &Path,
        verify_process: std::sync::Arc<dyn ProcessVerifier>,
        mode: JournalMode,
    ) -> HostResult<Self> {
        Self::from_parts(
            SharedOrBorrowed::Shared(inner),
            scratch,
            mode,
            Some(SharedOrBorrowed::Shared(verify_process)),
        )
    }
}

impl JournalHost<'_> {
    /// Complete recording after the child has stopped.
    pub(crate) fn finish(self) -> Journal {
        let state = self
            .state
            .into_inner()
            .unwrap_or_else(PoisonError::into_inner);
        Journal {
            trace: state.stamps.into_values().collect(),
            memo: self.memo,
            checks: JournalChecks(state.checks.into_iter().collect()),
            #[cfg(all(test, unix))]
            environment: state.environment,
            #[cfg(all(test, unix))]
            full_environment: state.full_environment,
            processes: state.processes,
            output: state.output,
            impure: state.impure,
        }
    }
}
