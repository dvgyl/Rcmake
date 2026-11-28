#[cfg(all(feature = "native", any(unix, windows)))]
use std::collections::{BTreeMap, BTreeSet, btree_map::Entry};
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
#[cfg(all(feature = "native", any(unix, windows)))]
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use rcmake_base::BString;
#[cfg(all(feature = "native", any(unix, windows)))]
use rcmake_host::FileKind;
use rcmake_host::{
    CopyOptions, DirEntry, FileLockGuard, FileTime, Follow, GracefulTimeout, Host, HostResult,
    Metadata, Output, ReadRange, RunOutput, RunRequest, Stream, SystemInfo, WriteMode,
};
#[cfg(all(feature = "native", any(unix, windows)))]
use rcmake_store::{Digest, Domain, KeyHasher};

use super::TIMEOUT;

#[cfg(all(feature = "native", any(unix, windows)))]
const MAX_TRACKED_READS: usize = 512;
#[cfg(all(feature = "native", any(unix, windows)))]
const MAX_TRACKED_PROBES: usize = 1024;
#[cfg(all(feature = "native", any(unix, windows)))]
const MAX_TRACKED_DIRS: usize = 256;
/// Largest `read_chunks` result that the tracker buffers to hash at read time.
const MAX_HASHED_CHUNK_BYTES: usize = 8 * 1024 * 1024;

/// Metadata fields of one path that the prelude observed.
#[cfg(all(feature = "native", any(unix, windows)))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct MetaObservation {
    pub(super) kind: FileKind,
    pub(super) mode: u32,
    pub(super) len: u64,
    pub(super) mtime: FileTime,
}

#[cfg(all(feature = "native", any(unix, windows)))]
impl MetaObservation {
    pub(super) fn of(meta: &Metadata) -> Self {
        Self {
            kind: meta.kind,
            mode: meta.mode,
            len: meta.len,
            mtime: meta.mtime,
        }
    }

    /// Return whether `meta` shows the same observable state.
    ///
    /// The length and modification time of a directory change when any entry changes,
    /// including the scratch directories of other probes. A prelude sees directory
    /// contents only through listings, which the snapshot verifies separately.
    pub(super) fn matches(&self, meta: &Metadata) -> bool {
        let now = Self::of(meta);
        if self.kind == FileKind::Dir {
            now.kind == self.kind && now.mode == self.mode
        } else {
            now == *self
        }
    }
}

/// Hash a directory listing exactly as [`rcmake_store::directory_digest`] does.
#[cfg(all(feature = "native", any(unix, windows)))]
pub(super) fn listing_digest(entries: &[DirEntry]) -> Digest {
    let mut sorted: Vec<&DirEntry> = entries.iter().collect();
    sorted.sort_unstable_by(|left, right| left.name.cmp(&right.name));
    let mut hasher = KeyHasher::new(Domain::BlobV1);
    hasher.field(0, b"directory");
    for entry in sorted {
        let tag = match entry.kind {
            FileKind::File => 1,
            FileKind::Dir => 2,
            FileKind::Symlink => 3,
            FileKind::Other => 4,
        };
        hasher.field(tag, &entry.name);
    }
    hasher.finish()
}

/// Filesystem and process observations recorded while capturing a try-compile prelude.
#[cfg(all(feature = "native", any(unix, windows)))]
pub(super) struct PreludeTrace {
    pub(super) mutated_external: bool,
    /// Content digest of the exact bytes that each read returned.
    pub(super) read_files: BTreeMap<PathBuf, Digest>,
    pub(super) missing_files: BTreeSet<(PathBuf, bool)>,
    /// Scratch-relative paths whose metadata lookup returned `NotFound` during the prelude
    /// (for example optional `include(...)` falling back to `CMAKE_CURRENT_SOURCE_DIR`).
    pub(super) missing_scratch: BTreeSet<(PathBuf, bool)>,
    pub(super) existing_files: BTreeMap<(PathBuf, bool), MetaObservation>,
    /// Digest of the exact entries that each directory listing returned.
    pub(super) listed_dirs: BTreeMap<PathBuf, Digest>,
    pub(super) read_links: BTreeMap<PathBuf, PathBuf>,
    pub(super) canonical_paths: BTreeMap<PathBuf, PathBuf>,
}

#[cfg(all(feature = "native", any(unix, windows)))]
struct PreludeTracker {
    binary_dir: PathBuf,
    canonical_binary_dir: Option<PathBuf>,
    strict_scratch: bool,
    harness_lists_read: bool,
    mutated_external: bool,
    read_files: BTreeMap<PathBuf, Digest>,
    missing_files: BTreeSet<(PathBuf, bool)>,
    missing_scratch: BTreeSet<(PathBuf, bool)>,
    existing_files: BTreeMap<(PathBuf, bool), MetaObservation>,
    listed_dirs: BTreeMap<PathBuf, Digest>,
    read_links: BTreeMap<PathBuf, PathBuf>,
    canonical_paths: BTreeMap<PathBuf, PathBuf>,
}

