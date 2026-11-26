// SPDX-License-Identifier: MIT OR Apache-2.0

use std::collections::{BTreeSet, HashMap};
#[cfg(not(target_arch = "wasm32"))]
use std::sync::Mutex;
#[cfg(not(target_arch = "wasm32"))]
use std::sync::atomic::{AtomicUsize, Ordering};

use rcmake_base::BString;

use super::*;

/// Hash the names, kinds, and link targets below a search directory.
///
/// Reject trees over 20,000 entries or 16 levels deep. Reject links to directories
/// because changes below those links would not appear in this listing.
pub fn tree_listing_digest(host: &dyn Host, root: &Path) -> HostResult<Digest> {
    ordinary_tree(host, root).map(|(digest, _)| digest)
}

pub(super) fn ordinary_tree(host: &dyn Host, root: &Path) -> HostResult<(Digest, bool)> {
    let mut confined = true;
    let mut key = KeyHasher::new(Domain::BlobV1);
    key.field(0, b"tree-listing");
    if host.metadata(root, Follow::No)?.kind == FileKind::Symlink {
        key.field(1, host.read_link(root)?.as_os_str().as_encoded_bytes());
    }
    let mut directories = vec![(root.to_path_buf(), 0)];
    let mut remaining = 20_000_usize;
    while let Some((directory, depth)) = directories.pop() {
        let mut entries = host.read_dir(&directory)?;
        remaining = remaining
            .checked_sub(entries.len())
            .filter(|_| depth <= 16)
            .ok_or_else(|| HostError::unsupported("Search directory exceeds the listing limit."))?;
        entries.sort_unstable_by(|left, right| left.name.cmp(&right.name));
        let relative = directory
            .strip_prefix(root)
            .map_err(|_| HostError::unsupported("Search directory is outside its root."))?;
        key.field(2, relative.as_os_str().as_encoded_bytes());
        for entry in entries {
            let name = rcmake_host::path::to_native(&entry.name);
            if !matches!(name.components().next(), Some(Component::Normal(_)))
                || name.components().count() != 1
            {
                return Err(HostError::unsupported("Invalid search directory entry."));
            }
            let path = directory.join(name);
            match entry.kind {
                FileKind::File => key.field(3, &entry.name),
                FileKind::Dir => {
                    key.field(4, &entry.name);
                    directories.push((path, depth + 1));
                }
                FileKind::Symlink => {
                    confined = false;
                    key.field(5, &entry.name);
                    key.field(6, host.read_link(&path)?.as_os_str().as_encoded_bytes());
                    match host.metadata(&path, Follow::Yes) {
                        Ok(stat) if stat.kind == FileKind::File => key.field(7, b"file"),
                        Err(error) if error.kind == ErrorKind::NotFound => key.field(7, b"missing"),
                        _ => {
                            return Err(HostError::unsupported(
                                "Search directory contains an unverifiable link.",
                            ));
                        }
                    }
                }
                FileKind::Other => {
                    return Err(HostError::unsupported(
                        "Search directory contains an unsupported entry.",
                    ));
                }
            }
        }
    }
    Ok((key.finish(), confined))
}

/// Hash a bounded SDK search graph, including directory links and dangling links.
///
/// Visit each canonical directory once. Reject inaccessible paths and graphs
/// over 65,536 entries or 32 levels deep.
pub fn sdk_tree_listing_digest(host: &dyn Host, root: &Path) -> HostResult<Digest> {
    let canonical = host.canonicalize(root)?;
    sdk_root_digest(host, root, sdk_graph_digest(host, &canonical)?)
}

pub(super) fn sdk_root_digest(host: &dyn Host, root: &Path, graph: Digest) -> HostResult<Digest> {
    root_digest(host.metadata(root, Follow::No)?, host, root, graph)
}

/// Like [`sdk_root_digest`], but read the root metadata without a cache.
pub(super) fn sdk_root_digest_fresh(
    host: &dyn Host,
    root: &Path,
    graph: Digest,
) -> HostResult<Digest> {
    root_digest(host.metadata_fresh(root, Follow::No)?, host, root, graph)
}

