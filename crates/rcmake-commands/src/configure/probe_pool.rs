//! Run private speculative probes through the journal host.

#![forbid(unsafe_code)]

use std::collections::{BTreeMap, VecDeque, hash_map::RandomState};
use std::hash::BuildHasher;
use std::num::NonZeroUsize;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use rcmake_base::BString;
use rcmake_eval::CancelToken;
use rcmake_host::{
    CommandSpec, FileKind, FileLockGuard, Follow, Host, HostError, HostResult, Input, Output,
    ProcessResult, ReadRange, RunRequest, WriteMode, path,
};
use rcmake_store::{Digest, StoreConfig};

use super::try_compile::{
    cache::{Artifact, ObservedOutcome, ProbeCache, ProbePublishOutcome, SkipReason},
    journal::{
        Journal, JournalHost, JournalMode, ProcessVerifier, SPECULATIVE_TIMEOUT, speculative_now,
    },
    plan::{
        DiagnosticStream, ProbeDiagnostic, ProbeEligibility, ProbeRecipe, ReuseScope,
        compute_probe_key, environment_fingerprint, request_digest,
    },
};

const MAX_QUEUE: usize = 64;
const MAX_QUEUE_BYTES: usize = 16 * 1024 * 1024;
const MAX_JOBS: usize = 32;
const MAX_RESULT_BYTES: u64 = 4 * 1024 * 1024;
const CANCEL_POLL: Duration = Duration::from_millis(50);
const GIB: u64 = 1024 * 1024 * 1024;
static NEXT_SCRATCH: AtomicU64 = AtomicU64::new(0);

pub(crate) fn physical_memory(host: &dyn Host) -> u64 {
    let system = host.system();
    if system.total_memory != 0 || system.sysname != "Darwin" {
        return system.total_memory;
    }
    let request = RunRequest {
        pipeline: vec![CommandSpec {
            program: "/usr/sbin/sysctl".into(),
            args: vec!["-n".into(), "hw.memsize".into()],
        }],
        cwd: host.initial_cwd(),
        env: Vec::new(),
        stdin: Input::Null,
        stdout: Output::Capture,
        stderr: Output::Null,
        timeout: Some(Duration::from_secs(1)),
        tee: false,
    };
    host.run_bounded(&request, 64)
        .ok()
        .filter(|output| output.results.as_slice() == [ProcessResult::Exited(0)])
        .and_then(|output| {
            std::str::from_utf8(output.stdout.trim_ascii())
                .ok()?
                .parse()
                .ok()
        })
        .unwrap_or_default()
}

/// Constraint that reduced the requested probe count.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum JobClamp {
    /// The host did not report physical memory.
    UnknownMemory,
    /// The host has insufficient memory for more concurrent jobs.
    Memory,
    /// The host has fewer logical processors than requested jobs.
    Cpu,
    /// The session reached the pool's maximum job count.
    PoolLimit,
    /// Cache publication was unavailable or a worker could not start.
    WorkerUnavailable,
}

/// Requested and effective probe counts, including the foreground slot.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct JobLimit {
    /// Requested total count after applying API, environment, and default precedence.
    pub(crate) requested: usize,
    /// Effective count including space for one foreground probe.
    pub(crate) effective: usize,
    /// Constraint that reduced the count, when applicable.
    pub(crate) clamp: Option<JobClamp>,
}

