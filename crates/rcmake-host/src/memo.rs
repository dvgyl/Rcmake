//! A configure-scoped cache for filesystem queries.

#![forbid(unsafe_code)]

use core::time::Duration;
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::io::ErrorKind;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock, RwLockReadGuard, RwLockWriteGuard};

use rcmake_base::BString;

use crate::{
    CopyOptions, DirEntry, FileKind, FileLockGuard, FileTime, Follow, GracefulTimeout, Host,
    HostResult, Metadata, ReadRange, RunOutput, RunRequest, Stream, WriteMode,
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

struct Cached<T> {
    result: HostResult<T>,
    direct: bool,
}

#[derive(Default)]
struct Queries {
    follow: Option<Cached<Metadata>>,
    no_follow: Option<Cached<Metadata>>,
    entries: Option<Cached<Vec<DirEntry>>>,
    canonical: Option<Cached<PathBuf>>,
}

impl Queries {
    fn keep_direct(&mut self) -> bool {
        fn retain<T>(entry: &mut Option<Cached<T>>) {
            if entry.as_ref().is_some_and(|entry| !entry.direct) {
                *entry = None;
            }
        }

        retain(&mut self.follow);
        retain(&mut self.no_follow);
        retain(&mut self.entries);
        retain(&mut self.canonical);
        self.follow.is_some()
            || self.no_follow.is_some()
            || self.entries.is_some()
            || self.canonical.is_some()
    }
}

#[derive(Default)]
struct Cache {
    // Keep the exact spelling. A trailing separator can change a query's result.
    paths: BTreeMap<OsString, Queries>,
    generation: u64,
    unscoped_changes: usize,
}

#[derive(Default)]
struct Affected {
    trees: BTreeSet<PathBuf>,
    parents: BTreeSet<PathBuf>,
}

impl Affected {
    fn add_tree(&mut self, p: &Path) {
        if let Some(parent) = p.parent() {
            self.parents.insert(parent.to_path_buf());
        }
        self.trees.insert(p.to_path_buf());
    }

    fn contains(&self, p: &Path) -> bool {
        self.parents.contains(p) || self.trees.iter().any(|root| p.starts_with(root))
    }

    fn add_path(&mut self, base: &(impl Host + ?Sized), p: &Path) -> Option<()> {
        if !p.has_root() || p.components().any(|part| part == Component::ParentDir) {
            return None;
        }
        self.add_tree(p);
        match base.canonicalize(p) {
            Ok(canonical) => self.add_tree(&canonical),
            Err(err) if err.kind == ErrorKind::NotFound => {}
            Err(_) => return None,
        }
        // Use the first missing component so create_dir_all also invalidates
        // the listing of the closest existing parent.
        let mut child = p;
        for parent in p.ancestors().skip(1) {
            match base.canonicalize(parent) {
                Ok(directory) => {
                    self.add_tree(&directory.join(child.file_name()?));
                    return Some(());
                }
                Err(err) if err.kind == ErrorKind::NotFound => child = parent,
                Err(_) => return None,
            }
        }
        Some(())
    }
}

/// Memoizes metadata, directory listings, and canonical paths for one configure.
///
/// The cache includes failed queries. Mutations through this adapter invalidate the
/// affected entries. A command or file lock can change paths outside that scope.
/// The adapter bypasses the cache while those calls are active.
///
/// Use a new adapter when the base may have changed outside this adapter.
/// File access timestamps reflect the time when the adapter cached the metadata.
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
        report: bool,
        get: impl FnOnce(&Queries) -> Option<&Cached<T>>,
        store: impl FnOnce(&mut Queries, Cached<T>),
        query: impl FnOnce() -> Cached<T>,
    ) -> HostResult<T> {
        let generation = {
            let cache = self.read_cache();
            if let Some(entry) = cache.paths.get(p.as_os_str()).and_then(get) {
                if report {
                    self.hits.fetch_add(1, Ordering::Relaxed);
                }
                return entry.result.clone();
            }
            cache.generation
        };
        if report {
            self.misses.fetch_add(1, Ordering::Relaxed);
        }
        let entry = query();
        let mut cache = self.write_cache();
        // A concurrent mutation can finish while the base query is in progress.
        if cache.generation != generation || cache.unscoped_changes != 0 {
            return entry.result;
        }
        let result = entry.result.clone();
        store(
            cache.paths.entry(p.as_os_str().to_owned()).or_default(),
            entry,
        );
        result
    }

    fn canonical(&self, p: &Path, report: bool) -> HostResult<PathBuf> {
        self.lookup(
            p,
            report,
            |queries| queries.canonical.as_ref(),
            |queries, entry| queries.canonical = Some(entry),
            || {
                let result = self.base.canonicalize(p);
                let direct = result
                    .as_ref()
                    .is_ok_and(|path| path.as_os_str() == p.as_os_str());
                Cached { direct, result }
            },
        )
    }

    fn direct(&self, p: &Path) -> bool {
        self.canonical(p, false)
            .is_ok_and(|canonical| canonical.as_os_str() == p.as_os_str())
    }

    fn change<T>(
        &self,
        paths: &[&Path],
        operation: impl FnOnce() -> HostResult<T>,
    ) -> HostResult<T> {
        let mut cache = self.write_cache();
        // On a panic the next reader must discard the cache as well.
        cache.generation = cache.generation.wrapping_add(1);
        self.invalidations.fetch_add(1, Ordering::Relaxed);
        let mut affected = Affected::default();
        let mut entries = std::mem::take(&mut cache.paths);
        let scoped = !entries.is_empty()
            && paths
                .iter()
                .all(|p| affected.add_path(self.base.as_ref(), p).is_some());
        let result = operation();
        if scoped
            && paths
                .iter()
                .all(|p| affected.add_path(self.base.as_ref(), p).is_some())
        {
            // An alias or a failed lookup can depend on a symlink outside this scope.
            entries.retain(|p, queries| !affected.contains(Path::new(p)) && queries.keep_direct());
            cache.paths = entries;
        }
        result
    }

    fn begin_unscoped_change(&self) -> UnscopedChange<'_, H> {
        let mut cache = self.write_cache();
        cache.generation = cache.generation.wrapping_add(1);
        cache.unscoped_changes += 1;
        cache.paths.clear();
        UnscopedChange { host: self }
    }
}

