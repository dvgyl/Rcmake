//! An in-memory filesystem view over a stable base host.

#![forbid(unsafe_code)]

mod paths;
mod state;

#[cfg(test)]
mod tests;

use core::time::Duration;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
#[cfg(not(target_family = "wasm"))]
use std::sync::Condvar;
use std::sync::{Arc, Mutex, RwLock, RwLockReadGuard, RwLockWriteGuard};

use rcmake_base::BString;

use crate::error::SyntheticError;
use crate::{
    CopyOptions, DirEntry, FILE_CHUNK_SIZE, FileKind, FileLockGuard, FileTime, Follow, Host,
    HostError, HostResult, Metadata, ReadRange, RunOutput, RunRequest, Stream, WriteMode, path,
};

use self::paths::{budget_error, validate_length};
use self::state::{CONTENTS_OVERHEAD, Contents, Edit, Entry, Inode, Node, Resolved, State};

pub use self::paths::{OverlayPathPolicy, OverlayPathStyle};

const MAX_OVERLAY_BYTES: usize = 256 * 1024 * 1024;
const MAX_HELD_LOCKS: usize = 65_536;

#[derive(Debug, Default)]
struct LockTable {
    held: Mutex<BTreeSet<Inode>>,
    #[cfg(not(target_family = "wasm"))]
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
        #[cfg(not(target_family = "wasm"))]
        self.table.released.notify_all();
    }
}

/// Counts of changes held in the overlay.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct OverlayAudit {
    /// Paths added, changed, or moved in the overlay, as in [`OverlayHost::written_paths`].
    pub written_paths: usize,
    /// Tombstones that hide base entries or subtrees.
    pub deleted_paths: usize,
    /// Files with overlay contents, counted once per inode.
    pub overlay_files: usize,
    /// Environment keys set or unset in the overlay.
    pub environment_overrides: usize,
}

/// Stores explicit filesystem and environment changes in memory.
///
/// The base must remain stable during use. Base reads can change access times.
/// Standard output and error go to the base. The overlay denies process requests.
/// Wrap it in [`crate::no_process::NoProcessHost`] to record those requests.
///
/// The overlay limits the retained state to 256 MiB and 65,536 entries. When an
/// environment change exceeds a limit, [`Host::env_set`] leaves the value in place.
pub struct OverlayHost<H: Host + ?Sized> {
    base: Arc<H>,
    state: RwLock<State<H>>,
    locks: Arc<LockTable>,
}

impl<H: Host> OverlayHost<H> {
    /// Wrap an owned host with the default path policy.
    #[must_use]
    pub fn new(base: H) -> Self {
        Self::from_shared(Arc::new(base))
    }
}

impl<H: Host + ?Sized> OverlayHost<H> {
    /// Wrap a shared host, including a shared trait object.
    ///
    /// The default uses virtual paths for [`crate::PureHost`]. For other hosts it
    /// uses the platform's path style. Windows and macOS use case-insensitive
    /// lookup by default. Set the policy explicitly for other filesystem rules.
    #[must_use]
    pub fn from_shared(base: Arc<H>) -> Self {
        let virtual_base = base.system().sysname.as_slice() == b"PureOS";
        let policy = OverlayPathPolicy {
            style: if virtual_base {
                OverlayPathStyle::Virtual
            } else if cfg!(windows) {
                OverlayPathStyle::Windows
            } else {
                OverlayPathStyle::Posix
            },
            case_sensitive: virtual_base || !cfg!(any(windows, target_os = "macos")),
        };
        Self::with_path_policy(base, policy)
    }

    /// Wrap a shared host with explicit root, separator, and case rules.
    #[must_use]
    pub fn with_path_policy(base: Arc<H>, policy: OverlayPathPolicy) -> Self {
        Self::with_limit(base, policy, MAX_OVERLAY_BYTES)
    }

    fn with_limit(base: Arc<H>, policy: OverlayPathPolicy, limit: usize) -> Self {
        Self {
            state: RwLock::new(State::new(Arc::clone(&base), policy, limit)),
            base,
            locks: Arc::default(),
        }
    }