/// Reserve 2 GiB for configure and 1 GiB per possible probe.
pub(crate) fn job_limit(
    host: &dyn Host,
    total_memory: u64,
    probe_jobs: Option<NonZeroUsize>,
) -> JobLimit {
    let cpus = (host.system().cpu_count as usize).max(1);
    let requested = probe_jobs
        .or_else(|| {
            host.env_get(b"RCMAKE_PROBE_JOBS")
                .and_then(|value| std::str::from_utf8(&value).ok()?.parse().ok())
        })
        .map_or(cpus.min(4), NonZeroUsize::get);
    let memory = (total_memory / GIB)
        .saturating_sub(2)
        .clamp(1, MAX_JOBS as u64) as usize;
    let effective = requested.min(memory).min(cpus).min(MAX_JOBS);
    let clamp = if effective == requested {
        None
    } else if total_memory == 0 {
        Some(JobClamp::UnknownMemory)
    } else if effective == MAX_JOBS {
        Some(JobClamp::PoolLimit)
    } else if effective == memory {
        Some(JobClamp::Memory)
    } else {
        Some(JobClamp::Cpu)
    };
    JobLimit {
        requested,
        effective,
        clamp,
    }
}

struct Job {
    key: Digest,
    recipe: ProbeRecipe,
    environment: Vec<(BString, BString)>,
    deadline: Instant,
    bytes: usize,
    child: Option<ChildJob>,
}

pub(crate) struct ChildBudget {
    pub(crate) cancel: CancelToken,
    pub(crate) shutdown: CancelToken,
    pub(crate) deadline: Instant,
}

pub(crate) type ChildJob = Box<
    dyn FnOnce(
            &Path,
            &ProbeRecipe,
            Vec<(BString, BString)>,
            ChildBudget,
        ) -> HostResult<(ObservedOutcome, Journal)>
        + Send,
>;

#[derive(Default)]
struct Queue {
    pending: VecDeque<Job>,
    inflight: BTreeMap<Digest, Instant>,
    complete: BTreeMap<Digest, ChildResult>,
    bytes: usize,
}

pub(crate) struct ChildResult {
    pub(crate) recipe: ProbeRecipe,
    pub(crate) outcome: ObservedOutcome,
    pub(crate) journal: Journal,
}

#[derive(Default)]
struct Counters {
    rejected: AtomicU64,
    startup_failures: AtomicU64,
    failures: AtomicU64,
    cancelled: AtomicU64,
    published: AtomicU64,
    cached: AtomicU64,
}

struct Shared {
    state: Mutex<Queue>,
    changed: Condvar,
    cancel: CancelToken,
    shutdown: CancelToken,
    counters: Counters,
}

impl Shared {
    fn state(&self) -> MutexGuard<'_, Queue> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn cancelled(&self) -> bool {
        self.cancel.is_cancelled() || self.shutdown.is_cancelled()
    }

    fn wait<'state>(
        &self,
        state: MutexGuard<'state, Queue>,
        timeout: Duration,
    ) -> MutexGuard<'state, Queue> {
        self.changed
            .wait_timeout(state, timeout.min(CANCEL_POLL))
            .unwrap_or_else(PoisonError::into_inner)
            .0
    }

    fn next(&self) -> Option<Job> {
        let mut state = self.state();
        while !self.cancelled() {
            if let Some(job) = state.pending.pop_front() {
                state.bytes -= job.bytes;
                return Some(job);
            }
            state = self.wait(state, CANCEL_POLL);
        }
        None
    }
}

/// Inputs shared by speculative workers.
pub(crate) struct WorkerContext<'a> {
    /// Host that owns the scratch and store files.
    pub(crate) host: &'a dyn Host,
    /// Store configuration. The workers use the same host to open the store.
    pub(crate) store: &'a StoreConfig,
    /// Cache policy and foreground counters.
    pub(crate) cache: &'a ProbeCache,
    /// Existing private parent directory for speculative scratch trees.
    pub(crate) scratch_root: &'a Path,
    /// Verifier for the complete compiler or linker process tree.
    pub(crate) process_verifier: Option<&'a dyn ProcessVerifier>,
}