fn root_digest(
    metadata: Metadata,
    host: &dyn Host,
    root: &Path,
    graph: Digest,
) -> HostResult<Digest> {
    let mut key = KeyHasher::new(Domain::BlobV1);
    key.field(0, b"sdk-tree-root");
    key.field(1, graph.as_bytes());
    if metadata.kind == FileKind::Symlink {
        key.field(2, host.read_link(root)?.as_os_str().as_encoded_bytes());
    }
    Ok(key.finish())
}

pub(super) fn sdk_graph_digest(host: &dyn Host, canonical: &Path) -> HostResult<Digest> {
    sdk_graph(host, canonical, None, |_, _| {})
}

/// The maximum number of directory stamps in one SDK listing.
pub(super) const MAX_DIR_STAMPS: usize = 8192;

/// The fresh metadata of one visited canonical SDK directory.
///
/// The walk reads the stamp before it reads the entries. Thus a change after the
/// stamp, or during the listing, gives a different stamp on the next check.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct DirStamp {
    path: PathBuf,
    dev: u64,
    ino: u64,
    mtime: rcmake_host::FileTime,
    ctime: Option<rcmake_host::FileTime>,
    mode: u32,
    owner: Option<u32>,
}

impl DirStamp {
    fn capture(host: &dyn Host, path: &Path) -> Option<Self> {
        let metadata = host.metadata_fresh(path, Follow::No).ok()?;
        (metadata.kind == FileKind::Dir).then(|| Self {
            path: path.to_owned(),
            dev: metadata.dev,
            ino: metadata.ino,
            mtime: metadata.mtime,
            ctime: metadata.ctime,
            mode: metadata.mode,
            owner: metadata.owner,
        })
    }

    /// Return `true` if a later change to the directory must give a different stamp.
    ///
    /// A zero inode, a missing change time, a whole-second change time, or a
    /// change time within one second of `now` gives `false`. A directory that a
    /// process of this user can change also gives `false`.
    pub(super) fn usable(&self, now: rcmake_host::FileTime, effective_uid: Option<u32>) -> bool {
        self.ino != 0
            && super::immutable(
                &Metadata {
                    kind: FileKind::Dir,
                    len: 0,
                    mode: self.mode,
                    mtime: self.mtime,
                    ctime: self.ctime,
                    atime: self.mtime,
                    dev: self.dev,
                    ino: self.ino,
                    nlink: 0,
                    owner: self.owner,
                },
                effective_uid,
            )
            && self
                .ctime
                .is_some_and(|ctime| ctime.nanos != 0 && ctime.secs < now.secs.saturating_sub(1))
    }

    fn matches(&self, host: &dyn Host) -> bool {
        host.metadata_fresh(&self.path, Follow::No)
            .is_ok_and(|metadata| {
                metadata.kind == FileKind::Dir
                    && metadata.dev == self.dev
                    && metadata.ino == self.ino
                    && metadata.mtime == self.mtime
                    && metadata.ctime == self.ctime
                    && metadata.mode == self.mode
                    && metadata.owner == self.owner
            })
    }
}

/// The directory stamps that one walk collects.
#[derive(Default)]
pub(super) struct DirStamps {
    pub(super) dirs: Vec<DirStamp>,
    /// `false` if the walk could not stamp a visited directory.
    pub(super) complete: bool,
}

/// Check all stamps and observed links with fresh queries.
pub(super) fn stamps_match(host: &dyn Host, dirs: &[DirStamp], links: &[SdkLink]) -> bool {
    let check = |index: usize| match index.checked_sub(dirs.len()) {
        None => dirs.get(index).is_some_and(|stamp| stamp.matches(host)),
        Some(link) => links.get(link).is_some_and(|link| link.matches_fresh(host)),
    };
    let total = dirs.len() + links.len();
    #[cfg(not(target_arch = "wasm32"))]
    {
        let next = AtomicUsize::new(0);
        let failed = std::sync::atomic::AtomicBool::new(false);
        let work = || {
            while !failed.load(Ordering::Relaxed) {
                let index = next.fetch_add(1, Ordering::Relaxed);
                if index >= total {
                    break;
                }
                if !check(index) {
                    failed.store(true, Ordering::Relaxed);
                }
            }
        };
        std::thread::scope(|scope| {
            let mut handles = Vec::with_capacity(3);
            for _ in 1..total.min(4) {
                if let Ok(handle) = std::thread::Builder::new().spawn_scoped(scope, work) {
                    handles.push(handle);
                }
            }
            work();
            for handle in handles {
                if handle.join().is_err() {
                    failed.store(true, Ordering::Relaxed);
                }
            }
        });
        !failed.load(Ordering::Relaxed)
    }
    #[cfg(target_arch = "wasm32")]
    {
        (0..total).all(check)
    }
}