/// Record `value` for `key`. Return `false` when the map is full or when an earlier
/// observation of the same key disagrees, because then the prelude is not reproducible.
#[cfg(all(feature = "native", any(unix, windows)))]
fn observe<K: Ord, V: PartialEq>(map: &mut BTreeMap<K, V>, limit: usize, key: K, value: V) -> bool {
    let len = map.len();
    match map.entry(key) {
        Entry::Occupied(entry) => *entry.get() == value,
        Entry::Vacant(entry) if len < limit => {
            entry.insert(value);
            true
        }
        Entry::Vacant(_) => false,
    }
}

/// The result of one read, reduced to what the tracker records.
#[cfg(all(feature = "native", any(unix, windows)))]
enum ReadObservation {
    /// The read succeeded and returned bytes with this digest.
    Read(Digest),
    /// The read succeeded, but the tracker cannot know the digest of the bytes.
    Unhashed,
    /// The read failed with this error kind.
    Failed(ErrorKind),
}

#[cfg(all(feature = "native", any(unix, windows)))]
impl ReadObservation {
    fn of<T>(result: &HostResult<T>, digest: impl FnOnce(&T) -> Option<Digest>) -> Self {
        match result {
            Ok(value) => digest(value).map_or(Self::Unhashed, Self::Read),
            Err(err) => Self::Failed(err.kind),
        }
    }
}

#[cfg(all(feature = "native", any(unix, windows)))]
impl PreludeTracker {
    fn new(parent: &dyn Host, binary_dir: &Path) -> Self {
        let norm_binary = normalize_lexical(binary_dir);
        let canonical_binary_dir = parent
            .canonicalize(binary_dir)
            .ok()
            .map(|canon| normalize_lexical(&canon))
            .filter(|canon| *canon != norm_binary);
        Self {
            binary_dir: norm_binary,
            canonical_binary_dir,
            strict_scratch: false,
            harness_lists_read: false,
            mutated_external: false,
            read_files: BTreeMap::new(),
            missing_files: BTreeSet::new(),
            missing_scratch: BTreeSet::new(),
            existing_files: BTreeMap::new(),
            listed_dirs: BTreeMap::new(),
            read_links: BTreeMap::new(),
            canonical_paths: BTreeMap::new(),
        }
    }

    fn scratch_relative(&self, p: &Path) -> Option<PathBuf> {
        if !p.is_absolute() {
            return None;
        }
        let norm = normalize_lexical(p);
        norm.strip_prefix(&self.binary_dir)
            .ok()
            .or_else(|| {
                self.canonical_binary_dir
                    .as_ref()
                    .and_then(|canon| norm.strip_prefix(canon).ok())
            })
            .map(Path::to_path_buf)
    }

    fn note_missing(&mut self, p: &Path, follow_yes: bool) {
        let key = (p.to_path_buf(), follow_yes);
        if self.missing_files.len() < MAX_TRACKED_PROBES || self.missing_files.contains(&key) {
            self.missing_files.insert(key);
        } else {
            self.mutated_external = true;
        }
    }

    fn note_failure(&mut self, p: &Path, follow_yes: bool, kind: ErrorKind) {
        if kind == ErrorKind::NotFound {
            self.note_missing(p, follow_yes);
        } else {
            self.mutated_external = true;
        }
    }

    fn note_read(&mut self, p: &Path, observation: ReadObservation) {
        if self.mutated_external {
            return;
        }
        if let Some(rel) = self.scratch_relative(p) {
            // Only the outer `run_configure` harness reading `CMakeLists.txt` once before
            // `project()` enters language bootstrap is exempt.
            if !self.strict_scratch
                && rel == Path::new("CMakeLists.txt")
                && !self.harness_lists_read
            {
                self.harness_lists_read = true;
            } else {
                self.mutated_external = true;
            }
            return;
        }
        match observation {
            ReadObservation::Read(digest) => {
                if !observe(
                    &mut self.read_files,
                    MAX_TRACKED_READS,
                    p.to_path_buf(),
                    digest,
                ) {
                    self.mutated_external = true;
                }
            }
            ReadObservation::Unhashed => self.mutated_external = true,
            ReadObservation::Failed(kind) => self.note_failure(p, true, kind),
        }
    }