/// Counts for speculative work. These do not change foreground cache counters.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[cfg(test)]
pub(crate) struct WorkerStats {
    /// Jobs rejected by admission, deduplication, or queue limits.
    pub(crate) rejected: u64,
    /// Threads that could not start.
    pub(crate) startup_failures: u64,
    /// Jobs that encountered an error, panicked, or could not publish.
    pub(crate) failures: u64,
    /// Jobs cancelled or expired before completion.
    pub(crate) cancelled: u64,
    /// Jobs with a result that passed publication.
    pub(crate) published: u64,
    /// Jobs skipped because a valid cache entry was already available.
    pub(crate) cached: u64,
}

/// Scoped queue with one queued or running job per key.
pub(crate) struct ProbePool {
    shared: Arc<Shared>,
    workers: usize,
    limit: JobLimit,
}

/// Queue handle that does not own the lifetime of the pool.
#[derive(Clone)]
pub(crate) struct ProbeQueue {
    shared: Arc<Shared>,
    available: bool,
}

impl ProbePool {
    /// Start workers for the scope of `use_pool`.
    ///
    /// A total limit of one leaves the foreground probe as the only job.
    /// Supply the session cancellation token when one is available.
    pub(crate) fn scoped<R>(
        context: &WorkerContext<'_>,
        total_memory: u64,
        probe_jobs: Option<NonZeroUsize>,
        cancel: CancelToken,
        use_pool: impl FnOnce(&Self) -> R,
    ) -> R {
        let limit = job_limit(context.host, total_memory, probe_jobs);
        thread::scope(|scope| {
            let shared = Arc::new(Shared {
                state: Mutex::default(),
                changed: Condvar::new(),
                cancel,
                shutdown: CancelToken::default(),
                counters: Counters::default(),
            });
            let mut pool = Self {
                shared,
                workers: 0,
                limit,
            };
            for _ in 1..limit.effective {
                let shared = Arc::clone(&pool.shared);
                if thread::Builder::new()
                    .name("rcmake-probe".to_owned())
                    .spawn_scoped(scope, move || worker(context, &shared))
                    .is_err()
                {
                    pool.shared
                        .counters
                        .startup_failures
                        .fetch_add(1, Ordering::Relaxed);
                    break;
                }
                pool.workers += 1;
            }
            if pool.workers + 1 < pool.limit.effective {
                pool.limit.effective = pool.workers + 1;
                pool.limit.clamp = Some(JobClamp::WorkerUnavailable);
            }
            use_pool(&pool)
        })
    }

    /// Return the requested limit and its effective value.
    #[cfg(test)]
    pub(crate) fn limit(&self) -> JobLimit {
        self.limit
    }

    /// Read the counts for this pool.
    #[cfg(test)]
    pub(crate) fn stats(&self) -> WorkerStats {
        let counters = &self.shared.counters;
        WorkerStats {
            rejected: counters.rejected.load(Ordering::Relaxed),
            startup_failures: counters.startup_failures.load(Ordering::Relaxed),
            failures: counters.failures.load(Ordering::Relaxed),
            cancelled: counters.cancelled.load(Ordering::Relaxed),
            published: counters.published.load(Ordering::Relaxed),
            cached: counters.cached.load(Ordering::Relaxed),
        }
    }

    pub(crate) fn queue(&self) -> ProbeQueue {
        ProbeQueue {
            shared: Arc::clone(&self.shared),
            available: self.workers != 0,
        }
    }

    /// Discard pending jobs and signal the active jobs without cancelling the session.
    pub(crate) fn cancel(&self) {
        let mut state = self.shared.state();
        self.shared.shutdown.cancel();
        let pending = std::mem::take(&mut state.pending);
        state.bytes = 0;
        state.complete.clear();
        for job in &pending {
            state.inflight.remove(&job.key);
        }
        self.shared
            .counters
            .cancelled
            .fetch_add(pending.len() as u64, Ordering::Relaxed);
        self.shared.changed.notify_all();
        drop(state);
        drop(pending);
    }
}

impl ProbeQueue {
    pub(crate) fn available(&self) -> bool {
        self.available && !self.shared.cancelled()
    }

