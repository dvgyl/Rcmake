//! Run private speculative probes and publish only through the probe cache.

#![forbid(unsafe_code)]

use std::collections::{BTreeSet, VecDeque};
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread;

use rcmake_host::{FileKind, Follow, Host, HostError, HostResult};
use rcmake_store::{Digest, Store};

use super::try_compile::{
    cache::{ObservedOutcome, ProbeCache},
    journal::{JournalHost, JournalMode, ProcessVerifier},
    plan::{ProbeEligibility, ProbeRecipe, ReuseScope, compute_probe_key},
};

const MAX_QUEUE: usize = 64;
const MAX_WORKERS: usize = 32;
const GIB: u64 = 1024 * 1024 * 1024;
static NEXT_SCRATCH: AtomicU64 = AtomicU64::new(0);

/// Select the worker limit from the host and the known memory budget.
///
/// Reserve 2 GiB for other work and 1 GiB for each worker. If the host has no
/// memory estimate, cap the limit at four. A limit of one disables speculation.
pub(crate) fn job_limit(host: &dyn Host, total_memory: Option<u64>) -> usize {
    let cpus = (host.system().cpu_count as usize).max(1);
    let memory = total_memory.map_or(4, |bytes| {
        let slots = (bytes / GIB).saturating_sub(2);
        slots.clamp(1, MAX_WORKERS as u64) as usize
    });
    let requested = host
        .env_get(b"RCMAKE_PROBE_JOBS")
        .and_then(|value| std::str::from_utf8(&value).ok()?.parse::<usize>().ok())
        .filter(|&jobs| jobs > 0)
        .unwrap_or(cpus.min(4));
    requested.min(memory).min(cpus)
}

/// Cooperative cancellation for a running speculative job.
#[derive(Default)]
pub(crate) struct Cancellation(AtomicBool);

impl Cancellation {
    /// Return true when the job must stop before its next operation.
    pub(crate) fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }
}

type Execute<'a> = dyn FnOnce(&JournalHost<'_>, &ProbeRecipe, &Path, &Cancellation) -> HostResult<ObservedOutcome>
    + Send
    + 'a;

struct Job<'a> {
    key: Digest,
    recipe: ProbeRecipe,
    execute: Box<Execute<'a>>,
}

#[derive(Default)]
struct Queue<'a> {
    pending: VecDeque<Job<'a>>,
    inflight: BTreeSet<Digest>,
}

struct Shared<'a> {
    state: Mutex<Queue<'a>>,
    changed: Condvar,
    cancellation: Cancellation,
}

impl<'a> Shared<'a> {
    fn state(&self) -> MutexGuard<'_, Queue<'a>> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn wait<'state>(&self, state: MutexGuard<'state, Queue<'a>>) -> MutexGuard<'state, Queue<'a>> {
        self.changed
            .wait(state)
            .unwrap_or_else(PoisonError::into_inner)
    }

    fn next(&self) -> Option<Job<'a>> {
        let mut state = self.state();
        while !self.cancellation.is_cancelled() {
            if let Some(job) = state.pending.pop_front() {
                return Some(job);
            }
            state = self.wait(state);
        }
        None
    }
}

/// Inputs shared by speculative workers.
///
/// Keep the scratch root private for the full scope. It must already exist.
/// Use the same host snapshot for execution and validation. The process verifier
/// supplies proof for external processes. It can be absent for host-only jobs.
pub(crate) struct WorkerContext<'a> {
    /// Host that owns the scratch and store files.
    pub(crate) host: &'a dyn Host,
    /// Cache store with write access.
    pub(crate) store: &'a Store<'a>,
    /// Cache policy and counters shared with the evaluator.
    pub(crate) cache: &'a ProbeCache,
    /// Private parent directory for speculative scratch trees.
    pub(crate) scratch_root: &'a Path,
    /// Optional proof for deterministic child processes.
    pub(crate) process_verifier: Option<&'a ProcessVerifier<'a>>,
}