    fn note_metadata(&mut self, p: &Path, follow: Follow, result: &HostResult<Metadata>) {
        if self.mutated_external {
            return;
        }
        let follow_yes = follow == Follow::Yes;
        if let Some(rel) = self.scratch_relative(p) {
            if !self.strict_scratch
                && (rel == Path::new("CMakeLists.txt")
                    || rel == Path::new("CMakeFiles")
                    || rel == Path::new("CMakeFiles/pkgRedirects"))
            {
                return;
            }
            if result
                .as_ref()
                .is_err_and(|err| err.kind == ErrorKind::NotFound)
                && !rel.as_os_str().is_empty()
            {
                let key = (rel, follow_yes);
                if self.missing_scratch.len() < MAX_TRACKED_PROBES
                    || self.missing_scratch.contains(&key)
                {
                    self.missing_scratch.insert(key);
                } else {
                    self.mutated_external = true;
                }
            } else {
                self.mutated_external = true;
            }
            return;
        }
        match result {
            Ok(meta) => {
                if !observe(
                    &mut self.existing_files,
                    MAX_TRACKED_PROBES,
                    (p.to_path_buf(), follow_yes),
                    MetaObservation::of(meta),
                ) {
                    self.mutated_external = true;
                }
            }
            Err(err) => self.note_failure(p, follow_yes, err.kind),
        }
    }

    fn note_read_dir(&mut self, p: &Path, result: Result<(&[DirEntry], bool), ErrorKind>) {
        if self.mutated_external {
            return;
        }
        if self.scratch_relative(p).is_some() {
            self.mutated_external = true;
            return;
        }
        match result {
            // A truncated listing has no digest that a later full listing can match.
            Ok((_, true)) => self.mutated_external = true,
            Ok((entries, false)) => {
                if !observe(
                    &mut self.listed_dirs,
                    MAX_TRACKED_DIRS,
                    p.to_path_buf(),
                    listing_digest(entries),
                ) {
                    self.mutated_external = true;
                }
            }
            Err(kind) => self.note_failure(p, true, kind),
        }
    }

    fn note_read_link(&mut self, p: &Path, result: &HostResult<PathBuf>) {
        if self.mutated_external {
            return;
        }
        if self.scratch_relative(p).is_some() {
            self.mutated_external = true;
            return;
        }
        match result {
            Ok(target) => {
                if !observe(
                    &mut self.read_links,
                    MAX_TRACKED_DIRS,
                    p.to_path_buf(),
                    target.clone(),
                ) {
                    self.mutated_external = true;
                }
            }
            Err(err) => self.note_failure(p, false, err.kind),
        }
    }

    fn note_canonicalize(&mut self, p: &Path, result: &HostResult<PathBuf>) {
        if self.mutated_external {
            return;
        }
        if self.scratch_relative(p).is_some() {
            self.mutated_external = true;
            return;
        }
        match result {
            Ok(canon) => {
                if !observe(
                    &mut self.canonical_paths,
                    MAX_TRACKED_DIRS,
                    p.to_path_buf(),
                    canon.clone(),
                ) {
                    self.mutated_external = true;
                }
            }
            Err(err) => self.note_failure(p, true, err.kind),
        }
    }

    fn into_trace(self) -> PreludeTrace {
        PreludeTrace {
            mutated_external: self.mutated_external,
            read_files: self.read_files,
            missing_files: self.missing_files,
            missing_scratch: self.missing_scratch,
            existing_files: self.existing_files,
            listed_dirs: self.listed_dirs,
            read_links: self.read_links,
            canonical_paths: self.canonical_paths,
        }
    }
}

#[cfg(all(feature = "native", any(unix, windows)))]
fn normalize_lexical(path: &Path) -> PathBuf {
    let cmake = rcmake_host::path::from_native_cmake(path);
    rcmake_host::path::to_native(&rcmake_base::path_lex::Lexical::new(&cmake).normalize())
}

/// The content digest that the prelude tracker records for one read.
#[cfg(all(feature = "native", any(unix, windows)))]
type ReadDigest = Digest;
#[cfg(not(all(feature = "native", any(unix, windows))))]
type ReadDigest = ();

/// Capture test-project output while forwarding operations to the parent host.
pub(super) struct CapturingHost {
    parent: Arc<dyn Host>,
    output: Mutex<BString>,
    /// Messages that the test project configure wrote to standard error. CMake shows
    /// them in the calling project's output.
    diagnostics: Mutex<BString>,
    pub(super) trace_output: OnceLock<Option<PathBuf>>,
    system: SystemInfo,
    cwd: PathBuf,
    #[cfg(all(feature = "native", any(unix, windows)))]
    prelude_tracking_active: AtomicBool,
    #[cfg(all(feature = "native", any(unix, windows)))]
    prelude_tracker: Mutex<Option<PreludeTracker>>,
}