    /// Queue an owned execution plan and a snapshot of its complete child environment.
    ///
    /// Use the same plan to compute the key when the original call consumes
    /// the result. The worker only executes source writes and the declared process.
    pub(crate) fn submit(
        &self,
        recipe: ProbeRecipe,
        environment: Vec<(BString, BString)>,
        child: Option<ChildJob>,
        child_bytes: usize,
    ) -> bool {
        let reject = || {
            self.shared
                .counters
                .rejected
                .fetch_add(1, Ordering::Relaxed);
            false
        };
        if !self.available()
            || recipe.eligibility != ProbeEligibility::Eligible
            || recipe.scope != ReuseScope::VerifiedRelocation
            || recipe.try_run
            || (recipe.speculative_run.is_none() && child.is_none())
        {
            return reject();
        }
        let bytes = environment.iter().fold(
            recipe.queued_bytes().saturating_add(child_bytes),
            |bytes, (key, value)| {
                bytes
                    .saturating_add(64)
                    .saturating_add(key.len())
                    .saturating_add(value.len())
            },
        );
        if bytes > MAX_QUEUE_BYTES || recipe.environment != environment_fingerprint(&environment) {
            return reject();
        }
        let key = compute_probe_key(&recipe);
        let mut state = self.shared.state();
        if self.shared.cancelled()
            || state.pending.len() >= MAX_QUEUE
            || state.bytes.saturating_add(bytes) > MAX_QUEUE_BYTES
            || state.inflight.contains_key(&key)
            || state.complete.contains_key(&key)
        {
            return reject();
        }
        let deadline = speculative_now() + SPECULATIVE_TIMEOUT;
        state.inflight.insert(key, deadline);
        state.bytes += bytes;
        state.pending.push_back(Job {
            key,
            recipe,
            environment,
            deadline,
            bytes,
            child,
        });
        self.shared.changed.notify_all();
        true
    }

    /// Wait until the key completes, the job deadline expires, or the session stops.
    ///
    /// Use ordinary cache lookup afterward. Run the foreground probe on a miss.
    pub(crate) fn wait_for(&self, key: Digest) {
        let mut state = self.shared.state();
        while let Some(deadline) = state.inflight.get(&key) {
            let remaining = deadline.saturating_duration_since(speculative_now());
            if remaining.is_zero() || self.shared.cancelled() {
                break;
            }
            state = self.shared.wait(state, remaining);
        }
    }

    pub(crate) fn take(&self, key: Digest) -> Option<ChildResult> {
        self.wait_for(key);
        self.shared.state().complete.remove(&key)
    }
}

impl Drop for ProbePool {
    fn drop(&mut self) {
        self.cancel();
    }
}

struct Complete<'a> {
    shared: &'a Shared,
    key: Digest,
}

impl Drop for Complete<'_> {
    fn drop(&mut self) {
        self.shared.state().inflight.remove(&self.key);
        self.shared.changed.notify_all();
    }
}

struct Scratch<'a> {
    host: &'a dyn Host,
    path: PathBuf,
    claim: PathBuf,
    lock: Option<Box<dyn FileLockGuard>>,
}

impl<'a> Scratch<'a> {
    fn create(host: &'a dyn Host, root: &Path) -> HostResult<Self> {
        if !root.is_absolute()
            || root
                .components()
                .any(|part| part == std::path::Component::ParentDir)
            || host.metadata(root, Follow::No)?.kind != FileKind::Dir
        {
            return Err(HostError::unsupported(
                "Expected a private speculative scratch root.",
            ));
        }
        let id = NEXT_SCRATCH.fetch_add(1, Ordering::Relaxed);
        let nonce = RandomState::new().hash_one((host.system().pid, id));
        let path = root.join(format!("Spec-{nonce:016x}-{id:x}"));
        let claim = path.with_extension("claim");
        host.write(
            &claim,
            b"rcmake speculative scratch v1\n",
            WriteMode::CreateNew,
        )?;
        let lock = match host.lock_file(&claim, Some(Duration::ZERO)) {
            Ok(lock) => lock,
            Err(error) => {
                let _ = host.remove_file(&claim);
                return Err(error);
            }
        };
        if !host
            .metadata(&path, Follow::No)
            .is_err_and(|error| error.kind == std::io::ErrorKind::NotFound)
        {
            let _ = host.remove_file(&claim);
            return Err(HostError::unsupported(
                "Speculative scratch path is already in use.",
            ));
        }
        let scratch = Self {
            host,
            path,
            claim,
            lock: Some(lock),
        };
        host.create_dir_all(&scratch.path)?;
        Ok(scratch)
    }
}

