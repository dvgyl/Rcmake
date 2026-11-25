//! A configure-scoped cache for filesystem queries.

#![forbid(unsafe_code)]

use core::time::Duration;
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock, RwLockReadGuard, RwLockWriteGuard};

use rcmake_base::BString;

use crate::{
    CopyOptions, DirEntry, FileLockGuard, FileTime, Follow, GracefulTimeout, Host, HostResult,
    Metadata, ReadRange, RunOutput, RunRequest, Stream, WriteMode,
};

/// Counts of cache operations since the adapter was created.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MemoStats {
    /// Queries answered from the cache.
    pub hits: u64,
    /// Queries that called the base host, including queries during a process.
    pub misses: u64,
    /// Filesystem mutation, lock, and process calls that invalidated cached queries.
    pub invalidations: u64,
}

const MAX_CACHE_ENTRIES: usize = 8_192;
const MAX_CACHE_BYTES: usize = 8 * 1024 * 1024;

#[derive(Default)]
struct Queries {
    follow: Option<HostResult<Metadata>>,
    no_follow: Option<HostResult<Metadata>>,
    entries: Option<HostResult<Vec<DirEntry>>>,
    canonical: Option<HostResult<PathBuf>>,
}

#[derive(Default)]
struct Cache {
    // Keep exact spelling. A trailing separator can change the result.
    paths: BTreeMap<OsString, Queries>,
    entries: usize,
    bytes: usize,
    generation: u64,
    active_changes: usize,
    tracked: BTreeMap<PathBuf, u64>,
}

fn path_bytes(path: &Path) -> usize {
    path.as_os_str().as_encoded_bytes().len()
}

fn listing_bytes(entries: &[DirEntry]) -> usize {
    entries.iter().fold(0_usize, |bytes, entry| {
        bytes
            .saturating_add(size_of::<DirEntry>())
            .saturating_add(entry.name.len())
    })
}

/// Caches metadata, directory listings, and canonical paths for one configure.
///
/// The cache stores successes and not-found results within fixed entry and byte
/// limits. Filesystem changes, file locks, and process calls clear all queries.
/// Queries bypass the cache while these calls are active.
///
/// Use a new adapter when the base can change through another handle.
/// Cached metadata reflects the access time at the time of the base query.
pub struct MemoHost<H: Host + ?Sized> {
    base: Arc<H>,
    cache: RwLock<Cache>,
    hits: AtomicU64,
    misses: AtomicU64,
    invalidations: AtomicU64,
}

impl<H: Host> MemoHost<H> {
    /// Wrap an owned host with an empty cache.
    #[must_use]
    pub fn new(base: H) -> Self {
        Self::from_shared(Arc::new(base))
    }
}

impl<H: Host + ?Sized> MemoHost<H> {
    /// Wrap a shared host, including a shared trait object, with an empty cache.
    #[must_use]
    pub fn from_shared(base: Arc<H>) -> Self {
        Self {
            base,
            cache: RwLock::default(),
            hits: AtomicU64::new(0),
            misses: AtomicU64::new(0),
            invalidations: AtomicU64::new(0),
        }
    }

    /// Return the current cache counters.
    #[must_use]
    pub fn stats(&self) -> MemoStats {
        MemoStats {
            hits: self.hits.load(Ordering::Relaxed),
            misses: self.misses.load(Ordering::Relaxed),
            invalidations: self.invalidations.load(Ordering::Relaxed),
        }
    }

    /// Return the current generation while no mutation or process is active.
    #[must_use]
    pub fn stable_generation(&self) -> Option<u64> {
        let cache = self.read_cache();
        (cache.active_changes == 0).then_some(cache.generation)
    }

    /// Return the generation for a canonical path while no change is active.
    ///
    /// Changes to intersecting paths and calls that can change any path invalidate
    /// the generation. The bounded table returns no generation when full.
    pub fn stable_generation_for(&self, path: &Path) -> Option<u64> {
        if !path.is_absolute() || path_bytes(path) > 4096 {
            return None;
        }
        let mut cache = self.write_cache();
        if cache.active_changes != 0 {
            return None;
        }
        if let Some(&generation) = cache.tracked.get(path) {
            return Some(generation);
        }
        if cache.tracked.len() >= 2048 {
            return None;
        }
        let generation = cache.generation;
        cache.tracked.insert(path.to_owned(), generation);
        Some(generation)
    }