#[derive(Clone, Debug)]
pub(super) struct SdkLink {
    pub(super) path: PathBuf,
    pub(super) resolved: Option<(PathBuf, FileKind)>,
}

impl SdkLink {
    /// Like [`SdkLink::matches`], but resolve the link without a cache.
    fn matches_fresh(&self, host: &dyn Host) -> bool {
        match &self.resolved {
            Some((target, kind)) => {
                host.metadata_fresh(&self.path, Follow::Yes)
                    .is_ok_and(|stat| stat.kind == *kind)
                    && host.canonicalize_fresh(&self.path).as_ref() == Ok(target)
            }
            None => host
                .metadata_fresh(&self.path, Follow::Yes)
                .is_err_and(|error| error.kind == ErrorKind::NotFound),
        }
    }

    pub(super) fn matches(&self, host: &dyn Host) -> bool {
        match &self.resolved {
            Some((target, kind)) => {
                host.metadata(&self.path, Follow::Yes)
                    .is_ok_and(|stat| stat.kind == *kind)
                    && host.canonicalize(&self.path).as_ref() == Ok(target)
            }
            None => host
                .metadata(&self.path, Follow::Yes)
                .is_err_and(|error| error.kind == ErrorKind::NotFound),
        }
    }
}

enum SdkEntry {
    File(BString),
    Dir {
        name: BString,
        child: PathBuf,
    },
    SymlinkMissing {
        name: BString,
        target: PathBuf,
        child: PathBuf,
    },
    SymlinkResolved {
        name: BString,
        target: PathBuf,
        child: PathBuf,
        resolved: PathBuf,
        kind: FileKind,
        observe: bool,
    },
}

struct SdkDirSnapshot {
    entry_count: usize,
    entries: HostResult<Vec<SdkEntry>>,
    stamp: Option<DirStamp>,
}

fn read_sdk_dir(
    host: &dyn Host,
    canonical: &Path,
    directory: &Path,
    stamp: bool,
) -> HostResult<SdkDirSnapshot> {
    // Stamp the directory before the listing, so that the stamp is never newer
    // than the entries.
    let stamp = stamp.then(|| DirStamp::capture(host, directory)).flatten();
    let mut snapshot = list_sdk_dir(host, canonical, directory)?;
    snapshot.stamp = stamp;
    Ok(snapshot)
}