impl CapturingHost {
    fn new(parent: Arc<dyn Host>, cwd: &Path, system: SystemInfo) -> Arc<Self> {
        Arc::new(Self {
            system,
            parent,
            output: Mutex::default(),
            diagnostics: Mutex::default(),
            trace_output: OnceLock::new(),
            cwd: cwd.to_owned(),
            #[cfg(all(feature = "native", any(unix, windows)))]
            prelude_tracking_active: AtomicBool::new(false),
            #[cfg(all(feature = "native", any(unix, windows)))]
            prelude_tracker: Mutex::new(None),
        })
    }

    #[cfg(all(feature = "native", any(unix, windows)))]
    pub(super) fn begin_prelude_tracking(&self, binary_dir: &Path) {
        let tracker = PreludeTracker::new(self.parent.as_ref(), binary_dir);
        *self
            .prelude_tracker
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(tracker);
        self.prelude_tracking_active.store(true, Ordering::Release);
    }

    /// Switch prelude tracking into strict `project()` body mode where any read, stat,
    /// or directory operation under the scratch directory marks the prelude non-cacheable.
    #[cfg(all(feature = "native", any(unix, windows)))]
    pub(super) fn enter_prelude_body(&self) {
        self.with_prelude_tracker(|tracker| tracker.strict_scratch = true);
    }

    #[cfg(all(feature = "native", any(unix, windows)))]
    pub(super) fn finish_prelude_tracking(&self) -> Option<PreludeTrace> {
        self.prelude_tracking_active.store(false, Ordering::Release);
        self.prelude_tracker
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
            .map(PreludeTracker::into_trace)
    }

    #[cfg(all(feature = "native", any(unix, windows)))]
    fn with_prelude_tracker(&self, f: impl FnOnce(&mut PreludeTracker)) {
        if !self.prelude_tracking_active.load(Ordering::Acquire) {
            return;
        }
        let mut guard = self
            .prelude_tracker
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(tracker) = guard.as_mut() {
            f(tracker);
        }
    }

    #[cfg(not(all(feature = "native", any(unix, windows))))]
    fn note_external_mutation(&self) {}

    #[cfg(all(feature = "native", any(unix, windows)))]
    fn note_external_mutation(&self) {
        self.with_prelude_tracker(|tracker| tracker.mutated_external = true);
    }

    #[cfg(not(all(feature = "native", any(unix, windows))))]
    fn note_write_path(&self, _p: &Path) {}

    /// Mark the prelude as not reproducible on any write. A snapshot hit does not replay
    /// writes, so a later child would miss the file, even below `<binary_dir>/CMakeFiles`.
    #[cfg(all(feature = "native", any(unix, windows)))]
    fn note_write_path(&self, _p: &Path) {
        self.note_external_mutation();
    }

    #[cfg(not(all(feature = "native", any(unix, windows))))]
    fn note_dir_mutation(&self, _p: &Path) {}

    #[cfg(all(feature = "native", any(unix, windows)))]
    fn note_dir_mutation(&self, p: &Path) {
        self.with_prelude_tracker(|tracker| {
            let allowed = tracker.scratch_relative(p).is_some_and(|rel| {
                !tracker.strict_scratch
                    && (rel.as_os_str().is_empty()
                        || rel == Path::new("CMakeFiles")
                        || rel == Path::new("CMakeFiles/pkgRedirects"))
            });
            if !allowed {
                tracker.mutated_external = true;
            }
        });
    }

    #[cfg(not(all(feature = "native", any(unix, windows))))]
    fn tracking(&self) -> bool {
        false
    }

    #[cfg(all(feature = "native", any(unix, windows)))]
    fn tracking(&self) -> bool {
        self.prelude_tracking_active.load(Ordering::Acquire)
    }

    /// Record a read of `p`. `digest` gives the content digest of the exact bytes that the
    /// read returned, or `None` when the tracker cannot know them.
    #[cfg(not(all(feature = "native", any(unix, windows))))]
    fn note_read_result<T>(
        &self,
        _p: &Path,
        _result: &HostResult<T>,
        _digest: impl FnOnce(&T) -> Option<ReadDigest>,
    ) {
    }

    #[cfg(all(feature = "native", any(unix, windows)))]
    fn note_read_result<T>(
        &self,
        p: &Path,
        result: &HostResult<T>,
        digest: impl FnOnce(&T) -> Option<Digest>,
    ) {
        if !self.tracking() {
            return;
        }
        let observation = ReadObservation::of(result, digest);
        self.with_prelude_tracker(|tracker| tracker.note_read(p, observation));
    }