    /// Invalidate queries after changes made through another host handle.
    pub fn clear(&self) {
        drop(self.begin_change());
    }

    fn read_cache(&self) -> RwLockReadGuard<'_, Cache> {
        self.cache
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn write_cache(&self) -> RwLockWriteGuard<'_, Cache> {
        self.cache
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn lookup<T: Clone>(
        &self,
        p: &Path,
        get: impl Fn(&Queries) -> &Option<HostResult<T>>,
        slot: impl FnOnce(&mut Queries) -> &mut Option<HostResult<T>>,
        retained_bytes: impl FnOnce(&T) -> usize,
        query: impl FnOnce() -> HostResult<T>,
    ) -> HostResult<T> {
        let generation = {
            let cache = self.read_cache();
            if let Some(entry) = cache.paths.get(p.as_os_str()).and_then(|q| get(q).as_ref()) {
                self.hits.fetch_add(1, Ordering::Relaxed);
                return entry.clone();
            }
            (cache.active_changes == 0 && cache.entries < MAX_CACHE_ENTRIES)
                .then_some(cache.generation)
        };
        self.misses.fetch_add(1, Ordering::Relaxed);
        let result = query();
        let Some(generation) = generation else {
            return result;
        };
        let value_bytes = match &result {
            Ok(value) => retained_bytes(value),
            Err(err) if err.kind == ErrorKind::NotFound => err
                .os_message
                .len()
                .saturating_add(err.path.as_deref().map_or(0, path_bytes)),
            Err(_) => return result,
        };
        // Charge each query for its full path and a map slot.
        let bytes = value_bytes
            .saturating_add(path_bytes(p))
            .saturating_add(size_of::<(OsString, Queries)>());
        let mut cache = self.write_cache();
        if cache.generation == generation
            && cache.active_changes == 0
            && cache.entries < MAX_CACHE_ENTRIES
            && bytes <= MAX_CACHE_BYTES - cache.bytes
        {
            let queries = cache.paths.entry(p.as_os_str().to_owned()).or_default();
            let slot = slot(queries);
            if slot.is_none() {
                *slot = Some(result.clone());
                cache.entries += 1;
                cache.bytes += bytes;
            }
        }
        result
    }

    fn begin_change(&self) -> ActiveChange<'_, H> {
        self.begin_change_for(&[])
    }

    fn begin_change_for(&self, paths: &[&Path]) -> ActiveChange<'_, H> {
        let mut cache = self.write_cache();
        cache.generation = cache.generation.wrapping_add(1);
        let generation = cache.generation;
        if !cache.tracked.is_empty() {
            let canonical: Option<Vec<_>> =
                paths.iter().map(|path| self.change_path(path)).collect();
            for (root, revision) in &mut cache.tracked {
                let overlaps = |path: &Path| path.starts_with(root) || root.starts_with(path);
                if paths.is_empty()
                    || paths.iter().any(|path| overlaps(path))
                    || canonical
                        .as_ref()
                        .is_none_or(|paths| paths.iter().any(|path| overlaps(path)))
                {
                    *revision = generation;
                }
            }
        }
        cache.active_changes += 1;
        cache.paths.clear();
        cache.entries = 0;
        cache.bytes = 0;
        ActiveChange { host: self }
    }

    fn change_path(&self, path: &Path) -> Option<PathBuf> {
        for parent in path.ancestors().take(64) {
            match self.base.canonicalize(parent) {
                Ok(root) => return Some(root.join(path.strip_prefix(parent).ok()?)),
                Err(error) if error.kind == ErrorKind::NotFound => {}
                Err(_) => return None,
            }
        }
        None
    }
}

struct ActiveChange<'a, H: Host + ?Sized> {
    host: &'a MemoHost<H>,
}

