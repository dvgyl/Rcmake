//! Record probe inputs through the host and reject unclassified effects.

#![forbid(unsafe_code)]

use std::collections::{BTreeMap, BTreeSet};
use std::io::ErrorKind;
use std::panic::Location;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use jiff::tz::TimeZone;
use rcmake_base::{BString, Limits};
use rcmake_eval::CancelToken;
use rcmake_host::{
    CopyOptions, DirEntry, FileKind, FileLockGuard, FileTime, Follow, GracefulTimeout, Host,
    HostError, HostResult, Input, Metadata, Output, ProcessResult, ReadRange, RunOutput,
    RunRequest, Stream, SystemInfo, WriteMode, path,
};
use rcmake_json::{FieldError, FromJson, ToJson, Value, deny_unknown, object, req};
use rcmake_store::{
    Digest, Domain, InputStamp, KeyHasher, Trace, TraceMemo, TreePolicy, VerifyOutcome,
    directory_digest, file_digest, path_from_json, path_to_json, verify_trace,
};

use super::plan::{process_environment_fingerprint, request_digest};
use host_impl::SharedOrBorrowed;

#[path = "depfile.rs"]
pub(super) mod depfile;
#[path = "journal_host.rs"]
mod host_impl;
mod intervals;
pub(crate) use intervals::CompileIntervals;
mod process;

/// Total time budget for one queued and running speculative job.
pub(crate) const SPECULATIVE_TIMEOUT: Duration = Duration::from_secs(30);

/// Read the clock that limits real worker waits and processes.
#[expect(
    clippy::disallowed_methods,
    reason = "The limit must also work with a fixed host clock"
)]
pub(crate) fn speculative_now() -> std::time::Instant {
    std::time::Instant::now()
}

const MAX_PROCESS_OUTPUT: usize = 4 * 1024 * 1024;

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

/// Files and search results observed during one completed process tree.
///
/// Include compiler and linker inputs, response files, plugins, and every
/// failed candidate before a match. For a failed search, include all candidate
/// directories. Return no closure if the executor cannot account for a read.
pub(crate) struct DependencyClosure {
    /// Absolute paths from compiler and linker evidence.
    pub(crate) files: BTreeSet<PathBuf>,
    /// Search candidates and the result observed by the process tree.
    pub(crate) searches: Vec<SearchObservation>,
}

/// The result of one compiler or linker search candidate.
pub(crate) struct SearchObservation {
    /// Absolute directory examined by the tool.
    pub(crate) root: PathBuf,
    /// Name examined within the directory.
    pub(crate) relative: PathBuf,
    /// Whether the tool found this candidate.
    pub(crate) found: bool,
}

/// Certification supplied by an executor that accounts for the full process tree.
///
/// Preflight certifies executable selection, launchers, descendants, config
/// paths, private writes, and any negative tool or config searches. The second
/// step verifies fresh dependency evidence from the completed request. A depfile
/// alone cannot certify the complete process tree.
pub(crate) trait ProcessVerifier: Send + Sync {
    /// Return whether the verified process writes only to scratch or temporary paths.
    fn confines_writes(&self, _request: &RunRequest) -> bool {
        false
    }

    /// Return a certified platform query sampled during this configure.
    fn recorded_platform_query(&self, _request: &RunRequest) -> Option<RunOutput> {
        None
    }

    /// Request dependency evidence from a known tool.
    fn instrument(&self, _request: &mut RunRequest) {}

    /// Check the request before launch and return the tool and config inputs.
    fn preflight(&self, request: &RunRequest) -> Option<Trace>;

    /// Read the dependency evidence for the exact request that ran.
    fn dependencies(
        &self,
        request: &RunRequest,
        output: &RunOutput,
        preflight: &[InputStamp],
    ) -> Option<DependencyClosure>;

    /// Return whether a verified request compiles the probe source.
    fn is_probe_compile(&self, _request: &RunRequest) -> bool {
        false
    }

    /// Maximum timestamp granularity in the verified file systems.
    fn timestamp_granularity(&self) -> Duration {
        Duration::from_secs(2)
    }
}

#[derive(Clone, Copy)]
struct ObservationTime {
    spawn: i128,
    cutoff: i128,
}

impl ObservationTime {
    fn changed(self, metadata: &Metadata) -> bool {
        // The mtime cutoff includes the host timestamp granularity.
        time_nanos(metadata.mtime) >= self.cutoff
            || metadata.ctime.is_some_and(|ctime| {
                let threshold = if ctime.nanos == 0 {
                    self.cutoff
                } else {
                    self.spawn
                };
                time_nanos(ctime) >= threshold
            })
    }
}

const MAX_OBSERVATIONS: usize = 16_384;

