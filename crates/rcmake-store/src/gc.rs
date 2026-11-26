use std::collections::BTreeMap;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::time::Duration;

use rcmake_host::{FileKind, FileTime, HostResult, Metadata, path};
use rcmake_json::{FieldError, FromJson, Value, deny_unknown, object, req, req_value};

use crate::{CacheRead, Digest, Domain, LockedStore, Store};

/// Files removed by one garbage collection pass.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct GcReport {
    /// Number of removed entries.
    pub entries_removed: usize,
    /// Number of removed blobs.
    pub blobs_removed: usize,
    /// Number of removed staging files.
    pub staging_removed: usize,
    /// Combined file size freed in bytes.
    pub bytes_removed: u64,
}

/// Counts and total size of the files that contain cache data.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct StoreStats {
    /// Number of entries.
    pub entries: usize,
    /// Number of blobs.
    pub blobs: usize,
    /// Combined size of entries and blobs in bytes.
    pub bytes: u64,
}

struct CachedFile {
    path: PathBuf,
    digest: Digest,
    stat: Metadata,
}

struct BlobReferences {
    blobs: Vec<Digest>,
}

impl FromJson for BlobReferences {
    fn from_json(value: &Value) -> Result<Self, FieldError> {
        let obj = object(value)?;
        deny_unknown(obj, &["trace", "blobs", "payload"])?;
        req_value(obj, "trace")?;
        req_value(obj, "payload")?;
        Ok(Self {
            blobs: req(obj, "blobs")?,
        })
    }
}

impl Store<'_> {
    /// Count entries and blobs in the current cache.
    pub fn stats(&self) -> HostResult<StoreStats> {
        let mut stats = StoreStats::default();
        if !self.readable()? {
            return Ok(stats);
        }
        for (directory, extension) in self.data_locations() {
            let files = self.cache_files(&directory, extension)?;
            if extension == "entry" {
                stats.entries += files.len();
            } else {
                stats.blobs += files.len();
            }
            for file in files {
                stats.bytes = stats.bytes.saturating_add(file.stat.len);
            }
        }
        Ok(stats)
    }

    /// Remove entries and blobs under the cache lock. Return the space freed.
    pub fn clear(&self) -> HostResult<GcReport> {
        let mut report = GcReport::default();
        if !self.writable() || !self.readable()? {
            return Ok(report);
        }
        let locked = self.lock_cache(crate::PUBLISH_WAIT)?;
        for (directory, extension) in self.data_locations() {
            for file in self.cache_files(&directory, extension)? {
                if locked.remove_if_unchanged(&file.path, &file.stat)? {
                    if extension == "entry" {
                        report.entries_removed += 1;
                    } else {
                        report.blobs_removed += 1;
                    }
                    report.bytes_removed = report.bytes_removed.saturating_add(file.stat.len);
                }
            }
        }
        Ok(report)
    }

    fn data_locations(&self) -> impl Iterator<Item = (PathBuf, &'static str)> {
        let entries = self.config.root.join("entries");
        Domain::ALL
            .into_iter()
            .map(move |domain| (entries.join(domain.as_str()), "entry"))
            .chain([(self.config.root.join("blobs"), "blob")])
    }

    /// Evict least recently used entries when over the size cap, then sweep old
    /// unreferenced blobs. A busy collection lock causes the pass to be skipped.
    pub fn gc(&self) -> HostResult<GcReport> {
        let mut report = GcReport::default();
        if !self.writable() || !self.directory_exists(&self.config.root)? {
            return Ok(report);
        }
        let locked = match self.lock_cache(Duration::ZERO) {
            Ok(locked) => locked,
            Err(error) if error.kind == ErrorKind::TimedOut => return Ok(report),
            Err(error) => return Err(error),
        };

        let now = self.host.now();
        let mut blobs: BTreeMap<_, _> = self
            .cache_files(&self.config.root.join("blobs"), "blob")?
            .into_iter()
            .map(|blob| (blob.digest, (blob, 0usize)))
            .collect();
        let mut total = blobs
            .values()
            .fold(0u64, |size, (file, _)| size.saturating_add(file.stat.len));
        let staging = self.config.root.join("tmp");
        for file in self.list_dir(&staging)? {
            let path = staging.join(path::to_native(&file.name));
            if file.kind != FileKind::File {
                continue;
            }
            if let Some(stat) = self.file_metadata(&path)? {
                if older_than(now, stat.mtime, Duration::from_secs(3600))
                    && locked.remove_if_unchanged(&path, &stat)?
                {
                    report.staging_removed += 1;
                    report.bytes_removed = report.bytes_removed.saturating_add(stat.len);
                } else {
                    total = total.saturating_add(stat.len);
                }
            }
        }
        let mut entries = Vec::new();
        for domain in Domain::ALL {
            let directory = self.config.root.join("entries").join(domain.as_str());
            for file in self.cache_files(&directory, "entry")? {
                let references = match self.read_entry::<BlobReferences>(
                    domain,
                    file.digest,
                    &file.path,
                    false,
                )? {
                    CacheRead::Found(entry) => entry.blobs,
                    CacheRead::Missing => continue,
                    CacheRead::Invalid => {
                        if locked.remove_if_unchanged(&file.path, &file.stat)? {
                            report.entries_removed += 1;
                            report.bytes_removed =
                                report.bytes_removed.saturating_add(file.stat.len);
                            continue;
                        }
                        Vec::new()
                    }
                };
                for digest in &references {
                    if let Some((_, count)) = blobs.get_mut(digest) {
                        *count += 1;
                    }
                }
                total = total.saturating_add(file.stat.len);
                entries.push((file, references));
            }
        }

        for (blob, count) in blobs.values() {
            if *count == 0 {
                total = total.saturating_sub(locked.sweep_blob(blob, now, &mut report)?);
            }
        }
        if total <= self.config.max_bytes {
            return Ok(report);
        }
        let goal = self.config.max_bytes - self.config.max_bytes / 5;
        entries.sort_unstable_by(|(left, _), (right, _)| {
            (left.stat.mtime, &left.path).cmp(&(right.stat.mtime, &right.path))
        });

        for (entry, references) in entries {
            if total <= goal {
                break;
            }
            if locked.remove_if_unchanged(&entry.path, &entry.stat)? {
                report.entries_removed += 1;
                report.bytes_removed = report.bytes_removed.saturating_add(entry.stat.len);
                total = total.saturating_sub(entry.stat.len);
                for digest in references {
                    if let Some((blob, count)) = blobs.get_mut(&digest) {
                        *count -= 1;
                        if *count == 0 {
                            total =
                                total.saturating_sub(locked.sweep_blob(blob, now, &mut report)?);
                        }
                    }
                }
            }
        }
        Ok(report)
    }

    fn cache_files(&self, directory: &Path, extension: &str) -> HostResult<Vec<CachedFile>> {
        let mut files = Vec::new();
        for shard in self.list_dir(directory)? {
            if shard.kind != FileKind::Dir || !lower_hex(&shard.name, 2) {
                continue;
            }
            let subdir = directory.join(path::to_native(&shard.name));
            for file in self.list_dir(&subdir)? {
                if file.kind != FileKind::File {
                    continue;
                }
                let Ok(name) = std::str::from_utf8(&file.name) else {
                    continue;
                };
                let Some((hex, suffix)) = name.split_once('.') else {
                    continue;
                };
                if suffix != extension
                    || !lower_hex(hex.as_bytes(), 64)
                    || !hex.as_bytes().starts_with(&shard.name)
                {
                    continue;
                }
                if let Some(digest) = Digest::from_hex(hex) {
                    let path = subdir.join(name);
                    if let Some(stat) = self.file_metadata(&path)? {
                        files.push(CachedFile { path, digest, stat });
                    }
                }
            }
        }
        Ok(files)
    }

    fn list_dir(&self, directory: &Path) -> HostResult<Vec<rcmake_host::DirEntry>> {
        if !self.directory_exists(directory)? {
            return Ok(Vec::new());
        }
        match self.host.read_dir(directory) {
            Err(error) if error.kind == ErrorKind::NotFound => Ok(Vec::new()),
            result => result,
        }
    }
}