impl Drop for Scratch<'_> {
    fn drop(&mut self) {
        let _ = self.host.remove_dir_all(&self.path);
        drop(self.lock.take());
        let _ = self.host.remove_file(&self.claim);
    }
}

pub(super) fn reap_scratch(host: &dyn Host, root: &Path) {
    if !host
        .metadata(root, Follow::No)
        .is_ok_and(|entry| entry.kind == FileKind::Dir)
    {
        return;
    }
    let Ok(entries) = host.read_dir(root) else {
        return;
    };
    for entry in entries.into_iter().take(20_000) {
        let Some(name) = entry.name.strip_prefix(b"Spec-") else {
            continue;
        };
        let dir_name = name.strip_suffix(b".claim").unwrap_or(name);
        let mut parts = dir_name.split(|&byte| byte == b'-');
        if !matches!((parts.next(), parts.next(), parts.next()), (Some(nonce), Some(serial), None)
            if nonce.len() == 16 && !serial.is_empty()
                && nonce.iter().chain(serial).all(u8::is_ascii_hexdigit))
        {
            continue;
        }
        let name = entry.name.strip_suffix(b".claim").unwrap_or(&entry.name);
        let dir = root.join(path::to_native(name));
        let claim = dir.with_extension("claim");
        match host.metadata(&claim, Follow::No) {
            Ok(meta) if meta.kind == FileKind::File => {
                let age = host.now().secs.saturating_sub(meta.mtime.secs);
                if age < 2 * SPECULATIVE_TIMEOUT.as_secs() as i64 {
                    continue;
                }
            }
            Err(error) if error.kind == std::io::ErrorKind::NotFound => {}
            _ => continue,
        }
        let Ok(lock) = host.lock_file(&claim, Some(Duration::ZERO)) else {
            continue;
        };
        if host
            .metadata(&dir, Follow::No)
            .is_ok_and(|meta| meta.kind == FileKind::Dir)
        {
            let _ = host.remove_dir_all(&dir);
        }
        drop(lock);
        let _ = host.remove_file(&claim);
    }
}

enum JobResult {
    Cached,
    Cancelled,
    Published,
    Ready,
    Failed,
}

fn worker(context: &WorkerContext<'_>, shared: &Shared) {
    while let Some(job) = shared.next() {
        let _complete = Complete {
            shared,
            key: job.key,
        };
        let result = catch_unwind(AssertUnwindSafe(|| execute(context, job, shared)));
        let counter = match result {
            Ok(Ok(JobResult::Cached | JobResult::Ready)) => &shared.counters.cached,
            Ok(Ok(JobResult::Cancelled)) => &shared.counters.cancelled,
            Ok(Ok(JobResult::Published)) => &shared.counters.published,
            Ok(Err(_)) if shared.cancelled() => &shared.counters.cancelled,
            _ => &shared.counters.failures,
        };
        counter.fetch_add(1, Ordering::Relaxed);
    }
}