#[derive(Eq, Ord, PartialEq, PartialOrd)]
enum StampKey {
    Stat(PathBuf),
    File(PathBuf),
    Directory(PathBuf),
    Tree(PathBuf, TreePolicy),
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
            InputStamp::TreeListing { path, policy, .. } => Self::Tree(path.clone(), *policy),
            InputStamp::Missing { path } => Self::Missing(path.clone()),
            InputStamp::EnvVar { name, .. } => Self::Env(name.clone()),
            InputStamp::NegativeLookup { root, relative, .. } => {
                Self::Lookup(root.clone(), relative.clone())
            }
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct Slice {
    offset: u64,
    len: Option<u64>,
}

impl ToJson for Slice {
    fn to_json(&self) -> Value {
        Value::object_from([
            ("offset", self.offset.to_json()),
            ("len", self.len.to_json()),
        ])
    }
}

impl FromJson for Slice {
    fn from_json(value: &Value) -> Result<Self, FieldError> {
        let obj = object(value)?;
        deny_unknown(obj, &["offset", "len"])?;
        Ok(Self {
            offset: req(obj, "offset")?,
            len: req(obj, "len")?,
        })
    }
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
enum Check {
    Metadata(PathBuf, bool),
    Slice(PathBuf, Slice),
    Chunks(PathBuf),
    DirectoryOrder(PathBuf),
    OrderedDirectoryMissing(PathBuf),
    FollowMissing(PathBuf),
    Link(PathBuf),
    Canonical(PathBuf),
    Environment,
    Executable,
    WorkingDirectory,
    System,
    Request(u32),
}

impl ToJson for Check {
    fn to_json(&self) -> Value {
        match self {
            Self::Metadata(path, follow) => Value::object_from([(
                "Metadata",
                Value::array_from([path_to_json(path), follow.to_json()]),
            )]),
            Self::Slice(path, range) => Value::object_from([(
                "Slice",
                Value::array_from([path_to_json(path), range.to_json()]),
            )]),
            Self::Chunks(path) => Value::object_from([("Chunks", path_to_json(path))]),
            Self::DirectoryOrder(path) => {
                Value::object_from([("DirectoryOrder", path_to_json(path))])
            }
            Self::OrderedDirectoryMissing(path) => {
                Value::object_from([("OrderedDirectoryMissing", path_to_json(path))])
            }
            Self::FollowMissing(path) => {
                Value::object_from([("FollowMissing", path_to_json(path))])
            }
            Self::Link(path) => Value::object_from([("Link", path_to_json(path))]),
            Self::Canonical(path) => Value::object_from([("Canonical", path_to_json(path))]),
            Self::Environment => Value::string("Environment"),
            Self::Executable => Value::string("Executable"),
            Self::WorkingDirectory => Value::string("WorkingDirectory"),
            Self::System => Value::string("System"),
            Self::Request(index) => Value::object_from([("Request", index.to_json())]),
        }
    }
}

impl FromJson for Check {
    fn from_json(value: &Value) -> Result<Self, FieldError> {
        match value {
            Value::String(_) => match rcmake_json::str(value)? {
                "Environment" => Ok(Self::Environment),
                "Executable" => Ok(Self::Executable),
                "WorkingDirectory" => Ok(Self::WorkingDirectory),
                "System" => Ok(Self::System),
                other => Err(FieldError::invalid(format!(
                    "unknown journal check '{other}'"
                ))),
            },
            Value::Object(obj) => {
                let mut iter = obj.iter();
                let Some((tag, payload)) = iter.next() else {
                    return Err(FieldError::invalid("expected journal check variant"));
                };
                if iter.next().is_some() {
                    return Err(FieldError::invalid(
                        "expected single-key journal check object",
                    ));
                }
                let tag = core::str::from_utf8(tag.as_slice())
                    .map_err(|_| FieldError::invalid("journal check variant is not valid UTF-8"))?;
                match tag {
                    "Metadata" => {
                        let [path, follow] =
                            array_pair(payload).map_err(|err| err.in_field("Metadata"))?;
                        Ok(Self::Metadata(
                            path_from_json(path).map_err(|err| err.in_field("Metadata"))?,
                            rcmake_json::bool(follow).map_err(|err| err.in_field("Metadata"))?,
                        ))
                    }
                    "Slice" => {
                        let [path, range] =
                            array_pair(payload).map_err(|err| err.in_field("Slice"))?;
                        Ok(Self::Slice(
                            path_from_json(path).map_err(|err| err.in_field("Slice"))?,
                            Slice::from_json(range).map_err(|err| err.in_field("Slice"))?,
                        ))
                    }
                    "Chunks" => path_from_json(payload)
                        .map(Self::Chunks)
                        .map_err(|err| err.in_field("Chunks")),
                    "DirectoryOrder" => path_from_json(payload)
                        .map(Self::DirectoryOrder)
                        .map_err(|err| err.in_field("DirectoryOrder")),
                    "OrderedDirectoryMissing" => path_from_json(payload)
                        .map(Self::OrderedDirectoryMissing)
                        .map_err(|err| err.in_field("OrderedDirectoryMissing")),
                    "FollowMissing" => path_from_json(payload)
                        .map(Self::FollowMissing)
                        .map_err(|err| err.in_field("FollowMissing")),
                    "Link" => path_from_json(payload)
                        .map(Self::Link)
                        .map_err(|err| err.in_field("Link")),
                    "Canonical" => path_from_json(payload)
                        .map(Self::Canonical)
                        .map_err(|err| err.in_field("Canonical")),
                    "Request" => u32::from_json(payload)
                        .map(Self::Request)
                        .map_err(|err| err.in_field("Request")),
                    other => Err(FieldError::invalid(format!(
                        "unknown journal check '{other}'"
                    ))),
                }
            }
            _ => Err(FieldError::expected("string or object", value)),
        }
    }
}

fn array_pair(value: &Value) -> Result<&[Value; 2], FieldError> {
    let items = rcmake_json::array(value)?;
    items.try_into().map_err(|_| {
        FieldError::invalid(format!("expected 2-element array, found {}", items.len()))
    })
}

impl Check {
    fn digest(&self, host: &dyn Host) -> Option<Digest> {
        self.digest_inner(host, false)
    }