fn list_sdk_dir(host: &dyn Host, canonical: &Path, directory: &Path) -> HostResult<SdkDirSnapshot> {
    let mut entries = host.read_dir(directory)?;
    let entry_count = entries.len();
    entries.sort_unstable_by(|left, right| left.name.cmp(&right.name));
    let mut parsed = Vec::with_capacity(entry_count);
    for entry in entries {
        let name = rcmake_host::path::to_native(&entry.name);
        if !matches!(name.components().next(), Some(Component::Normal(_)))
            || name.components().count() != 1
        {
            return Ok(SdkDirSnapshot {
                entry_count,
                entries: Err(HostError::unsupported(
                    "Invalid SDK search directory entry.",
                )),
                stamp: None,
            });
        }
        let child = directory.join(name);
        match entry.kind {
            FileKind::File => parsed.push(SdkEntry::File(entry.name)),
            FileKind::Dir => parsed.push(SdkEntry::Dir {
                name: entry.name,
                child,
            }),
            FileKind::Symlink => {
                let target = match host.read_link(&child) {
                    Ok(target) => target,
                    Err(error) => {
                        return Ok(SdkDirSnapshot {
                            entry_count,
                            entries: Err(error),
                            stamp: None,
                        });
                    }
                };
                let kind = match host.metadata(&child, Follow::Yes) {
                    Ok(stat) => stat.kind,
                    Err(error) if error.kind == ErrorKind::NotFound => {
                        parsed.push(SdkEntry::SymlinkMissing {
                            name: entry.name,
                            target,
                            child,
                        });
                        continue;
                    }
                    Err(error) => {
                        return Ok(SdkDirSnapshot {
                            entry_count,
                            entries: Err(error),
                            stamp: None,
                        });
                    }
                };
                let resolved = match host.canonicalize(&child) {
                    Ok(resolved) => resolved,
                    Err(error) => {
                        return Ok(SdkDirSnapshot {
                            entry_count,
                            entries: Err(error),
                            stamp: None,
                        });
                    }
                };
                if !matches!(kind, FileKind::Dir | FileKind::File) {
                    return Ok(SdkDirSnapshot {
                        entry_count,
                        entries: Err(HostError::unsupported("Unsupported SDK search entry.")),
                        stamp: None,
                    });
                }
                // Recheck a link whose target is absolute or has dot
                // components. Its resolution can pass outside the tree and
                // return, so the directory stamps do not cover it.
                let observe = !resolved.starts_with(canonical)
                    || !link_stays_in_tree(canonical, directory, &target)
                    || target
                        .components()
                        .any(|component| !matches!(component, Component::Normal(_)));
                parsed.push(SdkEntry::SymlinkResolved {
                    name: entry.name,
                    target,
                    child,
                    resolved,
                    kind,
                    observe,
                });
            }
            FileKind::Other => {
                return Ok(SdkDirSnapshot {
                    entry_count,
                    entries: Err(HostError::unsupported("Unsupported SDK search entry.")),
                    stamp: None,
                });
            }
        }
    }
    Ok(SdkDirSnapshot {
        entry_count,
        entries: Ok(parsed),
        stamp: None,
    })
}

#[cfg(not(target_arch = "wasm32"))]
fn prefetch_sdk_graph(
    host: &dyn Host,
    canonical: &Path,
    stamp: bool,
) -> HashMap<PathBuf, HostResult<SdkDirSnapshot>> {
    let mut prefetched = HashMap::new();
    let mut seen = BTreeSet::from([canonical.to_owned()]);
    let mut frontier = vec![canonical.to_owned()];
    let mut budget = 65_536_usize;

    for _depth in 0..=32 {
        if frontier.is_empty() {
            break;
        }
        let wave: Vec<(PathBuf, HostResult<SdkDirSnapshot>)> = if frontier.len() <= 2 {
            frontier
                .iter()
                .map(|dir| (dir.clone(), read_sdk_dir(host, canonical, dir, stamp)))
                .collect()
        } else {
            let next = AtomicUsize::new(0);
            let out = Mutex::new(Vec::with_capacity(frontier.len()));
            let worker_count = frontier.len().min(6);
            let work = || {
                let mut local = Vec::new();
                loop {
                    let idx = next.fetch_add(1, Ordering::Relaxed);
                    let Some(dir) = frontier.get(idx) else {
                        break;
                    };
                    local.push((dir.clone(), read_sdk_dir(host, canonical, dir, stamp)));
                }
                if let Ok(mut guard) = out.lock() {
                    guard.extend(local);
                }
            };
            std::thread::scope(|scope| {
                let mut handles = Vec::with_capacity(worker_count.saturating_sub(1));
                for _ in 1..worker_count {
                    if let Ok(handle) = std::thread::Builder::new().spawn_scoped(scope, work) {
                        handles.push(handle);
                    }
                }
                work();
                for handle in handles {
                    let _ = handle.join();
                }
            });
            out.into_inner()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
        };

        let mut next_frontier = Vec::new();
        let mut exhausted = false;
        for (dir, result) in wave {
            if let Ok(snapshot) = &result {
                if let Some(left) = budget.checked_sub(snapshot.entry_count) {
                    budget = left;
                    if let Ok(entries) = &snapshot.entries {
                        for entry in entries {
                            match entry {
                                SdkEntry::Dir { child, .. } if seen.insert(child.clone()) => {
                                    next_frontier.push(child.clone());
                                }
                                SdkEntry::SymlinkResolved {
                                    resolved,
                                    kind: FileKind::Dir,
                                    ..
                                } if seen.insert(resolved.clone()) => {
                                    next_frontier.push(resolved.clone());
                                }
                                _ => {}
                            }
                        }
                    }
                } else {
                    exhausted = true;
                }
            }
            prefetched.insert(dir, result);
        }
        if exhausted {
            break;
        }
        frontier = next_frontier;
    }
    prefetched
}