impl<H: Host + ?Sized> Drop for ActiveChange<'_, H> {
    fn drop(&mut self) {
        let mut cache = self.host.write_cache();
        cache.active_changes -= 1;
        cache.generation = cache.generation.wrapping_add(1);
        self.host.invalidations.fetch_add(1, Ordering::Relaxed);
    }
}

impl<H: Host + ?Sized> Host for MemoHost<H> {
    fn metadata(&self, p: &Path, follow: Follow) -> HostResult<Metadata> {
        self.lookup(
            p,
            |queries| match follow {
                Follow::Yes => &queries.follow,
                Follow::No => &queries.no_follow,
            },
            |queries| match follow {
                Follow::Yes => &mut queries.follow,
                Follow::No => &mut queries.no_follow,
            },
            |_| 0,
            || self.base.metadata(p, follow),
        )
    }

    fn read(&self, p: &Path, range: ReadRange) -> HostResult<Vec<u8>> {
        self.base.read(p, range)
    }

    fn read_chunks(&self, p: &Path, consume: &mut dyn FnMut(&[u8])) -> HostResult<()> {
        self.base.read_chunks(p, consume)
    }

    fn write(&self, p: &Path, data: &[u8], mode: WriteMode) -> HostResult<()> {
        let _change = self.begin_change_for(&[p]);
        self.base.write(p, data, mode)
    }

    fn read_dir(&self, p: &Path) -> HostResult<Vec<DirEntry>> {
        self.lookup(
            p,
            |queries| &queries.entries,
            |queries| &mut queries.entries,
            |entries| listing_bytes(entries),
            || self.base.read_dir(p),
        )
    }

    fn create_dir_all(&self, p: &Path) -> HostResult<()> {
        let _change = self.begin_change_for(&[p]);
        self.base.create_dir_all(p)
    }

    fn remove_file(&self, p: &Path) -> HostResult<()> {
        let _change = self.begin_change_for(&[p]);
        self.base.remove_file(p)
    }

    fn remove_dir(&self, p: &Path) -> HostResult<()> {
        let _change = self.begin_change_for(&[p]);
        self.base.remove_dir(p)
    }

    fn remove_dir_all(&self, p: &Path) -> HostResult<()> {
        let _change = self.begin_change_for(&[p]);
        self.base.remove_dir_all(p)
    }

    fn rename(&self, from: &Path, to: &Path) -> HostResult<()> {
        let _change = self.begin_change_for(&[from, to]);
        self.base.rename(from, to)
    }

    fn rename_noreplace(&self, from: &Path, to: &Path) -> HostResult<()> {
        let _change = self.begin_change_for(&[from, to]);
        self.base.rename_noreplace(from, to)
    }

    fn lock_file(
        &self,
        path: &Path,
        timeout: Option<Duration>,
    ) -> HostResult<Box<dyn FileLockGuard>> {
        let _change = self.begin_change_for(&[path]);
        self.base.lock_file(path, timeout)
    }

    fn copy_file(&self, from: &Path, to: &Path, opts: CopyOptions) -> HostResult<()> {
        let _change = self.begin_change_for(&[to]);
        self.base.copy_file(from, to, opts)
    }

    fn symlink(&self, target: &Path, link: &Path) -> HostResult<()> {
        let _change = self.begin_change_for(&[link]);
        self.base.symlink(target, link)
    }

    fn hard_link(&self, src: &Path, dst: &Path) -> HostResult<()> {
        let _change = self.begin_change_for(&[dst]);
        self.base.hard_link(src, dst)
    }

    fn read_link(&self, p: &Path) -> HostResult<PathBuf> {
        self.base.read_link(p)
    }

    fn canonicalize(&self, p: &Path) -> HostResult<PathBuf> {
        self.lookup(
            p,
            |queries| &queries.canonical,
            |queries| &mut queries.canonical,
            |path| path_bytes(path),
            || self.base.canonicalize(p),
        )
    }

    fn set_permissions(&self, p: &Path, mode: u32) -> HostResult<()> {
        let _change = self.begin_change_for(&[p]);
        self.base.set_permissions(p, mode)
    }

    fn set_times(
        &self,
        p: &Path,
        atime: Option<FileTime>,
        mtime: Option<FileTime>,
    ) -> HostResult<()> {
        let _change = self.begin_change_for(&[p]);
        self.base.set_times(p, atime, mtime)
    }