/// Scoped queue of speculative probes with one queued or running job per key.
pub(crate) struct ProbePool<'a> {
    shared: Arc<Shared<'a>>,
    workers: usize,
}

impl<'a> ProbePool<'a> {
    /// Start workers for the scope of `use_pool`.
    ///
    /// A limit of one disables speculation. A failed thread start reduces the
    /// worker count. At scope exit, discard queued work and wait for active jobs.
    pub(crate) fn scoped<R>(
        context: &WorkerContext<'a>,
        total_memory: Option<u64>,
        use_pool: impl FnOnce(&Self) -> R,
    ) -> R {
        let max_jobs = job_limit(context.host, total_memory);
        thread::scope(|scope| {
            let shared = Arc::new(Shared {
                state: Mutex::default(),
                changed: Condvar::new(),
                cancellation: Cancellation::default(),
            });
            let mut pool = Self { shared, workers: 0 };
            if max_jobs > 1 && context.cache.writable() {
                for _ in 0..max_jobs {
                    let shared = Arc::clone(&pool.shared);
                    if thread::Builder::new()
                        .name("rcmake-probe".to_owned())
                        .spawn_scoped(scope, move || worker(context, &shared))
                        .is_err()
                    {
                        break;
                    }
                    pool.workers += 1;
                }
            }
            use_pool(&pool)
        })
    }

    /// Queue one verified relocation candidate without waiting.
    ///
    /// Call only for a top-level probe with a verified private execution
    /// contract. The callback uses the supplied journal for all host work.
    /// It must check cancellation between bounded operations.
    pub(crate) fn submit(
        &self,
        recipe: ProbeRecipe,
        execute: impl FnOnce(
            &JournalHost<'_>,
            &ProbeRecipe,
            &Path,
            &Cancellation,
        ) -> HostResult<ObservedOutcome>
        + Send
        + 'a,
    ) -> bool {
        if self.workers == 0
            || recipe.eligibility != ProbeEligibility::Eligible
            || recipe.scope != ReuseScope::VerifiedRelocation
        {
            return false;
        }
        let key = compute_probe_key(&recipe);
        let mut state = self.shared.state();
        if self.shared.cancellation.is_cancelled()
            || state.pending.len() >= MAX_QUEUE
            || !state.inflight.insert(key)
        {
            return false;
        }
        state.pending.push_back(Job {
            key,
            recipe,
            execute: Box::new(execute),
        });
        self.shared.changed.notify_all();
        true
    }

    /// Wait for a queued or running key. Then use the ordinary cache lookup.
    pub(crate) fn wait_for(&self, key: Digest) {
        let mut state = self.shared.state();
        while state.inflight.contains(&key) && !self.shared.cancellation.is_cancelled() {
            state = self.shared.wait(state);
        }
    }

    /// Discard queued jobs and request cancellation of active jobs.
    pub(crate) fn cancel(&self) {
        let mut state = self.shared.state();
        self.shared.cancellation.0.store(true, Ordering::Release);
        let pending = std::mem::take(&mut state.pending);
        for job in &pending {
            state.inflight.remove(&job.key);
        }
        self.shared.changed.notify_all();
        drop(state);
        drop(pending);
    }
}

impl Drop for ProbePool<'_> {
    fn drop(&mut self) {
        self.cancel();
    }
}

struct Complete<'a, 'job> {
    shared: &'a Shared<'job>,
    key: Digest,
}

impl Drop for Complete<'_, '_> {
    fn drop(&mut self) {
        self.shared.state().inflight.remove(&self.key);
        self.shared.changed.notify_all();
    }
}

struct Scratch<'a> {
    host: &'a dyn Host,
    path: PathBuf,
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
        let pid = host.system().pid;
        let id = NEXT_SCRATCH.fetch_add(1, Ordering::Relaxed);
        let path = root.join(format!("Spec-{pid}-{id}"));
        if !host
            .metadata(&path, Follow::No)
            .is_err_and(|error| error.kind == ErrorKind::NotFound)
        {
            return Err(HostError::unsupported(
                "Speculative scratch path is already in use.",
            ));
        }
        host.create_dir_all(&path)?;
        Ok(Self { host, path })
    }
}