fn execute(context: &WorkerContext<'_>, job: Job, shared: &Shared) -> HostResult<JobResult> {
    let Job {
        key,
        mut recipe,
        environment,
        deadline,
        child,
        ..
    } = job;
    let cancelled = || shared.cancelled() || speculative_now() >= deadline;
    if cancelled() {
        return Ok(JobResult::Cancelled);
    }
    if context
        .cache
        .lookup_uncounted(context.host, context.store, &recipe)
        .is_some()
    {
        return Ok(JobResult::Cached);
    }
    if child.is_none() && !context.cache.writable() {
        return Ok(JobResult::Failed);
    }
    let scratch = Scratch::create(context.host, context.scratch_root)?;
    recipe.bindings = recipe
        .bindings
        .clone()
        .with_scratch(&scratch.path)
        .ok_or_else(|| HostError::unsupported("Cannot bind speculative scratch."))?
        .with_canonical(context.host);
    if let Some(child) = child {
        let (outcome, journal) = child(
            &scratch.path,
            &recipe,
            environment,
            ChildBudget {
                cancel: shared.cancel.clone(),
                shutdown: shared.shutdown.clone(),
                deadline,
            },
        )?;
        if cancelled() {
            return Ok(JobResult::Cancelled);
        }
        let mut queue = shared.state();
        if queue.complete.len() >= MAX_JOBS || shared.cancelled() {
            return Ok(JobResult::Cancelled);
        }
        queue.complete.insert(
            key,
            ChildResult {
                recipe,
                outcome,
                journal,
            },
        );
        return Ok(JobResult::Ready);
    }
    let (outcome, journal) = execute_process_plan(
        context,
        shared,
        &scratch.path,
        &recipe,
        environment,
        deadline,
    )?;
    if cancelled() {
        return Ok(JobResult::Cancelled);
    }
    let status = context.cache.publish_uncounted(
        context.host,
        context.store,
        &recipe,
        &outcome,
        &journal,
        cancelled,
    );
    Ok(match status {
        ProbePublishOutcome::Published | ProbePublishOutcome::AlreadyPresent => {
            JobResult::Published
        }
        ProbePublishOutcome::Skipped(SkipReason::Cancelled) => JobResult::Cancelled,
        ProbePublishOutcome::Skipped(_) | ProbePublishOutcome::StoreError(_) => JobResult::Failed,
    })
}

fn execute_process_plan(
    context: &WorkerContext<'_>,
    shared: &Shared,
    scratch: &Path,
    recipe: &ProbeRecipe,
    environment: Vec<(BString, BString)>,
    deadline: Instant,
) -> HostResult<(ObservedOutcome, Journal)> {
    let journal = JournalHost::new(
        context.host,
        scratch,
        JournalMode::Speculate,
        context.process_verifier,
    )?
    .with_speculation(
        environment.clone(),
        shared.cancel.clone(),
        shared.shutdown.clone(),
        deadline,
    );
    let remaining = deadline.saturating_duration_since(speculative_now());
    let plan = prepare(recipe, &environment, scratch, remaining)?;
    let mut outcome = run_plan(&journal, scratch, recipe, &plan)?;
    let journal = journal.finish();
    outcome.diagnostics.extend(
        journal
            .output()
            .iter()
            .map(|(stream, message)| ProbeDiagnostic {
                stream: match stream {
                    rcmake_host::Stream::Stdout => DiagnosticStream::Stdout,
                    rcmake_host::Stream::Stderr => DiagnosticStream::Stderr,
                },
                message: message.clone(),
            }),
    );
    let [executed] = journal.processes() else {
        return Err(HostError::unsupported(
            "The process did not match the planned probe.",
        ));
    };
    // JournalHost can reduce the timeout to account for setup time.
    let mut expected = plan;
    if executed.timeout.is_none() || executed.timeout > expected.timeout {
        return Err(HostError::unsupported(
            "The process did not match the planned probe.",
        ));
    }
    expected.timeout = executed.timeout;
    if environment_fingerprint(&executed.env) != recipe.environment
        || request_digest(executed) != request_digest(&expected)
    {
        return Err(HostError::unsupported(
            "The process did not match the planned probe.",
        ));
    }
    Ok((outcome, journal))
}