    /// Return the digest of the whole file at `p`, read at once through the parent.
    #[cfg(not(all(feature = "native", any(unix, windows))))]
    fn digest_now(&self, _p: &Path) -> Option<ReadDigest> {
        None
    }

    #[cfg(all(feature = "native", any(unix, windows)))]
    fn digest_now(&self, p: &Path) -> Option<Digest> {
        rcmake_store::file_digest(self.parent.as_ref(), p).ok()
    }

    #[cfg(not(all(feature = "native", any(unix, windows))))]
    fn note_metadata_result(&self, _p: &Path, _follow: Follow, _result: &HostResult<Metadata>) {}

    #[cfg(all(feature = "native", any(unix, windows)))]
    fn note_metadata_result(&self, p: &Path, follow: Follow, result: &HostResult<Metadata>) {
        self.with_prelude_tracker(|tracker| tracker.note_metadata(p, follow, result));
    }

    #[cfg(not(all(feature = "native", any(unix, windows))))]
    fn note_read_dir_result(&self, _p: &Path, _result: Result<(&[DirEntry], bool), ErrorKind>) {}

    #[cfg(all(feature = "native", any(unix, windows)))]
    fn note_read_dir_result(&self, p: &Path, result: Result<(&[DirEntry], bool), ErrorKind>) {
        self.with_prelude_tracker(|tracker| tracker.note_read_dir(p, result));
    }

    #[cfg(not(all(feature = "native", any(unix, windows))))]
    fn note_read_link_result(&self, _p: &Path, _result: &HostResult<PathBuf>) {}

    #[cfg(all(feature = "native", any(unix, windows)))]
    fn note_read_link_result(&self, p: &Path, result: &HostResult<PathBuf>) {
        self.with_prelude_tracker(|tracker| tracker.note_read_link(p, result));
    }

    #[cfg(not(all(feature = "native", any(unix, windows))))]
    fn note_canonicalize_result(&self, _p: &Path, _result: &HostResult<PathBuf>) {}

    #[cfg(all(feature = "native", any(unix, windows)))]
    fn note_canonicalize_result(&self, p: &Path, result: &HostResult<PathBuf>) {
        self.with_prelude_tracker(|tracker| tracker.note_canonicalize(p, result));
    }

    /// Return the content digest of `bytes` that a full read of a file returned.
    #[cfg(not(all(feature = "native", any(unix, windows))))]
    fn bytes_digest(_bytes: &[u8]) -> Option<ReadDigest> {
        None
    }

    #[cfg(all(feature = "native", any(unix, windows)))]
    fn bytes_digest(bytes: &[u8]) -> Option<Digest> {
        Some(Digest::hash(bytes))
    }

    fn read_digest_of(&self, p: &Path, range: ReadRange, bytes: &[u8]) -> Option<ReadDigest> {
        // Only a full read returns the bytes that a file digest covers.
        match range {
            ReadRange::All { .. } => Self::bytes_digest(bytes),
            ReadRange::Slice { .. } => self.digest_now(p),
        }
    }

    #[cfg(all(test, feature = "native", unix))]
    pub(super) fn for_test(parent: Arc<dyn Host>, cwd: &Path, system: SystemInfo) -> Arc<Self> {
        Self::new(parent, cwd, system)
    }