    /// Return sorted paths added, changed, or moved in the overlay.
    ///
    /// This includes both names supplied to [`Host::hard_link`] and base aliases
    /// changed by path. Other base hard links share the inode changes when accessed.
    /// The overlay does not search the base tree for their names. Implicit changes
    /// to parent directory times do not add paths to this list.
    #[must_use]
    pub fn written_paths(&self) -> Vec<PathBuf> {
        self.paths(true)
    }

    /// Return sorted tombstone paths.
    #[must_use]
    pub fn deleted_paths(&self) -> Vec<PathBuf> {
        self.paths(false)
    }

    fn paths(&self, written: bool) -> Vec<PathBuf> {
        let state = self
            .state
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut paths: Vec<_> = state
            .paths
            .values()
            .filter(|entry| entry.node.is_some() == written)
            .map(|entry| entry.path.clone())
            .collect();
        paths.sort_unstable();
        paths
    }

    /// Return a copy of the file's overlay contents, following symbolic links.
    ///
    /// Return `None` when the contents still come from the base host.
    #[must_use]
    pub fn read_overlay_file(&self, p: &Path) -> Option<Vec<u8>> {
        let state = self.state.read().ok()?;
        let entry = state.existing(p, Follow::Yes).ok()?;
        state
            .bytes(entry.existing(p).ok()?)
            .map(|bytes| bytes.0.to_vec())
    }

    /// Count retained overlay changes.
    #[must_use]
    pub fn audit(&self) -> OverlayAudit {
        let state = self
            .state
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let written_paths = state
            .paths
            .values()
            .filter(|entry| entry.node.is_some())
            .count();
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

    fn read_state(&self) -> HostResult<RwLockReadGuard<'_, State<H>>> {
        self.state.read().map_err(|_| poisoned())
    }

