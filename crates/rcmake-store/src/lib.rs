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
    FileKind, Follow, Host, HostError, HostResult, Metadata, ReadRange, WriteMode, path,
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
    /// Use a root with a 2 GiB size cap and a 10-minute blob grace period.
    pub fn new(root: PathBuf) -> Self {
        Self {
            root,
            max_bytes: 2 * 1024 * 1024 * 1024,
            blob_grace: Duration::from_secs(600),
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
}

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
        })
    }

    /// Write an immutable blob and return its digest.
    ///
    /// In off and read-only modes, return the digest without writing.
    pub fn put_blob(&self, bytes: &[u8]) -> HostResult<Digest> {
        let digest = Digest::hash(bytes);
        if self.writable() {
            let path = self.blob_path(digest);
            let frame = frame::encode(BLOB_MAGIC, bytes, blob_limit())
                .map_err(|error| io_at(error, &path))?;
            if !self.publish(&path, &frame, false)? {
                if self.get_blob(digest)?.is_some() {
                    let _ = self.host.set_times(&path, None, Some(self.host.now()));
                } else {
                    self.publish(&path, &frame, true)?;
                }
            }
        }
        Ok(digest)
    }

    /// Read a verified blob. Missing or invalid data is a cache miss.
    pub fn get_blob(&self, digest: Digest) -> HostResult<Option<Vec<u8>>> {
        if self.config.mode == StoreMode::Off {
            return Ok(None);
        }
        self.read_checked(&self.blob_path(digest), BLOB_MAGIC, blob_limit(), |bytes| {
            (Digest::hash(&bytes) == digest).then_some(bytes)
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
    /// All referenced blobs must already be present. Modes that disable writes
    /// return success without publishing. If collection holds the lock, skip
    /// writes that require it.
    pub fn put_entry<T: Serialize>(
        &self,
        domain: Domain,
        key: Digest,
        entry: &Entry<T>,
    ) -> HostResult<()> {
        if !self.writable() {
            return Ok(());
        }
        let path = self.entry_path(domain, key);
        let json = serde_json::to_vec(&Record { domain, key, entry })
            .map_err(|error| io_at(error.into(), &path))?;
        let frame =
            frame::encode(ENTRY_MAGIC, &json, ENTRY_LIMIT).map_err(|error| io_at(error, &path))?;
        if entry.blobs.is_empty() && self.publish(&path, &frame, false)? {
            return Ok(());
        }
        // Keep collection out of reference checks and stale entry replacement.
        self.ensure_dir(&self.config.root)?;
        let lock_path = self.gc_lock_path()?;
        let _guard = match self.host.lock_file(&lock_path, Some(Duration::ZERO)) {
            Ok(guard) => guard,
            Err(error) if error.kind == ErrorKind::TimedOut => return Ok(()),
            Err(error) => return Err(error),
        };
        for &digest in &entry.blobs {
            let path = self.blob_path(digest);
            if self.file_metadata(&path)?.is_none() {
                return Err(io_at(
                    io::Error::new(ErrorKind::NotFound, "A referenced cache blob is missing."),
                    &path,
                ));
            }
        }
        if self.publish(&path, &frame, false)? {
            return Ok(());
        }
        if let Some(previous) = self.read_entry::<Entry<IgnoredAny>>(domain, key, &path)?
            && self.entry_matches(&previous)?
        {
            return Ok(());
        }
        if verify_trace(self.host, &entry.trace) == VerifyOutcome::Hit {
            self.publish(&path, &frame, true)?;
        }
        Ok(())
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
        if self.config.mode == StoreMode::Off {
            return Ok(None);
        }
        let path = self.entry_path(domain, key);
        let Some(entry) = self.read_entry::<Entry<T>>(domain, key, &path)? else {
            return Ok(None);
        };
        if !self.entry_matches(&entry)? {
            return Ok(None);
        }
        if self.writable() {
            let _ = self.host.set_times(&path, None, Some(self.host.now()));
        }
        Ok(Some(entry))
    }

    fn entry_matches<T>(&self, entry: &Entry<T>) -> HostResult<bool> {
        if verify_trace(self.host, &entry.trace) != VerifyOutcome::Hit {
            return Ok(false);
        }
        for &digest in &entry.blobs {
            let present = if self.config.mode == StoreMode::Verify {
                self.get_blob(digest)?.is_some()
            } else {
                self.file_metadata(&self.blob_path(digest))?.is_some()
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
    ) -> HostResult<Option<T>> {
        self.read_checked(path, ENTRY_MAGIC, ENTRY_LIMIT, |payload| {
            let record = serde_json::from_slice::<Record<T>>(&payload).ok()?;
            (record.domain == domain && record.key == key).then_some(record.entry)
        })
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
        validate: impl FnOnce(Vec<u8>) -> Option<T>,
    ) -> HostResult<Option<T>> {
        let Some(stat) = self.file_metadata(path)? else {
            return Ok(None);
        };
        let max = frame::disk_limit(limit);
        let value = if stat.len <= max {
            match self.host.read(path, ReadRange::All { limit: max }) {
                Ok(bytes) => frame::decode(magic, &bytes, limit).and_then(validate),
                Err(error) if error.kind == ErrorKind::NotFound => return Ok(None),
                Err(error) if error.kind == ErrorKind::FileTooLarge => None,
                Err(error) => return Err(error),
            }
        } else {
            None
        };
        if value.is_none() && self.writable() {
            let _ = self.remove_if_unchanged(path, &stat);
        }
        Ok(value)
    }

    fn publish(&self, path: &Path, bytes: &[u8], replace: bool) -> HostResult<bool> {
        if let Some(parent) = path.parent() {
            self.ensure_dir(parent)?;
        }
        let staging = self.config.root.join("tmp");
        self.ensure_dir(&staging)?;
        let pid = self.host.system().pid;
        let now = self.host.now();
        for _ in 0..64 {
            let serial = NEXT_TEMP.fetch_add(1, Ordering::Relaxed);
            let temp = staging.join(format!("{pid}-{}-{}-{serial}", now.secs, now.nanos));
            let mut cleanup = StagingFile {
                host: self.host,
                path: &temp,
                armed: true,
            };
            match self.host.write(&temp, bytes, WriteMode::CreateNew) {
                Ok(()) => {}
                Err(error) if error.kind == ErrorKind::AlreadyExists => {
                    cleanup.armed = false;
                    continue;
                }
                Err(error) => return Err(error),
            }
            let renamed = if replace {
                self.host.rename(&temp, path)
            } else {
                self.rename_staged(&temp, path, || self.host.rename_noreplace(&temp, path))
            };
            return match renamed {
                Ok(()) => {
                    cleanup.armed = false;
                    Ok(true)
                }
                Err(error) if !replace && error.kind == ErrorKind::AlreadyExists => Ok(false),
                Err(error) => Err(error),
            };
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
        if !self.rename_fallback.load(Ordering::Relaxed) {
            match no_replace() {
                Err(error) if error.kind == ErrorKind::Unsupported => {
                    self.rename_fallback.store(true, Ordering::Relaxed);
                }
                result => return result,
            }
        }
        if self.file_metadata(path)?.is_some() {
            return Err(io_at(ErrorKind::AlreadyExists.into(), path));
        }
        self.host.rename(temp, path)
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