#[cfg(target_arch = "wasm32")]
fn prefetch_sdk_graph(
    _host: &dyn Host,
    _canonical: &Path,
    _stamp: bool,
) -> HashMap<PathBuf, HostResult<SdkDirSnapshot>> {
    HashMap::new()
}

/// Hash the SDK graph below `canonical`. With `stamps`, also stamp each visited directory.
pub(super) fn sdk_graph(
    host: &dyn Host,
    canonical: &Path,
    mut stamps: Option<&mut DirStamps>,
    mut observe_link: impl FnMut(PathBuf, Option<(PathBuf, FileKind)>),
) -> HostResult<Digest> {
    let stamp = stamps.is_some();
    if let Some(stamps) = stamps.as_deref_mut() {
        stamps.complete = true;
    }
    let mut prefetched = prefetch_sdk_graph(host, canonical, stamp);
    let mut key = KeyHasher::new(Domain::BlobV1);
    key.field(0, b"sdk-tree-graph");
    key.field(1, canonical.as_os_str().as_encoded_bytes());
    let mut directories = vec![(canonical.to_owned(), 0)];
    let mut visited = BTreeSet::new();
    let mut remaining = 65_536_usize;
    while let Some((directory, depth)) = directories.pop() {
        if !visited.insert(directory.clone()) {
            continue;
        }
        let snapshot = match prefetched.remove(&directory) {
            Some(result) => result?,
            None => read_sdk_dir(host, canonical, &directory, stamp)?,
        };
        if let Some(stamps) = stamps.as_deref_mut() {
            match snapshot.stamp {
                Some(stamp) if stamps.dirs.len() < MAX_DIR_STAMPS => stamps.dirs.push(stamp),
                _ => stamps.complete = false,
            }
        }
        remaining = remaining
            .checked_sub(snapshot.entry_count)
            .filter(|_| depth <= 32)
            .ok_or_else(|| HostError::unsupported("SDK search graph exceeds the listing limit."))?;
        key.field(3, directory.as_os_str().as_encoded_bytes());
        for entry in snapshot.entries? {
            match entry {
                SdkEntry::File(name) => key.field(4, &name),
                SdkEntry::Dir { name, child } => {
                    key.field(5, &name);
                    directories.push((child, depth + 1));
                }
                SdkEntry::SymlinkMissing {
                    name,
                    target,
                    child,
                } => {
                    key.field(6, &name);
                    key.field(7, target.as_os_str().as_encoded_bytes());
                    key.field(8, b"missing");
                    observe_link(child, None);
                }
                SdkEntry::SymlinkResolved {
                    name,
                    target,
                    child,
                    resolved,
                    kind,
                    observe,
                } => {
                    key.field(6, &name);
                    key.field(7, target.as_os_str().as_encoded_bytes());
                    if observe {
                        observe_link(child, Some((resolved.clone(), kind)));
                    }
                    match kind {
                        FileKind::Dir => {
                            key.field(8, b"directory");
                            directories.push((resolved.clone(), depth + 1));
                        }
                        FileKind::File => key.field(8, b"file"),
                        _ => return Err(HostError::unsupported("Unsupported SDK search entry.")),
                    }
                    key.field(9, resolved.as_os_str().as_encoded_bytes());
                }
            }
        }
    }
    Ok(key.finish())
}

fn link_stays_in_tree(root: &Path, directory: &Path, target: &Path) -> bool {
    let absolute = target.is_absolute();
    let (Some(directory), Some(target)) = (
        directory.strip_prefix(root).ok(),
        if target.is_absolute() {
            target.strip_prefix(root).ok()
        } else {
            Some(target)
        },
    ) else {
        return false;
    };
    let mut depth = if absolute {
        0
    } else {
        directory.components().count()
    };
    for component in target.components() {
        match component {
            Component::Normal(_) => depth += 1,
            Component::ParentDir if depth > 0 => depth -= 1,
            Component::CurDir => {}
            _ => return false,
        }
    }
    true
}
