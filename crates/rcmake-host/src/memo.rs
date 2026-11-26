//! A configure-scoped cache for filesystem queries.

#![forbid(unsafe_code)]

mod scope;

use core::time::Duration;
use std::collections::{BTreeMap, HashSet};
use std::ffi::OsString;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock, RwLockReadGuard, RwLockWriteGuard};

use crate::{
    CopyOptions, DirEntry, FileLockGuard, FileTime, Follow, GracefulTimeout, Host, HostError,
    HostResult, Metadata, RunOutput, RunRequest, Stream, WriteMode,
};
use scope::{Affected, Dirs, ScopeState};

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
    /// The number of filled slots, which the cache entry count includes.
    count: usize,
    /// The bytes that the slots charge to the cache byte count.
    bytes: usize,
    /// The objects that the queries of this path depend on.
    scope: ScopeState,
}

const MAX_DIRECTORY_NAMES: usize = 4096;
const MAX_DIRECTORY_BYTES: usize = 1024 * 1024;
const LIST_AFTER_MISSES: u8 = 8;

struct DirFacts {
    folded: HashSet<Box<[u8]>>,
    max_name_len: usize,
    missing: HostError,
    /// The bytes that these facts charge to the directory byte count.
    bytes: usize,
    /// The objects that the listing of this directory depends on.
    scope: ScopeState,
}

impl DirFacts {
    fn absent(&self, name: &[u8]) -> bool {
        name.len() <= self.max_name_len
            && if name.iter().any(u8::is_ascii_uppercase) {
                !self.folded.contains(name.to_ascii_lowercase().as_slice())
            } else {
                !self.folded.contains(name)
            }
    }

    fn missing(&self, path: &Path) -> HostError {
        HostError {
            path: Some(path.to_owned()),
            ..self.missing.clone()
        }
    }
}

/// A directory listing that a miss claimed under the write guard.
struct ListingClaim {
    parent: PathBuf,
    /// A cached successful listing of the parent, copied under the guard.
    cached: Option<Vec<DirEntry>>,
    missing: HostError,
}

/// Return `true` for a filesystem that can expose names that are absent from a listing.
fn unlisted_filesystem(path: &Path) -> bool {
    ["/proc", "/dev", "/sys", "/net"]
        .iter()
        .any(|prefix| path.starts_with(prefix))
}

fn negative_leaf(path: &Path) -> Option<(&Path, &[u8])> {
    if !cfg!(unix) || !path.is_absolute() {
        return None;
    }
    let raw = path.as_os_str().as_encoded_bytes();
    if raw.len() >= 1024 || raw.ends_with(b"/") || raw.ends_with(b"/.") || raw.ends_with(b"/..") {
        return None;
    }
    let name = path.file_name()?.as_encoded_bytes();
    (name.is_ascii() && !name.contains(&0)).then_some((path.parent()?, name))
}

#[derive(Default)]
struct Cache {
    // Keep exact spelling. A trailing separator can change the result.
    paths: BTreeMap<OsString, Queries>,
    entries: usize,
    bytes: usize,
    generation: u64,
    active_changes: usize,
    tracked: BTreeMap<PathBuf, Tracked>,
    directory_facts: BTreeMap<OsString, DirFacts>,
    directory_misses: BTreeMap<OsString, u8>,
    directory_bytes: usize,
    /// Directory identity chains for path-scoped invalidation.
    dirs: Dirs,
    /// The number of changes that started, which identifies each change.
    started: u64,
}

/// Cached state that a change takes out of the cache while it runs without the lock.
#[derive(Default)]
struct Taken {
    paths: BTreeMap<OsString, Queries>,
    entries: usize,
    bytes: usize,
    tracked: BTreeMap<PathBuf, Tracked>,
    directory_facts: BTreeMap<OsString, DirFacts>,
    directory_bytes: usize,
    dirs: Dirs,
}

/// The generation of one tracked canonical path.
struct Tracked {
    revision: u64,
    /// The objects that the path and its subtree depend on.
    scope: ScopeState,
}

/// The possible effect of one host operation.
#[derive(Clone, Copy)]
enum Effect<'a> {
    /// The operation can change any path.
    Global,
    /// The operation changes only these names and the objects that they name.
    /// Queries that depend on none of these objects stay in the cache.
    Names(&'a [&'a Path]),
    /// A process can write below these paths. All queries leave the cache,
    /// and only overlapping tracked paths get a new generation.
    Process(&'a [&'a Path]),
}

impl Cache {
    /// Move the cached state out of the cache. Clear the listing miss counters.
    fn take(&mut self) -> Taken {
        self.directory_misses.clear();
        Taken {
            paths: std::mem::take(&mut self.paths),
            entries: std::mem::take(&mut self.entries),
            bytes: std::mem::take(&mut self.bytes),
            tracked: std::mem::take(&mut self.tracked),
            directory_facts: std::mem::take(&mut self.directory_facts),
            directory_bytes: std::mem::take(&mut self.directory_bytes),
            dirs: std::mem::take(&mut self.dirs),
        }
    }

