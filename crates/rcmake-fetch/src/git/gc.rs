use std::{
    io::ErrorKind,
    path::{Path, PathBuf},
    time::Duration,
};

use rcmake_host::{FileKind, FileTime, Follow, Host, path::to_native};
use rcmake_store::Digest;

use crate::{FetchError, invalid, offline};

use super::GitMirror;

const DEFAULT_MAX_BYTES: u64 = 2 * 1024 * 1024 * 1024;
const MAX_TREE_ENTRIES: usize = 1_000_000;
const MAX_TREE_DEPTH: usize = 64;

/// Result of collecting the Git mirror cache. Archive blobs have a separate store report.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct GitGcReport {
    /// Total bytes in Git mirrors when collection starts.
    pub bytes_before: u64,
    /// Estimated bytes still in Git mirrors when collection ends.
    pub bytes_after: u64,
    /// Number of unpinned Git mirrors removed.
    pub mirrors_removed: usize,
    /// Number of Git mirrors retained for pins or unverified pin data.
    pub pinned_retained: usize,
    /// Number of Git mirrors skipped because their lock was busy.
    pub busy_skipped: usize,
}

impl GitMirror<'_> {
    /// Collect unpinned Git mirrors until the mirror cache meets its size cap.
    ///
    /// Use `RCMAKE_FETCH_GIT_MAX_SIZE` to set the cap in bytes. The default
    /// is 2 GiB, separate from the archive store cap. Keep pinned mirrors
    /// and skip busy mirrors even if the cache stays above the cap. Offline
    /// and immutable snapshot instances leave the cache intact.
    pub fn collect(&self) -> Result<GitGcReport, FetchError> {
        let cap = self
            .host
            .env_get(b"RCMAKE_FETCH_GIT_MAX_SIZE")
            .and_then(|value| std::str::from_utf8(&value).ok()?.parse().ok())
            .unwrap_or(DEFAULT_MAX_BYTES);
        let listing = match self.host.read_dir(&self.root) {
            Ok(listing) => listing,
            Err(error) if error.kind == ErrorKind::NotFound => return Ok(GitGcReport::default()),
            Err(error) => return Err(error.into()),
        };
        let mut candidates = Vec::new();
        let mut budget = MAX_TREE_ENTRIES;
        for entry in listing {
            let path = self.root.join(to_native(&entry.name));
            if entry.kind == FileKind::Dir
                && is_mirror_path(&path)
                && let Some((size, mtime)) = mirror_size(self.host, &path, &mut budget)?
            {
                candidates.push((mtime, path, size));
            }
        }
        let mut report = GitGcReport {
            bytes_before: candidates
                .iter()
                .map(|(_, _, size)| *size)
                .fold(0, u64::saturating_add),
            ..GitGcReport::default()
        };
        report.bytes_after = report.bytes_before;
        if self.read_only || offline(self.host) {
            return Ok(report);
        }

        candidates.sort_unstable();
        let cwd = self.host.initial_cwd();
        for (_, path, original_size) in candidates {
            if report.bytes_after <= cap {
                break;
            }
            let _guard = match self
                .host
                .lock_file(&path.with_added_extension("lock"), Some(Duration::ZERO))
            {
                Ok(guard) => guard,
                Err(error) if error.kind == ErrorKind::TimedOut => {
                    report.busy_skipped += 1;
                    continue;
                }
                Err(error) => return Err(error.into()),
            };
            report.bytes_after = report.bytes_after.saturating_sub(original_size);
            let Some((size, _)) = mirror_size(self.host, &path, &mut budget)? else {
                continue;
            };
            report.bytes_after = report.bytes_after.saturating_add(size);
            if report.bytes_after <= cap {
                continue;
            }
            if self.has_pinned_refs(&cwd, &path)? {
                report.pinned_retained += 1;
                continue;
            }
            self.host.remove_dir_all(&path)?;
            report.bytes_after = report.bytes_after.saturating_sub(size);
            report.mirrors_removed += 1;
        }
        Ok(report)
    }

    fn has_pinned_refs(&self, cwd: &Path, path: &Path) -> Result<bool, FetchError> {
        if !self.valid_mirror(cwd, path)? {
            // A damaged mirror can still contain objects for a pin. Leave it for repair.
            return Ok(true);
        }
        let refs = self.mirror_git(
            cwd,
            path,
            &[],
            &[
                "for-each-ref",
                "--count=1",
                "--format=%(refname)",
                "refs/rcmake/pins/",
            ],
            true,
        )?;
        Ok(!refs.trim_ascii().is_empty())
    }
}

fn is_mirror_path(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .and_then(|name| name.strip_suffix(".git"))
        .and_then(Digest::from_hex)
        .is_some()
}

fn mirror_size(
    host: &dyn Host,
    root: &Path,
    budget: &mut usize,
) -> Result<Option<(u64, FileTime)>, FetchError> {
    let metadata = match host.metadata(root, Follow::No) {
        Ok(metadata) if metadata.kind == FileKind::Dir => metadata,
        Ok(_) => return Ok(None),
        Err(error) if error.kind == ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let mut size = 0u64;
    // Use an explicit stack so untrusted cache contents cannot grow the call stack.
    let mut directories: Vec<(PathBuf, usize)> = vec![(root.to_owned(), 0)];
    while let Some((path, depth)) = directories.pop() {
        if depth > MAX_TREE_DEPTH {
            return Err(invalid(
                "Git mirror tree exceeds the collection depth limit.",
            ));
        }
        let entries = match host.read_dir(&path) {
            Ok(entries) => entries,
            Err(error) if error.kind == ErrorKind::NotFound => continue,
            Err(error) => return Err(error.into()),
        };
        *budget = budget
            .checked_sub(entries.len())
            .ok_or_else(|| invalid("Git mirror collection exceeds the entry limit."))?;
        for entry in entries {
            let path = path.join(to_native(&entry.name));
            match host.metadata(&path, Follow::No) {
                Ok(meta) if meta.kind == FileKind::Dir => directories.push((path, depth + 1)),
                Ok(meta) => size = size.saturating_add(meta.len),
                Err(error) if error.kind == ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
        }
    }
    Ok(Some((size, metadata.mtime)))
}
