use std::collections::{BTreeMap, VecDeque};
use std::path::{Path, PathBuf};
use std::time::Duration;

use rustix::io::Errno;

use crate::error::{HostResult, from_errno};
use crate::{
    CopyOptions, DirEntry, FileKind, FileTime, Follow, Metadata, ReadRange, WriteMode, path,
    require_absolute,
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

fn components(path: &Path, create_dirs: bool) -> VecDeque<(PathBuf, bool)> {
    let bytes = path::from_native(path);
    let bytes = bytes.as_slice();
    let mut parts: VecDeque<_> = bytes
        .split(|byte| *byte == b'/')
        .filter(|part| !part.is_empty())
        .map(|part| (path::to_native(part), create_dirs))
        .collect();
    if !parts.is_empty() && bytes.ends_with(b"/") {
        parts.push_back((PathBuf::from("."), create_dirs));
    }
    parts
}

pub(super) struct FileSystem {
    paths: BTreeMap<PathBuf, u64>,
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
        fs.insert(PathBuf::from("/"), Data::Dir);
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

    fn insert(&mut self, path: PathBuf, data: Data) -> u64 {
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

    fn lookup(&self, path: &Path) -> Option<&Inode> {
        self.inodes.get(self.paths.get(path)?)
    }

    fn unlink(&mut self, path: &Path) {
        if let Some(ino) = self.paths.remove(path)
            && let Some(inode) = self.inodes.get_mut(&ino)
        {
            inode.links -= 1;
            if inode.links == 0 {
                self.inodes.remove(&ino);
            }
        }
    }

    fn descendants<'a>(&'a self, path: &'a Path) -> impl Iterator<Item = &'a PathBuf> {
        use std::ops::Bound::{Excluded, Unbounded};
        self.paths
            .range::<Path, _>((Excluded(path), Unbounded))
            .map(|(entry, _)| entry)
            .take_while(move |entry| entry.starts_with(path))
    }

    fn resolve(&mut self, path: &Path, follow: Follow, missing: Missing) -> HostResult<PathBuf> {
        require_absolute(path)?;
        if path::from_native(path).as_slice().contains(&0) {
            return Err(from_errno(Errno::INVAL, Some(path)));
        }
        let mut resolved = PathBuf::from("/");
        let mut pending = components(path, missing == Missing::Directories);
        let mut hops = 0;
        while let Some((part, create_dir)) = pending.pop_front() {
            if part == Path::new(".") {
                continue;
            }
            if part == Path::new("..") {
                resolved.pop();
                continue;
            }
            resolved.push(part);
            let Some(inode) = self.lookup(&resolved) else {
                if create_dir {
                    self.insert(resolved.clone(), Data::Dir);
                    continue;
                }
                if pending.is_empty() && missing == Missing::Leaf {
                    return Ok(resolved);
                }
                return Err(from_errno(Errno::NOENT, Some(path)));
            };
            let last = pending.is_empty();
            match &inode.data {
                Data::Symlink(target) if !last || follow == Follow::Yes => {
                    hops += 1;
                    if hops > 40 {
                        return Err(from_errno(Errno::LOOP, Some(path)));
                    }
                    if target.is_absolute() {
                        resolved = PathBuf::from("/");
                    } else {
                        resolved.pop();
                    }
                    let mut parts = components(target, false);
                    parts.append(&mut pending);
                    pending = parts;
                }
                Data::Dir => {}
                _ if !last || missing == Missing::Directories => {
                    return Err(from_errno(Errno::NOTDIR, Some(path)));
                }
                _ => {}
            }
        }
        Ok(resolved)
    }

    fn existing(&mut self, path: &Path, follow: Follow) -> HostResult<(PathBuf, u64)> {
        let resolved = self.resolve(path, follow, Missing::Reject)?;
        let ino = self
            .paths
            .get(&resolved)
            .copied()
            .ok_or_else(|| from_errno(Errno::NOENT, Some(path)))?;
        Ok((resolved, ino))
    }

    fn inode(&self, ino: u64, path: &Path) -> HostResult<&Inode> {
        self.inodes
            .get(&ino)
            .ok_or_else(|| from_errno(Errno::NOENT, Some(path)))
    }

    fn inode_mut(&mut self, ino: u64, path: &Path) -> HostResult<&mut Inode> {
        self.inodes
            .get_mut(&ino)
            .ok_or_else(|| from_errno(Errno::NOENT, Some(path)))
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
                    .filter(|child| child.parent() == Some(resolved.as_path()))
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
            return Err(from_errno(Errno::ISDIR, Some(path)));
        };
        let bytes = match range {
            ReadRange::All { limit } => {
                if bytes.len() as u64 > limit {
                    return Err(from_errno(Errno::FBIG, Some(path)));
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

    pub(super) fn write(&mut self, path: &Path, bytes: &[u8], mode: WriteMode) -> HostResult<()> {
        let follow = if mode == WriteMode::CreateNew {
            Follow::No
        } else {
            Follow::Yes
        };
        let resolved = self.resolve(path, follow, Missing::Leaf)?;
        if let Some(ino) = self.paths.get(&resolved).copied() {
            if mode == WriteMode::CreateNew {
                return Err(from_errno(Errno::EXIST, Some(path)));
            }
            self.advance(Duration::from_nanos(1));
            let now = self.now;
            let inode = self.inode_mut(ino, path)?;
            let Data::File(data) = &mut inode.data else {
                return Err(from_errno(Errno::ISDIR, Some(path)));
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
            return Err(from_errno(Errno::NOTDIR, Some(path)));
        }
        Ok(self
            .descendants(&resolved)
            .filter(|child| child.parent() == Some(resolved.as_path()))
            .filter_map(|child| {
                Some(DirEntry {
                    name: path::from_native(Path::new(child.file_name()?)),
                    kind: self.lookup(child)?.data.kind(),
                })
            })
            .collect())
    }

    pub(super) fn create_dir_all(&mut self, path: &Path) -> HostResult<()> {
        let result = self.resolve(path, Follow::Yes, Missing::Directories);
        if result
            .as_ref()
            .is_err_and(|err| err.errno == Some(Errno::NOENT.raw_os_error()))
        {
            let unresolved = path.components().collect::<PathBuf>();
            if let Ok((_, ino)) = self.existing(&unresolved, Follow::No)
                && matches!(self.inode(ino, path)?.data, Data::Symlink(_))
            {
                return Err(from_errno(Errno::EXIST, Some(path)));
            }
        }
        result.map(|_| ())
    }

    pub(super) fn remove_file(&mut self, path: &Path) -> HostResult<()> {
        let (resolved, ino) = self.existing(path, Follow::No)?;
        if matches!(self.inode(ino, path)?.data, Data::Dir) {
            return Err(from_errno(Errno::ISDIR, Some(path)));
        }
        self.unlink(&resolved);
        Ok(())
    }

    pub(super) fn remove_dir(&mut self, path: &Path) -> HostResult<()> {
        let trimmed = path::directory_removal_path(path)?;
        let (resolved, ino) = self.existing(&trimmed, Follow::No)?;
        if !matches!(self.inode(ino, path)?.data, Data::Dir) {
            return Err(from_errno(Errno::NOTDIR, Some(path)));
        }
        if resolved.parent().is_none() {
            return Err(from_errno(Errno::BUSY, Some(path)));
        }
        if self.descendants(&resolved).next().is_some() {
            return Err(from_errno(Errno::NOTEMPTY, Some(path)));
        }
        self.unlink(&resolved);
        Ok(())
    }

    pub(super) fn remove_dir_all(&mut self, path: &Path) -> HostResult<()> {
        let trimmed = path::directory_removal_path(path)?;
        let (resolved, ino) = self.existing(&trimmed, Follow::No)?;
        if resolved.parent().is_none() {
            return Err(from_errno(Errno::BUSY, Some(path)));
        }
        match self.inode(ino, path)?.data {
            Data::Dir => {
                let children: Vec<_> = self.descendants(&resolved).cloned().collect();
                for child in children {
                    self.unlink(&child);
                }
            }
            Data::Symlink(_) => {}
            Data::File(_) => return Err(from_errno(Errno::NOTDIR, Some(path))),
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
        if src.parent().is_none() || dst.parent().is_none() {
            return Err(from_errno(Errno::BUSY, Some(from)));
        }
        let src_dir = matches!(self.inode(ino, from)?.data, Data::Dir);
        if src_dir && dst.starts_with(&src) {
            return Err(from_errno(Errno::INVAL, Some(to)));
        }
        if let Some(node) = self.lookup(&dst) {
            match (src_dir, &node.data) {
                (true, Data::File(_) | Data::Symlink(_)) => {
                    return Err(from_errno(Errno::NOTDIR, Some(to)));
                }
                (false, Data::Dir) => return Err(from_errno(Errno::ISDIR, Some(to))),
                (true, Data::Dir) if self.descendants(&dst).next().is_some() => {
                    return Err(from_errno(Errno::NOTEMPTY, Some(to)));
                }
                _ => {}
            }
        }
        self.unlink(&dst);
        let children: Vec<_> = self.descendants(&src).cloned().collect();
        for child in children {
            if let Ok(relative) = child.strip_prefix(&src)
                && let Some(ino) = self.paths.remove(&child)
            {
                self.paths.insert(dst.join(relative), ino);
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
            return Err(from_errno(Errno::INVAL, Some(to)));
        }
        let inode = self.inode(src, from)?;
        let Data::File(bytes) = &inode.data else {
            return Err(from_errno(Errno::INVAL, Some(from)));
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
            return Err(from_errno(Errno::EXIST, Some(link)));
        }
        if target.as_os_str().is_empty() {
            return Err(from_errno(Errno::NOENT, Some(link)));
        }
        if path::from_native(target).as_slice().contains(&0) {
            return Err(from_errno(Errno::INVAL, Some(link)));
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
            return Err(from_errno(Errno::EXIST, Some(dst)));
        }
        let inode = self.inode_mut(ino, src)?;
        if matches!(inode.data, Data::Dir) {
            return Err(from_errno(Errno::PERM, Some(src)));
        }
        inode.links += 1;
        self.paths.insert(resolved, ino);
        Ok(())
    }

    pub(super) fn read_link(&mut self, path: &Path) -> HostResult<PathBuf> {
        let (_, ino) = self.existing(path, Follow::No)?;
        match &self.inode(ino, path)?.data {
            Data::Symlink(target) => Ok(target.clone()),
            _ => Err(from_errno(Errno::INVAL, Some(path))),
        }
    }

    pub(super) fn canonicalize(&mut self, path: &Path) -> HostResult<PathBuf> {
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
            return Err(from_errno(Errno::INVAL, Some(path)));
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