    /// Merge state back into the cache. Other changes can have merged state too.
    fn restore(&mut self, taken: Taken) {
        for (path, queries) in taken.paths {
            if let std::collections::btree_map::Entry::Vacant(slot) = self.paths.entry(path) {
                self.entries += queries.count;
                self.bytes += queries.bytes;
                slot.insert(queries);
            }
        }
        for (path, facts) in taken.directory_facts {
            if let std::collections::btree_map::Entry::Vacant(slot) =
                self.directory_facts.entry(path)
            {
                self.directory_bytes += facts.bytes;
                slot.insert(facts);
            }
        }
        for (path, tracked) in taken.tracked {
            match self.tracked.entry(path) {
                std::collections::btree_map::Entry::Vacant(slot) => {
                    slot.insert(tracked);
                }
                std::collections::btree_map::Entry::Occupied(mut slot) => {
                    let current = slot.get_mut();
                    current.revision = current.revision.max(tracked.revision);
                    current.scope = ScopeState::Pending;
                }
            }
        }
        self.dirs.merge(taken.dirs);
    }
}

impl Taken {
    /// Remove every query and directory fact.
    fn clear_queries(&mut self) {
        self.paths.clear();
        self.directory_facts.clear();
        self.dirs.clear();
        self.entries = 0;
        self.bytes = 0;
        self.directory_bytes = 0;
    }