    /// Run `configure` and return its result, all captured output, and the messages
    /// that the test project wrote to standard error.
    pub(super) fn capture<T>(
        parent: Arc<dyn Host>,
        cwd: &Path,
        configure: impl FnOnce(Arc<Self>) -> T,
    ) -> (T, BString, BString) {
        let system = parent.system();
        let host = Self::new(parent, cwd, system);
        let outcome = configure(Arc::clone(&host));
        let take = |buffer: &Mutex<BString>| {
            std::mem::take(
                &mut *buffer
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner),
            )
        };
        (outcome, take(&host.output), take(&host.diagnostics))
    }

    /// Keep the inherited standard error of a child process for the test result only.
    #[cfg(all(feature = "native", any(unix, windows)))]
    pub(super) fn capture_inherited_stderr(&self, bytes: &[u8]) {
        self.capture_process_output(bytes);
    }

    /// Return `true` when no stdout/stderr output or diagnostics have been captured yet.
    #[cfg(all(feature = "native", any(unix, windows)))]
    pub(super) fn is_silent(&self) -> bool {
        let output_empty = self
            .output
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_empty();
        let diag_empty = self
            .diagnostics
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_empty();
        output_empty && diag_empty
    }

    /// Keep process output for the test result only.
    fn capture_process_output(&self, bytes: &[u8]) {
        self.output
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .extend_from_slice(bytes);
    }

    fn run_captured(
        &self,
        request: &RunRequest,
        on_output: &mut dyn FnMut(Stream, &[u8]),
        run: impl FnOnce(&RunRequest, &mut dyn FnMut(Stream, &[u8])) -> HostResult<RunOutput>,
    ) -> HostResult<RunOutput> {
        self.note_external_mutation();
        let (request, stdout, stderr) = capture_request(request);
        let mut output = run(&request, &mut |stream, bytes| {
            if (stream == Stream::Stdout && stdout) || (stream == Stream::Stderr && stderr) {
                self.capture_process_output(bytes);
            } else {
                on_output(stream, bytes);
            }
        })?;
        if stdout {
            output.stdout.clear();
        }
        if stderr {
            output.stderr.clear();
        }
        Ok(output)
    }

    fn run_bounded_captured(
        &self,
        req: &RunRequest,
        run: impl FnOnce(&RunRequest) -> HostResult<RunOutput>,
    ) -> HostResult<RunOutput> {
        self.note_external_mutation();
        let (mut req, stdout, stderr) = capture_request(req);
        req.tee = false;
        let mut result = run(&req)?;
        if stdout {
            self.capture_process_output(&std::mem::take(&mut result.stdout));
        }
        if stderr {
            self.capture_process_output(&std::mem::take(&mut result.stderr));
        }
        Ok(result)
    }
}

fn capture_request(request: &RunRequest) -> (RunRequest, bool, bool) {
    let mut request = request.clone();
    request.timeout = Some(
        request
            .timeout
            .map_or(TIMEOUT, |timeout| timeout.min(TIMEOUT)),
    );
    let stdout = request.stdout == Output::Inherit;
    let stderr = request.stderr == Output::Inherit;
    if stdout {
        request.stdout = Output::Capture;
    }
    if stderr {
        request.stderr = Output::Capture;
    }
    request.tee |= stdout || stderr;
    (request, stdout, stderr)
}

macro_rules! forward_host {
    ($name:ident($($arg:ident: $ty:ty),* $(,)?) -> $result:ty) => {
        fn $name(&self, $($arg: $ty),*) -> $result {
            self.parent.$name($($arg),*)
        }
    };
}

impl Host for CapturingHost {
    fn read_dir(&self, p: &Path) -> HostResult<Vec<DirEntry>> {
        let result = self.parent.read_dir(p);
        self.note_read_dir_result(
            p,
            result
                .as_ref()
                .map(|entries| (entries.as_slice(), false))
                .map_err(|err| err.kind),
        );
        result
    }

    fn read_dir_bounded(&self, p: &Path, limit: usize) -> HostResult<(Vec<DirEntry>, bool)> {
        let result = self.parent.read_dir_bounded(p, limit);
        self.note_read_dir_result(
            p,
            result
                .as_ref()
                .map(|(entries, truncated)| (entries.as_slice(), *truncated))
                .map_err(|err| err.kind),
        );
        result
    }

    fn create_dir_all(&self, p: &Path) -> HostResult<()> {
        self.note_dir_mutation(p);
        self.parent.create_dir_all(p)
    }

    fn create_private_dir(&self, p: &Path) -> HostResult<()> {
        self.note_dir_mutation(p);
        self.parent.create_private_dir(p)
    }

    fn remove_file(&self, p: &Path) -> HostResult<()> {
        self.note_external_mutation();
        self.parent.remove_file(p)
    }

    fn remove_dir(&self, p: &Path) -> HostResult<()> {
        self.note_external_mutation();
        self.parent.remove_dir(p)
    }

    fn remove_dir_all(&self, p: &Path) -> HostResult<()> {
        self.note_external_mutation();
        self.parent.remove_dir_all(p)
    }

    fn rename(&self, from: &Path, to: &Path) -> HostResult<()> {
        self.note_external_mutation();
        self.parent.rename(from, to)
    }

    fn rename_noreplace(&self, from: &Path, to: &Path) -> HostResult<()> {
        self.note_external_mutation();
        self.parent.rename_noreplace(from, to)
    }

    fn symlink(&self, target: &Path, link: &Path) -> HostResult<()> {
        self.note_external_mutation();
        self.parent.symlink(target, link)
    }

    fn hard_link(&self, src: &Path, dst: &Path) -> HostResult<()> {
        self.note_external_mutation();
        self.parent.hard_link(src, dst)
    }

    fn read_link(&self, p: &Path) -> HostResult<PathBuf> {
        let result = self.parent.read_link(p);
        self.note_read_link_result(p, &result);
        result
    }

    fn canonicalize(&self, p: &Path) -> HostResult<PathBuf> {
        let result = self.parent.canonicalize(p);
        self.note_canonicalize_result(p, &result);
        result
    }

