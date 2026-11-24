use std::{
    collections::{HashMap, VecDeque},
    sync::{Arc, Condvar, Mutex, Weak},
    thread::{self, JoinHandle},
    time::Duration,
};

use rcmake_host::Host;
use rcmake_store::{Store, StoreConfig};

use crate::{ArchiveCache, ContentHash, FetchError, GitMirror, git::is_commit, invalid};

const MAX_WORKERS: usize = 32;
const MAX_FLIGHTS: usize = 4096;

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
        *self
            .result
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = Some(result);
        self.ready.notify_all();
    }
}

/// A wait handle shared by duplicate prefetch requests.
#[derive(Clone)]
pub struct Ticket(Arc<Flight>);

impl Ticket {
    /// Wait for the fetch to finish.
    pub fn wait(&self) -> Result<(), FetchError> {
        self.wait_with_warning().map(|_| ())
    }

    /// Wait for the fetch to finish and return any cache warning.
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
    flights: HashMap<PrefetchSource, Weak<Flight>>,
}

#[derive(Default)]
struct Shared {
    queue: Mutex<Queue>,
    ready: Condvar,
}

/// A bounded background fetch pool for one configure.
///
/// Duplicate submissions share a wait handle while that handle remains live.
/// Drop cancels queued work and waits for running operations.
pub struct FetchPool {
    shared: Arc<Shared>,
    workers: Vec<JoinHandle<()>>,
}

impl FetchPool {
    /// Start workers using `RCMAKE_FETCH_JOBS` (default 4, bounded to 1 through 32).
    pub fn new(host: Arc<dyn Host>, config: StoreConfig) -> Result<Self, FetchError> {
        Self::new_with_timeout(host, config, None)
    }

    /// Start workers with an optional timeout for each fetch command and Git lock.
    pub fn new_with_timeout(
        host: Arc<dyn Host>,
        config: StoreConfig,
        timeout: Option<Duration>,
    ) -> Result<Self, FetchError> {
        let count = host
            .env_get(b"RCMAKE_FETCH_JOBS")
            .and_then(|value| std::str::from_utf8(&value).ok()?.parse::<usize>().ok())
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
        if let Some(flight) = queue.flights.get(&source).and_then(Weak::upgrade) {
            return Ok(Ticket(flight));
        }
        if queue.flights.len() >= MAX_FLIGHTS {
            queue.flights.retain(|_, flight| flight.strong_count() != 0);
            if queue.flights.len() >= MAX_FLIGHTS {
                return Err(invalid("Prefetch queue is full."));
            }
        }
        let flight = Arc::new(Flight::default());
        queue
            .flights
            .insert(source.clone(), Arc::downgrade(&flight));
        queue.jobs.push_back((source, Arc::clone(&flight)));
        self.shared.ready.notify_one();
        Ok(Ticket(flight))
    }
}

impl Drop for FetchPool {
    fn drop(&mut self) {
        {
            let mut queue = self
                .shared
                .queue
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            queue.closed = true;
            for (_, flight) in queue.jobs.drain(..) {
                flight.finish(Err(invalid("Prefetch was cancelled.")));
            }
            self.shared.ready.notify_all();
        }
        for worker in self.workers.drain(..) {
            let _ = worker.join();
        }
    }
}

fn work(shared: &Shared, host: &dyn Host, config: &StoreConfig, timeout: Option<Duration>) {
    loop {
        let job = {
            let queue = shared
                .queue
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            let mut queue = shared
                .ready
                .wait_while(queue, |queue| !queue.closed && queue.jobs.is_empty())
                .unwrap_or_else(|error| error.into_inner());
            queue.jobs.pop_front()
        };
        let Some((source, flight)) = job else { return };
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            fetch(host, config, &source, timeout)
        }))
        .unwrap_or_else(|_| Err(invalid("Prefetch worker failed.")));
        if !matches!(result, Ok(None)) {
            let mut queue = shared
                .queue
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            queue.flights.remove(&source);
        }
        flight.finish(result);
    }
}

fn fetch(
    host: &dyn Host,
    config: &StoreConfig,
    source: &PrefetchSource,
    timeout: Option<Duration>,
) -> PrefetchResult {
    match source {
        PrefetchSource::Url { url, hash } => {
            let store = Store::new(host, config.clone())?;
            let mut cache = ArchiveCache::new(host, &store);
            if let Some(timeout) = timeout {
                cache = cache.with_timeout(timeout);
            }
            Ok(cache.fetch(url, Some(hash))?.cache_warning)
        }
        PrefetchSource::Git { repository, commit } => {
            let mut mirror = GitMirror::new(host, &config.root)?;
            if let Some(timeout) = timeout {
                mirror = mirror.with_timeout(timeout);
            }
            mirror.prefetch(repository, commit)?;
            Ok(None)
        }
    }
}
