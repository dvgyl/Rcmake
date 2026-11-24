//! An in-memory overlay for a read-only base host.

#![forbid(unsafe_code)]

use core::time::Duration;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::ffi::OsString;
use std::io::ErrorKind;
use std::ops::Bound::{Excluded, Unbounded};
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, RwLock, RwLockReadGuard, RwLockWriteGuard};

use rcmake_base::BString;

use crate::error::SyntheticError;
use crate::{
    CopyOptions, DirEntry, FileKind, FileLockGuard, FileTime, Follow, Host, HostError, HostResult,
    Metadata, ReadRange, RunOutput, RunRequest, Stream, WriteMode, path,
};

const MAX_SYMLINK_HOPS: usize = 40;
const MAX_FILE_BYTES: usize = 256 * 1024 * 1024;
const MAX_REMOVAL_ENTRIES: usize = 1_000_000;

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
enum Inode {
    Base(u64, u64),
    Local(u64),
}

#[derive(Clone)]
struct Node {
    stat: Metadata,
    backing: Option<PathBuf>,
    target: Option<PathBuf>,
}

impl Node {
    fn inode(&self) -> Inode {
        if self.backing.is_some() {
            Inode::Base(self.stat.dev, self.stat.ino)
        } else {
            Inode::Local(self.stat.ino)
        }
    }

    fn target(&self, base: &(impl Host + ?Sized), p: &Path) -> HostResult<PathBuf> {
        if let Some(target) = &self.target {
            return Ok(target.clone());
        }
        base.read_link(self.backing(p)?)
    }

    fn backing(&self, p: &Path) -> HostResult<&Path> {
        self.backing
            .as_deref()
            .ok_or_else(|| SyntheticError::NotFound.at(Some(p)))
    }

    fn require_file(&self, p: &Path) -> HostResult<()> {
        match self.stat.kind {
            FileKind::File => Ok(()),
            FileKind::Dir => Err(SyntheticError::IsADirectory.at(Some(p))),
            _ => Err(SyntheticError::InvalidInput.at(Some(p))),
        }
    }
}

#[derive(Default)]
struct Override {
    bytes: Option<Vec<u8>>,
    mode: Option<u32>,
    atime: Option<FileTime>,
    mtime: Option<FileTime>,
    links: i64,
}

impl Override {
    fn apply(&self, stat: &mut Metadata) {
        if let Some(bytes) = &self.bytes {
            stat.len = bytes.len() as u64;
        }
        stat.mode = self.mode.unwrap_or(stat.mode);
        stat.atime = self.atime.unwrap_or(stat.atime);
        stat.mtime = self.mtime.unwrap_or(stat.mtime);
        stat.nlink = stat.nlink.saturating_add_signed(self.links);
    }
}

struct OverlayState<H: Host + ?Sized> {
    base: Arc<H>,
    // An absent node at a key hides the base path and its descendants.
    paths: BTreeMap<PathBuf, Option<Node>>,
    overrides: BTreeMap<Inode, Override>,
    env: BTreeMap<BString, Option<BString>>,
    next_ino: u64,
}

struct Resolved {
    path: PathBuf,
    node: Option<Node>,
}

impl Resolved {
    fn existing(self, original: &Path) -> HostResult<(PathBuf, Node)> {
        let node = self
            .node
            .ok_or_else(|| SyntheticError::NotFound.at(Some(original)))?;
        Ok((self.path, node))
    }
}

enum Part {
    Name(OsString, bool),
    Parent,
    Directory,
}

fn split(p: &Path, create_dirs: bool) -> (PathBuf, VecDeque<Part>) {
    let mut root = PathBuf::new();
    let mut parts = VecDeque::new();
    for component in p.components() {
        match component {
            Component::Prefix(_) | Component::RootDir => root.push(component.as_os_str()),
            Component::Normal(name) => parts.push_back(Part::Name(name.to_owned(), create_dirs)),
            Component::ParentDir => parts.push_back(Part::Parent),
            Component::CurDir => {}
        }
    }
    let bytes = path::from_native(p);
    let separator = |byte| byte == b'/' || (cfg!(windows) && byte == b'\\');
    if bytes.last().copied().is_some_and(separator)
        || matches!(bytes.as_slice(), [.., sep, b'.'] if separator(*sep))
    {
        parts.push_back(Part::Directory);
    }
    (root, parts)
}