struct UnscopedChange<'a, H: Host + ?Sized> {
    host: &'a MemoHost<H>,
}

impl<H: Host + ?Sized> Drop for UnscopedChange<'_, H> {
    fn drop(&mut self) {
        let mut cache = self.host.write_cache();
        cache.unscoped_changes -= 1;
        cache.generation = cache.generation.wrapping_add(1);
        self.host.invalidations.fetch_add(1, Ordering::Relaxed);
    }
}

impl<H: Host + ?Sized> Host for MemoHost<H> {
    fn metadata(&self, p: &Path, follow: Follow) -> HostResult<Metadata> {
        self.lookup(
            p,
            true,
            |queries| match follow {
                Follow::Yes => queries.follow.as_ref(),
                Follow::No => queries.no_follow.as_ref(),
            },
            |queries, entry| match follow {
                Follow::Yes => queries.follow = Some(entry),
                Follow::No => queries.no_follow = Some(entry),
            },
            || {
                let result = self.base.metadata(p, follow);
                let direct = result.as_ref().is_ok_and(|stat| {
                    (stat.kind == FileKind::Dir || stat.nlink == 1)
                        && stat.kind != FileKind::Symlink
                        && self.direct(p)
                });
                Cached { direct, result }
            },
        )
    }

    fn read(&self, p: &Path, range: ReadRange) -> HostResult<Vec<u8>> {
        self.base.read(p, range)
    }

    fn read_chunks(&self, p: &Path, consume: &mut dyn FnMut(&[u8])) -> HostResult<()> {
        self.base.read_chunks(p, consume)
    }

