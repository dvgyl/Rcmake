use std::{
    collections::{HashMap, VecDeque},
    sync::{Arc, Condvar, Mutex},
    thread::{self, JoinHandle},
    time::Duration,
};

use rcmake_host::Host;
use rcmake_store::{Store, StoreConfig};

use crate::{ArchiveCache, ContentHash, FetchError, GitMirror, git::is_commit, invalid};

const MAX_WORKERS: usize = 32;
const MAX_FLIGHTS: usize = 4096;
const ARCHIVE_BYTES: usize = 32 * 1024 * 1024;
const IN_FLIGHT_ARCHIVE_BYTES: usize = 2 * ARCHIVE_BYTES;
const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
const MAX_REQUEST_TIMEOUT: Duration = Duration::from_secs(600);
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(1);

/// An immutable source eligible for background prefetch.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum PrefetchSource {
    /// An archive identified by its expected hash.
    Url {
        /// Download URL.
        url: String,
        /// Expected archive hash.
        hash: ContentHash,
    },
    /// A Git source identified by its full commit ID.
    Git {
        /// Repository URL.
        repository: String,
        /// Full commit ID.
        commit: String,
    },
}

type PrefetchResult = Result<Option<FetchError>, FetchError>;

#[derive(Default)]
struct Flight {
    result: Mutex<Option<PrefetchResult>>,
    ready: Condvar,
}

impl Flight {
    fn finish(&self, result: PrefetchResult) {
        let mut slot = self
            .result
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if slot.is_none() {
            *slot = Some(result);
        }
        self.ready.notify_all();
    }
}

/// A wait handle shared by duplicate prefetch requests.
#[derive(Clone)]
pub struct Ticket(Arc<Flight>);

impl Ticket {
    /// Wait for confirmation that the cache contains the pin.
    pub fn wait(&self) -> Result<(), FetchError> {
        self.wait_with_warning()?.map_or(Ok(()), Err)
    }

    /// Wait for the fetch to finish.
    ///
    /// A cache warning means that the fetch could not confirm offline availability.
    pub fn wait_with_warning(&self) -> Result<Option<FetchError>, FetchError> {
        let result = self
            .0
            .result
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        self.0
            .ready
            .wait_while(result, |result| result.is_none())
            .unwrap_or_else(|error| error.into_inner())
            .clone()
            .ok_or_else(|| invalid("Prefetch finished without a result."))?
    }
}

#[derive(Default)]
struct Queue {
    closed: bool,
    jobs: VecDeque<(PrefetchSource, Arc<Flight>)>,
    flights: HashMap<PrefetchSource, Arc<Flight>>,
    active: Vec<Arc<Flight>>,
    archive_bytes: usize,
}

impl Queue {
    fn can_start(&self, source: &PrefetchSource) -> bool {
        self.archive_bytes + source.reserved_bytes() <= IN_FLIGHT_ARCHIVE_BYTES
    }
}

impl PrefetchSource {
    fn reserved_bytes(&self) -> usize {
        if matches!(self, Self::Url { .. }) {
            ARCHIVE_BYTES
        } else {
            0
        }
    }
}

#[derive(Default)]
struct Shared {
    queue: Mutex<Queue>,
    ready: Condvar,
}

/// A bounded background fetch pool for one configure.
///
/// Duplicate submissions share a wait handle while the job is queued or running.
/// Each archive can use up to 32 MiB of payload bytes. Active archive jobs
/// share a 64 MiB payload budget. Drop cancels queued jobs and waits up to
/// one second for active jobs. An active job keeps its own request timeout.
pub struct FetchPool {
    shared: Arc<Shared>,
    workers: Vec<JoinHandle<()>>,
}

impl FetchPool {
    /// Start workers using `RCMAKE_FETCH_JOBS` (default 4, bounded to 1 through 32).
    pub fn new(host: Arc<dyn Host>, config: StoreConfig) -> Result<Self, FetchError> {
        Self::new_with_timeout(host, config, None)
    }

    /// Start workers with a timeout for each job.
    ///
    /// The default is 15 seconds. Explicit timeouts are capped at 10 minutes.
    pub fn new_with_timeout(
        host: Arc<dyn Host>,
        config: StoreConfig,
        timeout: Option<Duration>,
    ) -> Result<Self, FetchError> {
        Self::start(host, config, timeout, None)
    }

    /// Start workers with a caller-provided limit or the host's configured limit.
    pub fn new_with_jobs(
        host: Arc<dyn Host>,
        config: StoreConfig,
        jobs: Option<usize>,
    ) -> Result<Self, FetchError> {
        Self::start(host, config, None, jobs)
    }