fn absolute(p: &Path) -> HostResult<()> {
    if !p.has_root() {
        return Err(HostError::not_absolute(p.to_path_buf()));
    }
    if path::from_native(p).as_slice().contains(&0) {
        return Err(SyntheticError::InvalidInput.at(Some(p)));
    }
    Ok(())
}

impl<H: Host + ?Sized> OverlayState<H> {
    fn new(base: Arc<H>) -> Self {
        Self {
            base,
            paths: BTreeMap::new(),
            overrides: BTreeMap::new(),
            env: BTreeMap::new(),
            next_ino: 0,
        }
    }

    fn new_node(&mut self, kind: FileKind, now: FileTime, target: Option<PathBuf>) -> Node {
        self.next_ino += 1;
        Node {
            stat: Metadata {
                kind,
                mode: match kind {
                    FileKind::Dir => 0o755,
                    FileKind::Symlink => 0o777,
                    _ => 0o644,
                },
                len: target
                    .as_ref()
                    .map_or(0, |p| path::from_native(p).len() as u64),
                atime: now,
                mtime: now,
                dev: u64::MAX,
                ino: self.next_ino,
                nlink: if kind == FileKind::Dir { 2 } else { 1 },
            },
            backing: None,
            target,
        }
    }

    fn lookup(&self, mut p: PathBuf, backing: Option<PathBuf>) -> HostResult<Resolved> {
        if let Some(node) = self.paths.get(&p) {
            return Ok(Resolved {
                path: p,
                node: node.clone(),
            });
        }
        let mut node = None;
        if let Some(mut backing) = backing
            && let Some(stat) = present(self.base.metadata(&backing, Follow::No))?
        {
            if stat.kind != FileKind::Symlink
                && let Ok(canonical) = self.base.canonicalize(&backing)
            {
                // Use the base's spelling, also when the directory has moved in the overlay.
                if let Some(name) = canonical.file_name() {
                    p.set_file_name(name);
                }
                backing = canonical;
            }
            node = Some(Node {
                stat,
                backing: Some(backing),
                target: None,
            });
        }
        let node = self.paths.get(&p).cloned().unwrap_or(node);
        Ok(Resolved { path: p, node })
    }

    fn child(&self, dir: &Resolved, name: &Path) -> HostResult<Resolved> {
        let backing = dir.node.as_ref().and_then(|node| node.backing.as_ref());
        self.lookup(dir.path.join(name), backing.map(|p| p.join(name)))
    }

    fn root(&self, root: PathBuf) -> HostResult<Resolved> {
        let resolved = self.lookup(root.clone(), Some(root.clone()))?;
        match resolved.node.as_ref().map(|node| node.stat.kind) {
            Some(FileKind::Dir) => Ok(resolved),
            None => Err(SyntheticError::NotFound.at(Some(&root))),
            _ => Err(SyntheticError::NotADirectory.at(Some(&root))),
        }
    }