    fn canonicalize_fresh(&self, p: &Path) -> HostResult<PathBuf> {
        let result = self.parent.canonicalize_fresh(p);
        self.note_canonicalize_result(p, &result);
        result
    }

    fn metadata(&self, p: &Path, follow: Follow) -> HostResult<Metadata> {
        let result = self.parent.metadata(p, follow);
        self.note_metadata_result(p, follow, &result);
        result
    }

    fn metadata_fresh(&self, p: &Path, follow: Follow) -> HostResult<Metadata> {
        let result = self.parent.metadata_fresh(p, follow);
        self.note_metadata_result(p, follow, &result);
        result
    }

    fn read(&self, p: &Path, range: ReadRange) -> HostResult<Vec<u8>> {
        let result = self.parent.read(p, range);
        self.note_read_result(p, &result, |bytes| self.read_digest_of(p, range, bytes));
        result
    }

    fn read_with_origin(&self, p: &Path, range: ReadRange, origin: &Path) -> HostResult<Vec<u8>> {
        let result = self.parent.read_with_origin(p, range, origin);
        self.note_read_result(p, &result, |bytes| self.read_digest_of(p, range, bytes));
        result
    }

    fn read_chunks(&self, p: &Path, consume: &mut dyn FnMut(&[u8])) -> HostResult<()> {
        if !self.tracking() {
            return self.parent.read_chunks(p, consume);
        }
        // Keep the exact bytes that the caller saw, so that their digest is the one recorded.
        let mut seen = Vec::new();
        let mut overflow = false;
        let result = self.parent.read_chunks(p, &mut |chunk| {
            if !overflow && seen.len() + chunk.len() <= MAX_HASHED_CHUNK_BYTES {
                seen.extend_from_slice(chunk);
            } else {
                overflow = true;
                seen.clear();
            }
            consume(chunk);
        });
        self.note_read_result(p, &result, |()| {
            (!overflow).then(|| Self::bytes_digest(&seen)).flatten()
        });
        result
    }

    fn lock_file(
        &self,
        path: &Path,
        timeout: Option<Duration>,
    ) -> HostResult<Box<dyn FileLockGuard>> {
        self.note_external_mutation();
        self.parent.lock_file(path, timeout)
    }

    fn copy_file(&self, from: &Path, to: &Path, opts: CopyOptions) -> HostResult<()> {
        self.note_write_path(to);
        let result = self.parent.copy_file(from, to, opts);
        self.note_read_result(from, &result, |()| None);
        result
    }

    fn set_permissions(&self, p: &Path, mode: u32) -> HostResult<()> {
        self.note_external_mutation();
        self.parent.set_permissions(p, mode)
    }

    fn set_times(
        &self,
        p: &Path,
        atime: Option<FileTime>,
        mtime: Option<FileTime>,
    ) -> HostResult<()> {
        self.note_external_mutation();
        self.parent.set_times(p, atime, mtime)
    }

    fn set_symlink_times(
        &self,
        p: &Path,
        atime: Option<FileTime>,
        mtime: Option<FileTime>,
    ) -> HostResult<()> {
        self.note_external_mutation();
        self.parent.set_symlink_times(p, atime, mtime)
    }

    forward_host!(env_get(key: &[u8]) -> Option<BString>);

    fn env_set(&self, key: &[u8], value: Option<&[u8]>) {
        self.note_external_mutation();
        self.parent.env_set(key, value);
    }

    forward_host!(env_all() -> Vec<(BString, BString)>);
    forward_host!(env_epoch() -> Option<u64>);
    forward_host!(supports_process_execution() -> bool);

    fn write(&self, p: &Path, data: &[u8], mode: WriteMode) -> HostResult<()> {
        // The empty path routes trace lines to the parent's trace destination.
        if p.as_os_str().is_empty() {
            return match self.trace_output.get() {
                Some(Some(path)) => self.parent.write(path, data, mode),
                Some(None) => {
                    self.parent.write_out(Stream::Stderr, data);
                    Ok(())
                }
                None => Ok(()),
            };
        }
        self.note_write_path(p);
        self.parent.write(p, data, mode)
    }

    fn registry_host_view(&self) -> Option<rcmake_host::RegistryView> {
        self.parent.registry_host_view()
    }

    fn registry_value(
        &self,
        root: rcmake_host::RegistryRoot,
        subkey: &[u8],
        name: Option<&[u8]>,
        view: rcmake_host::RegistryView,
    ) -> HostResult<Option<BString>> {
        // The snapshot cannot verify registry values later.
        self.note_external_mutation();
        self.parent.registry_value(root, subkey, name, view)
    }

