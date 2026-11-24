use std::collections::BTreeSet;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::time::Duration;

use rcmake_host::{FileKind, FileTime, HostResult, Metadata, path};
use serde::Deserialize;

use crate::{Digest, Domain, Store};

/// Files removed by one garbage collection pass.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct GcReport {
    /// Number of removed entries.
    pub entries_removed: usize,
    /// Number of removed blobs.
    pub blobs_removed: usize,
    /// Combined file size freed in bytes.
    pub bytes_removed: u64,
}

struct CachedFile {
    path: PathBuf,
    digest: Digest,
    stat: Metadata,
}

#[derive(Deserialize)]
struct BlobReferences {
    blobs: Vec<Digest>,
}

impl Store<'_> {
    /// Evict least recently used entries when over the size cap, then sweep old
    /// unreferenced blobs. A busy collection lock causes the pass to be skipped.
    pub fn gc(&self) -> HostResult<GcReport> {
        let mut report = GcReport::default();
        if !self.writable() || !self.directory_exists(&self.config.root)? {
            return Ok(report);
        }
        let _guard = match self
            .host
            .lock_file(&self.gc_lock_path()?, Some(Duration::ZERO))
        {
            Ok(guard) => guard,
            Err(error) if error.kind == ErrorKind::TimedOut => return Ok(report),
            Err(error) => return Err(error),
        };

        let now = self.host.now();
        let blobs = self.cache_files(&self.config.root.join("blobs"), "blob")?;
        let mut entries = Vec::new();
        for domain in Domain::ALL {
            let directory = self.config.root.join("entries").join(domain.as_str());
            entries.extend(
                self.cache_files(&directory, "entry")?
                    .into_iter()
                    .map(|file| (domain, file)),
            );
        }

        let mut total = blobs
            .iter()
            .chain(entries.iter().map(|(_, file)| file))
            .fold(0u64, |size, file| size.saturating_add(file.stat.len));
        let goal = if total > self.config.max_bytes {
            self.config.max_bytes - self.config.max_bytes / 5
        } else {
            self.config.max_bytes
        };
        entries.sort_unstable_by(|(_, left), (_, right)| {
            (left.stat.mtime, &left.path).cmp(&(right.stat.mtime, &right.path))
        });

        let mut live = BTreeSet::new();
        for (domain, entry) in entries {
            if total > goal && self.remove_if_unchanged(&entry)? {
                report.entries_removed += 1;
                report.bytes_removed += entry.stat.len;
                total -= entry.stat.len;
            } else if let Some(references) =
                self.read_entry::<BlobReferences>(domain, entry.digest, &entry.path)?
            {
                live.extend(references.blobs);
            }
        }
        for blob in blobs {
            if !live.contains(&blob.digest)
                && older_than(now, blob.stat.mtime, self.config.blob_grace)
                && self.remove_if_unchanged(&blob)?
            {
                report.blobs_removed += 1;
                report.bytes_removed = report.bytes_removed.saturating_add(blob.stat.len);
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

    fn remove_if_unchanged(&self, file: &CachedFile) -> HostResult<bool> {
        let Some(current) = self.file_metadata(&file.path)? else {
            return Ok(false);
        };
        if (current.dev, current.ino, current.len, current.mtime)
            != (file.stat.dev, file.stat.ino, file.stat.len, file.stat.mtime)
        {
            return Ok(false);
        }
        match self.host.remove_file(&file.path) {
            Ok(()) => Ok(true),
            Err(error) if error.kind == ErrorKind::NotFound => Ok(false),
            Err(error) => Err(error),
        }
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

fn older_than(now: FileTime, mtime: FileTime, grace: Duration) -> bool {
    let nanos = |time: FileTime| i128::from(time.secs) * 1_000_000_000 + i128::from(time.nanos);
    nanos(now) - nanos(mtime) >= grace.as_nanos() as i128
}
