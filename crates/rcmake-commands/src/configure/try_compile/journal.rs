//! Record probe inputs through the host and reject unclassified effects.

#![forbid(unsafe_code)]

use std::collections::BTreeMap;
use std::io::ErrorKind;
use std::path::{Component, Path, PathBuf};
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use jiff::tz::TimeZone;
use rcmake_base::{BString, Limits};
use rcmake_host::{
    CopyOptions, DirEntry, FileKind, FileLockGuard, FileTime, Follow, GracefulTimeout, Host,
    HostError, HostResult, Input, Metadata, Output, ProcessResult, ReadRange, RunOutput,
    RunRequest, Stream, SystemInfo, WriteMode, path,
};
use rcmake_store::{
    Digest, Domain, FastStat, InputStamp, KeyHasher, Trace, VerifyOutcome, directory_digest,
    file_digest, verify_trace,
};
use serde::{Deserialize, Serialize};

use super::plan::environment_fingerprint;

/// The first operation that prevented cache publication.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ImpureReason {
    /// A write could affect a path outside private scratch space.
    ExternalWrite,
    /// The probe used the shared environment or an advisory lock.
    SharedState,
    /// The probe observed mutable host state that the cache cannot replay.
    NondeterministicInput,
    /// The child process or one of its effects lacks a deterministic contract.
    UnverifiedProcess,
    /// The child process stopped or failed to start.
    ProcessFailure,
    /// A host result has no supported validator.
    UnverifiableInput,
    /// An input changed during the probe.
    ChangedInput,
    /// The probe exceeded the number of recorded operations.
    JournalLimit,
}

/// How to handle an unverified operation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum JournalMode {
    /// Observe a call at its original position and preserve its host behavior.
    Observe,
    /// Block unclassified effects in a speculative call.
    Speculate,
}

/// Provide a trace for a deterministic process and all its descendants.
///
/// Certification covers the complete request, including executable selection,
/// compiler and linker dependencies, negative searches, and the effects of
/// the process. It requires private writes and stable inputs through use.
/// A depfile or a tool name alone cannot establish this contract.
pub(crate) type ProcessVerifier<'a> = dyn Fn(&RunRequest) -> Option<Trace> + Send + Sync + 'a;

const MAX_OBSERVATIONS: usize = 16_384;

#[derive(Eq, Ord, PartialEq, PartialOrd)]
enum StampKey {
    Stat(PathBuf),
    File(PathBuf),
    Directory(PathBuf),
    Missing(PathBuf),
    Env(Vec<u8>),
    Lookup(PathBuf, PathBuf),
}