    fn digest_fresh(&self, host: &dyn Host) -> Option<Digest> {
        self.digest_inner(host, true)
    }

    fn digest_inner(&self, host: &dyn Host, fresh: bool) -> Option<Digest> {
        match self {
            Self::Metadata(path, follow) => {
                let follow = if *follow { Follow::Yes } else { Follow::No };
                let meta = if fresh {
                    host.metadata_fresh(path, follow)
                } else {
                    host.metadata(path, follow)
                };
                meta.ok().map(|value| metadata_digest(&value))
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
            Self::OrderedDirectoryMissing(path) => host
                .read_dir(path)
                .is_err_and(|error| error.kind == ErrorKind::NotFound)
                .then(|| Digest::hash(b"missing")),
            Self::FollowMissing(path) => {
                let meta = if fresh {
                    host.metadata_fresh(path, Follow::Yes)
                } else {
                    host.metadata(path, Follow::Yes)
                };
                meta.is_err_and(|error| error.kind == ErrorKind::NotFound)
                    .then(|| Digest::hash(b"missing"))
            }
            Self::Link(path) => host.read_link(path).ok().map(|target| path_digest(&target)),
            Self::Canonical(path) => {
                let canon = if fresh {
                    host.canonicalize_fresh(path)
                } else {
                    host.canonicalize(path)
                };
                canon.ok().map(|target| path_digest(&target))
            }
            Self::Environment => Some(process_environment_fingerprint(&host.env_all())),
            Self::Executable => host.current_exe().ok().map(|exe| path_digest(&exe)),
            Self::WorkingDirectory => Some(path_digest(&host.initial_cwd())),
            Self::System => Some(system_digest(&host.system())),
            Self::Request(_) => None,
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
    key.field(8, &metadata.dev.to_le_bytes());
    key.field(9, &metadata.ino.to_le_bytes());
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
    key.finish()
}

type Impurity = (ImpureReason, &'static Location<'static>);

#[derive(Default)]
struct State {
    stamps: BTreeMap<StampKey, InputStamp>,
    checks: BTreeMap<Check, Digest>,
    environment: BTreeMap<Vec<u8>, Option<BString>>,
    full_environment: Option<Vec<(BString, BString)>>,
    processes: Vec<RunRequest>,
    output: Vec<(Stream, Vec<u8>)>,
    output_bytes: usize,
    impure: Option<ImpureReason>,
    impurities: Option<Vec<Impurity>>,
}

impl State {
    #[track_caller]
    fn mark_impure(&mut self, reason: ImpureReason) {
        self.impure.get_or_insert(reason);
        if let Some(impurities) = &mut self.impurities {
            let entry = (reason, Location::caller());
            if !impurities.contains(&entry) {
                impurities.push(entry);
            }
        }
    }

    fn can_record(&mut self) -> bool {
        if self.stamps.len() + self.checks.len() + self.environment.len() + self.processes.len()
            >= MAX_OBSERVATIONS
        {
            self.mark_impure(ImpureReason::JournalLimit);
            return false;
        }
        true
    }

    fn stamp(&mut self, stamp: InputStamp) {
        if self.can_record() && conflicts(&mut self.stamps, StampKey::from(&stamp), stamp) {
            self.mark_impure(ImpureReason::ChangedInput);
        }
    }

    fn check(&mut self, check: Check, expected: Digest) {
        if self.can_record() && conflicts(&mut self.checks, check, expected) {
            self.mark_impure(ImpureReason::ChangedInput);
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
#[derive(Clone, Debug)]
#[cfg_attr(test, derive(PartialEq, Eq))]
pub(crate) struct JournalChecks(Vec<(Check, Digest)>);

impl ToJson for JournalChecks {
    fn to_json(&self) -> Value {
        Value::array_from(
            self.0
                .iter()
                .map(|(check, digest)| Value::array_from([check.to_json(), digest.to_json()])),
        )
    }
}

impl FromJson for JournalChecks {
    fn from_json(value: &Value) -> Result<Self, FieldError> {
        let items = rcmake_json::array(value)?;
        if items.len() > MAX_OBSERVATIONS {
            return Err(FieldError::invalid(format!(
                "journal check count {} exceeds limit of {MAX_OBSERVATIONS}",
                items.len()
            )));
        }
        items
            .iter()
            .map(|item| {
                let [check, digest] = array_pair(item)?;
                Ok((Check::from_json(check)?, Digest::from_json(digest)?))
            })
            .collect::<Result<Vec<_>, _>>()
            .map(Self)
    }
}

impl JournalChecks {
    /// Return one check of each variant, for the store schema parity test.
    #[cfg(all(test, unix))]
    pub(crate) fn every_variant() -> Self {
        let path = || PathBuf::from("/work/input");
        let digest = Digest::from_bytes([0x5a; 32]);
        Self(
            [
                Check::Metadata(path(), true),
                Check::Slice(
                    path(),
                    Slice {
                        offset: 4,
                        len: Some(8),
                    },
                ),
                Check::Slice(
                    path(),
                    Slice {
                        offset: 0,
                        len: None,
                    },
                ),
                Check::Chunks(path()),
                Check::DirectoryOrder(path()),
                Check::OrderedDirectoryMissing(path()),
                Check::FollowMissing(path()),
                Check::Link(path()),
                Check::Canonical(path()),
                Check::Environment,
                Check::Executable,
                Check::WorkingDirectory,
                Check::System,
                Check::Request(2),
            ]
            .into_iter()
            .map(|check| (check, digest))
            .collect(),
        )
    }

    /// Estimate the serialized byte count without allocating a frame.
    pub(crate) fn encoded_size_bound(&self) -> usize {
        self.0.iter().fold(16usize, |sum, (check, _)| {
            let path = match check {
                Check::Metadata(path, _)
                | Check::Slice(path, _)
                | Check::Chunks(path)
                | Check::DirectoryOrder(path)
                | Check::OrderedDirectoryMissing(path)
                | Check::FollowMissing(path)
                | Check::Link(path)
                | Check::Canonical(path) => Some(path),
                _ => None,
            };
            let path_bytes = path.map_or(0, |path| path.as_os_str().as_encoded_bytes().len());
            sum.saturating_add(256)
                .saturating_add(path_bytes.saturating_mul(6))
        })
    }

    /// Check all supplemental observations.
    pub(crate) fn verify(&self, host: &dyn Host) -> bool {
        self.0.iter().all(|(check, expected)| match check {
            Check::Request(_) => true,
            _ => check.digest(host) == Some(*expected),
        })
    }
}

/// Evidence collected from one execution. Raw environment values stay in memory.
pub(crate) struct Journal {
    trace: Trace,
    memo: Option<Arc<TraceMemo>>,
    checks: JournalChecks,
    #[cfg(all(test, unix))]
    environment: BTreeMap<Vec<u8>, Option<BString>>,
    #[cfg(all(test, unix))]
    full_environment: Option<Vec<(BString, BString)>>,
    processes: Vec<RunRequest>,
    output: Vec<(Stream, Vec<u8>)>,
    impure: Option<ImpureReason>,
    impurities: Option<Vec<Impurity>>,
}

impl Journal {
    /// Return the first reason that the outcome cannot be stored.
    pub(crate) fn impure(&self) -> Option<ImpureReason> {
        self.impure
    }

    pub(super) fn impurities(&self) -> &[Impurity] {
        self.impurities.as_deref().unwrap_or_default()
    }

    /// Return recorded scalar environment reads, including absent values.
    #[cfg(all(test, unix))]
    pub(crate) fn environment(&self) -> &BTreeMap<Vec<u8>, Option<BString>> {
        &self.environment
    }

    /// Return the exact environment from the first enumeration.
    #[cfg(all(test, unix))]
    pub(crate) fn full_environment(&self) -> Option<&[(BString, BString)]> {
        self.full_environment.as_deref()
    }

    /// Return recorded process requests in invocation order.
    pub(crate) fn processes(&self) -> &[RunRequest] {
        &self.processes
    }

    /// Get output that was captured without writing to the parent host.
    pub(crate) fn output(&self) -> &[(Stream, Vec<u8>)] {
        &self.output
    }

    /// Return the persistent validators only for a pure, unchanged run.
    pub(crate) fn verified_inputs(&self, host: &dyn Host) -> Option<(&Trace, &JournalChecks)> {
        (self.impure.is_none()
            && self.checks.verify(host)
            && verify_inputs(host, &self.trace, self.memo.as_deref()) == VerifyOutcome::Hit)
            .then_some((&self.trace, &self.checks))
    }

    #[cfg(all(test, target_os = "macos"))]
    pub(super) fn checks(&self) -> &JournalChecks {
        &self.checks
    }

    #[cfg(all(test, unix))]
    pub(super) fn request_check_digests(&self) -> Vec<(u32, Digest)> {
        self.checks
            .0
            .iter()
            .filter_map(|(check, digest)| match check {
                Check::Request(index) => Some((*index, *digest)),
                _ => None,
            })
            .collect()
    }
}

fn verify_inputs(host: &dyn Host, trace: &[InputStamp], memo: Option<&TraceMemo>) -> VerifyOutcome {
    memo.map_or_else(
        || verify_trace(host, trace),
        |memo| memo.verify(host, trace),
    )
}

/// Owned, self-contained snapshot of every [`JournalHost`] observation captured during
/// a `try_compile` prelude.
///
/// Spliced back into a subsequent journaled child's [`JournalHost`] only after live
/// re-validation of every stamp, check, and environment read succeeds.
#[derive(Clone)]
pub(super) struct PreludeJournalSlice {
    stamps: Vec<InputStamp>,
    checks: Vec<(Check, Digest)>,
    environment: Vec<(Vec<u8>, Option<BString>)>,
    full_environment: Option<Vec<(BString, BString)>>,
    processes: Vec<RunRequest>,
    output: Vec<(Stream, Vec<u8>)>,
    impure: Option<ImpureReason>,
    missing_scratch: Vec<(PathBuf, Follow)>,
}

impl PreludeJournalSlice {
    #[cfg(all(test, unix))]
    pub(super) fn from_requests_for_test(requests: Vec<RunRequest>) -> Self {
        let checks = requests
            .iter()
            .enumerate()
            .map(|(index, req)| (Check::Request(index as u32), request_digest(req)))
            .collect();
        Self {
            stamps: Vec::new(),
            checks,
            environment: Vec::new(),
            full_environment: None,
            processes: requests,
            output: Vec::new(),
            impure: None,
            missing_scratch: Vec::new(),
        }
    }

    /// Verify that every scratch-relative path whose metadata returned `NotFound` during
    /// capture is still absent in the target probe's `scratch` directory.
    pub(super) fn validate_scratch(&self, live_host: &dyn Host, scratch: &Path) -> bool {
        self.missing_scratch.iter().all(|(rel, follow)| {
            live_host
                .metadata_fresh(&scratch.join(rel), *follow)
                .is_err_and(|err| err.kind == ErrorKind::NotFound)
        })
    }

    /// Validate every recorded observation against the live unwrapped host before splicing.
    pub(super) fn validate(&self, live_host: &dyn Host, trace_memo: &TraceMemo) -> bool {
        if self.impure.is_some() || !self.output.is_empty() {
            return false;
        }
        if trace_memo.verify(live_host, &self.stamps) != VerifyOutcome::Hit {
            return false;
        }
        for (name, expected) in &self.environment {
            if live_host.env_get(name) != *expected {
                return false;
            }
        }
        if let Some(expected_full) = &self.full_environment
            && live_host.env_all() != *expected_full
        {
            return false;
        }
        for (check, expected) in &self.checks {
            match check {
                Check::Request(index) => {
                    let Some(req) = self.processes.get(*index as usize) else {
                        return false;
                    };
                    if request_digest(req) != *expected {
                        return false;
                    }
                }
                _ => {
                    if check.digest_fresh(live_host) != Some(*expected) {
                        return false;
                    }
                }
            }
        }
        true
    }
}

const PRELUDE_WATCH_OFF: u8 = 0;
const PRELUDE_WATCH_HARNESS: u8 = 1;
const PRELUDE_WATCH_BODY: u8 = 2;

#[derive(Clone, Copy, Eq, PartialEq)]
pub(super) enum ScratchAccessKind {
    Read,
    Metadata,
    MetadataMissing { follow_yes: bool },
    CreateDir,
    Other,
}

/// Host adapter for a single probe.
///
/// The caller owns a fresh scratch directory and keeps it private for this
/// adapter's lifetime. External inputs require a stable snapshot through use.
pub(crate) struct JournalHost<'a> {
    inner: SharedOrBorrowed<'a, dyn Host + 'a>,
    scratch: PathBuf,
    canonical_scratch: PathBuf,
    mode: JournalMode,
    verify_process: Option<SharedOrBorrowed<'a, dyn ProcessVerifier + 'a>>,
    memo: Option<Arc<TraceMemo>>,
    speculation: Option<Speculation>,
    compile_intervals: Option<Arc<CompileIntervals>>,
    sorted_listings: bool,
    /// Files whose contents the probe key binds. Reads do not stamp them.
    bound: BTreeSet<PathBuf>,
    prelude_watch: AtomicU8,
    harness_lists_reads: AtomicU8,
    prelude_touched_scratch: AtomicBool,
    prelude_missing_scratch: Mutex<BTreeSet<(PathBuf, bool)>>,
    state: Mutex<State>,
}

struct Speculation {
    environment: Vec<(BString, BString)>,
    cancel: CancelToken,
    shutdown: CancelToken,
    deadline: std::time::Instant,
}

impl<'a> JournalHost<'a> {
    /// Observe a probe in an existing private directory.
    pub(crate) fn new(
        inner: &'a dyn Host,
        scratch: &Path,
        mode: JournalMode,
        verify_process: Option<&'a dyn ProcessVerifier>,
    ) -> HostResult<Self> {
        Self::from_parts(
            SharedOrBorrowed::Borrowed(inner),
            scratch,
            mode,
            verify_process.map(SharedOrBorrowed::Borrowed),
        )
    }

    fn from_parts(
        inner: SharedOrBorrowed<'a, dyn Host + 'a>,
        scratch: &Path,
        mode: JournalMode,
        verify_process: Option<SharedOrBorrowed<'a, dyn ProcessVerifier + 'a>>,
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
            memo: None,
            speculation: None,
            compile_intervals: None,
            sorted_listings: false,
            bound: BTreeSet::new(),
            prelude_watch: AtomicU8::new(PRELUDE_WATCH_OFF),
            harness_lists_reads: AtomicU8::new(0),
            prelude_touched_scratch: AtomicBool::new(false),
            prelude_missing_scratch: Mutex::default(),
            state: Mutex::default(),
        })
    }

    /// Arm outer-harness scratch-access detection before `run_configure` loads the
    /// generated `CMakeLists.txt`. Transitions to strict body mode once `project()` enters
    /// language bootstrap via [`Self::enter_prelude_body`].
    pub(super) fn begin_prelude_harness(&self) {
        self.prelude_touched_scratch.store(false, Ordering::Release);
        self.harness_lists_reads.store(0, Ordering::Release);
        self.prelude_missing_scratch
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clear();
        self.prelude_watch
            .store(PRELUDE_WATCH_HARNESS, Ordering::Release);
    }

    /// Transition from outer harness setup into the `project()` prelude body, where ANY
    /// read, metadata stat, or mutation under scratch marks the prelude non-cacheable.
    pub(super) fn enter_prelude_body(&self) {
        self.prelude_watch
            .store(PRELUDE_WATCH_BODY, Ordering::Release);
    }

    /// Arm strict scratch-access detection directly for the prelude body (zero exemptions).
    #[cfg(all(test, unix))]
    pub(super) fn begin_prelude_watch(&self) {
        self.prelude_touched_scratch.store(false, Ordering::Release);
        self.harness_lists_reads.store(0, Ordering::Release);
        self.prelude_missing_scratch
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clear();
        self.prelude_watch
            .store(PRELUDE_WATCH_BODY, Ordering::Release);
    }

    /// Disarm scratch-access detection and clone all pure journal observations recorded
    /// during the prelude. Returns `None` if the prelude was impure, produced output,
    /// ran any external process, exceeded the observation limit, or accessed scratch paths.
    pub(super) fn capture_prelude_slice(&self) -> Option<PreludeJournalSlice> {
        self.prelude_watch
            .store(PRELUDE_WATCH_OFF, Ordering::Release);
        if self.prelude_touched_scratch.load(Ordering::Acquire) {
            return None;
        }
        let missing_scratch: Vec<(PathBuf, Follow)> = self
            .prelude_missing_scratch
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .map(|(rel, follow_yes)| {
                (
                    rel.clone(),
                    if *follow_yes { Follow::Yes } else { Follow::No },
                )
            })
            .collect();
        let state = self.state();
        let observations = state
            .stamps
            .len()
            .saturating_add(state.checks.len())
            .saturating_add(state.environment.len())
            .saturating_add(state.processes.len())
            .saturating_add(missing_scratch.len());
        if state.impure.is_some()
            || !state.output.is_empty()
            || !state.processes.is_empty()
            || observations >= MAX_OBSERVATIONS
        {
            return None;
        }
        Some(PreludeJournalSlice {
            stamps: state.stamps.values().cloned().collect(),
            checks: state.checks.iter().map(|(k, v)| (k.clone(), *v)).collect(),
            environment: state
                .environment
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect(),
            full_environment: state.full_environment.clone(),
            processes: state.processes.clone(),
            output: state.output.clone(),
            impure: state.impure,
            missing_scratch,
        })
    }

    /// Splice a previously validated [`PreludeJournalSlice`] into this journal.
    ///
    /// Validates capacity and conflict freedom before mutating state so a rejected splice
    /// leaves the journal clean for live fallback execution.
    pub(super) fn splice_prelude_slice(&self, slice: &PreludeJournalSlice) -> bool {
        if slice.impure.is_some() || !slice.output.is_empty() {
            return false;
        }
        let mut state = self.state();
        if state.impure.is_some() {
            return false;
        }
        let projected = state
            .stamps
            .len()
            .saturating_add(slice.stamps.len())
            .saturating_add(state.checks.len())
            .saturating_add(slice.checks.len())
            .saturating_add(state.environment.len())
            .saturating_add(slice.environment.len())
            .saturating_add(state.processes.len())
            .saturating_add(slice.processes.len());
        if projected >= MAX_OBSERVATIONS {
            return false;
        }
        let process_base = match u32::try_from(state.processes.len()) {
            Ok(base) => base,
            Err(_) => return false,
        };
        // Pre-validate all entries before mutating state.
        for stamp in &slice.stamps {
            if let Some(existing) = state.stamps.get(&StampKey::from(stamp))
                && existing != stamp
            {
                return false;
            }
        }
        for (check, digest) in &slice.checks {
            let rebased = match check {
                Check::Request(index) => match process_base.checked_add(*index) {
                    Some(new_index) => Check::Request(new_index),
                    None => return false,
                },
                other => other.clone(),
            };
            if let Some(existing) = state.checks.get(&rebased)
                && existing != digest
            {
                return false;
            }
        }
        for (name, value) in &slice.environment {
            if let Some(existing) = state.environment.get(name)
                && existing != value
            {
                return false;
            }
        }
        if let (Some(existing), Some(incoming)) = (&state.full_environment, &slice.full_environment)
            && existing != incoming
        {
            return false;
        }
        // Commit all entries cleanly.
        for stamp in &slice.stamps {
            state.stamps.insert(StampKey::from(stamp), stamp.clone());
        }
        for (check, digest) in &slice.checks {
            let rebased = match check {
                Check::Request(index) => Check::Request(process_base + *index),
                other => other.clone(),
            };
            state.checks.insert(rebased, *digest);
        }
        for (name, value) in &slice.environment {
            state.environment.insert(name.clone(), value.clone());
        }
        if state.full_environment.is_none() {
            state.full_environment.clone_from(&slice.full_environment);
        }
        state.processes.extend(slice.processes.iter().cloned());
        true
    }

    pub(super) fn note_prelude_scratch_access(&self, path: &Path, kind: ScratchAccessKind) {
        let mode = self.prelude_watch.load(Ordering::Acquire);
        if mode == PRELUDE_WATCH_OFF {
            return;
        }
        let Some(rel) = self.scratch_relative(path) else {
            return;
        };
        if mode == PRELUDE_WATCH_BODY {
            if let ScratchAccessKind::MetadataMissing { follow_yes } = kind
                && !rel.as_os_str().is_empty()
            {
                let mut missing = self
                    .prelude_missing_scratch
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner);
                let key = (rel, follow_yes);
                if missing.len() < MAX_OBSERVATIONS || missing.contains(&key) {
                    missing.insert(key);
                } else {
                    self.prelude_touched_scratch.store(true, Ordering::Release);
                }
                return;
            }
            self.prelude_touched_scratch.store(true, Ordering::Release);
            return;
        }
        // Only the outer `run_configure` harness parsing the generated `CMakeLists.txt`
        // and initializing `CMakeFiles/pkgRedirects` is exempt prior to `project()`.
        let allowed = match kind {
            ScratchAccessKind::Read => {
                rel == Path::new("CMakeLists.txt")
                    && self.harness_lists_reads.fetch_add(1, Ordering::AcqRel) == 0
            }
            ScratchAccessKind::Metadata | ScratchAccessKind::MetadataMissing { .. } => {
                rel == Path::new("CMakeLists.txt")
                    || rel == Path::new("CMakeFiles")
                    || rel == Path::new("CMakeFiles/pkgRedirects")
            }
            ScratchAccessKind::CreateDir => {
                rel.as_os_str().is_empty()
                    || rel == Path::new("CMakeFiles")
                    || rel == Path::new("CMakeFiles/pkgRedirects")
            }
            ScratchAccessKind::Other => false,
        };
        if !allowed {
            self.prelude_touched_scratch.store(true, Ordering::Release);
        }
    }

    fn scratch_relative(&self, p: &Path) -> Option<PathBuf> {
        if !p.is_absolute() {
            return None;
        }
        let cmake = path::from_native_cmake(p);
        let norm = path::to_native(&rcmake_base::path_lex::Lexical::new(&cmake).normalize());
        norm.strip_prefix(&self.scratch)
            .ok()
            .or_else(|| norm.strip_prefix(&self.canonical_scratch).ok())
            .map(Path::to_path_buf)
    }

    pub(super) fn with_impurity_sites(self, enabled: bool) -> Self {
        if enabled {
            self.state().impurities = Some(Vec::new());
        }
        self
    }

    /// Skip read evidence for files whose contents the probe key binds.
    ///
    /// The key binds the generated platform files with their paths templated.
    /// Compiler identification rewrites them after the ABI probe, so a stamp
    /// on the absolute path would reject a later relocated hit.
    pub(super) fn with_bound_inputs(mut self, files: BTreeSet<PathBuf>) -> Self {
        self.bound = files;
        self
    }

    /// Return whether reads of the path need no evidence.
    fn unobserved_read(&self, path: &Path) -> bool {
        self.private_path(path) || self.bound.contains(path)
    }

    pub(super) fn with_trace_memo(mut self, memo: Arc<TraceMemo>) -> Self {
        self.memo = Some(memo);
        self.sorted_listings = true;
        self
    }

    /// Use the owned job environment and the session's speculative deadline.
    pub(crate) fn with_speculation(
        mut self,
        environment: Vec<(BString, BString)>,
        cancel: CancelToken,
        shutdown: CancelToken,
        deadline: std::time::Instant,
    ) -> Self {
        self.speculation = Some(Speculation {
            environment,
            cancel,
            shutdown,
            deadline,
        });
        self
    }

    /// Record a dependency from a verified compiler or linker observation.
    ///
    /// Use this for paths from a depfile and for directly resolved headers,
    /// libraries, toolchain executables, and other regular files.
    pub(crate) fn record_dependency(&self, path: &Path) -> HostResult<()> {
        self.record_dependency_since(path, None)
    }

    pub(super) fn record_generator_dependency(
        &self,
        path: &Path,
        digest: Digest,
    ) -> HostResult<()> {
        if self.private_path(path) {
            return Err(HostError::unsupported(
                "The Ninja program is in probe scratch space.",
            ));
        }
        self.record_dependency(path)?;
        let mut state = self.state();
        state.stamp(InputStamp::FileHash {
            path: path.to_owned(),
            digest,
        });
        Ok(())
    }

    pub(super) fn reject_generator_fact(&self) {
        self.mark_impure(ImpureReason::UnverifiedProcess);
    }

    fn record_dependency_since(
        &self,
        path: &Path,
        time: Option<ObservationTime>,
    ) -> HostResult<()> {
        if !path.is_absolute() {
            return Err(HostError::not_absolute(path.to_owned()));
        }
        if self.private_path(path) {
            return Ok(());
        }
        let metadata = self.inner.metadata(path, Follow::Yes)?;
        let canonical = self.inner.canonicalize(path)?;
        if time.is_some_and(|time| {
            time.changed(&metadata)
                || path.ancestors().any(|ancestor| {
                    self.inner
                        .metadata(ancestor, Follow::No)
                        .is_ok_and(|stat| stat.kind == FileKind::Symlink && time.changed(&stat))
                })
        }) {
            self.mark_impure(ImpureReason::ChangedInput);
        }
        let result = self.memo.as_ref().map_or_else(
            || file_digest(&*self.inner, path),
            |memo| memo.file_digest(&*self.inner, path),
        );
        if let Ok(digest) = result {
            let mut state = self.state();
            state.check(Check::Canonical(path.to_owned()), path_digest(&canonical));
            state.check(
                Check::Metadata(path.to_owned(), true),
                metadata_digest(&metadata),
            );
            state.stamp(InputStamp::FileHash {
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
    #[cfg(all(test, unix))]
    pub(crate) fn record_search(&self, root: &Path, relative: &Path) -> HostResult<bool> {
        self.record_search_since(root, relative, None)
    }

    fn record_search_since(
        &self,
        root: &Path,
        relative: &Path,
        time: Option<ObservationTime>,
    ) -> HostResult<bool> {
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
                self.record_dependency_since(&candidate, time)?;
                Ok(true)
            }
            Err(error) if error.kind == ErrorKind::NotFound && self.private_path(&candidate) => {
                Ok(false)
            }
            Err(error) if error.kind == ErrorKind::NotFound => {
                let result =
                    self.record_negative_search(root, relative, time.map(|time| time.cutoff));
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

    fn record_negative_search(
        &self,
        root: &Path,
        relative: &Path,
        cutoff: Option<i128>,
    ) -> HostResult<()> {
        let mut directory = match self.inner.metadata(root, Follow::Yes) {
            Ok(metadata) if metadata.kind == FileKind::Dir => root.to_owned(),
            Err(error) if error.kind == ErrorKind::NotFound && missing(&*self.inner, root) => {
                self.state().stamp(InputStamp::Missing {
                    path: root.to_owned(),
                });
                return Ok(());
            }
            Err(error) if error.kind == ErrorKind::NotFound => {
                return self.record_dangling_search(root, cutoff);
            }
            _ => {
                return Err(HostError::unsupported(
                    "Cannot verify the search directory.",
                ));
            }
        };
        for part in relative.components() {
            let next = directory.join(part.as_os_str());
            if missing(&*self.inner, &next) {
                if let Some(cutoff) = cutoff {
                    let metadata = self.inner.metadata(&directory, Follow::Yes)?;
                    if time_nanos(metadata.mtime) >= cutoff {
                        self.mark_impure(ImpureReason::ChangedInput);
                    }
                }
                let listing = directory_digest(&*self.inner, &directory)?;
                self.state().stamp(InputStamp::NegativeLookup {
                    root: root.to_owned(),
                    relative: relative.to_owned(),
                    deepest: directory,
                    listing,
                });
                return Ok(());
            }
            match self.inner.metadata(&next, Follow::Yes) {
                Ok(metadata) if metadata.kind == FileKind::Dir => {}
                Err(error) if error.kind == ErrorKind::NotFound => {
                    return self.record_dangling_search(&next, cutoff);
                }
                _ => break,
            }
            directory = next;
        }
        Err(HostError::unsupported(
            "Cannot verify the missing search candidate.",
        ))
    }

    fn record_dangling_search(&self, link: &Path, cutoff: Option<i128>) -> HostResult<()> {
        let metadata = self.inner.metadata(link, Follow::No)?;
        if metadata.kind != FileKind::Symlink {
            return Err(HostError::unsupported(
                "Cannot verify the missing search path.",
            ));
        }
        if cutoff.is_some_and(|cutoff| time_nanos(metadata.mtime) >= cutoff) {
            self.mark_impure(ImpureReason::ChangedInput);
        }
        let target = self.inner.read_link(link)?;
        let mut state = self.state();
        state.check(Check::Link(link.to_owned()), path_digest(&target));
        state.check(
            Check::FollowMissing(link.to_owned()),
            Digest::hash(b"missing"),
        );
        Ok(())
    }

    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    #[track_caller]
    fn mark_impure(&self, reason: ImpureReason) {
        self.state().mark_impure(reason);
    }

    #[track_caller]
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
        if error.kind == ErrorKind::NotFound && missing(&*self.inner, path) {
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
            let index = state.processes.len() as u32;
            state.check(Check::Request(index), request_digest(request));
            state.processes.push(request.clone());
        }
    }
}

fn time_nanos(time: FileTime) -> i128 {
    i128::from(time.secs) * 1_000_000_000 + i128::from(time.nanos)
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

#[cfg(all(test, unix))]
#[path = "journal_tests.rs"]
mod tests;