    fn write(&self, p: &Path, data: &[u8], mode: WriteMode) -> HostResult<()> {
        self.change(&[p], || self.base.write(p, data, mode))
    }

    fn read_dir(&self, p: &Path) -> HostResult<Vec<DirEntry>> {
        self.lookup(
            p,
            true,
            |queries| queries.entries.as_ref(),
            |queries, entry| queries.entries = Some(entry),
            || {
                let result = self.base.read_dir(p);
                let direct = result.is_ok() && self.direct(p);
                Cached { direct, result }
            },
        )
    }

    fn create_dir_all(&self, p: &Path) -> HostResult<()> {
        self.change(&[p], || self.base.create_dir_all(p))
    }

    fn remove_file(&self, p: &Path) -> HostResult<()> {
        self.change(&[p], || self.base.remove_file(p))
    }

    fn remove_dir(&self, p: &Path) -> HostResult<()> {
        self.change(&[p], || self.base.remove_dir(p))
    }

    fn remove_dir_all(&self, p: &Path) -> HostResult<()> {
        self.change(&[p], || self.base.remove_dir_all(p))
    }

    fn rename(&self, from: &Path, to: &Path) -> HostResult<()> {
        self.change(&[from, to], || self.base.rename(from, to))
    }

    fn rename_noreplace(&self, from: &Path, to: &Path) -> HostResult<()> {
        self.change(&[from, to], || self.base.rename_noreplace(from, to))
    }

    fn lock_file(
        &self,
        path: &Path,
        timeout: Option<Duration>,
    ) -> HostResult<Box<dyn FileLockGuard>> {
        let _change = self.begin_unscoped_change();
        self.base.lock_file(path, timeout)
    }

    fn copy_file(&self, from: &Path, to: &Path, opts: CopyOptions) -> HostResult<()> {
        self.change(&[from, to], || self.base.copy_file(from, to, opts))
    }

    fn symlink(&self, target: &Path, link: &Path) -> HostResult<()> {
        self.change(&[link], || self.base.symlink(target, link))
    }

    fn hard_link(&self, src: &Path, dst: &Path) -> HostResult<()> {
        self.change(&[src, dst], || self.base.hard_link(src, dst))
    }

    fn read_link(&self, p: &Path) -> HostResult<PathBuf> {
        self.base.read_link(p)
    }

    fn canonicalize(&self, p: &Path) -> HostResult<PathBuf> {
        self.canonical(p, true)
    }

    fn set_permissions(&self, p: &Path, mode: u32) -> HostResult<()> {
        self.change(&[p], || self.base.set_permissions(p, mode))
    }

    fn set_times(
        &self,
        p: &Path,
        atime: Option<FileTime>,
        mtime: Option<FileTime>,
    ) -> HostResult<()> {
        self.change(&[p], || self.base.set_times(p, atime, mtime))
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

    fn run(
        &self,
        req: &RunRequest,
        on_output: &mut dyn FnMut(Stream, &[u8]),
    ) -> HostResult<RunOutput> {
        let _change = self.begin_unscoped_change();
        self.base.run(req, on_output)
    }

    fn run_with_timeout_signal(
        &self,
        req: &RunRequest,
        timeout: GracefulTimeout,
        on_output: &mut dyn FnMut(Stream, &[u8]),
    ) -> HostResult<RunOutput> {
        let _change = self.begin_unscoped_change();
        self.base.run_with_timeout_signal(req, timeout, on_output)
    }

    fn run_bounded(&self, req: &RunRequest, limit: usize) -> HostResult<RunOutput> {
        let _change = self.begin_unscoped_change();
        self.base.run_bounded(req, limit)
    }

    crate::no_process::forward_host_services!(base);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{CommandSpec, Input, Output, PureHost, TimeoutSignal};

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
                hits: 6,
                misses: 4,
                invalidations: 0
            }
        );
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
        assert_eq!(host.stats().hits, before.hits + 1);

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