    fn set_symlink_times(
        &self,
        p: &Path,
        atime: Option<FileTime>,
        mtime: Option<FileTime>,
    ) -> HostResult<()> {
        let _change = self.begin_change_for(&[p]);
        self.base.set_symlink_times(p, atime, mtime)
    }

    fn env_get(&self, key: &[u8]) -> Option<BString> {
        self.base.env_get(key)
    }

    fn env_set(&self, key: &[u8], value: Option<&[u8]>) {
        self.base.env_set(key, value);
    }

    fn env_all(&self) -> Vec<(BString, BString)> {
        self.base.env_all()
    }

    fn supports_process_execution(&self) -> bool {
        self.base.supports_process_execution()
    }

    fn run(
        &self,
        req: &RunRequest,
        on_output: &mut dyn FnMut(Stream, &[u8]),
    ) -> HostResult<RunOutput> {
        let _change = self.begin_change();
        self.base.run(req, on_output)
    }

    fn run_with_timeout_signal(
        &self,
        req: &RunRequest,
        timeout: GracefulTimeout,
        on_output: &mut dyn FnMut(Stream, &[u8]),
    ) -> HostResult<RunOutput> {
        let _change = self.begin_change();
        self.base.run_with_timeout_signal(req, timeout, on_output)
    }

