//! Content-addressed blobs and verified cache entries for Rcmake.

#![forbid(unsafe_code)]
#![deny(missing_docs)]

mod digest;
mod frame;
mod gc;
mod stamp;

use std::io::{self, ErrorKind};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use rcmake_base::Limits;
use rcmake_host::{
    FileKind, FileLockGuard, Follow, Host, HostError, HostResult, Metadata, ReadRange, WriteMode,
    path,
};
use serde::{
    Deserialize, Serialize,
    de::{DeserializeOwned, IgnoredAny},
};

pub use digest::{Digest, Domain, KeyHasher};
pub use gc::{GcReport, gc};
pub use stamp::{
    FastStat, InputStamp, MissReason, Trace, VerifyOutcome, directory_digest, file_digest,
    verify_trace,
};

use frame::{BLOB_MAGIC, ENTRY_LIMIT, ENTRY_MAGIC};

static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);
const PUBLISH_WAIT: Duration = Duration::from_millis(250);

/// Controls access to the cache.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum StoreMode {
    /// Disable all cache access.
    Off,
    /// Read and verify entries without recording hits for eviction.
    ReadOnly,
    /// Read, publish, and collect cache data.
    #[default]
    ReadWrite,
    /// Read only and verify the contents of all referenced blobs before an entry hit.
    Verify,
}

/// Result of a cache publication request.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PublishOutcome {
    /// The store wrote at least one requested object. All requested objects are present.
    Published,
    /// The store already held all requested objects, or a valid entry for the key.
    AlreadyPresent,
    /// The cache lock stayed busy for the full wait period.
    SkippedBusy,
    /// The current store mode disables writes.
    Disabled,
    /// The host does not support the cache lock required for safe publication.
    SkippedUnsupported,
    /// The entry's input trace no longer matches.
    Stale,
}

/// Location and retention settings for a store.
#[derive(Clone, Debug)]
pub struct StoreConfig {
    /// Absolute cache directory. Ignored in off mode.
    pub root: PathBuf,
    /// Start entry eviction when the store exceeds this size in bytes.
    pub max_bytes: u64,
    /// Minimum age for an unreferenced blob to be eligible for deletion.
    pub blob_grace: Duration,
    /// Access mode.
    pub mode: StoreMode,
}

impl StoreConfig {
    /// Use a root with a 2 GiB size cap and a one-hour blob grace period.
    pub fn new(root: PathBuf) -> Self {
        Self {
            root,
            max_bytes: 2 * 1024 * 1024 * 1024,
            blob_grace: Duration::from_secs(3600),
            mode: StoreMode::ReadWrite,
        }
    }

    /// Select the system cache directory or use the RCMAKE_CACHE_DIR override.
    ///
    /// The mode is off if the host has no absolute cache directory. A valid
    /// RCMAKE_CACHE_MAX_SIZE value overrides the cap in bytes.
    pub fn from_host(host: &dyn Host) -> Self {
        let env_path = |name: &[u8]| {
            host.env_get(name)
                .filter(|value| !value.is_empty())
                .map(|value| path::to_native(&value))
        };
        let home = || env_path(b"HOME").filter(|dir| dir.is_absolute());
        let root = env_path(b"RCMAKE_CACHE_DIR")
            .map(|dir| host.initial_cwd().join(dir))
            .or_else(|| match host.system().sysname.as_slice() {
                b"Darwin" => home().map(|dir| dir.join("Library/Caches/rcmake")),
                b"Windows" => env_path(b"LOCALAPPDATA")
                    .filter(|dir| dir.is_absolute())
                    .map(|dir| dir.join("rcmake/cache")),
                _ => env_path(b"XDG_CACHE_HOME")
                    .filter(|dir| dir.is_absolute())
                    .or_else(|| home().map(|dir| dir.join(".cache")))
                    .map(|dir| dir.join("rcmake")),
            });
        let mut config = Self::new(root.unwrap_or_default());
        if !config.root.is_absolute() {
            config.mode = StoreMode::Off;
        }
        if let Some(cap) = host
            .env_get(b"RCMAKE_CACHE_MAX_SIZE")
            .and_then(|value| std::str::from_utf8(&value).ok()?.parse().ok())
        {
            config.max_bytes = cap;
        }
        config
    }
}