    fn resolve(
        &self,
        p: &Path,
        follow: Follow,
        allow_missing_leaf: bool,
        mut new_dirs: Option<&mut BTreeSet<PathBuf>>,
    ) -> HostResult<Resolved> {
        absolute(p)?;
        let (root, mut pending) = split(p, new_dirs.is_some());
        let mut parents = vec![self.root(root)?];
        let mut hops = 0;
        while let Some(part) = pending.pop_front() {
            let Part::Name(name, may_create) = part else {
                if matches!(part, Part::Parent) && parents.len() > 1 {
                    parents.pop();
                }
                continue;
            };
            let dir = parents
                .last()
                .ok_or_else(|| SyntheticError::NotFound.at(Some(p)))?;
            let child = self.child(dir, Path::new(&name))?;
            match child.node.as_ref().map(|node| node.stat.kind) {
                None => {
                    if let Some(dirs) = new_dirs.as_deref_mut()
                        && (may_create || dirs.contains(&child.path))
                    {
                        dirs.insert(child.path.clone());
                        parents.push(child);
                    } else if allow_missing_leaf && pending.is_empty() {
                        return Ok(child);
                    } else {
                        return Err(SyntheticError::NotFound.at(Some(p)));
                    }
                }
                Some(FileKind::Dir) => parents.push(child),
                Some(FileKind::Symlink) if follow == Follow::Yes || !pending.is_empty() => {
                    hops += 1;
                    if hops > MAX_SYMLINK_HOPS {
                        return Err(SyntheticError::LinkLoop.at(Some(p)));
                    }
                    let (_, node) = child.existing(p)?;
                    let mut target = node.target(self.base.as_ref(), p)?;
                    if cfg!(windows) && target.has_root() && !target.is_absolute() {
                        target = dir.path.join(target);
                    }
                    let (root, mut parts) = split(&target, false);
                    if root.has_root() {
                        parents = vec![self.root(root)?];
                    } else if !root.as_os_str().is_empty() {
                        return Err(SyntheticError::InvalidInput.at(Some(p)));
                    }
                    parts.append(&mut pending);
                    pending = parts;
                }
                Some(_) if pending.is_empty() && new_dirs.is_none() => return Ok(child),
                Some(_) => return Err(SyntheticError::NotADirectory.at(Some(p))),
            }
        }
        parents
            .pop()
            .ok_or_else(|| SyntheticError::NotFound.at(Some(p)))
    }

    fn existing(&self, p: &Path, follow: Follow) -> HostResult<(PathBuf, Node)> {
        self.resolve(p, follow, false, None)?.existing(p)
    }

    fn destination(&self, p: &Path, follow: Follow) -> HostResult<Resolved> {
        self.resolve(p, follow, true, None)
    }

    fn descendants<'a>(
        &'a self,
        dir: &'a Path,
    ) -> impl Iterator<Item = (&'a PathBuf, &'a Option<Node>)> {
        self.paths
            .range::<Path, _>((Excluded(dir), Unbounded))
            .take_while(move |(p, _)| p.starts_with(dir))
    }

    fn children(&self, p: &Path, dir: &Node) -> HostResult<Vec<DirEntry>> {
        let mut entries = match &dir.backing {
            Some(backing) => self
                .base
                .read_dir(backing)?
                .into_iter()
                .map(|entry| (entry.name, entry.kind))
                .collect::<BTreeMap<_, _>>(),
            None => BTreeMap::new(),
        };
        for (child, node) in self
            .descendants(p)
            .filter(|(child, _)| child.parent() == Some(p))
        {
            if let Some(name) = child
                .file_name()
                .map(|name| path::from_native(Path::new(name)))
            {
                match node {
                    Some(node) => {
                        entries.insert(name, node.stat.kind);
                    }
                    None => {
                        entries.remove(&name);
                    }
                }
            }
        }
        Ok(entries
            .into_iter()
            .map(|(name, kind)| DirEntry { name, kind })
            .collect())
    }

    fn stat(&self, node: &Node) -> Metadata {
        let mut stat = node.stat.clone();
        if let Some(change) = self.overrides.get(&node.inode()) {
            change.apply(&mut stat);
        }
        stat
    }

    fn bytes(&self, node: &Node) -> Option<&[u8]> {
        self.overrides.get(&node.inode())?.bytes.as_deref()
    }

    fn store(&mut self, p: PathBuf, node: Node) -> &mut Override {
        let key = node.inode();
        self.paths.insert(p, Some(node));
        self.overrides.entry(key).or_default()
    }

    fn unlink(&mut self, node: &Node) {
        if node.stat.kind != FileKind::Dir && self.stat(node).nlink > 1 {
            self.overrides.entry(node.inode()).or_default().links -= 1;
        } else {
            self.overrides.remove(&node.inode());
        }
    }

    fn clear_tree(&mut self, p: &Path) {
        let children: Vec<_> = self.descendants(p).map(|(p, _)| p.clone()).collect();
        for child in children {
            self.paths.remove(&child);
        }
        self.paths.insert(p.to_path_buf(), None);
    }

    fn removed_nodes(&self, p: PathBuf, node: Node) -> HostResult<Vec<Node>> {
        let mut nodes = Vec::new();
        let mut pending = vec![(p, node)];
        while let Some((p, node)) = pending.pop() {
            if node.stat.kind == FileKind::Dir {
                let children = self.children(&p, &node)?;
                let dir = Resolved {
                    path: p.clone(),
                    node: Some(node.clone()),
                };
                for child in children {
                    let child_path = path::to_native(&child.name);
                    let mut parts = child_path.components();
                    if !matches!(
                        (parts.next(), parts.next()),
                        (Some(Component::Normal(_)), None)
                    ) {
                        return Err(SyntheticError::InvalidInput.at(Some(&p)));
                    }
                    pending.push(self.child(&dir, &child_path)?.existing(&p)?);
                    if pending.len().saturating_add(nodes.len()) > MAX_REMOVAL_ENTRIES {
                        return Err(HostError::unsupported(
                            "Too many entries in the overlay directory tree",
                        ));
                    }
                }
            }
            nodes.push(node);
        }
        Ok(nodes)
    }
}