    /// Remove only the queries that can depend on an affected object.
    fn retain_unaffected<H: Host + ?Sized>(&mut self, base: &H, affected: &Affected) {
        let Self {
            paths,
            entries,
            bytes,
            directory_facts,
            directory_bytes,
            dirs,
            ..
        } = self;
        paths.retain(|path, queries| {
            let keep = queries
                .scope
                .survives(dirs, base, Path::new(path), affected, false);
            if !keep {
                *entries -= queries.count;
                *bytes -= queries.bytes;
            }
            keep
        });
        directory_facts.retain(|path, facts| {
            let keep = facts
                .scope
                .survives(dirs, base, Path::new(path), affected, false);
            if !keep {
                *directory_bytes -= facts.bytes;
            }
            keep
        });
        // Remove the chains last, because the scopes above can read them.
        dirs.retain_unaffected(affected);
    }
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
/// limits. A filesystem change through this adapter removes the queries that
/// depend on a changed object. The device and inode of each object identify it,
/// so aliases of one object compare equal. Recursive removal, file locks,
/// process calls, and objects without an identity clear all queries.
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
    measure: bool,
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
        let measure = base
            .env_get(b"RCMAKE_MEASURE_MEMO")
            .is_some_and(|value| value == "1");
        Self {
            base,
            cache: RwLock::default(),
            hits: AtomicU64::new(0),
            misses: AtomicU64::new(0),
            invalidations: AtomicU64::new(0),
            measure,
        }
    }

    /// Return the host that this adapter wraps. Queries through it bypass the cache.
    #[must_use]
    pub fn base(&self) -> &Arc<H> {
        &self.base
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
        if let Some(tracked) = cache.tracked.get(path) {
            return Some(tracked.revision);
        }
        if cache.tracked.len() >= 2048 {
            return None;
        }
        let generation = cache.generation;
        cache.tracked.insert(
            path.to_owned(),
            Tracked {
                revision: generation,
                scope: ScopeState::Pending,
            },
        );
        Some(generation)
    }

    /// Invalidate queries after changes made through another host handle.
    pub fn clear(&self) {
        drop(self.begin_change(Effect::Global));
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

    /// Count a native not-found miss under the write guard. On the eighth miss in a
    /// directory, claim the listing and copy a cached listing result.
    ///
    /// The caller runs the claim after it drops the guard.
    fn claim_directory_listing(
        &self,
        cache: &mut Cache,
        parent: &Path,
        path: &Path,
        error: &HostError,
    ) -> Option<ListingClaim> {
        if error.kind != ErrorKind::NotFound
            || cache.directory_facts.contains_key(parent.as_os_str())
            || cache.directory_bytes >= MAX_DIRECTORY_BYTES
        {
            return None;
        }
        let expected = crate::error::SyntheticError::NotFound.at(Some(path));
        if *error != expected {
            return None;
        }
        if cache.directory_misses.len() >= MAX_CACHE_ENTRIES {
            return None;
        }
        let count = cache
            .directory_misses
            .entry(parent.as_os_str().to_owned())
            .or_default();
        *count = count.saturating_add(1);
        if *count != LIST_AFTER_MISSES {
            return None;
        }
        let cached = match cache
            .paths
            .get(parent.as_os_str())
            .and_then(|q| q.entries.as_ref())
        {
            Some(Ok(entries)) => Some(entries.clone()),
            Some(Err(_)) => return None,
            None => None,
        };
        Some(ListingClaim {
            parent: parent.to_owned(),
            cached,
            missing: expected,
        })
    }

    /// Build the directory facts of a claim without a lock, then publish them only if no
    /// change happened since `generation` and the facts still fit.
    fn publish_directory_facts(&self, generation: u64, claim: ListingClaim) {
        let Some((facts, bytes)) = self.directory_facts(&claim.parent, claim.cached, claim.missing)
        else {
            return;
        };
        let mut cache = self.write_cache();
        if cache.generation == generation
            && cache.active_changes == 0
            && !cache.directory_facts.contains_key(claim.parent.as_os_str())
            && bytes <= MAX_DIRECTORY_BYTES.saturating_sub(cache.directory_bytes)
        {
            cache.directory_bytes += bytes;
            cache
                .directory_facts
                .insert(claim.parent.into_os_string(), DirFacts { bytes, ..facts });
        }
    }

    fn directory_facts(
        &self,
        parent: &Path,
        cached: Option<Vec<DirEntry>>,
        mut missing: HostError,
    ) -> Option<(DirFacts, usize)> {
        if unlisted_filesystem(parent) {
            return None;
        }
        let resolved = self.base.canonicalize(parent).ok()?;
        if unlisted_filesystem(&resolved) {
            return None;
        }
        let entries = if let Some(cached) = cached {
            cached
        } else {
            self.misses.fetch_add(1, Ordering::Relaxed);
            self.base.read_dir(parent).ok()?
        };
        let entries = &entries;
        if entries.len() > MAX_DIRECTORY_NAMES || entries.iter().any(|entry| !entry.name.is_ascii())
        {
            return None;
        }
        let bytes = listing_bytes(entries)
            .saturating_add(path_bytes(parent))
            .saturating_add(size_of::<DirFacts>());
        // Use an observed length or the minimum filename length that POSIX supports.
        let max_name_len = entries
            .iter()
            .map(|entry| entry.name.len())
            .max()
            .unwrap_or(14)
            .max(14);
        let folded = entries
            .iter()
            .map(|entry| entry.name.to_ascii_lowercase().into_boxed_slice())
            .collect();
        missing.path = None;
        Some((
            DirFacts {
                folded,
                max_name_len,
                missing,
                bytes: 0,
                scope: ScopeState::Pending,
            },
            bytes,
        ))
    }

    fn lookup<T: Clone>(
        &self,
        p: &Path,
        get: impl Fn(&Queries) -> &Option<HostResult<T>>,
        slot: impl FnOnce(&mut Queries) -> &mut Option<HostResult<T>>,
        retained_bytes: impl FnOnce(&T) -> usize,
        query: impl FnOnce() -> HostResult<T>,
        listing_negatives: bool,
    ) -> HostResult<T> {
        let negative = listing_negatives.then(|| negative_leaf(p)).flatten();
        let generation = {
            let cache = self.read_cache();
            // A change can be in progress on another thread. Use no cached
            // answer until it completes.
            if cache.active_changes == 0 {
                if let Some((parent, name)) = negative
                    && let Some(facts) = cache.directory_facts.get(parent.as_os_str())
                    && facts.absent(name)
                {
                    self.hits.fetch_add(1, Ordering::Relaxed);
                    return Err(facts.missing(p));
                }
                if let Some(entry) = cache.paths.get(p.as_os_str()).and_then(|q| get(q).as_ref()) {
                    self.hits.fetch_add(1, Ordering::Relaxed);
                    return entry.clone();
                }
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
        let mut guard = self.write_cache();
        let cache = &mut *guard;
        let mut claim = None;
        if cache.generation == generation
            && cache.active_changes == 0
            && cache.entries < MAX_CACHE_ENTRIES
            && bytes <= MAX_CACHE_BYTES - cache.bytes
        {
            if let (Some((parent, _)), Err(error)) = (negative, &result) {
                claim = self.claim_directory_listing(cache, parent, p, error);
            }
            let queries = cache.paths.entry(p.as_os_str().to_owned()).or_default();
            let filled = slot(queries);
            if filled.is_none() {
                *filled = Some(result.clone());
                queries.count += 1;
                queries.bytes += bytes;
                cache.entries += 1;
                cache.bytes += bytes;
            }
        }
        // List the directory without the guard, so that other readers can proceed.
        drop(guard);
        if let Some(claim) = claim {
            self.publish_directory_facts(generation, claim);
        }
        result
    }

    fn begin_change_for(&self, paths: &[&Path]) -> ActiveChange<'_, H> {
        self.begin_change(Effect::Names(paths))
    }

    /// Start a change. Queries bypass the cache until the change ends.
    ///
    /// The change takes the cached state out of the cache, so that it can query
    /// the base without the lock. It merges the unaffected state back only if no
    /// other change started meanwhile. Overlapping changes invalidate everything.
    fn begin_change(&self, effect: Effect<'_>) -> ActiveChange<'_, H> {
        let (ticket, generation, overlap, mut taken) = {
            let mut cache = self.write_cache();
            cache.generation = cache.generation.wrapping_add(1);
            cache.started = cache.started.wrapping_add(1);
            let overlap = cache.active_changes != 0;
            cache.active_changes += 1;
            (cache.started, cache.generation, overlap, cache.take())
        };
        let paths: &[&Path] = match effect {
            Effect::Global => &[],
            Effect::Names(paths) | Effect::Process(paths) => paths,
        };
        let base = self.base.as_ref();
        // Find the affected objects before the change. Unknown identities
        // give `None`, which removes every query.
        let affected = match effect {
            _ if overlap || paths.is_empty() => None,
            Effect::Global => None,
            Effect::Process(_) if taken.tracked.is_empty() => None,
            Effect::Names(_) | Effect::Process(_) => paths
                .iter()
                .map(|path| taken.dirs.affected(base, path))
                .collect::<Option<Vec<_>>>()
                .map(Affected::merge),
        };
        if !taken.tracked.is_empty() {
            let canonical: Option<Vec<_>> = if affected.is_some() {
                paths.iter().map(|path| self.change_path(path)).collect()
            } else {
                None
            };
            let Taken { tracked, dirs, .. } = &mut taken;
            for (root, entry) in tracked.iter_mut() {
                let overlaps = |path: &Path| path.starts_with(root) || root.starts_with(path);
                let changed = affected.as_ref().is_none_or(|affected| {
                    paths.iter().any(|path| overlaps(path))
                        || canonical
                            .as_ref()
                            .is_none_or(|paths| paths.iter().any(|path| overlaps(path)))
                        || !entry.scope.survives(dirs, base, root, affected, true)
                });
                if changed {
                    entry.revision = generation;
                    entry.scope = ScopeState::Pending;
                }
            }
        }
        match (effect, &affected) {
            (Effect::Names(_), Some(affected)) => taken.retain_unaffected(base, affected),
            _ => taken.clear_queries(),
        }
        let mut cache = self.write_cache();
        if cache.started != ticket {
            // Another change started while this one held the state. Keep no
            // query, and give every tracked path a new generation.
            taken.clear_queries();
            let generation = cache.generation;
            for entry in taken.tracked.values_mut() {
                entry.revision = generation;
                entry.scope = ScopeState::Pending;
            }
        }
        cache.restore(taken);
        drop(cache);
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

impl<H: Host + ?Sized> Drop for MemoHost<H> {
    fn drop(&mut self) {
        if self.measure {
            let MemoStats {
                hits,
                misses,
                invalidations,
            } = self.stats();
            self.base.write_out(
                Stream::Stderr,
                format!("RCMAKE_MEASURE_MEMO hits={hits} misses={misses} invalidations={invalidations}\n")
                    .as_bytes(),
            );
        }
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
            true,
        )
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
            false,
        )
    }

    fn create_dir_all(&self, p: &Path) -> HostResult<()> {
        let _change = self.begin_change_for(&[p]);
        self.base.create_dir_all(p)
    }

    fn metadata_fresh(&self, p: &Path, follow: Follow) -> HostResult<Metadata> {
        self.base.metadata_fresh(p, follow)
    }

    fn open_read_with_metadata(
        &self,
        p: &Path,
    ) -> HostResult<(Box<dyn crate::ReadSeek + '_>, Option<Metadata>)> {
        self.base.open_read_with_metadata(p)
    }

    fn directory_times_track_names(&self) -> bool {
        self.base.directory_times_track_names()
    }

    fn canonicalize_fresh(&self, p: &Path) -> HostResult<PathBuf> {
        self.base.canonicalize_fresh(p)
    }

    fn create_private_dir(&self, p: &Path) -> HostResult<()> {
        let _change = self.begin_change_for(&[p]);
        self.base.create_private_dir(p)
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
        // Removal of a subtree changes the link counts of hard links outside it.
        let _change = self.begin_change(Effect::Process(&[p]));
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
        // Another process can change any file before it releases the lock.
        let _change = self.begin_change(Effect::Global);
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
        // A new link changes the link count of the source object.
        let _change = self.begin_change_for(&[src, dst]);
        self.base.hard_link(src, dst)
    }

    fn canonicalize(&self, p: &Path) -> HostResult<PathBuf> {
        self.lookup(
            p,
            |queries| &queries.canonical,
            |queries| &mut queries.canonical,
            |path| path_bytes(path),
            || self.base.canonicalize(p),
            true,
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

    fn run(
        &self,
        req: &RunRequest,
        on_output: &mut dyn FnMut(Stream, &[u8]),
    ) -> HostResult<RunOutput> {
        let _change = self.begin_change(Effect::Global);
        self.base.run(req, on_output)
    }

    fn run_confined(
        &self,
        req: &RunRequest,
        scope: crate::WriteScope<'_>,
        on_output: &mut dyn FnMut(Stream, &[u8]),
    ) -> HostResult<RunOutput> {
        // A declared scope does not prove where links and hard links lead.
        let _change = self.begin_change(Effect::Global);
        self.base.run_confined(req, scope, on_output)
    }

    fn run_bounded_confined(
        &self,
        req: &RunRequest,
        limit: usize,
        scope: crate::WriteScope<'_>,
    ) -> HostResult<RunOutput> {
        let _change = self.begin_change(Effect::Global);
        self.base.run_bounded_confined(req, limit, scope)
    }

    fn run_with_timeout_signal(
        &self,
        req: &RunRequest,
        timeout: GracefulTimeout,
        on_output: &mut dyn FnMut(Stream, &[u8]),
    ) -> HostResult<RunOutput> {
        let _change = self.begin_change(Effect::Global);
        self.base.run_with_timeout_signal(req, timeout, on_output)
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
        let _change = self.begin_change(Effect::Global);
        self.base.run_controlled(req, graceful, hard_limit, update)
    }

    fn run_bounded(&self, req: &RunRequest, limit: usize) -> HostResult<RunOutput> {
        let _change = self.begin_change(Effect::Global);
        self.base.run_bounded(req, limit)
    }

    crate::forward_host!(
        base,
        env,
        services,
        read,
        read_with_origin,
        read_chunks,
        read_link,
        open_read,
        scratch_file,
        scratch_file_in,
        supports_process_execution,
        open_jobserver,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{CommandSpec, FileKind, Input, Output, PureHost, ReadRange, TimeoutSignal};
    use rcmake_base::BString;

    use crate::HostError;
    use std::sync::Mutex;

    type Observer = Arc<dyn Fn() -> HostResult<()> + Send + Sync>;

    #[derive(Default)]
    struct ProbeHost {
        base: PureHost,
        metadata_error: Mutex<Option<ErrorKind>>,
        observer: Mutex<Option<Observer>>,
        listing_gate: Mutex<Option<Arc<Gate>>>,
    }

    /// Pause a directory listing until the test releases it.
    #[derive(Default)]
    struct Gate {
        /// Whether a listing entered the gate, and whether the test released it.
        state: Mutex<(bool, bool)>,
        changed: std::sync::Condvar,
    }

    impl Gate {
        fn pass(&self) {
            let mut state = self.state.lock().unwrap();
            state.0 = true;
            self.changed.notify_all();
            while !state.1 {
                state = self.changed.wait(state).unwrap();
            }
        }

        #[cfg(unix)]
        fn wait_until_entered(&self) {
            let mut state = self.state.lock().unwrap();
            while !state.0 {
                state = self.changed.wait(state).unwrap();
            }
        }

        #[cfg(unix)]
        fn release(&self) {
            self.state.lock().unwrap().1 = true;
            self.changed.notify_all();
        }
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
            let gate = self
                .listing_gate
                .lock()
                .map_err(|_| fixture_error())?
                .clone();
            if let Some(gate) = gate {
                gate.pass();
            }
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

        crate::forward_host!(base, services);
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
            let p = Path::new("/work").join(format!("é{id}"));
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
        // Changes to other names leave the sibling answer in the cache.
        let before = host.stats();
        assert_eq!(host.metadata(&sibling, Follow::Yes)?.len, 4);
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
    #[test]
    #[cfg(unix)]
    fn directory_negatives_match_native_enoent_and_expire_on_create() -> HostResult<()> {
        let base = Arc::new(crate::NativeHost::new()?);
        let temp = tempfile::tempdir().map_err(|error| HostError::from_io(error, None))?;
        let root = base.canonicalize(temp.path())?;
        let present = root.join("KIND");
        base.write(&present, b"exists", WriteMode::CreateNew)?;
        let memo = MemoHost::from_shared(base.clone());
        for n in 0..LIST_AFTER_MISSES {
            let path = root.join(format!("missing{n}"));
            assert_eq!(
                memo.metadata(&path, Follow::No),
                base.metadata(&path, Follow::No)
            );
        }
        let missing = root.join("libsample");
        let before = memo.stats();
        assert_eq!(
            memo.metadata(&missing, Follow::Yes),
            base.metadata(&missing, Follow::Yes)
        );
        assert_eq!(
            memo.metadata(&missing, Follow::No),
            base.metadata(&missing, Follow::No)
        );
        assert_eq!(memo.canonicalize(&missing), base.canonicalize(&missing));
        assert_eq!(memo.stats().hits, before.hits + 3);
        assert_eq!(
            memo.metadata(&root.join("kind"), Follow::Yes),
            base.metadata(&root.join("kind"), Follow::Yes)
        );
        assert_eq!(memo.metadata(&present, Follow::No)?.len, 6);
        memo.write(&missing, b"found", WriteMode::CreateNew)?;
        assert_eq!(memo.metadata(&missing, Follow::Yes)?.len, 5);
        Ok(())
    }

    #[test]
    #[cfg(unix)]
    fn directory_listing_runs_without_the_lock_and_cannot_publish_stale_facts() -> HostResult<()> {
        use std::sync::mpsc;
        let dir = Path::new("/work/dir");
        let base = Arc::new(ProbeHost::default());
        base.base.seed_file(&dir.join("present"), b"x")?;
        let gate = Arc::new(Gate::default());
        *base.listing_gate.lock().map_err(|_| fixture_error())? = Some(Arc::clone(&gate));
        let memo = Arc::new(MemoHost::from_shared(base));

        // The eighth miss in the directory starts a listing, which the gate pauses.
        let lister = {
            let memo = Arc::clone(&memo);
            std::thread::spawn(move || {
                for n in 0..LIST_AFTER_MISSES {
                    let _ = memo.metadata(&dir.join(format!("missing{n}")), Follow::No);
                }
            })
        };
        gate.wait_until_entered();

        // Another reader proceeds while the listing is paused.
        let (sent, received) = mpsc::channel();
        let reader = {
            let memo = Arc::clone(&memo);
            std::thread::spawn(move || {
                let _ = sent.send(memo.metadata(&dir.join("present"), Follow::No).is_ok());
            })
        };
        assert_eq!(received.recv_timeout(Duration::from_secs(5)), Ok(true));
        reader.join().unwrap();

        // A file created during the listing must not be hidden by the stale listing.
        memo.write(&dir.join("late"), b"y", WriteMode::CreateNew)?;
        gate.release();
        lister.join().unwrap();
        assert!(memo.metadata(&dir.join("late"), Follow::No).is_ok());
        Ok(())
    }

    /// Compare every cached answer for `paths` with a fresh base query.
    #[cfg(unix)]
    fn assert_matches_base<H: Host>(memo: &MemoHost<H>, base: &H, paths: &[PathBuf]) {
        // Reads can change the access time, so compare everything else.
        fn stat(result: HostResult<Metadata>) -> Result<Metadata, ErrorKind> {
            result
                .map(|metadata| Metadata {
                    atime: FileTime::default(),
                    ..metadata
                })
                .map_err(|error| error.kind)
        }
        fn listing(
            result: HostResult<Vec<DirEntry>>,
        ) -> Result<Vec<(BString, FileKind)>, ErrorKind> {
            result
                .map(|entries| {
                    let mut names: Vec<_> = entries.into_iter().map(|e| (e.name, e.kind)).collect();
                    names.sort_by(|left, right| left.0.cmp(&right.0));
                    names
                })
                .map_err(|error| error.kind)
        }
        for path in paths {
            for follow in [Follow::Yes, Follow::No] {
                assert_eq!(
                    stat(memo.metadata(path, follow)),
                    stat(base.metadata(path, follow)),
                    "{} {follow:?}",
                    path.display()
                );
            }
            assert_eq!(
                listing(memo.read_dir(path)),
                listing(base.read_dir(path)),
                "{}",
                path.display()
            );
            assert_eq!(
                memo.canonicalize(path).map_err(|error| error.kind),
                base.canonicalize(path).map_err(|error| error.kind),
                "{}",
                path.display()
            );
        }
    }

    /// Return the cache charges recounted from the retained records.
    fn recount<H: Host + ?Sized>(memo: &MemoHost<H>) -> (usize, usize, usize) {
        let cache = memo.read_cache();
        let entries = cache.paths.values().map(|queries| queries.count).sum();
        let bytes = cache.paths.values().map(|queries| queries.bytes).sum();
        let directory = cache
            .directory_facts
            .values()
            .map(|facts| facts.bytes)
            .sum();
        assert_eq!(
            (cache.entries, cache.bytes, cache.directory_bytes),
            (entries, bytes, directory)
        );
        (entries, bytes, directory)
    }

    #[cfg(unix)]
    fn native_fixture() -> HostResult<(tempfile::TempDir, Arc<crate::NativeHost>, PathBuf)> {
        let temp = tempfile::tempdir().map_err(|error| HostError::from_io(error, None))?;
        let base = Arc::new(crate::NativeHost::new()?);
        let root = base.canonicalize(temp.path())?;
        Ok((temp, base, root))
    }

    #[test]
    #[cfg(unix)]
    fn writes_through_a_directory_link_alias_invalidate_the_other_spelling() -> HostResult<()> {
        let (_temp, base, root) = native_fixture()?;
        let real = root.join("private/tmp");
        base.create_dir_all(&real)?;
        base.write(&real.join("file"), b"a", WriteMode::CreateNew)?;
        // `tmp` points to `private/tmp`, as `/tmp` does on macOS.
        let alias = root.join("tmp");
        base.symlink(Path::new("private/tmp"), &alias)?;
        let memo = MemoHost::from_shared(Arc::clone(&base));
        let paths = [
            alias.join("file"),
            real.join("file"),
            alias.clone(),
            real.clone(),
            alias.join("new"),
            real.join("new"),
        ];
        assert_matches_base(&memo, &base, &paths);
        memo.write(&alias.join("file"), b"longer", WriteMode::Truncate)?;
        assert_matches_base(&memo, &base, &paths);
        memo.write(&real.join("new"), b"n", WriteMode::CreateNew)?;
        assert_matches_base(&memo, &base, &paths);
        memo.remove_file(&alias.join("new"))?;
        assert_matches_base(&memo, &base, &paths);
        recount(&memo);
        Ok(())
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn writes_through_tmp_invalidate_private_tmp() -> HostResult<()> {
        let base = Arc::new(crate::NativeHost::new()?);
        let temp = tempfile::Builder::new()
            .tempdir_in("/tmp")
            .map_err(|error| HostError::from_io(error, None))?;
        let name = temp.path().file_name().ok_or_else(fixture_error)?;
        let tmp = Path::new("/tmp").join(name);
        let private = Path::new("/private/tmp").join(name);
        base.write(&tmp.join("file"), b"a", WriteMode::CreateNew)?;
        let memo = MemoHost::from_shared(Arc::clone(&base));
        let paths = [
            tmp.join("file"),
            private.join("file"),
            tmp.clone(),
            private.clone(),
        ];
        assert_matches_base(&memo, &base, &paths);
        memo.write(&tmp.join("file"), b"changed", WriteMode::Truncate)?;
        assert_matches_base(&memo, &base, &paths);
        memo.write(&private.join("other"), b"o", WriteMode::CreateNew)?;
        assert_matches_base(&memo, &base, &paths);
        Ok(())
    }

    #[test]
    #[cfg(unix)]
    fn hard_link_changes_invalidate_every_name_of_the_object() -> HostResult<()> {
        let (_temp, base, root) = native_fixture()?;
        let (one, two) = (root.join("one"), root.join("two"));
        base.create_dir_all(&one)?;
        base.create_dir_all(&two)?;
        let a = one.join("a");
        let b = two.join("b");
        let c = two.join("c");
        base.write(&a, b"data", WriteMode::CreateNew)?;
        base.hard_link(&a, &b)?;
        let memo = MemoHost::from_shared(Arc::clone(&base));
        let paths = [a.clone(), b.clone(), c.clone()];
        assert_matches_base(&memo, &base, &paths);
        memo.write(&a, b"longer data", WriteMode::Truncate)?;
        assert_matches_base(&memo, &base, &paths);
        memo.set_permissions(&a, 0o600)?;
        assert_matches_base(&memo, &base, &paths);
        // A new link and an unlink change the link count of the other names.
        memo.hard_link(&a, &c)?;
        assert_matches_base(&memo, &base, &paths);
        memo.remove_file(&b)?;
        assert_matches_base(&memo, &base, &paths);
        recount(&memo);
        Ok(())
    }

    #[test]
    #[cfg(unix)]
    fn renames_and_link_targets_invalidate_dependent_resolutions() -> HostResult<()> {
        let (_temp, base, root) = native_fixture()?;
        let (from, to, other) = (root.join("from"), root.join("to"), root.join("other"));
        base.create_dir_all(&from.join("sub"))?;
        base.create_dir_all(&to.join("old"))?;
        base.create_dir_all(&other)?;
        base.write(&from.join("sub/file"), b"from", WriteMode::CreateNew)?;
        base.write(&to.join("old/file"), b"to", WriteMode::CreateNew)?;
        // A link elsewhere points into the renamed subtree.
        let link = other.join("link");
        base.symlink(&from.join("sub"), &link)?;
        // A dangling link becomes valid when its target appears.
        let dangling = other.join("dangling");
        base.symlink(&root.join("later/file"), &dangling)?;
        let memo = MemoHost::from_shared(Arc::clone(&base));
        let paths = [
            from.clone(),
            from.join("sub"),
            from.join("sub/file"),
            to.clone(),
            to.join("sub/file"),
            to.join("old/file"),
            link.clone(),
            link.join("file"),
            dangling.clone(),
            root.join("later"),
            root.join("later/file"),
            other.clone(),
        ];
        assert_matches_base(&memo, &base, &paths);
        memo.remove_dir_all(&to)?;
        assert_matches_base(&memo, &base, &paths);
        memo.rename(&from, &to)?;
        assert_matches_base(&memo, &base, &paths);
        memo.create_dir_all(&root.join("later"))?;
        assert_matches_base(&memo, &base, &paths);
        memo.write(&root.join("later/file"), b"later", WriteMode::CreateNew)?;
        assert_matches_base(&memo, &base, &paths);
        // Replace the link with a link to a different target.
        memo.remove_file(&link)?;
        memo.symlink(&to.join("sub"), &link)?;
        assert_matches_base(&memo, &base, &paths);
        recount(&memo);
        Ok(())
    }

    #[test]
    #[cfg(unix)]
    fn names_created_through_an_alias_expire_negative_facts() -> HostResult<()> {
        let (_temp, base, root) = native_fixture()?;
        let real = root.join("real");
        base.create_dir_all(&real)?;
        let alias = root.join("alias");
        base.symlink(&real, &alias)?;
        let memo = MemoHost::from_shared(Arc::clone(&base));
        for n in 0..LIST_AFTER_MISSES {
            assert!(
                memo.metadata(&real.join(format!("missing{n}")), Follow::No)
                    .is_err()
            );
        }
        assert!(
            memo.read_cache()
                .directory_facts
                .contains_key(real.as_os_str())
        );
        let created = real.join("created");
        assert!(memo.metadata(&created, Follow::No).is_err());
        memo.write(&alias.join("created"), b"c", WriteMode::CreateNew)?;
        assert_eq!(memo.metadata(&created, Follow::No)?.len, 1);
        assert_matches_base(&memo, &base, &[real.clone(), created]);
        recount(&memo);
        Ok(())
    }

    #[test]
    #[cfg(unix)]
    fn scoped_writes_keep_unrelated_answers_and_exact_charges() -> HostResult<()> {
        let (_temp, base, root) = native_fixture()?;
        let (tools, scratch) = (root.join("tools"), root.join("scratch"));
        base.create_dir_all(&tools)?;
        base.create_dir_all(&scratch)?;
        let tool = tools.join("cc");
        base.write(&tool, b"tool", WriteMode::CreateNew)?;
        let memo = MemoHost::from_shared(Arc::clone(&base));
        for round in 0..4 {
            memo.metadata(&tool, Follow::Yes)?;
            memo.canonicalize(&tool)?;
            memo.read_dir(&tools)?;
            let file = scratch.join(format!("probe{round}"));
            assert!(memo.metadata(&file, Follow::No).is_err());
            let before = memo.stats();
            memo.write(&file, b"x", WriteMode::CreateNew)?;
            // The write leaves the tool answers in the cache.
            memo.metadata(&tool, Follow::Yes)?;
            memo.canonicalize(&tool)?;
            memo.read_dir(&tools)?;
            assert_eq!(memo.stats().hits, before.hits + 3);
            assert_eq!(memo.metadata(&file, Follow::No)?.len, 1);
            recount(&memo);
        }
        // A process call still clears every answer.
        drop(memo.begin_change(Effect::Global));
        assert_eq!(recount(&memo), (0, 0, 0));
        Ok(())
    }

    #[test]
    fn queries_bypass_retained_answers_while_a_change_is_active() -> HostResult<()> {
        let base = PureHost::default();
        let kept = Path::new("/work/kept");
        base.seed_file(kept, b"kept")?;
        base.seed_file(Path::new("/scratch/file"), b"s")?;
        let memo = MemoHost::new(base);
        memo.metadata(kept, Follow::Yes)?;
        let first = memo.begin_change_for(&[Path::new("/scratch/file")]);
        let second = memo.begin_change_for(&[Path::new("/scratch/other")]);
        let before = memo.stats();
        memo.metadata(kept, Follow::Yes)?;
        assert_eq!(memo.stats().hits, before.hits);
        drop(first);
        memo.metadata(kept, Follow::Yes)?;
        assert_eq!(memo.stats().hits, before.hits);
        drop(second);
        // The changes overlapped, so the answer left the cache.
        assert_eq!(memo.metadata(kept, Follow::Yes)?.len, 4);
        assert_eq!(memo.stats().hits, before.hits);
        memo.metadata(kept, Follow::Yes)?;
        assert_eq!(memo.stats().hits, before.hits + 1);
        Ok(())
    }

    #[test]
    fn deep_writes_through_a_mount_alias_change_tracked_generations() -> HostResult<()> {
        let base = ProbeHost::default();
        base.base
            .seed_file(Path::new("/work/sub/deep/old.h"), b"o")?;
        let host = MemoHost::new(base);
        let before = host.stable_generation_for(Path::new("/work"));
        let other = host.stable_generation_for(Path::new("/other"));
        // `/mount` reaches the same objects as `/work`, without a link.
        host.write(
            Path::new("/mount/sub/deep/new.h"),
            b"n",
            WriteMode::CreateNew,
        )?;
        assert_ne!(host.stable_generation_for(Path::new("/work")), before);
        assert_eq!(host.stable_generation_for(Path::new("/other")), other);
        Ok(())
    }

    #[test]
    fn overlapping_changes_keep_no_answer() -> HostResult<()> {
        let base = PureHost::default();
        let kept = Path::new("/work/kept");
        base.seed_file(kept, b"kept")?;
        let memo = MemoHost::new(base);
        memo.metadata(kept, Follow::Yes)?;
        let first = memo.begin_change_for(&[Path::new("/scratch/one")]);
        // The first change retains the unrelated answer.
        assert_eq!(recount(&memo).0, 1);
        let second = memo.begin_change_for(&[Path::new("/scratch/two")]);
        // A change during another change removes every answer.
        assert_eq!(recount(&memo), (0, 0, 0));
        drop(first);
        drop(second);
        Ok(())
    }

    #[test]
    fn listings_skip_filesystems_with_hidden_names() {
        for path in ["/proc/1", "/dev/fd", "/sys/kernel", "/net/server/share"] {
            assert!(unlisted_filesystem(Path::new(path)), "{path}");
        }
        assert!(!unlisted_filesystem(Path::new("/network/share")));
    }

    #[test]
    #[cfg(windows)]
    fn directory_negatives_are_disabled_on_windows() {
        assert!(negative_leaf(Path::new("C:\\work\\missing")).is_none());
    }
}