    fn start(
        host: Arc<dyn Host>,
        config: StoreConfig,
        timeout: Option<Duration>,
        jobs: Option<usize>,
    ) -> Result<Self, FetchError> {
        let timeout = timeout
            .unwrap_or(DEFAULT_REQUEST_TIMEOUT)
            .min(MAX_REQUEST_TIMEOUT);
        let count = jobs
            .or_else(|| {
                host.env_get(b"RCMAKE_FETCH_JOBS")
                    .and_then(|value| std::str::from_utf8(&value).ok()?.parse::<usize>().ok())
            })
            .unwrap_or(4)
            .clamp(1, MAX_WORKERS);
        Store::new(host.as_ref(), config.clone())?;
        let mut pool = Self {
            shared: Arc::default(),
            workers: Vec::with_capacity(count),
        };
        for index in 0..count {
            let (shared, host, config) =
                (Arc::clone(&pool.shared), Arc::clone(&host), config.clone());
            let worker = thread::Builder::new()
                .name(format!("rcmake-fetch-{index}"))
                .spawn(move || work(&shared, host.as_ref(), &config, timeout))
                .map_err(|error| invalid(format!("Cannot start a fetch worker ({error}).")))?;
            pool.workers.push(worker);
        }
        Ok(pool)
    }

    /// Queue a pinned source or return its existing ticket.
    pub fn submit(&self, mut source: PrefetchSource) -> Result<Ticket, FetchError> {
        let url = match &source {
            PrefetchSource::Url { url, .. } => url,
            PrefetchSource::Git { repository, .. } => repository,
        };
        if url.is_empty() || url.len() > 8192 || url.chars().any(char::is_control) {
            return Err(invalid("Invalid prefetch source URL."));
        }
        if let PrefetchSource::Git { commit, .. } = &mut source {
            if !is_commit(commit) {
                return Err(invalid("Prefetch requires a full Git commit ID."));
            }
            commit.make_ascii_lowercase();
        }
        let mut queue = self
            .shared
            .queue
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if let Some(flight) = queue.flights.get(&source) {
            return Ok(Ticket(Arc::clone(flight)));
        }
        if queue.flights.len() >= MAX_FLIGHTS {
            return Err(invalid("Prefetch queue is full."));
        }
        let flight = Arc::new(Flight::default());
        queue.flights.insert(source.clone(), Arc::clone(&flight));
        queue.jobs.push_back((source, Arc::clone(&flight)));
        self.shared.ready.notify_one();
        Ok(Ticket(flight))
    }
}

impl Drop for FetchPool {
    fn drop(&mut self) {
        let mut queue = self
            .shared
            .queue
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        queue.closed = true;
        queue.flights.clear();
        for (_, flight) in queue.jobs.drain(..) {
            flight.finish(Err(invalid("Prefetch was cancelled.")));
        }
        self.shared.ready.notify_all();
        let (queue, _) = self
            .shared
            .ready
            .wait_timeout_while(queue, SHUTDOWN_TIMEOUT, |queue| !queue.active.is_empty())
            .unwrap_or_else(|error| error.into_inner());
        for flight in &queue.active {
            flight.finish(Err(invalid("Prefetch exceeded the shutdown time limit.")));
        }
        drop(queue);
        for worker in self.workers.drain(..) {
            if worker.is_finished() {
                let _ = worker.join();
            }
        }
    }
}

fn work(shared: &Shared, host: &dyn Host, config: &StoreConfig, timeout: Duration) {
    loop {
        let (source, flight) = {
            let queue = shared
                .queue
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            let mut queue = shared
                .ready
                .wait_while(queue, |queue| {
                    !queue.closed && !queue.jobs.iter().any(|(source, _)| queue.can_start(source))
                })
                .unwrap_or_else(|error| error.into_inner());
            if queue.closed {
                return;
            }
            let Some(index) = queue
                .jobs
                .iter()
                .position(|(source, _)| queue.can_start(source))
            else {
                return;
            };
            let Some((source, flight)) = queue.jobs.remove(index) else {
                return;
            };
            queue.archive_bytes += source.reserved_bytes();
            queue.active.push(Arc::clone(&flight));
            (source, flight)
        };
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            fetch(host, config, &source, timeout)
        }))
        .unwrap_or_else(|_| Err(invalid("Prefetch worker failed.")));
        let mut queue = shared
            .queue
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        queue.flights.remove(&source);
        flight.finish(result);
        queue.archive_bytes -= source.reserved_bytes();
        queue.active.retain(|active| !Arc::ptr_eq(active, &flight));
        shared.ready.notify_all();
    }
}

fn fetch(
    host: &dyn Host,
    config: &StoreConfig,
    source: &PrefetchSource,
    timeout: Duration,
) -> PrefetchResult {
    match source {
        PrefetchSource::Url { url, hash } => {
            let store = Store::new(host, config.clone())?;
            let cache = ArchiveCache::new(host, &store)
                .with_byte_limit(ARCHIVE_BYTES)
                .with_timeout(timeout);
            let result = cache.fetch(url, Some(hash))?;
            if result.offline_seeded {
                Ok(None)
            } else {
                Ok(Some(result.cache_warning.unwrap_or_else(|| invalid(
                    "The archive is unavailable for offline use. Check the cache path and seed the pin again."
                ))))
            }
        }
        PrefetchSource::Git { repository, commit } => {
            let deadline = host.monotonic().saturating_add(timeout);
            GitMirror::new(host, &config.root)?
                .with_deadline(deadline)
                .prefetch(repository, commit)?;
            Ok(None)
        }
    }
}