/// A cached result and the inputs and blobs it needs.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Entry<T> {
    /// Inputs that must still match before reuse.
    pub trace: Trace,
    /// Blob digests used by this result. Garbage collection retains these blobs.
    pub blobs: Vec<Digest>,
    /// Domain-specific result.
    pub payload: T,
}

#[derive(Serialize, Deserialize)]
pub(crate) struct Record<T> {
    pub(crate) domain: Domain,
    pub(crate) key: Digest,
    pub(crate) entry: T,
}

/// A cache that uses a host for all file, time, and environment operations.
pub struct Store<'a> {
    host: &'a dyn Host,
    config: StoreConfig,
    rename_fallback: AtomicBool,
    #[cfg(test)]
    test_hook: Option<&'a TestHook<'a>>,
}

struct LockedStore<'store, 'host> {
    store: &'store Store<'host>,
    _lock: Box<dyn FileLockGuard>,
}

enum CacheRead<T> {
    Missing,
    Invalid,
    Found(T),
}

#[cfg(test)]
#[derive(Clone, Copy, Eq, PartialEq)]
enum TestEvent {
    InvalidRead,
    BeforeRemove,
    FallbackRename,
}

#[cfg(test)]
type TestHook<'a> = dyn Fn(TestEvent, &Path) + Send + Sync + 'a;

impl<'a> Store<'a> {
    /// Open a store. Directories are created on the first write.
    pub fn new(host: &'a dyn Host, config: StoreConfig) -> HostResult<Self> {
        if config.mode != StoreMode::Off && !config.root.is_absolute() {
            return Err(HostError::not_absolute(config.root));
        }
        Ok(Self {
            host,
            config,
            rename_fallback: AtomicBool::new(false),
            #[cfg(test)]
            test_hook: None,
        })
    }

    /// Return the configured cache directory.
    pub fn root(&self) -> &Path {
        &self.config.root
    }

    /// Write an immutable blob and return its digest.
    ///
    /// In off and read-only modes, return the digest without writing. Return an
    /// error if a writable store cannot obtain its cache lock.
    pub fn put_blob(&self, bytes: &[u8]) -> HostResult<Digest> {
        let (digest, outcome) = self.put_blob_with_aliases(bytes, Domain::BlobV1, &[])?;
        match outcome {
            PublishOutcome::Published
            | PublishOutcome::AlreadyPresent
            | PublishOutcome::Disabled => Ok(digest),
            PublishOutcome::SkippedBusy => Err(HostError::timed_out(
                "The cache lock is busy.",
                Some(self.config.root.join("gc.lock")),
            )),
            PublishOutcome::SkippedUnsupported => Err(HostError::unsupported(
                "The host does not support the cache lock.",
            )),
            PublishOutcome::Stale => Err(io_at(
                io::Error::new(ErrorKind::InvalidData, "The cache blob is stale."),
                &self.blob_path(digest),
            )),
        }
    }

    /// Publish a blob and its alias keys under one cache lock. Wait up to 250
    /// milliseconds for the lock.
    ///
    /// Each alias is an [`Entry<Digest>`] with an empty trace. Its payload and
    /// only blob reference are the new digest. An alias to another existing
    /// blob returns an error. If a write fails, earlier writes can remain in the cache.
    pub fn put_blob_with_aliases(
        &self,
        bytes: &[u8],
        domain: Domain,
        aliases: &[Digest],
    ) -> HostResult<(Digest, PublishOutcome)> {
        let digest = Digest::hash(bytes);
        if !self.writable() {
            return Ok((digest, PublishOutcome::Disabled));
        }
        let path = self.blob_path(digest);
        let frame =
            frame::encode(BLOB_MAGIC, bytes, blob_limit()).map_err(|error| io_at(error, &path))?;
        let outcome = self.with_publication(|locked| {
            let mut published = locked.put_blob(digest, &path, &frame, bytes.len())?;
            let entry = Entry {
                trace: Vec::new(),
                blobs: vec![digest],
                payload: digest,
            };
            for &key in aliases {
                published |= locked.put_alias(domain, key, &entry)?;
            }
            Ok(if published {
                PublishOutcome::Published
            } else {
                PublishOutcome::AlreadyPresent
            })
        })?;
        Ok((digest, outcome))
    }