    fn write_state(&self) -> HostResult<RwLockWriteGuard<'_, State<H>>> {
        self.state.write().map_err(|_| poisoned())
    }

    fn set_entry_times(
        &self,
        p: &Path,
        atime: Option<FileTime>,
        mtime: Option<FileTime>,
        follow: Follow,
    ) -> HostResult<()> {
        if atime
            .into_iter()
            .chain(mtime)
            .any(|time| time.nanos >= 1_000_000_000)
        {
            return Err(SyntheticError::InvalidInput.at(Some(p)));
        }
        let mut state = self.write_state()?;
        let resolved = state.existing(p, follow)?;
        if atime.is_some() || mtime.is_some() {
            let node = resolved.existing(p)?.clone();
            let mut edit = Edit::default();
            let change = edit.change(&state, &node);
            change.atime = atime.or(change.atime);
            change.mtime = mtime.or(change.mtime);
            edit.store(resolved, node);
            state.check(&edit, 0, p)?;
            state.commit(edit);
        }
        Ok(())
    }

    fn rename_entry(&self, from: &Path, to: &Path, replace: bool) -> HostResult<()> {
        let mut state = self.write_state()?;
        let trailing_from = state
            .policy
            .with_absolute(from, |p| Ok(p.endpoint(from)?.1))?;
        let (name, trailing_to) = state.policy.with_absolute(to, |p| {
            let (name, trailing) = p.endpoint(to)?;
            Ok((name.to_vec(), trailing))
        })?;
        let src = state.resolve(from, Follow::No, false, None, true)?;
        let dst = state.resolve(to, Follow::No, true, None, true)?;
        let node = src.existing(from)?;
        let src_dir = node.stat.kind == FileKind::Dir;
        if trailing_from && !src_dir {
            return Err(SyntheticError::NotADirectory.at(Some(from)));
        }
        if trailing_to
            && (!src_dir
                || dst
                    .entry
                    .node
                    .as_ref()
                    .is_some_and(|n| n.stat.kind != FileKind::Dir))
        {
            return Err(SyntheticError::NotADirectory.at(Some(to)));
        }
        if !replace && dst.entry.node.is_some() {
            return Err(SyntheticError::AlreadyExists.at(Some(to)));
        }

        let renamed = dst.path_with_name(&name);
        let same_key = src.key == dst.key;
        let mut edit = Edit::default();
        if let Some(other) = &dst.entry.node {
            if other.inode() == node.inode() {
                if !same_key || renamed == src.entry.path {
                    return Ok(());
                }
            } else {
                match (src_dir, other.stat.kind) {
                    (true, FileKind::Dir) if !state.children(&dst, other)?.is_empty() => {
                        return Err(SyntheticError::DirectoryNotEmpty.at(Some(to)));
                    }
                    (true, FileKind::Dir) => {}
                    (true, _) => return Err(SyntheticError::NotADirectory.at(Some(to))),
                    (false, FileKind::Dir) => return Err(SyntheticError::IsADirectory.at(Some(to))),
                    (false, _) => {}
                }
                edit.unlink(&state, other);
            }
        }
        if !same_key && dst.key.is_under(&src.key) {
            return Err(SyntheticError::InvalidInput.at(Some(to)));
        }
        if !same_key {
            for (key, _) in state.descendants(&dst.key) {
                edit.paths.insert(key.clone(), None);
            }
            edit.paths.insert(src.key.clone(), state.tombstone(&src)?);
        }

        let dest_display = path::from_native(&renamed);
        let mut name_bytes = 0_usize;
        for (key, entry) in state.descendants(&src.key) {
            let display = path::from_native(&entry.path);
            let levels = key
                .0
                .strip_prefix(src.key.0.as_slice())
                .unwrap_or_default()
                .split(|&byte| byte == 0)
                .count()
                - 1;
            let prefix = display
                .rsplitn(levels + 1, |&byte| byte == b'/')
                .nth(levels)
                .ok_or_else(|| SyntheticError::InvalidInput.at(Some(from)))?;
            let suffix = display.get(prefix.len()..).unwrap_or_default();
            name_bytes = name_bytes.saturating_add(
                dest_display.len() + suffix.len() + dst.key.0.len() + key.0.len() - src.key.0.len(),
            );
            if name_bytes > state.limit() {
                return Err(budget_error(to));
            }
            let name = path::to_native(&[dest_display.as_slice(), suffix].concat());
            validate_length(&name)?;
            edit.paths.insert(key.clone(), None);
            edit.paths.insert(
                key.rebased(&src.key, &dst.key),
                Some(Entry {
                    path: name,
                    node: entry.node.clone(),
                }),
            );
        }

        let now = state.tick();
        edit.touch_parent(&state, &src, now);
        edit.touch_parent(&state, &dst, now);
        edit.paths.insert(
            dst.key,
            Some(Entry {
                path: renamed,
                node: Some(node.clone()),
            }),
        );
        state.check(&edit, 0, to)?;
        state.commit(edit);
        Ok(())
    }

    fn remove_entry(&self, p: &Path, directory: Option<bool>) -> HostResult<()> {
        let mut state = self.write_state()?;
        let resolved = if directory.is_some() {
            state
                .policy
                .with_absolute(p, |parsed| parsed.endpoint(p).map(|_| ()))?;
            state.resolve(p, Follow::No, false, None, true)?
        } else {
            state.existing(p, Follow::No)?
        };
        let node = resolved.existing(p)?;
        let mut edit = Edit::default();
        match (directory, node.stat.kind) {
            (None, FileKind::Dir) => return Err(SyntheticError::IsADirectory.at(Some(p))),
            (Some(false), FileKind::Dir) => {
                if !state.children(&resolved, node)?.is_empty() {
                    return Err(SyntheticError::DirectoryNotEmpty.at(Some(p)));
                }
            }
            (Some(true), FileKind::Dir | FileKind::Symlink) | (None, _) => {}
            (Some(_), _) => return Err(SyntheticError::NotADirectory.at(Some(p))),
        }
        if directory == Some(true) {
            state.remove_tree(&mut edit, &resolved, p)?;
        } else {
            edit.unlink(&state, node);
        }
        let now = state.tick();
        edit.touch_parent(&state, &resolved, now);
        for (key, _) in state.descendants(&resolved.key) {
            edit.paths.insert(key.clone(), None);
        }
        let tombstone = state.tombstone(&resolved)?;
        edit.paths.insert(resolved.key, tombstone);
        state.check(&edit, 0, p)?;
        state.commit(edit);
        Ok(())
    }

    fn file_edit(
        state: &mut State<H>,
        dest: Resolved,
        len: usize,
        p: &Path,
    ) -> HostResult<(Edit, Node)> {
        let now = state.tick();
        let mut edit = Edit::default();
        let node = if let Some(node) = &dest.entry.node {
            node.require_file(p)?;
            node.clone()
        } else {
            edit.touch_parent(state, &dest, now);
            state.new_node(FileKind::File, now)
        };
        edit.store(dest, node.clone());
        let change = edit.change(state, &node);
        change.bytes = None;
        change.mtime = Some(now);
        change.atime = Some(now);
        state.check(&edit, len.saturating_add(CONTENTS_OVERHEAD), p)?;
        Ok((edit, node))
    }

    fn join_file_bytes(
        &self,
        state: &State<H>,
        source: &Node,
        prefix_len: usize,
        suffix: &[u8],
        p: &Path,
    ) -> HostResult<Arc<[u8]>> {
        if let Some(bytes) = state.bytes(source) {
            return Ok(bytes.0.iter().chain(suffix).copied().collect());
        }
        // Read directly into the final allocation. Appending never grows a Vec
        // or buffers a second copy of the base contents.
        let len = prefix_len + suffix.len();
        let mut bytes: Arc<[u8]> = std::iter::repeat_n(0, len).collect();
        let dest = Arc::get_mut(&mut bytes).ok_or_else(poisoned)?;
        let (prefix, tail) = dest.split_at_mut(prefix_len);
        let mut offset = 0_usize;
        self.base.read_chunks(source.backing(p)?, &mut |chunk| {
            let end = offset.saturating_add(chunk.len());
            if let Some(slot) = prefix.get_mut(offset..end) {
                slot.copy_from_slice(chunk);
            }
            offset = end;
        })?;
        if offset != prefix_len {
            return Err(HostError::from_io(
                std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "The base file size changed",
                ),
                Some(p.to_path_buf()),
            ));
        }
        tail.copy_from_slice(suffix);
        Ok(bytes)
    }
}

