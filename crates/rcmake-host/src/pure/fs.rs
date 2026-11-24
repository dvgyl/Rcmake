use super::require_absolute;
use std::collections::{BTreeMap, VecDeque};
use std::path::{Path, PathBuf};
use std::time::Duration;

use rcmake_base::BString;

use crate::error::{HostResult, SyntheticError};
use crate::{
    CopyOptions, DirEntry, FileKind, FileTime, Follow, Metadata, ReadRange, WriteMode, path,
};

enum Data {
    File(Vec<u8>),
    Dir,
    Symlink(PathBuf),
}

impl Data {
    fn kind(&self) -> FileKind {
        match self {
            Self::File(_) => FileKind::File,
            Self::Dir => FileKind::Dir,
            Self::Symlink(_) => FileKind::Symlink,
        }
    }

    fn default_mode(&self) -> u32 {
        match self {
            Self::File(_) => 0o644,
            Self::Dir | Self::Symlink(_) => 0o755,
        }
    }
}

struct Inode {
    data: Data,
    mode: u32,
    links: u64,
    atime: FileTime,
    mtime: FileTime,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Missing {
    Reject,
    Leaf,
    Directories,
}

fn windows_separator(byte: u8) -> bool {
    matches!(byte, b'/' | b'\\')
}

fn separator(byte: u8, windows: bool) -> bool {
    byte == b'/' || (windows && byte == b'\\')
}

fn drive_prefix(bytes: &[u8]) -> bool {
    matches!(bytes, [letter, b':', ..] if letter.is_ascii_alphabetic())
}

pub(super) fn root(bytes: &[u8]) -> Option<(BString, &[u8], bool)> {
    if let [letter, b':', sep, rest @ ..] = bytes
        && letter.is_ascii_alphabetic()
        && windows_separator(*sep)
    {
        return Some((
            vec![letter.to_ascii_uppercase(), b':', b'/'].into(),
            rest,
            true,
        ));
    }
    let [first, rest @ ..] = bytes else {
        return None;
    };
    if !windows_separator(*first) {
        return None;
    }
    if let [second, remote @ ..] = rest
        && windows_separator(*second)
    {
        let mut parts = remote.splitn(3, |&byte| windows_separator(byte));
        if let (Some(server), Some(share)) = (parts.next(), parts.next())
            && !server.is_empty()
            && !share.is_empty()
            && !matches!(server, b"." | b".." | b"?")
            && !matches!(share, b"." | b"..")
        {
            return Some((
                [b"//", server, b"/", share, b"/"].concat().into(),
                parts.next().unwrap_or_default(),
                true,
            ));
        }
    }
    Some(("/".into(), rest, cfg!(windows) || *first == b'\\'))
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(super) struct VirtualPath {
    root: BString,
    // A zero byte ends each component. All descendants share this byte prefix.
    relative: BString,
}

impl VirtualPath {
    fn at_root(root: BString) -> Self {
        Self {
            root,
            relative: BString::default(),
        }
    }

    fn is_root(&self) -> bool {
        self.relative.is_empty()
    }

    fn push(&mut self, part: &[u8]) {
        self.relative.extend_from_slice(part);
        self.relative.push(0);
    }

    fn pop(&mut self) {
        let parent_len = self.parent_and_name().0.len();
        self.relative.truncate(parent_len);
    }

    fn parent_and_name(&self) -> (&[u8], &[u8]) {
        let relative = self.relative.strip_suffix(b"\0").unwrap_or_default();
        let end = relative
            .iter()
            .rposition(|&byte| byte == 0)
            .map_or(0, |i| i + 1);
        relative.split_at(end)
    }

    fn is_under(&self, path: &Self) -> bool {
        self.root == path.root && self.relative.starts_with(&path.relative)
    }

    pub(super) fn to_path_buf(&self) -> PathBuf {
        let mut bytes = self.root.clone();
        let relative = self.relative.strip_suffix(b"\0").unwrap_or_default();
        bytes.extend(
            relative
                .iter()
                .map(|&byte| if byte == 0 { b'/' } else { byte }),
        );
        path::to_native(&bytes)
    }
}

fn removal_path(input: &Path) -> HostResult<PathBuf> {
    let bytes = path::from_native(input);
    let (_, rest, windows) =
        root(&bytes).ok_or_else(|| crate::HostError::not_absolute(input.to_path_buf()))?;
    if rest.iter().all(|&byte| separator(byte, windows)) {
        return Err(SyntheticError::ResourceBusy.at(Some(input)));
    }
    path::directory_removal_path(input, require_absolute, windows)
}

fn components(bytes: &[u8], windows: bool, create_dirs: bool) -> VecDeque<(BString, bool)> {
    let separator = |byte| separator(byte, windows);
    let mut parts: VecDeque<_> = bytes
        .split(|&byte| separator(byte))
        .filter(|part| !part.is_empty())
        .map(|part| (part.into(), create_dirs))
        .collect();
    if !parts.is_empty() && bytes.last().copied().is_some_and(separator) {
        parts.push_back((".".into(), create_dirs));
    }
    parts
}

pub(super) struct FileSystem {
    paths: BTreeMap<VirtualPath, u64>,
    inodes: BTreeMap<u64, Inode>,
    next_ino: u64,
    pub(super) now: FileTime,
    pub(super) monotonic: Duration,
}

impl FileSystem {
    pub(super) fn new(cwd: &Path) -> Self {
        let mut fs = Self {
            paths: BTreeMap::new(),
            inodes: BTreeMap::new(),
            next_ino: 1,
            now: FileTime {
                secs: 1_700_000_000,
                nanos: 0,
            },
            monotonic: Duration::ZERO,
        };
        fs.insert(VirtualPath::at_root("/".into()), Data::Dir);
        let _ = fs.create_dir_all(cwd);
        fs
    }

    pub(super) fn advance(&mut self, duration: Duration) {
        self.monotonic = self.monotonic.saturating_add(duration);
        let nanos = self.now.nanos + duration.subsec_nanos();
        let secs = i128::from(self.now.secs)
            + i128::from(duration.as_secs())
            + i128::from(nanos / 1_000_000_000);
        self.now = match i64::try_from(secs) {
            Ok(secs) => FileTime {
                secs,
                nanos: nanos % 1_000_000_000,
            },
            Err(_) => FileTime {
                secs: i64::MAX,
                nanos: 999_999_999,
            },
        };
    }

    fn insert(&mut self, path: VirtualPath, data: Data) -> u64 {
        self.advance(Duration::from_nanos(1));
        let ino = self.next_ino;
        self.next_ino += 1;
        self.inodes.insert(
            ino,
            Inode {
                mode: data.default_mode(),
                data,
                links: 1,
                atime: self.now,
                mtime: self.now,
            },
        );
        self.paths.insert(path, ino);
        ino
    }

    fn lookup(&self, path: &VirtualPath) -> Option<&Inode> {
        self.inodes.get(self.paths.get(path)?)
    }

    fn unlink(&mut self, path: &VirtualPath) {
        if let Some(ino) = self.paths.remove(path)
            && let Some(inode) = self.inodes.get_mut(&ino)
        {
            inode.links -= 1;
            if inode.links == 0 {
                self.inodes.remove(&ino);
            }
        }
    }

    fn descendants<'a>(&'a self, path: &'a VirtualPath) -> impl Iterator<Item = &'a VirtualPath> {
        use std::ops::Bound::{Excluded, Unbounded};
        self.paths
            .range((Excluded(path), Unbounded))
            .map(|(entry, _)| entry)
            .take_while(move |entry| entry.is_under(path))
    }

    fn resolve(
        &mut self,
        path: &Path,
        follow: Follow,
        missing: Missing,
    ) -> HostResult<VirtualPath> {
        let bytes = path::from_native(path);
        let (root_name, rest, mut windows) =
            root(&bytes).ok_or_else(|| crate::HostError::not_absolute(path.to_path_buf()))?;
        if bytes.as_slice().contains(&0) {
            return Err(SyntheticError::InvalidInput.at(Some(path)));
        }
        let mut resolved = VirtualPath::at_root(root_name);
        if !self.paths.contains_key(&resolved) {
            if missing != Missing::Directories {
                return Err(SyntheticError::NotFound.at(Some(path)));
            }
            self.insert(resolved.clone(), Data::Dir);
        }
        let mut pending = components(rest, windows, missing == Missing::Directories);
        let mut hops = 0;
        while let Some((part, create_dir)) = pending.pop_front() {
            if part == "." {
                continue;
            }
            if part == ".." {
                resolved.pop();
                continue;
            }
            resolved.push(&part);
            let Some(inode) = self.lookup(&resolved) else {
                if create_dir {
                    self.insert(resolved.clone(), Data::Dir);
                    continue;
                }
                if pending.is_empty() && missing == Missing::Leaf {
                    return Ok(resolved);
                }
                return Err(SyntheticError::NotFound.at(Some(path)));
            };
            let last = pending.is_empty();
            match &inode.data {
                Data::Symlink(target) if !last || follow == Follow::Yes => {
                    hops += 1;
                    if hops > 40 {
                        return Err(SyntheticError::LinkLoop.at(Some(path)));
                    }
                    let target = path::from_native(target);
                    let rest = if let Some((root_name, rest, target_windows)) = root(&target) {
                        resolved = VirtualPath::at_root(root_name);
                        windows = target_windows;
                        if !self.paths.contains_key(&resolved) {
                            return Err(SyntheticError::NotFound.at(Some(path)));
                        }
                        rest
                    } else {
                        if drive_prefix(&target) {
                            return Err(SyntheticError::InvalidInput.at(Some(path)));
                        }
                        resolved.pop();
                        target.as_slice()
                    };
                    let mut parts = components(rest, windows, false);
                    parts.append(&mut pending);
                    pending = parts;
                }
                Data::Dir => {}
                _ if !last || missing == Missing::Directories => {
                    return Err(SyntheticError::NotADirectory.at(Some(path)));
                }
                _ => {}
            }
        }
        Ok(resolved)
    }

    fn existing(&mut self, path: &Path, follow: Follow) -> HostResult<(VirtualPath, u64)> {
        let resolved = self.resolve(path, follow, Missing::Reject)?;
        let ino = self
            .paths
            .get(&resolved)
            .copied()
            .ok_or_else(|| SyntheticError::NotFound.at(Some(path)))?;
        Ok((resolved, ino))
    }

    fn inode(&self, ino: u64, path: &Path) -> HostResult<&Inode> {
        self.inodes
            .get(&ino)
            .ok_or_else(|| SyntheticError::NotFound.at(Some(path)))
    }

    fn inode_mut(&mut self, ino: u64, path: &Path) -> HostResult<&mut Inode> {
        self.inodes
            .get_mut(&ino)
            .ok_or_else(|| SyntheticError::NotFound.at(Some(path)))
    }

    pub(super) fn metadata(&mut self, path: &Path, follow: Follow) -> HostResult<Metadata> {
        let (resolved, ino) = self.existing(path, follow)?;
        let inode = self.inode(ino, path)?;
        let (len, nlink) = match &inode.data {
            Data::File(bytes) => (bytes.len() as u64, inode.links),
            Data::Symlink(target) => (path::from_native(target).len() as u64, inode.links),
            Data::Dir => {
                let dirs = self
                    .descendants(&resolved)
                    .filter(|child| child.parent_and_name().0 == resolved.relative.as_slice())
                    .filter(|child| {
                        self.lookup(child)
                            .is_some_and(|node| matches!(node.data, Data::Dir))
                    })
                    .count();
                (0, 2 + dirs as u64)
            }
        };
        Ok(Metadata {
            kind: inode.data.kind(),
            len,
            mode: inode.mode,
            mtime: inode.mtime,
            atime: inode.atime,
            dev: 1,
            ino,
            nlink,
        })
    }

    pub(super) fn read(&mut self, path: &Path, range: ReadRange) -> HostResult<Vec<u8>> {
        let (_, ino) = self.existing(path, Follow::Yes)?;
        let Data::File(bytes) = &self.inode(ino, path)?.data else {
            return Err(SyntheticError::IsADirectory.at(Some(path)));
        };
        let bytes = match range {
            ReadRange::All { limit } => {
                if bytes.len() as u64 > limit {
                    return Err(SyntheticError::FileTooLarge.at(Some(path)));
                }
                bytes.clone()
            }
            ReadRange::Slice { offset, len } => {
                let start = usize::try_from(offset)
                    .unwrap_or(usize::MAX)
                    .min(bytes.len());
                let len = len
                    .and_then(|len| usize::try_from(len).ok())
                    .unwrap_or(usize::MAX);
                let end = start.saturating_add(len).min(bytes.len());
                bytes.get(start..end).unwrap_or_default().to_vec()
            }
        };
        self.advance(Duration::from_nanos(1));
        self.inode_mut(ino, path)?.atime = self.now;
        Ok(bytes)
    }

    pub(super) fn seed_file(&mut self, input: &Path, bytes: &[u8]) -> HostResult<()> {
        let name = path::from_native(input);
        let (root_name, rest, windows) =
            root(&name).ok_or_else(|| crate::HostError::not_absolute(input.to_path_buf()))?;
        if let Some(end) = rest.iter().rposition(|&byte| separator(byte, windows)) {
            let mut parent = root_name;
            if parent == "/" && name.starts_with(b"\\") {
                parent = "\\".into();
            }
            parent.extend_from_slice(rest.get(..end).unwrap_or_default());
            self.create_dir_all(&path::to_native(&parent))?;
        } else {
            self.create_dir_all(&path::to_native(&root_name))?;
        }
        self.write(input, bytes, WriteMode::Truncate)
    }

    pub(super) fn write(&mut self, path: &Path, bytes: &[u8], mode: WriteMode) -> HostResult<()> {
        let follow = if mode == WriteMode::CreateNew {
            Follow::No
        } else {
            Follow::Yes
        };
        let resolved = self.resolve(path, follow, Missing::Leaf)?;
        if let Some(ino) = self.paths.get(&resolved).copied() {
            if mode == WriteMode::CreateNew {
                return Err(SyntheticError::AlreadyExists.at(Some(path)));
            }
            self.advance(Duration::from_nanos(1));
            let now = self.now;
            let inode = self.inode_mut(ino, path)?;
            let Data::File(data) = &mut inode.data else {
                return Err(SyntheticError::IsADirectory.at(Some(path)));
            };
            if mode == WriteMode::Truncate {
                data.clear();
            }
            data.extend_from_slice(bytes);
            inode.mtime = now;
            inode.atime = now;
        } else {
            self.insert(resolved, Data::File(bytes.to_vec()));
        }
        Ok(())
    }

    pub(super) fn read_dir(&mut self, path: &Path) -> HostResult<Vec<DirEntry>> {
        let (resolved, ino) = self.existing(path, Follow::Yes)?;
        if !matches!(self.inode(ino, path)?.data, Data::Dir) {
            return Err(SyntheticError::NotADirectory.at(Some(path)));
        }
        Ok(self
            .descendants(&resolved)
            .filter(|child| child.parent_and_name().0 == resolved.relative.as_slice())
            .filter_map(|child| {
                Some(DirEntry {
                    name: child.parent_and_name().1.into(),
                    kind: self.lookup(child)?.data.kind(),
                })
            })
            .collect())
    }

    pub(super) fn create_dir_all(&mut self, path: &Path) -> HostResult<()> {
        let result = self.resolve(path, Follow::Yes, Missing::Directories);
        if result
            .as_ref()
            .is_err_and(|err| err.kind == std::io::ErrorKind::NotFound)
            && let Ok(unresolved) = removal_path(path)
            && let Ok((_, ino)) = self.existing(&unresolved, Follow::No)
            && matches!(self.inode(ino, path)?.data, Data::Symlink(_))
        {
            return Err(SyntheticError::AlreadyExists.at(Some(path)));
        }
        result.map(|_| ())
    }

    pub(super) fn remove_file(&mut self, path: &Path) -> HostResult<()> {
        let (resolved, ino) = self.existing(path, Follow::No)?;
        if matches!(self.inode(ino, path)?.data, Data::Dir) {
            return Err(SyntheticError::IsADirectory.at(Some(path)));
        }
        self.unlink(&resolved);
        Ok(())
    }

    pub(super) fn remove_dir(&mut self, path: &Path) -> HostResult<()> {
        let trimmed = removal_path(path)?;
        let (resolved, ino) = self.existing(&trimmed, Follow::No)?;
        if !matches!(self.inode(ino, path)?.data, Data::Dir) {
            return Err(SyntheticError::NotADirectory.at(Some(path)));
        }
        if resolved.is_root() {
            return Err(SyntheticError::ResourceBusy.at(Some(path)));
        }
        if self.descendants(&resolved).next().is_some() {
            return Err(SyntheticError::DirectoryNotEmpty.at(Some(path)));
        }
        self.unlink(&resolved);
        Ok(())
    }

    pub(super) fn remove_dir_all(&mut self, path: &Path) -> HostResult<()> {
        let trimmed = removal_path(path)?;
        let (resolved, ino) = self.existing(&trimmed, Follow::No)?;
        if resolved.is_root() {
            return Err(SyntheticError::ResourceBusy.at(Some(path)));
        }
        match self.inode(ino, path)?.data {
            Data::Dir => {
                let children: Vec<_> = self.descendants(&resolved).cloned().collect();
                for child in children {
                    self.unlink(&child);
                }
            }
            Data::Symlink(_) => {}
            Data::File(_) => return Err(SyntheticError::NotADirectory.at(Some(path))),
        }
        self.unlink(&resolved);
        Ok(())
    }

    pub(super) fn rename(&mut self, from: &Path, to: &Path) -> HostResult<()> {
        require_absolute(from)?;
        require_absolute(to)?;
        let (src, ino) = self.existing(from, Follow::No)?;
        let dst = self.resolve(to, Follow::No, Missing::Leaf)?;
        if self.paths.get(&dst) == Some(&ino) {
            return Ok(());
        }
        if src.is_root() || dst.is_root() {
            return Err(SyntheticError::ResourceBusy.at(Some(from)));
        }
        let src_dir = matches!(self.inode(ino, from)?.data, Data::Dir);
        if src_dir && dst.is_under(&src) {
            return Err(SyntheticError::InvalidInput.at(Some(to)));
        }
        if let Some(node) = self.lookup(&dst) {
            match (src_dir, &node.data) {
                (true, Data::File(_) | Data::Symlink(_)) => {
                    return Err(SyntheticError::NotADirectory.at(Some(to)));
                }
                (false, Data::Dir) => return Err(SyntheticError::IsADirectory.at(Some(to))),
                (true, Data::Dir) if self.descendants(&dst).next().is_some() => {
                    return Err(SyntheticError::DirectoryNotEmpty.at(Some(to)));
                }
                _ => {}
            }
        }
        self.unlink(&dst);
        let children: Vec<_> = self.descendants(&src).cloned().collect();
        for child in children {
            if let Some(relative) = child.relative.strip_prefix(src.relative.as_slice())
                && let Some(ino) = self.paths.remove(&child)
            {
                let mut entry = dst.clone();
                entry.relative.extend_from_slice(relative);
                self.paths.insert(entry, ino);
            }
        }
        self.paths.remove(&src);
        self.paths.insert(dst, ino);
        Ok(())
    }

    pub(super) fn copy_file(
        &mut self,
        from: &Path,
        to: &Path,
        opts: CopyOptions,
    ) -> HostResult<()> {
        require_absolute(from)?;
        require_absolute(to)?;
        let (_, src) = self.existing(from, Follow::Yes)?;
        let dst = self.resolve(to, Follow::Yes, Missing::Leaf)?;
        if self.paths.get(&dst) == Some(&src) {
            return Err(SyntheticError::InvalidInput.at(Some(to)));
        }
        let inode = self.inode(src, from)?;
        let Data::File(bytes) = &inode.data else {
            return Err(SyntheticError::InvalidInput.at(Some(from)));
        };
        let (bytes, mode, mtime) = (bytes.clone(), inode.mode, inode.mtime);
        self.write(to, &bytes, WriteMode::Truncate)?;
        if let Some(ino) = self.paths.get(&dst).copied() {
            let inode = self.inode_mut(ino, to)?;
            if opts.preserve_permissions {
                inode.mode = mode;
            }
            if opts.preserve_mtime {
                inode.mtime = mtime;
            }
        }
        Ok(())
    }

    pub(super) fn symlink(&mut self, target: &Path, link: &Path) -> HostResult<()> {
        let resolved = self.resolve(link, Follow::No, Missing::Leaf)?;
        if self.paths.contains_key(&resolved) {
            return Err(SyntheticError::AlreadyExists.at(Some(link)));
        }
        if target.as_os_str().is_empty() {
            return Err(SyntheticError::NotFound.at(Some(link)));
        }
        if path::from_native(target).as_slice().contains(&0) {
            return Err(SyntheticError::InvalidInput.at(Some(link)));
        }
        self.insert(resolved, Data::Symlink(target.to_path_buf()));
        Ok(())
    }

    pub(super) fn hard_link(&mut self, src: &Path, dst: &Path) -> HostResult<()> {
        require_absolute(src)?;
        require_absolute(dst)?;
        let (_, ino) = self.existing(src, Follow::No)?;
        let resolved = self.resolve(dst, Follow::No, Missing::Leaf)?;
        if self.paths.contains_key(&resolved) {
            return Err(SyntheticError::AlreadyExists.at(Some(dst)));
        }
        let inode = self.inode_mut(ino, src)?;
        if matches!(inode.data, Data::Dir) {
            return Err(SyntheticError::PermissionDenied.at(Some(src)));
        }
        inode.links += 1;
        self.paths.insert(resolved, ino);
        Ok(())
    }

    pub(super) fn read_link(&mut self, path: &Path) -> HostResult<PathBuf> {
        let (_, ino) = self.existing(path, Follow::No)?;
        match &self.inode(ino, path)?.data {
            Data::Symlink(target) => Ok(target.clone()),
            _ => Err(SyntheticError::InvalidInput.at(Some(path))),
        }
    }

    pub(super) fn canonicalize(&mut self, path: &Path) -> HostResult<PathBuf> {
        self.canonical_key(path).map(|key| key.to_path_buf())
    }

    pub(super) fn canonical_key(&mut self, path: &Path) -> HostResult<VirtualPath> {
        self.resolve(path, Follow::Yes, Missing::Reject)
    }

    pub(super) fn set_permissions(&mut self, path: &Path, mode: u32) -> HostResult<()> {
        let (_, ino) = self.existing(path, Follow::Yes)?;
        self.inode_mut(ino, path)?.mode = mode & 0o7777;
        Ok(())
    }

    pub(super) fn set_times(
        &mut self,
        path: &Path,
        atime: Option<FileTime>,
        mtime: Option<FileTime>,
    ) -> HostResult<()> {
        require_absolute(path)?;
        if atime
            .into_iter()
            .chain(mtime)
            .any(|time| time.nanos >= 1_000_000_000)
        {
            return Err(SyntheticError::InvalidInput.at(Some(path)));
        }
        let (_, ino) = self.existing(path, Follow::Yes)?;
        let inode = self.inode_mut(ino, path)?;
        if let Some(atime) = atime {
            inode.atime = atime;
        }
        if let Some(mtime) = mtime {
            inode.mtime = mtime;
        }
        Ok(())
    }
}