    /// Read a verified blob. Missing or invalid data is a cache miss.
    pub fn get_blob(&self, digest: Digest) -> HostResult<Option<Vec<u8>>> {
        if self.config.mode == StoreMode::Off {
            return Ok(None);
        }
        let path = self.blob_path(digest);
        self.read_with_cleanup(&path, false, || {
            self.read_blob(digest, &path, blob_limit(), false)
        })
    }

    /// Read a verified blob that fits within the caller's byte limit.
    ///
    /// A blob that exceeds the limit is a cache miss. When the limit is lower
    /// than the store limit, leave invalid or oversized files in the cache.
    pub fn get_blob_with_limit(&self, digest: Digest, limit: usize) -> HostResult<Option<Vec<u8>>> {
        if self.config.mode == StoreMode::Off {
            return Ok(None);
        }
        if limit >= blob_limit() {
            return self.get_blob(digest);
        }
        let path = self.blob_path(digest);
        Ok(match self.read_blob(digest, &path, limit, false)? {
            CacheRead::Found(bytes) => Some(bytes),
            CacheRead::Invalid | CacheRead::Missing => None,
        })
    }

    /// Read a verified blob and report invalid cache data or a blocked path.
    ///
    /// A missing blob or one whose header declares a payload over the caller's
    /// limit is a miss. The store can remove invalid data in read-write mode.
    pub fn get_blob_with_diagnostics(
        &self,
        digest: Digest,
        limit: usize,
    ) -> HostResult<Option<Vec<u8>>> {
        if self.config.mode == StoreMode::Off {
            return Ok(None);
        }
        let path = self.blob_path(digest);
        self.read_with_cleanup(&path, true, || {
            self.read_blob(digest, &path, limit.min(blob_limit()), true)
        })
    }

    /// Write a verified blob to a destination and report whether the blob was present.
    ///
    /// Read-only store modes can write the destination. The cache stays unchanged.
    pub fn materialize_blob(
        &self,
        digest: Digest,
        destination: &Path,
        mode: WriteMode,
    ) -> HostResult<bool> {
        let Some(bytes) = self.get_blob(digest)? else {
            return Ok(false);
        };
        self.host.write(destination, &bytes, mode)?;
        Ok(true)
    }

    /// Publish an entry. Keep the first valid entry for a key.
    ///
    /// All referenced blobs must already be present. Wait up to 250 milliseconds
    /// for the cache lock. Return the publication outcome if the store skips a write.
    pub fn put_entry<T: Serialize>(
        &self,
        domain: Domain,
        key: Digest,
        entry: &Entry<T>,
    ) -> HostResult<PublishOutcome> {
        if !self.writable() {
            return Ok(PublishOutcome::Disabled);
        }
        let path = self.entry_path(domain, key);
        let frame = entry_frame(domain, key, entry, &path)?;
        self.with_publication(|locked| {
            for &digest in &entry.blobs {
                let path = self.blob_path(digest);
                if self.file_metadata(&path)?.is_none() {
                    return Err(io_at(
                        io::Error::new(ErrorKind::NotFound, "A referenced cache blob is missing."),
                        &path,
                    ));
                }
            }
            let previous = self.read_entry::<Entry<IgnoredAny>>(domain, key, &path, false)?;
            if let CacheRead::Found(previous) = &previous
                && self.entry_matches(previous, blob_limit(), false)?
            {
                return Ok(PublishOutcome::AlreadyPresent);
            }
            if verify_trace(self.host, &entry.trace) != VerifyOutcome::Hit {
                return Ok(PublishOutcome::Stale);
            }
            locked.publish(&path, &frame, !matches!(previous, CacheRead::Missing))?;
            Ok(PublishOutcome::Published)
        })
    }

    /// Read and verify an entry, its input trace, and its blob references.
    ///
    /// An absent, stale, oversized, or invalid entry is a cache miss.
    /// A blob can be collected after a hit. Treat an absent blob as a miss.
    pub fn get_entry<T: DeserializeOwned>(
        &self,
        domain: Domain,
        key: Digest,
    ) -> HostResult<Option<Entry<T>>> {
        self.get_entry_with_blob_limit(domain, key, blob_limit())
    }