impl<H: Host + ?Sized> Host for OverlayHost<H> {
    fn metadata(&self, p: &Path, follow: Follow) -> HostResult<Metadata> {
        let state = self.read_state()?;
        let resolved = state.existing(p, follow)?;
        let node = resolved.existing(p)?;
        let mut stat = state.stat(node);
        if stat.kind == FileKind::Dir
            && stat.nlink >= 2
            && state
                .descendants(&resolved.key)
                .any(|(key, _)| key.is_child_of(&resolved.key))
            && let Ok(entries) = state.children(&resolved, node)
        {
            stat.nlink = 2 + entries
                .iter()
                .filter(|entry| entry.kind == FileKind::Dir)
                .count() as u64;
        }
        Ok(stat)
    }

    fn read(&self, p: &Path, range: ReadRange) -> HostResult<Vec<u8>> {
        self.read_with_origin(p, range, p)
    }

    fn read_with_origin(&self, p: &Path, range: ReadRange, origin: &Path) -> HostResult<Vec<u8>> {
        let state = self.read_state()?;
        let resolved = state.existing(p, Follow::Yes)?;
        let node = resolved.existing(p)?;
        if node.stat.kind == FileKind::Dir {
            return Err(SyntheticError::IsADirectory.at(Some(p)));
        }
        let Some(bytes) = state.bytes(node) else {
            let backing = node.backing(p)?;
            drop(state);
            return self.base.read_with_origin(backing, range, origin);
        };
        let bytes = &bytes.0;
        match range {
            ReadRange::All { limit } if bytes.len() as u64 > limit => {
                Err(SyntheticError::FileTooLarge.at(Some(p)))
            }
            ReadRange::All { .. } => Ok(bytes.to_vec()),
            ReadRange::Slice { offset, len } => {
                let start = offset.min(bytes.len() as u64) as usize;
                let end = offset
                    .saturating_add(len.unwrap_or(u64::MAX))
                    .min(bytes.len() as u64);
                Ok(bytes.get(start..end as usize).unwrap_or_default().to_vec())
            }
        }
    }