impl Drop for Scratch<'_> {
    fn drop(&mut self) {
        let _ = self.host.remove_dir_all(&self.path);
    }
}

fn worker(context: &WorkerContext<'_>, shared: &Shared<'_>) {
    while let Some(job) = shared.next() {
        let _complete = Complete {
            shared,
            key: job.key,
        };
        if !shared.cancellation.is_cancelled() {
            let _ = execute(context, job, &shared.cancellation);
        }
    }
}

fn execute(
    context: &WorkerContext<'_>,
    job: Job<'_>,
    cancellation: &Cancellation,
) -> HostResult<()> {
    let Job {
        mut recipe,
        execute,
        ..
    } = job;
    if context
        .cache
        .lookup(context.host, context.store, &recipe)
        .is_some()
        || cancellation.is_cancelled()
    {
        return Ok(());
    }
    let scratch = Scratch::create(context.host, context.scratch_root)?;
    recipe.bindings = recipe
        .bindings
        .with_scratch(&scratch.path)
        .ok_or_else(|| HostError::unsupported("Cannot bind speculative scratch."))?;
    let journal = JournalHost::new(
        context.host,
        &scratch.path,
        JournalMode::Speculate,
        context.process_verifier,
    )?;
    let outcome = execute(&journal, &recipe, &scratch.path, cancellation)?;
    if !cancellation.is_cancelled() {
        context.cache.publish(
            context.host,
            context.store,
            &recipe,
            &outcome,
            &journal.finish(),
        )?;
    }
    Ok(())
}

#[cfg(all(test, unix))]
mod tests {
    use std::sync::atomic::AtomicUsize;
    use std::sync::mpsc;
    use std::time::Duration;

    use rcmake_host::PureHost;
    use rcmake_store::{StoreConfig, StoreMode};

    use super::super::try_compile::cache::tests::{recipe, success};
    use super::*;

    #[test]
    fn bounds_active_workers_and_deduplicates_queued_and_running_keys() -> HostResult<()> {
        let host = PureHost::default();
        let root = Path::new("/work/spec");
        host.create_dir_all(root)?;
        let store = Store::new(&host, StoreConfig::new("/work/cache".into()))?;
        let cache = ProbeCache::new(StoreMode::ReadWrite);
        let context = WorkerContext {
            host: &host,
            store: &store,
            cache: &cache,
            scratch_root: root,
            process_verifier: None,
        };
        let first = recipe(&host, Path::new("/first"))?;
        let mut second = first.clone();
        second.flags.push("-DSECOND".into());
        let mut third = first.clone();
        third.flags.push("-DTHIRD".into());
        let peak = AtomicUsize::new(0);
        let active = AtomicUsize::new(0);
        let (started, jobs) = mpsc::channel();
        let (release, released) = mpsc::channel();
        let released = Mutex::new(released);

        ProbePool::scoped(&context, Some(4 * GIB), |pool| -> HostResult<()> {
            for recipe in [&first, &second, &third] {
                let started = &started;
                let released = &released;
                let active = &active;
                let peak = &peak;
                assert!(
                    pool.submit(recipe.clone(), move |_, _, scratch, cancellation| {
                        let count = active.fetch_add(1, Ordering::AcqRel) + 1;
                        peak.fetch_max(count, Ordering::AcqRel);
                        let _ = started.send(());
                        let completed = released
                            .lock()
                            .is_ok_and(|rx| rx.recv_timeout(Duration::from_secs(2)).is_ok());
                        active.fetch_sub(1, Ordering::AcqRel);
                        if !completed || cancellation.is_cancelled() {
                            return Err(HostError::unsupported("The test job was cancelled."));
                        }
                        let mut outcome = success();
                        outcome.artifact = None;
                        outcome.output =
                            rcmake_host::path::from_native_cmake(&scratch.join("cmTC_1")).to_vec();
                        Ok(outcome)
                    })
                );
            }
            for _ in 0..2 {
                jobs.recv_timeout(Duration::from_secs(2))
                    .map_err(|error| HostError::unsupported(error.to_string()))?;
            }
            assert!(!pool.submit(first.clone(), |_, _, _, _| Ok(success())));
            assert!(!pool.submit(third.clone(), |_, _, _, _| Ok(success())));
            assert_eq!(active.load(Ordering::Acquire), 2);
            for _ in 0..3 {
                release
                    .send(())
                    .map_err(|error| HostError::unsupported(error.to_string()))?;
            }
            for recipe in [&first, &second, &third] {
                pool.wait_for(compute_probe_key(recipe));
                let hit = cache
                    .lookup(&host, &store, recipe)
                    .ok_or_else(|| HostError::unsupported("Expected a speculative cache hit."))?;
                assert_eq!(hit.output, b"/first/scratch/cmTC_1");
            }
            Ok(())
        })?;
        assert_eq!(peak.load(Ordering::Acquire), 2);
        assert!(host.read_dir(root)?.is_empty());
        Ok(())
    }