    /// Read an entry and apply a byte limit to reference checks in verify mode.
    ///
    /// In verify mode, an oversized referenced blob causes a cache miss. In
    /// other modes, this method checks only whether each referenced blob exists.
    pub fn get_entry_with_blob_limit<T: DeserializeOwned>(
        &self,
        domain: Domain,
        key: Digest,
        blob_limit: usize,
    ) -> HostResult<Option<Entry<T>>> {
        self.lookup_entry(domain, key, blob_limit, false)
    }

    /// Read an entry and report invalid cache data or a blocked path.
    ///
    /// In verify mode, apply the byte limit to referenced blobs. Missing or
    /// stale entries are misses. The store can remove invalid data in read-write mode.
    pub fn get_entry_with_diagnostics<T: DeserializeOwned>(
        &self,
        domain: Domain,
        key: Digest,
        blob_limit: usize,
    ) -> HostResult<Option<Entry<T>>> {
        self.lookup_entry(domain, key, blob_limit, true)
    }

    fn lookup_entry<T: DeserializeOwned>(
        &self,
        domain: Domain,
        key: Digest,
        blob_limit: usize,
        diagnose: bool,
    ) -> HostResult<Option<Entry<T>>> {
        if self.config.mode == StoreMode::Off {
            return Ok(None);
        }
        let path = self.entry_path(domain, key);
        let Some(entry) = self.read_with_cleanup(&path, diagnose, || {
            self.read_entry::<Entry<T>>(domain, key, &path, diagnose)
        })?
        else {
            return Ok(None);
        };
        if !self.entry_matches(&entry, blob_limit, diagnose)? {
            return Ok(None);
        }
        if self.writable() {
            let _ = self.host.set_times(&path, None, Some(self.host.now()));
        }
        Ok(Some(entry))
    }

    fn entry_matches<T>(
        &self,
        entry: &Entry<T>,
        blob_limit: usize,
        diagnose: bool,
    ) -> HostResult<bool> {
        if verify_trace(self.host, &entry.trace) != VerifyOutcome::Hit {
            return Ok(false);
        }
        for &digest in &entry.blobs {
            let present = match (self.config.mode, diagnose) {
                (StoreMode::Verify, true) => self
                    .get_blob_with_diagnostics(digest, blob_limit)?
                    .is_some(),
                (StoreMode::Verify, false) => {
                    self.get_blob_with_limit(digest, blob_limit)?.is_some()
                }
                _ => self
                    .lookup_metadata(&self.blob_path(digest), diagnose)?
                    .is_some(),
            };
            if !present {
                return Ok(false);
            }
        }
        Ok(true)
    }

    fn gc_lock_path(&self) -> HostResult<PathBuf> {
        let path = self.config.root.join("gc.lock");
        if self.file_metadata(&path)?.is_some() || stamp::missing(self.host, &path) {
            Ok(path)
        } else {
            Err(io_at(
                io::Error::new(ErrorKind::InvalidData, "Invalid cache collection lock."),
                &path,
            ))
        }
    }

    fn lock_cache(&self, timeout: Duration) -> HostResult<LockedStore<'_, 'a>> {
        self.ensure_dir(&self.config.root)?;
        Ok(LockedStore {
            store: self,
            _lock: self.host.lock_file(&self.gc_lock_path()?, Some(timeout))?,
        })
    }

    fn with_publication(
        &self,
        publish: impl FnOnce(&LockedStore<'_, 'a>) -> HostResult<PublishOutcome>,
    ) -> HostResult<PublishOutcome> {
        let locked = match self.lock_cache(PUBLISH_WAIT) {
            Ok(locked) => locked,
            Err(error) if error.kind == ErrorKind::TimedOut => {
                return Ok(PublishOutcome::SkippedBusy);
            }
            Err(error) if error.kind == ErrorKind::Unsupported => {
                return Ok(PublishOutcome::SkippedUnsupported);
            }
            Err(error) => return Err(error),
        };
        publish(&locked)
    }