fn present<T>(result: HostResult<T>) -> HostResult<Option<T>> {
    match result {
        Ok(value) => Ok(Some(value)),
        Err(err) if err.kind == ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err),
    }
}

fn poisoned() -> HostError {
    HostError::unsupported("Cannot access the overlay host state")
}

#[derive(Debug, Default)]
struct LockTable {
    held: Mutex<BTreeSet<Inode>>,
    released: Condvar,
}

#[derive(Debug)]
struct OverlayLock {
    table: Arc<LockTable>,
    key: Inode,
    path: PathBuf,
}

impl FileLockGuard for OverlayLock {
    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for OverlayLock {
    fn drop(&mut self) {
        if let Ok(mut held) = self.table.held.lock() {
            held.remove(&self.key);
        }
        self.table.released.notify_all();
    }
}

/// Counts of changes present in the overlay.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct OverlayAudit {
    /// Paths added, changed, or moved in the overlay.
    pub written_paths: usize,
    /// Paths with tombstones. A directory tombstone also hides its descendants.
    pub deleted_paths: usize,
    /// File contents stored in memory, counted once per inode.
    pub overlay_files: usize,
    /// Environment keys set or unset in the overlay.
    pub environment_overrides: usize,
}

/// Stores filesystem and environment changes in memory.
///
/// The adapter never calls a base filesystem mutation method or starts a process.
/// The base must remain stable while the overlay is in use. Base reads can change
/// access timestamps. The adapter can store up to 256 MiB per file. Use
/// [`NoProcessHost`](crate::no_process::NoProcessHost) around this host to log process denials.
pub struct OverlayHost<H: Host + ?Sized> {
    base: Arc<H>,
    state: RwLock<OverlayState<H>>,
    locks: Arc<LockTable>,
}

impl<H: Host> OverlayHost<H> {
    /// Wrap an owned base host.
    #[must_use]
    pub fn new(base: H) -> Self {
        Self::from_shared(Arc::new(base))
    }
}

impl<H: Host + ?Sized> OverlayHost<H> {
    /// Wrap a shared base host, including a shared trait object.
    #[must_use]
    pub fn from_shared(base: Arc<H>) -> Self {
        Self {
            state: RwLock::new(OverlayState::new(Arc::clone(&base))),
            base,
            locks: Arc::default(),
        }
    }

    /// Return paths added, changed, or moved in the overlay, in sorted order.
    #[must_use]
    pub fn written_paths(&self) -> Vec<PathBuf> {
        self.paths(true)
    }

    /// Return tombstone paths in sorted order.
    #[must_use]
    pub fn deleted_paths(&self) -> Vec<PathBuf> {
        self.paths(false)
    }

    fn paths(&self, written: bool) -> Vec<PathBuf> {
        self.state
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .paths
            .iter()
            .filter(|(_, node)| node.is_some() == written)
            .map(|(p, _)| p.clone())
            .collect()
    }