impl From<&InputStamp> for StampKey {
    fn from(stamp: &InputStamp) -> Self {
        match stamp {
            InputStamp::FileStat { path, .. } => Self::Stat(path.clone()),
            InputStamp::FileHash { path, .. } => Self::File(path.clone()),
            InputStamp::DirEntries { path, .. } => Self::Directory(path.clone()),
            InputStamp::Missing { path } => Self::Missing(path.clone()),
            InputStamp::EnvVar { name, .. } => Self::Env(name.clone()),
            InputStamp::NegativeLookup { root, relative, .. } => {
                Self::Lookup(root.clone(), relative.clone())
            }
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
struct Slice {
    offset: u64,
    len: Option<u64>,
}

#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
enum Check {
    Metadata(PathBuf, bool),
    Slice(PathBuf, Slice),
    Chunks(PathBuf),
    DirectoryOrder(PathBuf),
    Link(PathBuf),
    Canonical(PathBuf),
    Environment,
    Executable,
    WorkingDirectory,
    System,
}

impl Check {
    fn digest(&self, host: &dyn Host) -> Option<Digest> {
        match self {
            Self::Metadata(path, follow) => {
                let follow = if *follow { Follow::Yes } else { Follow::No };
                host.metadata(path, follow)
                    .ok()
                    .map(|value| metadata_digest(&value))
            }
            Self::Slice(path, range) => host
                .read(
                    path,
                    ReadRange::Slice {
                        offset: range.offset,
                        len: range.len,
                    },
                )
                .ok()
                .map(|bytes| Digest::hash(&bytes)),
            Self::Chunks(path) if bounded_file(host, path) => {
                let mut key = KeyHasher::new(Domain::ProbeV1);
                host.read_chunks(path, &mut |chunk| key.field(1, chunk))
                    .ok()?;
                Some(key.finish())
            }
            Self::Chunks(_) => None,
            Self::DirectoryOrder(path) => host
                .read_dir(path)
                .ok()
                .map(|entries| listing_digest(&entries, false)),
            Self::Link(path) => host.read_link(path).ok().map(|target| path_digest(&target)),
            Self::Canonical(path) => host
                .canonicalize(path)
                .ok()
                .map(|target| path_digest(&target)),
            Self::Environment => Some(environment_fingerprint(&host.env_all())),
            Self::Executable => host.current_exe().ok().map(|exe| path_digest(&exe)),
            Self::WorkingDirectory => Some(path_digest(&host.initial_cwd())),
            Self::System => Some(system_digest(&host.system())),
        }
    }
}

fn path_digest(path: &Path) -> Digest {
    Digest::hash(path.as_os_str().as_encoded_bytes())
}

fn kind_tag(kind: FileKind) -> u16 {
    match kind {
        FileKind::File => 1,
        FileKind::Dir => 2,
        FileKind::Symlink => 3,
        FileKind::Other => 4,
    }
}

fn listing_digest(entries: &[DirEntry], sorted: bool) -> Digest {
    let mut entries: Vec<_> = entries.iter().collect();
    if sorted {
        entries.sort_unstable_by(|left, right| left.name.cmp(&right.name));
    }
    let mut key = KeyHasher::new(Domain::BlobV1);
    key.field(0, b"directory");
    for entry in entries {
        key.field(kind_tag(entry.kind), &entry.name);
    }
    key.finish()
}

fn metadata_digest(metadata: &Metadata) -> Digest {
    let mut key = KeyHasher::new(Domain::ProbeV1);
    key.field(0, b"metadata");
    key.field(1, &kind_tag(metadata.kind).to_le_bytes());
    key.field(2, &metadata.len.to_le_bytes());
    key.field(3, &metadata.mode.to_le_bytes());
    key.field(4, &metadata.mtime.secs.to_le_bytes());
    key.field(5, &metadata.mtime.nanos.to_le_bytes());
    key.field(6, &metadata.atime.secs.to_le_bytes());
    key.field(7, &metadata.atime.nanos.to_le_bytes());
    key.field(8, &metadata.dev.to_le_bytes());
    key.field(9, &metadata.ino.to_le_bytes());
    key.field(10, &metadata.nlink.to_le_bytes());
    key.finish()
}

fn system_digest(system: &SystemInfo) -> Digest {
    let mut key = KeyHasher::new(Domain::ProbeV1);
    key.field(0, b"system");
    key.field(1, &system.sysname);
    key.field(2, &system.release);
    key.field(3, &system.version);
    key.field(4, &system.machine);
    key.field(5, &system.hostname);
    key.field(6, &system.cpu_count.to_le_bytes());
    key.field(7, &system.pid.to_le_bytes());
    key.finish()
}

#[derive(Default)]
struct State {
    stamps: BTreeMap<StampKey, InputStamp>,
    checks: BTreeMap<Check, Digest>,
    environment: BTreeMap<Vec<u8>, Option<BString>>,
    full_environment: Option<Vec<(BString, BString)>>,
    processes: Vec<RunRequest>,
    impure: Option<ImpureReason>,
}

impl State {
    fn can_record(&mut self) -> bool {
        if self.stamps.len() + self.checks.len() + self.environment.len() + self.processes.len()
            >= MAX_OBSERVATIONS
        {
            self.impure.get_or_insert(ImpureReason::JournalLimit);
            return false;
        }
        true
    }

    fn stamp(&mut self, stamp: InputStamp) {
        if self.can_record() && conflicts(&mut self.stamps, StampKey::from(&stamp), stamp) {
            self.impure.get_or_insert(ImpureReason::ChangedInput);
        }
    }

    fn check(&mut self, check: Check, expected: Digest) {
        if self.can_record() && conflicts(&mut self.checks, check, expected) {
            self.impure.get_or_insert(ImpureReason::ChangedInput);
        }
    }
}

fn conflicts<K: Ord, V: PartialEq>(map: &mut BTreeMap<K, V>, key: K, value: V) -> bool {
    use std::collections::btree_map::Entry;
    match map.entry(key) {
        Entry::Vacant(entry) => {
            entry.insert(value);
            false
        }
        Entry::Occupied(entry) => entry.get() != &value,
    }
}

/// Exact observations that have no stamp in the shared store format.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct JournalChecks(Vec<(Check, Digest)>);

impl JournalChecks {
    /// Check all supplemental observations.
    pub(crate) fn verify(&self, host: &dyn Host) -> bool {
        self.0
            .iter()
            .all(|(check, expected)| check.digest(host) == Some(*expected))
    }
}

/// Evidence collected from one execution. Raw environment values stay in memory.
pub(crate) struct Journal {
    trace: Trace,
    checks: JournalChecks,
    environment: BTreeMap<Vec<u8>, Option<BString>>,
    full_environment: Option<Vec<(BString, BString)>>,
    processes: Vec<RunRequest>,
    impure: Option<ImpureReason>,
}

impl Journal {
    /// Return the first reason that the outcome cannot be stored.
    pub(crate) fn impure(&self) -> Option<ImpureReason> {
        self.impure
    }

    /// Return recorded scalar environment reads, including absent values.
    pub(crate) fn environment(&self) -> &BTreeMap<Vec<u8>, Option<BString>> {
        &self.environment
    }

    /// Return the exact environment from the first enumeration.
    pub(crate) fn full_environment(&self) -> Option<&[(BString, BString)]> {
        self.full_environment.as_deref()
    }

    /// Return recorded process requests in invocation order.
    pub(crate) fn processes(&self) -> &[RunRequest] {
        &self.processes
    }

    /// Return the persistent validators only for a pure, unchanged run.
    pub(crate) fn verified_inputs(&self, host: &dyn Host) -> Option<(&Trace, &JournalChecks)> {
        (self.impure.is_none()
            && self.checks.verify(host)
            && verify_trace(host, &self.trace) == VerifyOutcome::Hit)
            .then_some((&self.trace, &self.checks))
    }
}

/// Host adapter for a single probe.
///
/// The caller owns a fresh scratch directory and keeps it private for this
/// adapter's lifetime. External inputs require a stable snapshot through use.
pub(crate) struct JournalHost<'a> {
    inner: &'a dyn Host,
    scratch: PathBuf,
    canonical_scratch: PathBuf,
    mode: JournalMode,
    verify_process: Option<&'a ProcessVerifier<'a>>,
    state: Mutex<State>,
}

impl<'a> JournalHost<'a> {
    /// Observe a probe in an existing private directory.
    pub(crate) fn new(
        inner: &'a dyn Host,
        scratch: &Path,
        mode: JournalMode,
        verify_process: Option<&'a ProcessVerifier<'a>>,
    ) -> HostResult<Self> {
        if !is_absolute(scratch) || scratch.parent().is_none() {
            return Err(HostError::unsupported(
                "Expected a private scratch directory.",
            ));
        }
        let canonical_scratch = inner.canonicalize(scratch)?;
        if inner.metadata(scratch, Follow::No)?.kind != FileKind::Dir
            || !is_absolute(&canonical_scratch)
            || canonical_scratch.parent().is_none()
        {
            return Err(HostError::unsupported(
                "Expected a private scratch directory.",
            ));
        }
        Ok(Self {
            inner,
            scratch: scratch.to_owned(),
            canonical_scratch,
            mode,
            verify_process,
            state: Mutex::default(),
        })
    }

    /// Complete recording after the child has stopped.
    pub(crate) fn finish(self) -> Journal {
        let state = self
            .state
            .into_inner()
            .unwrap_or_else(PoisonError::into_inner);
        Journal {
            trace: state.stamps.into_values().collect(),
            checks: JournalChecks(state.checks.into_iter().collect()),
            environment: state.environment,
            full_environment: state.full_environment,
            processes: state.processes,
            impure: state.impure,
        }
    }

    /// Record a dependency from a verified compiler or linker observation.
    ///
    /// Use this for paths from a depfile and for directly resolved headers,
    /// libraries, toolchain executables, and other regular files.
    pub(crate) fn record_dependency(&self, path: &Path) -> HostResult<()> {
        if self.private_path(path) {
            return Ok(());
        }
        let result = file_digest(self.inner, path);
        if let Ok(digest) = result {
            self.state().stamp(InputStamp::FileHash {
                path: path.to_owned(),
                digest,
            });
            Ok(())
        } else {
            self.mark_impure(ImpureReason::UnverifiableInput);
            result.map(|_| ())
        }
    }

    /// Record one header or library lookup before accepting a later candidate.
    ///
    /// A miss stamps the deepest existing directory. A missing search root
    /// gets its own stamp, so creating it also invalidates the result.
    pub(crate) fn record_search(&self, root: &Path, relative: &Path) -> HostResult<bool> {
        if !is_absolute(root)
            || relative.as_os_str().is_empty()
            || !relative
                .components()
                .all(|part| matches!(part, Component::Normal(_)))
        {
            self.mark_impure(ImpureReason::UnverifiableInput);
            return Err(HostError::unsupported(
                "Expected a relative search candidate.",
            ));
        }
        let candidate = root.join(relative);
        match self.inner.metadata(&candidate, Follow::Yes) {
            Ok(metadata) if metadata.kind == FileKind::File => {
                self.record_dependency(&candidate)?;
                Ok(true)
            }
            Err(error) if error.kind == ErrorKind::NotFound && self.private_path(&candidate) => {
                Ok(false)
            }
            Err(error) if error.kind == ErrorKind::NotFound => {
                let result = self.record_negative_search(root, relative);
                if result.is_err() {
                    self.mark_impure(ImpureReason::UnverifiableInput);
                }
                result.map(|()| false)
            }
            _ => {
                self.mark_impure(ImpureReason::UnverifiableInput);
                Err(HostError::unsupported(
                    "Cannot verify the search candidate.",
                ))
            }
        }
    }

    fn record_negative_search(&self, root: &Path, relative: &Path) -> HostResult<()> {
        let mut directory = match self.inner.metadata(root, Follow::Yes) {
            Ok(metadata) if metadata.kind == FileKind::Dir => root.to_owned(),
            Err(error) if error.kind == ErrorKind::NotFound && missing(self.inner, root) => {
                self.state().stamp(InputStamp::Missing {
                    path: root.to_owned(),
                });
                return Ok(());
            }
            _ => {
                return Err(HostError::unsupported(
                    "Cannot verify the search directory.",
                ));
            }
        };
        for part in relative.components() {
            let next = directory.join(part.as_os_str());
            if missing(self.inner, &next) {
                let listing = directory_digest(self.inner, &directory)?;
                self.state().stamp(InputStamp::NegativeLookup {
                    root: root.to_owned(),
                    relative: relative.to_owned(),
                    deepest: directory,
                    listing,
                });
                return Ok(());
            }
            if self.inner.metadata(&next, Follow::Yes)?.kind != FileKind::Dir {
                break;
            }
            directory = next;
        }
        Err(HostError::unsupported(
            "Cannot verify the missing search candidate.",
        ))
    }

    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn mark_impure(&self, reason: ImpureReason) {
        self.state().impure.get_or_insert(reason);
    }

    fn guard_effect(&self, reason: ImpureReason) -> HostResult<()> {
        self.mark_impure(reason);
        match self.mode {
            JournalMode::Observe => Ok(()),
            JournalMode::Speculate => Err(HostError::unsupported(
                "Unclassified speculative probe effect.",
            )),
        }
    }

    fn private_path(&self, path: &Path) -> bool {
        if !is_absolute(path) {
            return false;
        }
        let Ok(relative) = path
            .strip_prefix(&self.scratch)
            .or_else(|_| path.strip_prefix(&self.canonical_scratch))
        else {
            return false;
        };
        let mut directory = self.canonical_scratch.clone();
        for part in relative.components() {
            directory.push(part.as_os_str());
            match self.inner.metadata(&directory, Follow::No) {
                Ok(meta)
                    if meta.kind == FileKind::Dir
                        || (meta.kind == FileKind::File && meta.nlink == 1) => {}
                Err(error) if error.kind == ErrorKind::NotFound => return true,
                _ => return false,
            }
        }
        true
    }

    fn guard_write(&self, path: &Path) -> HostResult<()> {
        if self.private_path(path) {
            Ok(())
        } else {
            self.guard_effect(ImpureReason::ExternalWrite)
        }
    }

    fn record_error(&self, path: &Path, error: &HostError) {
        if error.kind == ErrorKind::NotFound && missing(self.inner, path) {
            self.state().stamp(InputStamp::Missing {
                path: path.to_owned(),
            });
        } else {
            self.mark_impure(ImpureReason::UnverifiableInput);
        }
    }

    fn record_request(&self, request: &RunRequest) {
        let mut state = self.state();
        if state.can_record() {
            state.processes.push(request.clone());
        }
    }

    fn before_process(&self, request: &RunRequest) -> HostResult<()> {
        self.record_request(request);
        if let Input::File(path) = &request.stdin {
            let _ = self.record_dependency(path);
        }
        for output in [&request.stdout, &request.stderr] {
            if let Output::File { path, .. } = output {
                self.guard_write(path)?;
            }
        }

        let bounded = request.timeout.is_some_and(|time| {
            !time.is_zero()
                && (self.mode == JournalMode::Observe || time <= Duration::from_secs(120))
        });
        let private = self.private_path(&request.cwd)
            && request.stdin != Input::Inherit
            && request.stdout != Output::Inherit
            && request.stderr != Output::Inherit;
        if !bounded || !private {
            return self.guard_effect(ImpureReason::UnverifiedProcess);
        }
        let Some(trace) = self.verify_process.and_then(|verify| verify(request)) else {
            return self.guard_effect(ImpureReason::UnverifiedProcess);
        };
        for command in &request.pipeline {
            let program = path::to_native(&command.program);
            if !is_absolute(&program) || self.record_dependency(&program).is_err() {
                return self.guard_effect(ImpureReason::UnverifiedProcess);
            }
        }
        if verify_trace(self.inner, &trace) != VerifyOutcome::Hit {
            return self.guard_effect(ImpureReason::ChangedInput);
        }
        let mut state = self.state();
        for stamp in trace {
            state.stamp(stamp);
        }
        if self.mode == JournalMode::Speculate && state.impure.is_some() {
            return Err(HostError::unsupported(
                "Cannot verify speculative probe inputs.",
            ));
        }
        Ok(())
    }

    fn after_process(&self, request: &RunRequest, result: &HostResult<RunOutput>) {
        if !result.as_ref().is_ok_and(|output| {
            output.results.len() == request.pipeline.len()
                && output
                    .results
                    .iter()
                    .all(|result| matches!(result, ProcessResult::Exited(_)))
        }) {
            self.mark_impure(ImpureReason::ProcessFailure);
        }
    }
}

fn is_absolute(path: &Path) -> bool {
    path.is_absolute() && !path.components().any(|part| part == Component::ParentDir)
}

fn bounded_file(host: &dyn Host, path: &Path) -> bool {
    host.metadata(path, Follow::Yes).is_ok_and(|value| {
        value.kind == FileKind::File && value.len <= Limits::default().max_file_read
    })
}

fn missing(host: &dyn Host, path: &Path) -> bool {
    is_absolute(path)
        && host
            .metadata(path, Follow::No)
            .is_err_and(|error| error.kind == ErrorKind::NotFound)
}

impl Host for JournalHost<'_> {
    fn metadata(&self, path: &Path, follow: Follow) -> HostResult<Metadata> {
        let result = self.inner.metadata(path, follow);
        if !self.private_path(path) {
            match &result {
                Ok(meta) if is_absolute(path) && meta.kind != FileKind::Other => {
                    let mut state = self.state();
                    state.check(
                        Check::Metadata(path.to_owned(), follow == Follow::Yes),
                        metadata_digest(meta),
                    );
                    if meta.kind == FileKind::File {
                        state.stamp(InputStamp::FileStat {
                            path: path.to_owned(),
                            stat: FastStat::from(meta),
                            digest: None,
                        });
                    }
                }
                Err(error) => self.record_error(path, error),
                _ => self.mark_impure(ImpureReason::UnverifiableInput),
            }
        }
        result
    }

    fn read(&self, path: &Path, range: ReadRange) -> HostResult<Vec<u8>> {
        let result = self.inner.read(path, range);
        if !self.private_path(path) {
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
        if !bounded_file(self.inner, path) {
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
        let result = self.inner.read_dir(path);
        if !self.private_path(path) {
            match &result {
                Ok(entries) if is_absolute(path) => {
                    let mut state = self.state();
                    state.stamp(InputStamp::DirEntries {
                        path: path.to_owned(),
                        digest: listing_digest(entries, true),
                    });
                    state.check(
                        Check::DirectoryOrder(path.to_owned()),
                        listing_digest(entries, false),
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
        if !self.private_path(path) {
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
        let environment = self.inner.env_all();
        let mut state = self.state();
        if state.full_environment.is_none() {
            state.full_environment = Some(environment.clone());
        }
        state.check(Check::Environment, environment_fingerprint(&environment));
        environment
    }

    fn run(
        &self,
        request: &RunRequest,
        on_output: &mut dyn FnMut(Stream, &[u8]),
    ) -> HostResult<RunOutput> {
        self.before_process(request)?;
        let result = self.inner.run(request, on_output);
        self.after_process(request, &result);
        result
    }

    fn run_bounded(&self, request: &RunRequest, limit: usize) -> HostResult<RunOutput> {
        self.before_process(request)?;
        let result = self.inner.run_bounded(request, limit);
        self.after_process(request, &result);
        result
    }

    fn run_with_timeout_signal(
        &self,
        request: &RunRequest,
        timeout: GracefulTimeout,
        on_output: &mut dyn FnMut(Stream, &[u8]),
    ) -> HostResult<RunOutput> {
        self.guard_effect(ImpureReason::UnverifiedProcess)?;
        self.before_process(request)?;
        let result = self
            .inner
            .run_with_timeout_signal(request, timeout, on_output);
        self.after_process(request, &result);
        result
    }

    fn write_out(&self, stream: Stream, bytes: &[u8]) {
        if self.guard_effect(ImpureReason::SharedState).is_ok() {
            self.inner.write_out(stream, bytes);
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
        self.mark_impure(ImpureReason::NondeterministicInput);
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

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use rcmake_host::PureHost;

    #[test]
    fn keeps_exact_environment_and_validates_a_read() -> HostResult<()> {
        let host = PureHost::default();
        let scratch = Path::new("/work/scratch");
        let header = Path::new("/work/input.h");
        host.create_dir_all(scratch)?;
        host.seed_file(header, b"old")?;
        host.env_set(b"LC_MESSAGES", Some(b"C"));
        let journal = JournalHost::new(&host, scratch, JournalMode::Observe, None)?;
        assert_eq!(journal.read(header, ReadRange::All { limit: 32 })?, b"old");
        journal.env_get(b"LC_MESSAGES");
        journal.env_all();
        let journal = journal.finish();
        assert_eq!(journal.full_environment(), Some(host.env_all().as_slice()));
        assert_eq!(
            journal.environment().get(b"LC_MESSAGES".as_slice()),
            Some(&Some("C".into()))
        );
        assert!(journal.verified_inputs(&host).is_some());
        host.env_set(b"LC_MESSAGES", Some(b"en_US"));
        assert!(journal.verified_inputs(&host).is_none());
        Ok(())
    }

    #[test]
    fn observes_external_writes_and_blocks_them_in_speculation() -> HostResult<()> {
        let host = PureHost::default();
        let scratch = Path::new("/work/scratch");
        let outside = Path::new("/work/outside");
        host.create_dir_all(scratch)?;
        let journal = JournalHost::new(&host, scratch, JournalMode::Observe, None)?;
        journal.write(outside, b"first", WriteMode::Truncate)?;
        assert_eq!(journal.finish().impure(), Some(ImpureReason::ExternalWrite));

        host.symlink(outside, &scratch.join("alias"))?;
        let journal = JournalHost::new(&host, scratch, JournalMode::Speculate, None)?;
        assert!(
            journal
                .write(&scratch.join("alias"), b"second", WriteMode::Truncate)
                .is_err()
        );
        assert_eq!(host.read(outside, ReadRange::All { limit: 32 })?, b"first");
        assert_eq!(journal.finish().impure(), Some(ImpureReason::ExternalWrite));
        Ok(())
    }
}