    fn writable(&self) -> bool {
        self.config.mode == StoreMode::ReadWrite
    }

    fn blob_path(&self, digest: Digest) -> PathBuf {
        shard(self.config.root.join("blobs"), digest, "blob")
    }

    fn entry_path(&self, domain: Domain, key: Digest) -> PathBuf {
        shard(
            self.config.root.join("entries").join(domain.as_str()),
            key,
            "entry",
        )
    }

    fn read_entry<T: DeserializeOwned>(
        &self,
        domain: Domain,
        key: Digest,
        path: &Path,
        diagnose: bool,
    ) -> HostResult<CacheRead<T>> {
        self.read_checked(
            path,
            ENTRY_MAGIC,
            ENTRY_LIMIT,
            ENTRY_LIMIT,
            diagnose,
            |payload| {
                let record = serde_json::from_slice::<Record<T>>(&payload).ok()?;
                (record.domain == domain && record.key == key).then_some(record.entry)
            },
        )
    }

    fn read_blob(
        &self,
        digest: Digest,
        path: &Path,
        limit: usize,
        diagnose: bool,
    ) -> HostResult<CacheRead<Vec<u8>>> {
        self.read_checked(path, BLOB_MAGIC, limit, blob_limit(), diagnose, |bytes| {
            (Digest::hash(&bytes) == digest).then_some(bytes)
        })
    }