impl LockedStore<'_, '_> {
    fn sweep_blob(
        &self,
        blob: &CachedFile,
        now: FileTime,
        report: &mut GcReport,
    ) -> HostResult<u64> {
        if older_than(now, blob.stat.mtime, self.store.config.blob_grace)
            && self.remove_if_unchanged(&blob.path, &blob.stat)?
        {
            report.blobs_removed += 1;
            report.bytes_removed = report.bytes_removed.saturating_add(blob.stat.len);
            Ok(blob.stat.len)
        } else {
            Ok(0)
        }
    }

    fn remove_if_unchanged(&self, path: &Path, stat: &Metadata) -> HostResult<bool> {
        let Some(current) = self.store.file_metadata(path)? else {
            return Ok(false);
        };
        if crate::FastStat::from(&current) != crate::FastStat::from(stat) {
            return Ok(false);
        }
        self.remove(path)
    }
}

/// Run garbage collection with the configuration from the host environment.
pub fn gc(host: &dyn rcmake_host::Host) -> HostResult<GcReport> {
    Store::new(host, crate::StoreConfig::from_host(host))?.gc()
}

fn lower_hex(bytes: &[u8], len: usize) -> bool {
    bytes.len() == len
        && bytes
            .iter()
            .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
}

pub(crate) fn older_than(now: FileTime, mtime: FileTime, grace: Duration) -> bool {
    let nanos = |time: FileTime| i128::from(time.secs) * 1_000_000_000 + i128::from(time.nanos);
    nanos(now) - nanos(mtime) >= grace.as_nanos() as i128
}