    /// Return the bytes stored in the overlay for this path, following symbolic links.
    ///
    /// A file that still uses its base contents has no bytes in the overlay.
    #[must_use]
    pub fn read_overlay_file(&self, p: &Path) -> Option<Vec<u8>> {
        let state = self.state.read().ok()?;
        let (_, node) = state.existing(p, Follow::Yes).ok()?;
        state.bytes(&node).map(<[u8]>::to_vec)
    }

    /// Count changes held in memory. The counts cover this adapter's state.
    #[must_use]
    pub fn audit_clean_base(&self) -> OverlayAudit {
        let state = self
            .state
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let written_paths = state.paths.values().filter(|node| node.is_some()).count();
        OverlayAudit {
            written_paths,
            deleted_paths: state.paths.len() - written_paths,
            overlay_files: state
                .overrides
                .values()
                .filter(|change| change.bytes.is_some())
                .count(),
            environment_overrides: state.env.len(),
        }
    }

    fn read_state(&self) -> HostResult<RwLockReadGuard<'_, OverlayState<H>>> {
        self.state.read().map_err(|_| poisoned())
    }

    fn write_state(&self) -> HostResult<RwLockWriteGuard<'_, OverlayState<H>>> {
        self.state.write().map_err(|_| poisoned())
    }

    fn store_file<'a>(
        &self,
        state: &'a mut OverlayState<H>,
        dest: Resolved,
        bytes: Vec<u8>,
    ) -> &'a mut Override {
        let now = self.base.now();
        let node = dest
            .node
            .unwrap_or_else(|| state.new_node(FileKind::File, now, None));
        let change = state.store(dest.path, node);
        change.bytes = Some(bytes);
        change.mtime = Some(now);
        change.atime = Some(now);
        change
    }

    fn rename_entry(&self, from: &Path, to: &Path, replace: bool) -> HostResult<()> {
        absolute(from)?;
        absolute(to)?;
        let mut state = self.write_state()?;
        let (src, node) = state.existing(from, Follow::No)?;
        let dst = state.destination(to, Follow::No)?;
        if !replace && dst.node.is_some() {
            return Err(SyntheticError::AlreadyExists.at(Some(to)));
        }
        if dst
            .node
            .as_ref()
            .is_some_and(|other| other.inode() == node.inode())
        {
            return Ok(());
        }
        if src.parent().is_none() || dst.path.parent().is_none() {
            return Err(SyntheticError::ResourceBusy.at(Some(from)));
        }
        if dst.path.starts_with(&src) {
            return Err(SyntheticError::InvalidInput.at(Some(to)));
        }
        if let Some(other) = &dst.node {
            match (node.stat.kind, other.stat.kind) {
                (FileKind::Dir, FileKind::Dir) => {
                    if !state.children(&dst.path, other)?.is_empty() {
                        return Err(SyntheticError::DirectoryNotEmpty.at(Some(to)));
                    }
                }
                (FileKind::Dir, _) => return Err(SyntheticError::NotADirectory.at(Some(to))),
                (_, FileKind::Dir) => return Err(SyntheticError::IsADirectory.at(Some(to))),
                _ => {}
            }
            state.unlink(other);
        }
        let moved: Vec<_> = state
            .descendants(&src)
            .filter_map(|(p, entry)| {
                Some((dst.path.join(p.strip_prefix(&src).ok()?), entry.clone()))
            })
            .collect();
        state.clear_tree(&dst.path);
        state.clear_tree(&src);
        state.paths.insert(dst.path, Some(node));
        state.paths.extend(moved);
        Ok(())
    }

    fn contents(&self, state: &OverlayState<H>, node: &Node, p: &Path) -> HostResult<Vec<u8>> {
        if let Some(bytes) = state.bytes(node) {
            return Ok(bytes.to_vec());
        }
        let range = ReadRange::All {
            limit: MAX_FILE_BYTES as u64,
        };
        self.base.read(node.backing(p)?, range)
    }
}