    #[test]
    fn cancels_pending_jobs_and_discards_running_results() -> HostResult<()> {
        let host = PureHost::default();
        let root = Path::new("/work/spec");
        host.create_dir_all(root)?;
        let store = Store::new(&host, StoreConfig::new("/work/cache".into()))?;
        let cache = ProbeCache::new(StoreMode::ReadWrite);
        let context = WorkerContext {
            host: &host,
            store: &store,
            cache: &cache,
            scratch_root: root,
            process_verifier: None,
        };
        let first = recipe(&host, Path::new("/work"))?;
        let mut second = first.clone();
        second.flags.push("-DSECOND".into());
        let mut third = first.clone();
        third.flags.push("-DTHIRD".into());
        let (started, jobs) = mpsc::channel();
        let (release, released) = mpsc::channel();
        let released = Mutex::new(released);
        let ran = AtomicBool::new(false);

        ProbePool::scoped(&context, Some(4 * GIB), |pool| -> HostResult<()> {
            for recipe in [&first, &second] {
                let started = &started;
                let released = &released;
                assert!(pool.submit(recipe.clone(), move |_, _, _, cancellation| {
                    let _ = started.send(());
                    let released = released
                        .lock()
                        .is_ok_and(|rx| rx.recv_timeout(Duration::from_secs(2)).is_ok());
                    assert!(released && cancellation.is_cancelled());
                    Ok(success())
                }));
            }
            for _ in 0..2 {
                jobs.recv_timeout(Duration::from_secs(2))
                    .map_err(|error| HostError::unsupported(error.to_string()))?;
            }
            assert!(pool.submit(third.clone(), |_, _, _, _| {
                ran.store(true, Ordering::Release);
                Ok(success())
            }));
            pool.cancel();
            assert!(!pool.submit(first.clone(), |_, _, _, _| Ok(success())));
            for _ in 0..2 {
                release
                    .send(())
                    .map_err(|error| HostError::unsupported(error.to_string()))?;
            }
            Ok(())
        })?;
        assert!(!ran.load(Ordering::Acquire));
        assert!(cache.lookup(&host, &store, &first).is_none());
        assert!(cache.lookup(&host, &store, &second).is_none());
        assert!(cache.lookup(&host, &store, &third).is_none());
        assert!(host.read_dir(root)?.is_empty());
        Ok(())
    }

    #[test]
    fn limits_requested_workers_by_the_host_budget() {
        let host = PureHost::default();
        assert_eq!(job_limit(&host, None), 4);
        host.env_set(b"RCMAKE_PROBE_JOBS", Some(b"100"));
        assert_eq!(job_limit(&host, Some(4 * GIB)), 2);
        host.env_set(b"RCMAKE_PROBE_JOBS", Some(b"1"));
        assert_eq!(job_limit(&host, Some(16 * GIB)), 1);
    }
}