    fn open_read(&self, p: &Path) -> HostResult<Box<dyn rcmake_host::ReadSeek + '_>> {
        let result = self.parent.open_read(p);
        // The caller reads through the handle later, so hash the file when it opens.
        self.note_read_result(p, &result, |_| self.digest_now(p));
        result
    }

    fn open_read_with_metadata(
        &self,
        p: &Path,
    ) -> HostResult<(Box<dyn rcmake_host::ReadSeek + '_>, Option<Metadata>)> {
        let result = self.parent.open_read_with_metadata(p);
        self.note_read_result(p, &result, |_| self.digest_now(p));
        result
    }

    fn scratch_file(&self) -> HostResult<Box<dyn rcmake_host::ScratchFile + '_>> {
        self.note_external_mutation();
        self.parent.scratch_file()
    }

    fn scratch_file_in(&self, dir: &Path) -> HostResult<Box<dyn rcmake_host::ScratchFile + '_>> {
        self.note_dir_mutation(dir);
        self.parent.scratch_file_in(dir)
    }

    fn write_from(
        &self,
        p: &Path,
        mode: WriteMode,
        input: &mut dyn std::io::Read,
        len: u64,
    ) -> HostResult<()> {
        self.note_write_path(p);
        self.parent.write_from(p, mode, input, len)
    }

    fn run_controlled(
        &self,
        req: &RunRequest,
        graceful: Option<GracefulTimeout>,
        hard_limit: Option<std::time::Duration>,
        update: &mut rcmake_host::DeadlineUpdate<'_>,
    ) -> HostResult<rcmake_host::ControlledOutput> {
        self.note_external_mutation();
        let (req, stdout, stderr) = capture_request(req);
        let mut result = self.parent.run_controlled(
            &req,
            graceful,
            Some(hard_limit.map_or(TIMEOUT, |limit| limit.min(TIMEOUT))),
            &mut |stream, bytes| {
                if (stream == Stream::Stdout && stdout) || (stream == Stream::Stderr && stderr) {
                    self.capture_process_output(bytes);
                }
                update(stream, bytes)
            },
        )?;
        if stdout {
            result.output.stdout.clear();
        }
        if stderr {
            result.output.stderr.clear();
        }
        Ok(result)
    }

    fn run(
        &self,
        req: &RunRequest,
        on_output: &mut dyn FnMut(Stream, &[u8]),
    ) -> HostResult<RunOutput> {
        self.run_captured(req, on_output, |req, output| self.parent.run(req, output))
    }

    fn run_confined(
        &self,
        req: &RunRequest,
        scope: rcmake_host::WriteScope<'_>,
        on_output: &mut dyn FnMut(Stream, &[u8]),
    ) -> HostResult<RunOutput> {
        self.run_captured(req, on_output, |req, output| {
            self.parent.run_confined(req, scope, output)
        })
    }

    fn run_with_timeout_signal(
        &self,
        req: &RunRequest,
        timeout: GracefulTimeout,
        on_output: &mut dyn FnMut(Stream, &[u8]),
    ) -> HostResult<RunOutput> {
        self.run_captured(req, on_output, |req, output| {
            self.parent.run_with_timeout_signal(req, timeout, output)
        })
    }

    fn run_bounded(&self, req: &RunRequest, limit: usize) -> HostResult<RunOutput> {
        self.run_bounded_captured(req, |req| self.parent.run_bounded(req, limit))
    }

    fn run_bounded_confined(
        &self,
        req: &RunRequest,
        limit: usize,
        scope: rcmake_host::WriteScope<'_>,
    ) -> HostResult<RunOutput> {
        self.run_bounded_captured(req, |req| {
            self.parent.run_bounded_confined(req, limit, scope)
        })
    }

    fn write_out(&self, stream: Stream, bytes: &[u8]) {
        if stream == Stream::Stderr {
            self.diagnostics
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .extend_from_slice(bytes);
        }
        self.capture_process_output(bytes);
    }

    forward_host!(read_stdin(limit: u64) -> HostResult<Vec<u8>>);
    forward_host!(is_terminal(stream: Stream) -> bool);
    forward_host!(now() -> FileTime);
    forward_host!(monotonic() -> Duration);
    forward_host!(sleep(duration: Duration) -> ());
    forward_host!(time_zone() -> jiff::tz::TimeZone);
    forward_host!(current_exe() -> HostResult<PathBuf>);
    forward_host!(effective_uid() -> Option<u32>);

    fn initial_cwd(&self) -> PathBuf {
        self.cwd.clone()
    }

    fn system(&self) -> SystemInfo {
        self.system.clone()
    }
}