impl<H: Host + ?Sized> Host for OverlayHost<H> {
    fn metadata(&self, p: &Path, follow: Follow) -> HostResult<Metadata> {
        let state = self.read_state()?;
        let (key, node) = state.existing(p, follow)?;
        let mut stat = state.stat(&node);
        if stat.kind == FileKind::Dir
            && stat.nlink >= 2
            && state
                .descendants(&key)
                .any(|(child, _)| child.parent() == Some(key.as_path()))
            && let Ok(entries) = state.children(&key, &node)
        {
            stat.nlink = 2 + entries
                .iter()
                .filter(|entry| entry.kind == FileKind::Dir)
                .count() as u64;
        }
        Ok(stat)
    }

    fn read(&self, p: &Path, range: ReadRange) -> HostResult<Vec<u8>> {
        let state = self.read_state()?;
        let (_, node) = state.existing(p, Follow::Yes)?;
        if node.stat.kind == FileKind::Dir {
            return Err(SyntheticError::IsADirectory.at(Some(p)));
        }
        let Some(bytes) = state.bytes(&node) else {
            return self.base.read(node.backing(p)?, range);
        };
        match range {
            ReadRange::All { limit } if bytes.len() as u64 > limit => {
                Err(SyntheticError::FileTooLarge.at(Some(p)))
            }
            ReadRange::All { .. } => Ok(bytes.to_vec()),
            ReadRange::Slice { offset, len } => {
                let start = (offset.min(bytes.len() as u64)) as usize;
                let end = offset
                    .saturating_add(len.unwrap_or(u64::MAX))
                    .min(bytes.len() as u64);
                Ok(bytes.get(start..end as usize).unwrap_or_default().to_vec())
            }
        }
    }

    fn write(&self, p: &Path, data: &[u8], mode: WriteMode) -> HostResult<()> {
        let mut state = self.write_state()?;
        let follow = if mode == WriteMode::CreateNew {
            Follow::No
        } else {
            Follow::Yes
        };
        let dest = state.destination(p, follow)?;
        let mut bytes = Vec::new();
        if let Some(node) = &dest.node {
            if mode == WriteMode::CreateNew {
                return Err(SyntheticError::AlreadyExists.at(Some(p)));
            }
            node.require_file(p)?;
            if mode == WriteMode::Append {
                bytes = self.contents(&state, node, p)?;
            }
        }
        if bytes.len().saturating_add(data.len()) > MAX_FILE_BYTES {
            return Err(SyntheticError::FileTooLarge.at(Some(p)));
        }
        bytes.extend_from_slice(data);
        self.store_file(&mut state, dest, bytes);
        Ok(())
    }

    fn read_dir(&self, p: &Path) -> HostResult<Vec<DirEntry>> {
        let state = self.read_state()?;
        let (dir, node) = state.existing(p, Follow::Yes)?;
        if node.stat.kind != FileKind::Dir {
            return Err(SyntheticError::NotADirectory.at(Some(p)));
        }
        state.children(&dir, &node)
    }

    fn create_dir_all(&self, p: &Path) -> HostResult<()> {
        let mut state = self.write_state()?;
        let mut new_dirs = BTreeSet::new();
        state.resolve(p, Follow::Yes, false, Some(&mut new_dirs))?;
        for dir in new_dirs {
            let node = state.new_node(FileKind::Dir, self.base.now(), None);
            state.paths.insert(dir, Some(node));
        }
        Ok(())
    }

    fn remove_file(&self, p: &Path) -> HostResult<()> {
        let mut state = self.write_state()?;
        let (key, node) = state.existing(p, Follow::No)?;
        if node.stat.kind == FileKind::Dir {
            return Err(SyntheticError::IsADirectory.at(Some(p)));
        }
        state.unlink(&node);
        state.paths.insert(key, None);
        Ok(())
    }

    fn remove_dir(&self, p: &Path) -> HostResult<()> {
        let trimmed = path::directory_removal_path(p, absolute, cfg!(windows))?;
        let mut state = self.write_state()?;
        let (key, node) = state.existing(&trimmed, Follow::No)?;
        if node.stat.kind != FileKind::Dir {
            return Err(SyntheticError::NotADirectory.at(Some(p)));
        }
        if !state.children(&key, &node)?.is_empty() {
            return Err(SyntheticError::DirectoryNotEmpty.at(Some(p)));
        }
        state.unlink(&node);
        state.clear_tree(&key);
        Ok(())
    }