    fn read_chunks(&self, p: &Path, consume: &mut dyn FnMut(&[u8])) -> HostResult<()> {
        let state = self.read_state()?;
        let resolved = state.existing(p, Follow::Yes)?;
        let node = resolved.existing(p)?;
        if node.stat.kind == FileKind::Dir {
            return Err(SyntheticError::IsADirectory.at(Some(p)));
        }
        if let Some(bytes) = state.bytes(node).cloned() {
            drop(state);
            for chunk in bytes.0.chunks(FILE_CHUNK_SIZE) {
                consume(chunk);
            }
            Ok(())
        } else {
            let backing = node.backing(p)?;
            drop(state);
            self.base.read_chunks(backing, consume)
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
        if mode == WriteMode::CreateNew && dest.entry.node.is_some() {
            return Err(SyntheticError::AlreadyExists.at(Some(p)));
        }
        let append = dest
            .entry
            .node
            .clone()
            .filter(|_| mode == WriteMode::Append);
        let prefix_len = append.as_ref().map_or(0, |node| state.stat(node).len);
        let prefix_len = usize::try_from(prefix_len).map_err(|_| budget_error(p))?;
        let len = prefix_len.saturating_add(data.len());
        let (mut edit, node) = Self::file_edit(&mut state, dest, len, p)?;
        let bytes = if let Some(source) = &append {
            self.join_file_bytes(&state, source, prefix_len, data, p)?
        } else {
            Arc::from(data)
        };
        edit.change(&state, &node).bytes = Some(Arc::new(Contents(bytes)));
        state.commit(edit);
        Ok(())
    }

    fn read_dir(&self, p: &Path) -> HostResult<Vec<DirEntry>> {
        let state = self.read_state()?;
        let dir = state.existing(p, Follow::Yes)?;
        let node = dir.existing(p)?;
        if node.stat.kind != FileKind::Dir {
            return Err(SyntheticError::NotADirectory.at(Some(p)));
        }
        state.children(&dir, node)
    }

    fn create_dir_all(&self, p: &Path) -> HostResult<()> {
        let mut state = self.write_state()?;
        let mut dirs = BTreeMap::new();
        state.resolve(p, Follow::Yes, false, Some(&mut dirs), false)?;
        if dirs.is_empty() {
            return Ok(());
        }
        let now = state.tick();
        let mut edit = Edit::default();
        for (key, (mut entry, parent)) in dirs {
            if let Some(parent) = parent {
                edit.change(&state, &parent).mtime = Some(now);
            }
            entry.node = Some(state.new_node(FileKind::Dir, now));
            edit.paths.insert(key, Some(entry));
        }
        state.check(&edit, 0, p)?;
        state.commit(edit);
        Ok(())
    }

    fn remove_file(&self, p: &Path) -> HostResult<()> {
        self.remove_entry(p, None)
    }

    fn remove_dir(&self, p: &Path) -> HostResult<()> {
        self.remove_entry(p, Some(false))
    }

    fn remove_dir_all(&self, p: &Path) -> HostResult<()> {
        self.remove_entry(p, Some(true))
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
            let display = dest.entry.path.clone();
            let key = if let Some(node) = &dest.entry.node {
                node.require_file(path)?;
                node.inode()
            } else {
                let (mut edit, node) = Self::file_edit(&mut state, dest, 0, path)?;
                edit.change(&state, &node).bytes = Some(Arc::new(Contents(Arc::<[u8]>::default())));
                state.commit(edit);
                node.inode()
            };
            (key, display)
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
        if held.len() >= MAX_HELD_LOCKS {
            return Err(budget_error(&path));
        }
        held.insert(key.clone());
        Ok(Box::new(OverlayLock {
            table: Arc::clone(&self.locks),
            key,
            path,
        }))
    }

    fn copy_file(&self, from: &Path, to: &Path, opts: CopyOptions) -> HostResult<()> {
        let mut state = self.write_state()?;
        state.policy.require_absolute(from)?;
        state.policy.require_absolute(to)?;
        let source = state.existing(from, Follow::Yes)?;
        let source = source.existing(from)?;
        if source.stat.kind != FileKind::File {
            return Err(SyntheticError::InvalidInput.at(Some(from)));
        }
        let dest = state.destination(to, Follow::Yes)?;
        if dest
            .entry
            .node
            .as_ref()
            .is_some_and(|node| node.inode() == source.inode())
        {
            return Err(SyntheticError::InvalidInput.at(Some(to)));
        }
        let stat = state.stat(source);
        let len = usize::try_from(stat.len).map_err(|_| budget_error(to))?;
        let (mut edit, node) = Self::file_edit(&mut state, dest, len, to)?;
        let bytes = self.join_file_bytes(&state, source, len, &[], from)?;
        let change = edit.change(&state, &node);
        change.bytes = Some(Arc::new(Contents(bytes)));
        if opts.preserve_permissions {
            change.mode = Some(stat.mode);
        }
        if opts.preserve_mtime {
            change.mtime = Some(stat.mtime);
        }
        state.commit(edit);
        Ok(())
    }

    fn symlink(&self, target: &Path, link: &Path) -> HostResult<()> {
        let mut state = self.write_state()?;
        let dest = state.destination(link, Follow::No)?;
        if dest.entry.node.is_some() {
            return Err(SyntheticError::AlreadyExists.at(Some(link)));
        }
        if target.as_os_str().is_empty() {
            return Err(SyntheticError::NotFound.at(Some(link)));
        }
        validate_length(target)?;
        let now = state.tick();
        let mut edit = Edit::default();
        edit.touch_parent(&state, &dest, now);
        let mut node = state.new_node(FileKind::Symlink, now);
        let key = dest.key.clone();
        edit.store(dest, node.clone());
        state.check(&edit, target.as_os_str().as_encoded_bytes().len(), link)?;
        node.set_target(target);
        if let Some(Some(entry)) = edit.paths.get_mut(&key) {
            entry.node = Some(node);
        }
        state.commit(edit);
        Ok(())
    }

    fn hard_link(&self, src: &Path, dst: &Path) -> HostResult<()> {
        let mut state = self.write_state()?;
        state.policy.require_absolute(src)?;
        state.policy.require_absolute(dst)?;
        let source = state.existing(src, Follow::No)?;
        let dest = state.destination(dst, Follow::No)?;
        if dest.entry.node.is_some() {
            return Err(SyntheticError::AlreadyExists.at(Some(dst)));
        }
        let node = source.existing(src)?.clone();
        if node.stat.kind == FileKind::Dir {
            return Err(SyntheticError::PermissionDenied.at(Some(src)));
        }
        let now = state.tick();
        let mut edit = Edit::default();
        edit.touch_parent(&state, &dest, now);
        edit.link(&state, &node);
        edit.store(source, node.clone());
        edit.store(dest, node);
        state.check(&edit, 0, dst)?;
        state.commit(edit);
        Ok(())
    }

    fn read_link(&self, p: &Path) -> HostResult<PathBuf> {
        let resolved = self.read_state()?.existing(p, Follow::No)?;
        let node = resolved.existing(p)?;
        if node.stat.kind != FileKind::Symlink {
            return Err(SyntheticError::InvalidInput.at(Some(p)));
        }
        node.target(self.base.as_ref(), p)
    }

    fn canonicalize(&self, p: &Path) -> HostResult<PathBuf> {
        Ok(self.read_state()?.existing(p, Follow::Yes)?.entry.path)
    }

    fn set_permissions(&self, p: &Path, mode: u32) -> HostResult<()> {
        let mut state = self.write_state()?;
        let resolved = state.existing(p, Follow::Yes)?;
        let node = resolved.existing(p)?.clone();
        let mut edit = Edit::default();
        edit.change(&state, &node).mode = Some(mode & 0o7777);
        edit.store(resolved, node);
        state.check(&edit, 0, p)?;
        state.commit(edit);
        Ok(())
    }

    fn set_times(
        &self,
        p: &Path,
        atime: Option<FileTime>,
        mtime: Option<FileTime>,
    ) -> HostResult<()> {
        self.set_entry_times(p, atime, mtime, Follow::Yes)
    }

    fn set_symlink_times(
        &self,
        p: &Path,
        atime: Option<FileTime>,
        mtime: Option<FileTime>,
    ) -> HostResult<()> {
        self.set_entry_times(p, atime, mtime, Follow::No)
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
        if key.len().saturating_add(value.map_or(0, <[u8]>::len)) <= MAX_OVERLAY_BYTES
            && let Ok(mut state) = self.state.write()
        {
            state.set_env(env_key(key), value);
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

    fn supports_process_execution(&self) -> bool {
        false
    }

    fn run(
        &self,
        _req: &RunRequest,
        _on_output: &mut dyn FnMut(Stream, &[u8]),
    ) -> HostResult<RunOutput> {
        Err(HostError::unsupported(crate::no_process::DENIED_MESSAGE))
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

fn poisoned() -> HostError {
    HostError::unsupported("Cannot access the overlay host state")
}