    fn open_read(&self, p: &Path) -> HostResult<Box<dyn crate::ReadSeek + '_>> {
        self.base.open_read(p)
    }

    fn scratch_file(&self) -> HostResult<Box<dyn crate::ScratchFile + '_>> {
        self.base.scratch_file()
    }

    fn write_from(
        &self,
        p: &Path,
        mode: WriteMode,
        input: &mut dyn std::io::Read,
        len: u64,
    ) -> HostResult<()> {
        let _change = self.begin_change_for(&[p]);
        self.base.write_from(p, mode, input, len)
    }

    fn run_controlled(
        &self,
        req: &RunRequest,
        graceful: Option<GracefulTimeout>,
        hard_limit: Option<Duration>,
        update: &mut crate::DeadlineUpdate<'_>,
    ) -> HostResult<crate::ControlledOutput> {
        let _change = self.begin_change();
        self.base.run_controlled(req, graceful, hard_limit, update)
    }

    fn run_bounded(&self, req: &RunRequest, limit: usize) -> HostResult<RunOutput> {
        let _change = self.begin_change();
        self.base.run_bounded(req, limit)
    }

    crate::no_process::forward_host_services!(base);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{CommandSpec, FileKind, Input, Output, PureHost, TimeoutSignal};

    use crate::HostError;
    use std::sync::Mutex;

    type Observer = Arc<dyn Fn() -> HostResult<()> + Send + Sync>;

    #[derive(Default)]
    struct ProbeHost {
        base: PureHost,
        metadata_error: Mutex<Option<ErrorKind>>,
        observer: Mutex<Option<Observer>>,
    }

    fn fixture_error() -> HostError {
        HostError::unsupported("Cannot access the test host")
    }

    fn map_mount(path: &Path) -> PathBuf {
        path.strip_prefix("/mount")
            .map_or_else(|_| path.to_path_buf(), |rest| Path::new("/work").join(rest))
    }

    impl Host for ProbeHost {
        fn supports_process_execution(&self) -> bool {
            self.base.supports_process_execution()
        }

        fn run(
            &self,
            req: &RunRequest,
            on_output: &mut dyn FnMut(Stream, &[u8]),
        ) -> HostResult<RunOutput> {
            self.base.run(req, on_output)
        }

        fn metadata(&self, p: &Path, follow: Follow) -> HostResult<Metadata> {
            if let Some(kind) = self
                .metadata_error
                .lock()
                .map_err(|_| fixture_error())?
                .take()
            {
                return Err(HostError::from_io(kind.into(), Some(p.to_path_buf())));
            }
            self.base.metadata(&map_mount(p), follow)
        }

        fn read(&self, p: &Path, range: ReadRange) -> HostResult<Vec<u8>> {
            self.base.read(p, range)
        }

        fn read_chunks(&self, p: &Path, consume: &mut dyn FnMut(&[u8])) -> HostResult<()> {
            self.base.read_chunks(p, consume)
        }

        fn write(&self, p: &Path, data: &[u8], mode: WriteMode) -> HostResult<()> {
            self.base.write(&map_mount(p), data, mode)?;
            let observer = self.observer.lock().map_err(|_| fixture_error())?.clone();
            if let Some(observer) = observer {
                observer()?;
            }
            Ok(())
        }

        fn read_dir(&self, p: &Path) -> HostResult<Vec<DirEntry>> {
            if p == Path::new("/large") {
                return Ok(vec![DirEntry {
                    name: vec![b'x'; MAX_CACHE_BYTES].into(),
                    kind: FileKind::File,
                }]);
            }
            self.base.read_dir(&map_mount(p))
        }

        fn create_dir_all(&self, p: &Path) -> HostResult<()> {
            self.base.create_dir_all(p)
        }

        fn remove_file(&self, p: &Path) -> HostResult<()> {
            self.base.remove_file(p)
        }

        fn remove_dir(&self, p: &Path) -> HostResult<()> {
            self.base.remove_dir(p)
        }

        fn remove_dir_all(&self, p: &Path) -> HostResult<()> {
            self.base.remove_dir_all(p)
        }

        fn rename(&self, from: &Path, to: &Path) -> HostResult<()> {
            self.base.rename(from, to)
        }

        fn rename_noreplace(&self, from: &Path, to: &Path) -> HostResult<()> {
            self.base.rename_noreplace(from, to)
        }

        fn lock_file(
            &self,
            path: &Path,
            timeout: Option<Duration>,
        ) -> HostResult<Box<dyn FileLockGuard>> {
            self.base.lock_file(path, timeout)
        }

        fn copy_file(&self, from: &Path, to: &Path, opts: CopyOptions) -> HostResult<()> {
            self.base.copy_file(from, to, opts)
        }

        fn symlink(&self, target: &Path, link: &Path) -> HostResult<()> {
            self.base.symlink(target, link)
        }

        fn hard_link(&self, src: &Path, dst: &Path) -> HostResult<()> {
            self.base.hard_link(src, dst)
        }

        fn read_link(&self, p: &Path) -> HostResult<PathBuf> {
            self.base.read_link(p)
        }

        fn canonicalize(&self, p: &Path) -> HostResult<PathBuf> {
            self.base.canonicalize(&map_mount(p))?;
            Ok(p.to_path_buf())
        }

        fn set_permissions(&self, p: &Path, mode: u32) -> HostResult<()> {
            self.base.set_permissions(p, mode)
        }

        fn set_times(
            &self,
            p: &Path,
            atime: Option<FileTime>,
            mtime: Option<FileTime>,
        ) -> HostResult<()> {
            self.base.set_times(p, atime, mtime)
        }

        fn set_symlink_times(
            &self,
            p: &Path,
            atime: Option<FileTime>,
            mtime: Option<FileTime>,
        ) -> HostResult<()> {
            self.base.set_symlink_times(p, atime, mtime)
        }

        fn env_get(&self, key: &[u8]) -> Option<BString> {
            self.base.env_get(key)
        }

        fn env_set(&self, key: &[u8], value: Option<&[u8]>) {
            self.base.env_set(key, value);
        }

        fn env_all(&self) -> Vec<(BString, BString)> {
            self.base.env_all()
        }

        crate::no_process::forward_host_services!(base);
    }

    #[test]
    fn invalidates_metadata_and_listings_across_mount_aliases() -> HostResult<()> {
        let base = ProbeHost::default();
        base.base.seed_file(Path::new("/work/file"), b"a")?;
        let host = MemoHost::new(base);
        let alias = Path::new("/mount/file");
        let root = Path::new("/mount");
        assert_eq!(host.canonicalize(alias)?, alias);
        let stat = host.metadata(alias, Follow::No)?;
        assert_eq!((stat.len, stat.nlink), (1, 1));
        assert_eq!(host.read_dir(root)?.len(), 1);

        host.write(Path::new("/work/file"), b"changed", WriteMode::Truncate)?;
        assert_eq!(host.metadata(alias, Follow::No)?.len, 7);
        host.write(Path::new("/work/other"), b"new", WriteMode::CreateNew)?;
        assert_eq!(host.read_dir(root)?.len(), 2);
        Ok(())
    }

    #[test]
    fn transient_errors_do_not_enter_the_cache() -> HostResult<()> {
        let base = Arc::new(ProbeHost::default());
        let p = Path::new("/work/file");
        base.base.seed_file(p, b"data")?;
        let host = MemoHost::from_shared(Arc::clone(&base));
        for (kind, follow) in [
            (ErrorKind::Interrupted, Follow::Yes),
            (ErrorKind::PermissionDenied, Follow::No),
        ] {
            *base.metadata_error.lock().map_err(|_| fixture_error())? = Some(kind);
            assert!(matches!(host.metadata(p, follow), Err(err) if err.kind == kind));
            assert_eq!(host.metadata(p, follow)?.len, 4);
        }
        assert_eq!(host.stats().hits, 0);
        Ok(())
    }

    #[test]
    fn limits_entries_and_bypasses_large_listings() -> HostResult<()> {
        let host = MemoHost::new(ProbeHost::default());
        for _ in 0..2 {
            assert_eq!(host.read_dir(Path::new("/large"))?.len(), 1);
        }
        assert_eq!(host.read_cache().entries, 0);
        assert_eq!(host.stats().misses, 2);
        for id in 0..=MAX_CACHE_ENTRIES {
            let p = Path::new("/work").join(id.to_string());
            assert!(host.metadata(&p, Follow::No).is_err());
        }
        assert_eq!(host.read_cache().entries, MAX_CACHE_ENTRIES);
        assert!(host.read_cache().bytes <= MAX_CACHE_BYTES);
        Ok(())
    }

    #[test]
    fn mutation_can_query_the_adapter_before_it_returns() -> HostResult<()> {
        let base = Arc::new(ProbeHost::default());
        let host = Arc::new(MemoHost::from_shared(Arc::clone(&base)));
        let p = Path::new("/work/file");
        assert!(host.metadata(p, Follow::No).is_err());
        let weak = Arc::downgrade(&host);
        *base.observer.lock().map_err(|_| fixture_error())? = Some(Arc::new(move || {
            let host = weak.upgrade().ok_or_else(fixture_error)?;
            assert!(host.cache.try_read().is_ok());
            assert_eq!(host.metadata(p, Follow::No)?.len, 3);
            assert_eq!(host.read_cache().entries, 0);
            Ok(())
        }));
        host.write(p, b"new", WriteMode::CreateNew)?;
        assert_eq!(host.metadata(p, Follow::No)?.len, 3);
        assert_eq!(host.read_cache().entries, 1);
        Ok(())
    }

    #[test]
    fn caches_successes_failures_and_follow_modes() -> HostResult<()> {
        let base = PureHost::default();
        let link = Path::new("/work/link");
        let target = Path::new("/work/file");
        let missing = Path::new("/work/missing");
        base.seed_file(target, b"file")?;
        base.symlink(target, link)?;
        let host = MemoHost::new(base);
        for _ in 0..2 {
            assert_eq!(host.metadata(link, Follow::No)?.kind, FileKind::Symlink);
            assert_eq!(host.metadata(link, Follow::Yes)?.kind, FileKind::File);
            assert!(
                matches!(host.metadata(missing, Follow::Yes), Err(err) if err.kind == ErrorKind::NotFound)
            );
            assert_eq!(host.read_dir(Path::new("/work"))?.len(), 2);
            assert_eq!(host.canonicalize(link)?, target);
        }
        assert_eq!(
            host.stats(),
            MemoStats {
                hits: 5,
                misses: 5,
                invalidations: 0
            }
        );
        Ok(())
    }

    #[test]
    fn refreshes_cached_link_times_after_a_forwarded_update() -> HostResult<()> {
        let base = PureHost::default();
        let link = Path::new("/work/link");
        base.symlink(Path::new("missing"), link)?;
        let host = MemoHost::new(crate::NoProcessHost::new(base));
        let before = host.metadata(link, Follow::No)?;

        let mtime = FileTime { secs: 11, nanos: 5 };
        host.set_symlink_times(link, None, Some(mtime))?;
        let updated = host.metadata(link, Follow::No)?;
        assert_eq!(updated.mtime, mtime);
        assert_eq!(updated.atime, before.atime);
        Ok(())
    }

    #[test]
    fn invalidates_paths_parents_subtrees_and_aliases() -> HostResult<()> {
        let base = PureHost::default();
        let work = Path::new("/work");
        let path = work.join("dir/item");
        let sibling = work.join("sibling");
        base.seed_file(&sibling, b"same")?;
        let host = MemoHost::new(base);

        assert_eq!(host.read_dir(work)?.len(), 1);
        assert!(host.metadata(&path, Follow::Yes).is_err());
        host.metadata(&sibling, Follow::Yes)?;
        host.create_dir_all(&work.join("dir"))?;
        host.write(&path, b"new", WriteMode::Truncate)?;
        assert_eq!(host.read_dir(work)?.len(), 2);
        let before = host.stats();
        host.metadata(&sibling, Follow::Yes)?;
        assert_eq!(host.stats().misses, before.misses + 1);

        let link = work.join("link");
        let hard = work.join("hard");
        host.symlink(&path, &link)?;
        host.hard_link(&path, &hard)?;
        assert_eq!(host.metadata(&path, Follow::Yes)?.len, 3);
        assert_eq!(host.metadata(&link, Follow::Yes)?.len, 3);
        let before = host.stats();
        host.write(&hard, b"er", WriteMode::Append)?;
        assert_eq!(host.metadata(&path, Follow::Yes)?.len, 5);
        assert_eq!(host.metadata(&link, Follow::Yes)?.len, 5);
        assert_eq!(host.stats().invalidations, before.invalidations + 1);

        let renamed = work.join("moved");
        host.rename_noreplace(&work.join("dir"), &renamed)?;
        assert!(host.metadata(&path, Follow::Yes).is_err());
        assert!(host.canonicalize(&link).is_err());
        assert_eq!(host.metadata(&renamed.join("item"), Follow::No)?.len, 5);
        host.remove_dir_all(&renamed)?;
        assert!(host.metadata(&renamed.join("item"), Follow::No).is_err());
        Ok(())
    }

    #[test]
    fn process_methods_clear_the_cache_and_allow_reentrant_queries() -> HostResult<()> {
        let base = PureHost::default();
        let file = Path::new("/work/file");
        base.seed_file(file, b"")?;
        base.set_command_handler(move |base, _, output| {
            base.write(file, b"a", WriteMode::Append)?;
            output(Stream::Stdout, b"update");
            base.write(file, b"b", WriteMode::Append)?;
            Ok(RunOutput::default())
        })?;
        let host = MemoHost::new(base);
        let request = RunRequest {
            pipeline: vec![CommandSpec {
                program: "mock".into(),
                args: Vec::new(),
            }],
            cwd: PathBuf::from("/work"),
            env: Vec::new(),
            stdin: Input::Null,
            stdout: Output::Capture,
            stderr: Output::Capture,
            timeout: None,
            tee: true,
        };
        assert_eq!(host.metadata(file, Follow::Yes)?.len, 0);
        let mut during_run = None;
        host.run(&request, &mut |_, _| {
            during_run = host.metadata(file, Follow::Yes).ok().map(|stat| stat.len);
        })?;
        assert_eq!(during_run, Some(1));
        assert_eq!(host.metadata(file, Follow::Yes)?.len, 2);
        let graceful = GracefulTimeout {
            signal: TimeoutSignal::Terminate,
            grace: Duration::ZERO,
        };
        host.run_with_timeout_signal(&request, graceful, &mut |_, _| {})?;
        assert_eq!(host.metadata(file, Follow::Yes)?.len, 4);
        host.run_bounded(&request, 128)?;
        assert_eq!(host.metadata(file, Follow::Yes)?.len, 6);
        assert_eq!(host.stats().invalidations, 3);
        Ok(())
    }
}