fn prepare(
    recipe: &ProbeRecipe,
    environment: &[(BString, BString)],
    scratch: &Path,
    remaining: Duration,
) -> HostResult<RunRequest> {
    let invalid = || HostError::unsupported("Invalid speculative process plan.");
    let plan = recipe.speculative_run.as_ref().ok_or_else(invalid)?;
    let mut bytes_left = MAX_QUEUE_BYTES;
    let mut args = Vec::with_capacity(plan.args.len());
    for arg in &plan.args {
        let bytes = arg
            .render_bounded(&recipe.bindings, bytes_left)
            .ok_or_else(invalid)?;
        bytes_left -= bytes.len();
        args.push(BString::from(bytes));
    }
    Ok(RunRequest {
        pipeline: vec![CommandSpec {
            program: plan.program.clone(),
            args,
        }],
        cwd: scratch.to_owned(),
        env: environment.to_vec(),
        stdin: Input::Null,
        stdout: Output::Capture,
        stderr: Output::SharedWithStdout,
        timeout: Some(remaining.min(plan.timeout)),
        tee: false,
    })
}

fn run_plan(
    host: &dyn Host,
    scratch: &Path,
    recipe: &ProbeRecipe,
    plan: &RunRequest,
) -> HostResult<ObservedOutcome> {
    let invalid = || HostError::unsupported("Invalid speculative probe file.");
    let run = recipe.speculative_run.as_ref().ok_or_else(invalid)?;
    let mut bytes_left = MAX_QUEUE_BYTES;
    for (name, contents) in &recipe.files {
        let path = checked_relative(name)?;
        let file = scratch.join(path);
        if let Some(parent) = file.parent() {
            host.create_dir_all(parent)?;
        }
        let bytes = contents
            .render_bounded(&recipe.bindings, bytes_left)
            .ok_or_else(invalid)?;
        bytes_left -= bytes.len();
        host.write(&file, &bytes, WriteMode::CreateNew)?;
    }
    let mut output = host.run_bounded(plan, MAX_RESULT_BYTES as usize)?;
    let [ProcessResult::Exited(exit_code)] = output.results.as_slice() else {
        return Err(HostError::unsupported(
            "Speculative process did not complete.",
        ));
    };
    let artifact = if *exit_code == 0 {
        run.artifact
            .as_ref()
            .map(|template| {
                let bytes = template.render(&recipe.bindings).ok_or_else(invalid)?;
                let relative_path = checked_relative(&bytes)?;
                let file = scratch.join(&relative_path);
                let canonical = host.canonicalize(&file)?;
                if !canonical.starts_with(host.canonicalize(scratch)?) {
                    return Err(invalid());
                }
                let metadata = host.metadata(&file, Follow::No)?;
                if metadata.kind != FileKind::File || metadata.nlink != 1 {
                    return Err(invalid());
                }
                let bytes = host.read(
                    &file,
                    ReadRange::All {
                        limit: MAX_RESULT_BYTES,
                    },
                )?;
                Ok(Artifact {
                    bytes,
                    relative_path,
                    mode: metadata.mode,
                })
            })
            .transpose()?
    } else {
        None
    };
    output.stdout.extend_from_slice(&output.stderr);
    Ok(ObservedOutcome {
        max_start_depth: u32::MAX,
        compiled: *exit_code == 0,
        exit_code: *exit_code,
        output: output.stdout,
        artifact,
        diagnostics: Vec::new(),
    })
}

fn checked_relative(bytes: &[u8]) -> HostResult<PathBuf> {
    let file = path::to_native(bytes);
    if !super::try_compile::cache::is_relative_artifact(&file)
        || path::from_native(&file).as_slice() != bytes
    {
        return Err(HostError::unsupported("Invalid speculative probe path."));
    }
    Ok(file)
}

#[cfg(test)]
mod tests;