    fn lookup_metadata(&self, path: &Path, diagnose: bool) -> HostResult<Option<Metadata>> {
        if !diagnose {
            return self.file_metadata(path);
        }
        let mut dirs: Vec<_> = path
            .ancestors()
            .skip(1)
            .take_while(|dir| dir.starts_with(self.root()))
            .collect();
        while let Some(dir) = dirs.pop() {
            match self.host.metadata(dir, Follow::No) {
                Ok(meta) if meta.kind == FileKind::Dir => {}
                Ok(_) => {
                    return Err(io_at(
                        io::Error::new(ErrorKind::NotADirectory, "Expected a cache directory."),
                        dir,
                    ));
                }
                Err(error) if error.kind == ErrorKind::NotFound => return Ok(None),
                Err(error) => return Err(error),
            }
        }
        match self.host.metadata(path, Follow::No) {
            Ok(meta) if meta.kind == FileKind::File => Ok(Some(meta)),
            Ok(_) => Err(io_at(
                io::Error::new(ErrorKind::InvalidData, "Expected a regular cache file."),
                path,
            )),
            Err(error) if error.kind == ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error),
        }
    }

    fn file_metadata(&self, path: &Path) -> HostResult<Option<Metadata>> {
        if let Some(parent) = path.parent()
            && !self.directory_exists(parent)?
        {
            return Ok(None);
        }
        match self.host.metadata(path, Follow::No) {
            Ok(meta) => Ok((meta.kind == FileKind::File).then_some(meta)),
            Err(error) if matches!(error.kind, ErrorKind::NotFound | ErrorKind::NotADirectory) => {
                Ok(None)
            }
            Err(error) => Err(error),
        }
    }

    fn directory_exists(&self, path: &Path) -> HostResult<bool> {
        let mut dirs = path.ancestors();
        for dir in &mut dirs {
            match self.host.metadata(dir, Follow::No) {
                Ok(meta) if meta.kind == FileKind::Dir => {}
                Ok(_) => return Ok(false),
                Err(error)
                    if matches!(error.kind, ErrorKind::NotFound | ErrorKind::NotADirectory) =>
                {
                    return Ok(false);
                }
                Err(error) => return Err(error),
            }
            if dir == self.config.root {
                return Ok(true);
            }
        }
        Ok(false)
    }

    fn ensure_dir(&self, path: &Path) -> HostResult<()> {
        if self.directory_exists(path)? {
            return Ok(());
        }
        // Create only after checking that no existing store component is a link.
        for dir in path.ancestors() {
            match self.host.metadata(dir, Follow::No) {
                Ok(meta) if meta.kind != FileKind::Dir => {
                    return Err(io_at(
                        io::Error::new(ErrorKind::NotADirectory, "Expected a cache directory."),
                        dir,
                    ));
                }
                Ok(_) => {}
                Err(error) if error.kind == ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            }
            if dir == self.config.root {
                break;
            }
        }
        self.host.create_dir_all(path)
    }

    fn read_checked<T>(
        &self,
        path: &Path,
        magic: &[u8; 8],
        limit: usize,
        store_limit: usize,
        diagnose: bool,
        validate: impl FnOnce(Vec<u8>) -> Option<T>,
    ) -> HostResult<CacheRead<T>> {
        let Some(stat) = self.lookup_metadata(path, diagnose)? else {
            return Ok(CacheRead::Missing);
        };
        if diagnose && limit < store_limit {
            let header = match self.host.read(
                path,
                ReadRange::Slice {
                    offset: 0,
                    len: Some(frame::HEADER_LEN as u64),
                },
            ) {
                Ok(header) => header,
                Err(error) if error.kind == ErrorKind::NotFound => return Ok(CacheRead::Missing),
                Err(error) => return Err(error),
            };
            let Some(length) = frame::payload_len(magic, &header) else {
                return Ok(CacheRead::Invalid);
            };
            if length > store_limit || stat.len > frame::disk_limit(length) {
                return Ok(CacheRead::Invalid);
            }
            if length > limit {
                return Ok(CacheRead::Missing);
            }
        }
        let max = frame::disk_limit(limit);
        let value = if stat.len <= max {
            match self.host.read(path, ReadRange::All { limit: max }) {
                Ok(bytes) => frame::decode(magic, &bytes, limit).and_then(validate),
                Err(error) if error.kind == ErrorKind::NotFound => return Ok(CacheRead::Missing),
                Err(error) if error.kind == ErrorKind::FileTooLarge => None,
                Err(error) => return Err(error),
            }
        } else {
            None
        };
        Ok(value.map_or(CacheRead::Invalid, CacheRead::Found))
    }

    fn read_with_cleanup<T>(
        &self,
        path: &Path,
        diagnose: bool,
        read: impl Fn() -> HostResult<CacheRead<T>>,
    ) -> HostResult<Option<T>> {
        match read()? {
            CacheRead::Found(value) => return Ok(Some(value)),
            CacheRead::Missing => return Ok(None),
            CacheRead::Invalid => {}
        }
        #[cfg(test)]
        self.test_event(TestEvent::InvalidRead, path);
        if self.writable()
            && let Ok(locked) = self.lock_cache(Duration::ZERO)
        {
            // Read again because another writer could have repaired the file.
            match read()? {
                CacheRead::Found(value) => return Ok(Some(value)),
                CacheRead::Missing => return Ok(None),
                CacheRead::Invalid => {
                    let _ = locked.remove(path);
                }
            }
        }
        if diagnose {
            return Err(io_at(
                io::Error::new(ErrorKind::InvalidData, "The cache frame is invalid."),
                path,
            ));
        }
        Ok(None)
    }

    #[cfg(test)]
    fn test_event(&self, event: TestEvent, path: &Path) {
        if let Some(hook) = self.test_hook {
            hook(event, path);
        }
    }
}

impl LockedStore<'_, '_> {
    fn put_blob(
        &self,
        digest: Digest,
        path: &Path,
        frame: &[u8],
        limit: usize,
    ) -> HostResult<bool> {
        let previous = self.store.read_blob(digest, path, limit, false)?;
        if matches!(previous, CacheRead::Found(_)) {
            let _ = self
                .store
                .host
                .set_times(path, None, Some(self.store.host.now()));
            return Ok(false);
        }
        self.publish(path, frame, matches!(previous, CacheRead::Invalid))?;
        Ok(true)
    }

    fn put_alias(&self, domain: Domain, key: Digest, entry: &Entry<Digest>) -> HostResult<bool> {
        let path = self.store.entry_path(domain, key);
        let previous = self
            .store
            .read_entry::<Entry<Digest>>(domain, key, &path, false)?;
        if let CacheRead::Found(previous) = &previous {
            if previous == entry {
                return Ok(false);
            }
            if previous.trace.is_empty()
                && previous.blobs.as_slice() == [previous.payload]
                && self
                    .store
                    .file_metadata(&self.store.blob_path(previous.payload))?
                    .is_some()
            {
                return Err(io_at(
                    io::Error::new(
                        ErrorKind::AlreadyExists,
                        "The cache alias names a different blob.",
                    ),
                    &path,
                ));
            }
        }
        let frame = entry_frame(domain, key, entry, &path)?;
        self.publish(&path, &frame, !matches!(previous, CacheRead::Missing))?;
        Ok(true)
    }