    fn remove_dir_all(&self, p: &Path) -> HostResult<()> {
        let trimmed = path::directory_removal_path(p, absolute, cfg!(windows))?;
        let mut state = self.write_state()?;
        let (key, node) = state.existing(&trimmed, Follow::No)?;
        if !matches!(node.stat.kind, FileKind::Dir | FileKind::Symlink) {
            return Err(SyntheticError::NotADirectory.at(Some(p)));
        }
        for node in state.removed_nodes(key.clone(), node)? {
            state.unlink(&node);
        }
        state.clear_tree(&key);
        Ok(())
    }

    fn rename(&self, from: &Path, to: &Path) -> HostResult<()> {
        self.rename_entry(from, to, true)
    }

    fn rename_noreplace(&self, from: &Path, to: &Path) -> HostResult<()> {
        self.rename_entry(from, to, false)
    }

    fn lock_file(
        &self,
        path: &Path,
        timeout: Option<Duration>,
    ) -> HostResult<Box<dyn FileLockGuard>> {
        let (key, path) = {
            let mut state = self.write_state()?;
            let dest = state.destination(path, Follow::Yes)?;
            let key = if let Some(node) = &dest.node {
                node.require_file(path)?;
                node.inode()
            } else {
                let node = state.new_node(FileKind::File, self.base.now(), None);
                let key = node.inode();
                state.store(dest.path.clone(), node).bytes = Some(Vec::new());
                key
            };
            (key, dest.path)
        };
        let mut held = self.locks.held.lock().map_err(|_| poisoned())?;
        if held.contains(&key) {
            if timeout == Some(Duration::ZERO) {
                return Err(HostError::timed_out("Timeout reached", Some(path)));
            }
            #[cfg(target_family = "wasm")]
            return Err(HostError::unsupported(
                "Blocking file locks are unavailable on WebAssembly",
            ));
            #[cfg(not(target_family = "wasm"))]
            {
                let busy = |held: &mut BTreeSet<Inode>| held.contains(&key);
                let released = &self.locks.released;
                held = match timeout {
                    None => released.wait_while(held, busy).map_err(|_| poisoned())?,
                    Some(limit) => {
                        released
                            .wait_timeout_while(held, limit, busy)
                            .map_err(|_| poisoned())?
                            .0
                    }
                };
                if held.contains(&key) {
                    return Err(HostError::timed_out("Timeout reached", Some(path)));
                }
            }
        }
        held.insert(key.clone());
        Ok(Box::new(OverlayLock {
            table: Arc::clone(&self.locks),
            key,
            path,
        }))
    }

    fn copy_file(&self, from: &Path, to: &Path, opts: CopyOptions) -> HostResult<()> {
        absolute(from)?;
        absolute(to)?;
        let mut state = self.write_state()?;
        let (_, source) = state.existing(from, Follow::Yes)?;
        if source.stat.kind != FileKind::File {
            return Err(SyntheticError::InvalidInput.at(Some(from)));
        }
        let dest = state.destination(to, Follow::Yes)?;
        if let Some(node) = &dest.node {
            node.require_file(to)?;
            if node.inode() == source.inode() {
                return Err(SyntheticError::InvalidInput.at(Some(to)));
            }
        }
        let stat = state.stat(&source);
        let bytes = self.contents(&state, &source, from)?;
        let change = self.store_file(&mut state, dest, bytes);
        if opts.preserve_permissions {
            change.mode = Some(stat.mode);
        }
        if opts.preserve_mtime {
            change.mtime = Some(stat.mtime);
        }
        Ok(())
    }