    fn publish(&self, path: &Path, bytes: &[u8], replace: bool) -> HostResult<()> {
        let store = self.store;
        if let Some(parent) = path.parent() {
            store.ensure_dir(parent)?;
        }
        let staging = store.config.root.join("tmp");
        store.ensure_dir(&staging)?;
        let pid = store.host.system().pid;
        let now = store.host.now();
        for _ in 0..64 {
            let serial = NEXT_TEMP.fetch_add(1, Ordering::Relaxed);
            let temp = staging.join(format!("{pid}-{}-{}-{serial}", now.secs, now.nanos));
            let mut cleanup = StagingFile {
                host: store.host,
                path: &temp,
                armed: true,
            };
            match store.host.write(&temp, bytes, WriteMode::CreateNew) {
                Ok(()) => {}
                Err(error) if error.kind == ErrorKind::AlreadyExists => {
                    cleanup.armed = false;
                    continue;
                }
                Err(error) => return Err(error),
            }
            let renamed = if replace {
                store.host.rename(&temp, path)
            } else {
                self.rename_staged(&temp, path, || store.host.rename_noreplace(&temp, path))
            };
            renamed?;
            cleanup.armed = false;
            return Ok(());
        }
        Err(io_at(
            io::Error::new(
                ErrorKind::AlreadyExists,
                "No cache staging file is available.",
            ),
            &staging,
        ))
    }

    fn rename_staged(
        &self,
        temp: &Path,
        path: &Path,
        no_replace: impl FnOnce() -> HostResult<()>,
    ) -> HostResult<()> {
        let store = self.store;
        if !store.rename_fallback.load(Ordering::Relaxed) {
            match no_replace() {
                Err(error) if error.kind == ErrorKind::Unsupported => {
                    store.rename_fallback.store(true, Ordering::Relaxed);
                }
                result => return result,
            }
        }
        match store.host.metadata(path, Follow::No) {
            Ok(_) => return Err(io_at(ErrorKind::AlreadyExists.into(), path)),
            Err(error) if error.kind == ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        #[cfg(test)]
        store.test_event(TestEvent::FallbackRename, path);
        store.host.rename(temp, path)
    }

    fn remove(&self, path: &Path) -> HostResult<bool> {
        #[cfg(test)]
        self.store.test_event(TestEvent::BeforeRemove, path);
        match self.store.host.remove_file(path) {
            Ok(()) => Ok(true),
            Err(error) if error.kind == ErrorKind::NotFound => Ok(false),
            Err(error) => Err(error),
        }
    }
}

struct StagingFile<'a> {
    host: &'a dyn Host,
    path: &'a Path,
    armed: bool,
}

impl Drop for StagingFile<'_> {
    fn drop(&mut self) {
        if self.armed {
            let _ = self.host.remove_file(self.path);
        }
    }
}

fn blob_limit() -> usize {
    Limits::default().max_file_read as usize
}

fn entry_frame<T: Serialize>(
    domain: Domain,
    key: Digest,
    entry: &Entry<T>,
    path: &Path,
) -> HostResult<Vec<u8>> {
    let json = serde_json::to_vec(&Record { domain, key, entry })
        .map_err(|error| io_at(error.into(), path))?;
    frame::encode(ENTRY_MAGIC, &json, ENTRY_LIMIT).map_err(|error| io_at(error, path))
}

fn shard(directory: PathBuf, digest: Digest, extension: &str) -> PathBuf {
    let hex = digest.to_hex();
    directory
        .join(hex.get(..2).unwrap_or_default())
        .join(format!("{hex}.{extension}"))
}

fn io_at(error: io::Error, path: &Path) -> HostError {
    HostError::from_io(error, Some(path.to_owned()))
}

#[cfg(test)]
mod tests;