    fn symlink(&self, target: &Path, link: &Path) -> HostResult<()> {
        let mut state = self.write_state()?;
        let dest = state.destination(link, Follow::No)?;
        if dest.node.is_some() {
            return Err(SyntheticError::AlreadyExists.at(Some(link)));
        }
        if target.as_os_str().is_empty() {
            return Err(SyntheticError::NotFound.at(Some(link)));
        }
        if path::from_native(target).as_slice().contains(&0) {
            return Err(SyntheticError::InvalidInput.at(Some(link)));
        }
        let node = state.new_node(
            FileKind::Symlink,
            self.base.now(),
            Some(target.to_path_buf()),
        );
        state.paths.insert(dest.path, Some(node));
        Ok(())
    }

    fn hard_link(&self, src: &Path, dst: &Path) -> HostResult<()> {
        absolute(src)?;
        absolute(dst)?;
        let mut state = self.write_state()?;
        let (_, source) = state.existing(src, Follow::No)?;
        let dest = state.destination(dst, Follow::No)?;
        if dest.node.is_some() {
            return Err(SyntheticError::AlreadyExists.at(Some(dst)));
        }
        if source.stat.kind == FileKind::Dir {
            return Err(SyntheticError::PermissionDenied.at(Some(src)));
        }
        state.store(dest.path, source).links += 1;
        Ok(())
    }

    fn read_link(&self, p: &Path) -> HostResult<PathBuf> {
        let (_, node) = self.read_state()?.existing(p, Follow::No)?;
        if node.stat.kind != FileKind::Symlink {
            return Err(SyntheticError::InvalidInput.at(Some(p)));
        }
        node.target(self.base.as_ref(), p)
    }

    fn canonicalize(&self, p: &Path) -> HostResult<PathBuf> {
        self.read_state()?.existing(p, Follow::Yes).map(|(p, _)| p)
    }

    fn set_permissions(&self, p: &Path, mode: u32) -> HostResult<()> {
        let mut state = self.write_state()?;
        let (key, node) = state.existing(p, Follow::Yes)?;
        state.store(key, node).mode = Some(mode & 0o7777);
        Ok(())
    }

    fn set_times(
        &self,
        p: &Path,
        atime: Option<FileTime>,
        mtime: Option<FileTime>,
    ) -> HostResult<()> {
        absolute(p)?;
        if atime
            .into_iter()
            .chain(mtime)
            .any(|time| time.nanos >= 1_000_000_000)
        {
            return Err(SyntheticError::InvalidInput.at(Some(p)));
        }
        let mut state = self.write_state()?;
        let (key, node) = state.existing(p, Follow::Yes)?;
        if atime.is_some() || mtime.is_some() {
            let change = state.store(key, node);
            change.atime = atime.or(change.atime);
            change.mtime = mtime.or(change.mtime);
        }
        Ok(())
    }

    fn env_get(&self, key: &[u8]) -> Option<BString> {
        let state = self.state.read().ok()?;
        let key = env_key(key);
        match state.env.get(&key) {
            Some(value) => value.clone(),
            None => self.base.env_get(&key),
        }
    }

    fn env_set(&self, key: &[u8], value: Option<&[u8]>) {
        if let Ok(mut state) = self.state.write() {
            state.env.insert(env_key(key), value.map(BString::from));
        }
    }

    fn env_all(&self) -> Vec<(BString, BString)> {
        let mut values: BTreeMap<_, _> = self.base.env_all().into_iter().collect();
        if let Ok(state) = self.state.read() {
            for (key, value) in &state.env {
                match value {
                    Some(value) => {
                        values.insert(key.clone(), value.clone());
                    }
                    None => {
                        values.remove(key);
                    }
                }
            }
        }
        values.into_iter().collect()
    }

    fn run(
        &self,
        _req: &RunRequest,
        _on_output: &mut dyn FnMut(Stream, &[u8]),
    ) -> HostResult<RunOutput> {
        Err(HostError::unsupported(
            "process execution is disabled in this host mode",
        ))
    }

    crate::no_process::forward_host_services!(base);
}

fn env_key(key: &[u8]) -> BString {
    let mut key = BString::from(key);
    if cfg!(windows) {
        key.make_ascii_uppercase();
    }
    key
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::PureHost;

    #[test]
    fn conforms_to_shared_host_checks() {
        let host = OverlayHost::new(PureHost::default());
        crate::conformance::run_all(&host, Path::new("/work/conformance"));
    }
}
