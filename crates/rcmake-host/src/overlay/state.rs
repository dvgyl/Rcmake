//! Overlay entries, path resolution, and mutation accounting.

use std::collections::BTreeMap;
use std::io::ErrorKind;
use std::ops::Bound::{Excluded, Unbounded};
use std::sync::{Mutex, Weak};

use super::paths::{Key, Part, child_path, display_name, validate_length};
use super::*;

const MAX_SYMLINK_HOPS: usize = 40;
const MAX_REMOVAL_ENTRIES: usize = 1_000_000;
const MAX_PENDING_COMPONENTS: usize = 512;
const MAX_ENTRIES: usize = 65_536;
const MAX_BASE_NAMES: usize = 32_768;
const MAX_BASE_NAME_BYTES: usize = 4 * 1024 * 1024;

#[derive(Default)]
struct BaseNames {
    directories: BTreeMap<PathBuf, BTreeMap<Vec<u8>, BString>>,
    names: usize,
    bytes: usize,
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(super) enum Inode {
    Base(u64, u64),
    Local(u64),
}

#[derive(Clone)]
pub(super) struct Node {
    pub(super) stat: Metadata,
    pub(super) backing: Option<Arc<Path>>,
    target: Option<Arc<Path>>,
}

impl Node {
    pub(super) fn inode(&self) -> Inode {
        if self.backing.is_some() {
            Inode::Base(self.stat.dev, self.stat.ino)
        } else {
            Inode::Local(self.stat.ino)
        }
    }

    pub(super) fn target(&self, base: &(impl Host + ?Sized), p: &Path) -> HostResult<PathBuf> {
        match &self.target {
            Some(target) => Ok(target.to_path_buf()),
            None => base.read_link(self.backing(p)?),
        }
    }

    pub(super) fn backing(&self, p: &Path) -> HostResult<&Path> {
        self.backing
            .as_deref()
            .ok_or_else(|| SyntheticError::NotFound.at(Some(p)))
    }

    pub(super) fn set_target(&mut self, target: &Path) {
        self.stat.len = path_len(target) as u64;
        self.target = Some(Arc::from(target));
    }

    pub(super) fn require_file(&self, p: &Path) -> HostResult<()> {
        match self.stat.kind {
            FileKind::File => Ok(()),
            FileKind::Dir => Err(SyntheticError::IsADirectory.at(Some(p))),
            _ => Err(SyntheticError::InvalidInput.at(Some(p))),
        }
    }
}

// The weak reference to this wrapper does not retain the slice allocation.
pub(super) struct Contents(pub(super) Arc<[u8]>);

#[derive(Clone, Default)]
pub(super) struct Override {
    pub(super) bytes: Option<Arc<Contents>>,
    pub(super) mode: Option<u32>,
    pub(super) atime: Option<FileTime>,
    pub(super) mtime: Option<FileTime>,
    links: i64,
}

impl Override {
    fn apply(&self, stat: &mut Metadata) {
        if let Some(bytes) = &self.bytes {
            stat.len = bytes.0.len() as u64;
        }
        stat.mode = self.mode.unwrap_or(stat.mode);
        stat.atime = self.atime.unwrap_or(stat.atime);
        stat.mtime = self.mtime.unwrap_or(stat.mtime);
        stat.nlink = stat.nlink.saturating_add_signed(self.links);
    }
}

#[derive(Clone)]
pub(super) struct Entry {
    pub(super) path: PathBuf,
    pub(super) node: Option<Node>,
}

pub(super) struct Resolved {
    pub(super) key: Key,
    pub(super) entry: Entry,
    parent: Option<Node>,
    windows: bool,
}

type NewDirectories = BTreeMap<Key, (Entry, Option<Node>)>;

impl Resolved {
    pub(super) fn path_with_name(&self, name: &[u8]) -> PathBuf {
        let bytes = path::from_native(&self.entry.path);
        let end = bytes
            .iter()
            .rposition(|&b| b == b'/')
            .map_or(0, |pos| pos + 1);
        path::to_native(&[bytes.get(..end).unwrap_or_default(), name].concat())
    }

    pub(super) fn existing(&self, p: &Path) -> HostResult<&Node> {
        self.entry
            .node
            .as_ref()
            .ok_or_else(|| SyntheticError::NotFound.at(Some(p)))
    }
}

#[derive(Default)]
pub(super) struct Edit {
    pub(super) paths: BTreeMap<Key, Option<Entry>>,
    overrides: BTreeMap<Inode, Option<Override>>,
}

impl Edit {
    pub(super) fn store(&mut self, dest: Resolved, node: Node) {
        self.paths.insert(
            dest.key,
            Some(Entry {
                path: dest.entry.path,
                node: Some(node),
            }),
        );
    }

    pub(super) fn change<'a>(
        &'a mut self,
        state: &State<impl Host + ?Sized>,
        node: &Node,
    ) -> &'a mut Override {
        let key = node.inode();
        self.overrides
            .entry(key.clone())
            .or_insert_with(|| state.overrides.get(&key).cloned())
            .get_or_insert_with(Override::default)
    }

    pub(super) fn touch_parent(
        &mut self,
        state: &State<impl Host + ?Sized>,
        entry: &Resolved,
        now: FileTime,
    ) {
        if let Some(node) = &entry.parent {
            self.change(state, node).mtime = Some(now);
        }
    }

    pub(super) fn unlink(&mut self, state: &State<impl Host + ?Sized>, node: &Node) {
        let inode = node.inode();
        let change = self
            .overrides
            .get(&inode)
            .map_or_else(|| state.overrides.get(&inode), Option::as_ref);
        if node.stat.kind != FileKind::Dir
            && node
                .stat
                .nlink
                .saturating_add_signed(change.map_or(0, |change| change.links))
                > 1
        {
            self.change(state, node).links -= 1;
        } else if change.is_some() {
            self.overrides.insert(inode, None);
        }
    }

    pub(super) fn link(&mut self, state: &State<impl Host + ?Sized>, node: &Node) {
        self.change(state, node).links += 1;
    }
}

#[derive(Clone, Copy, Default)]
struct Usage {
    entries: usize,
    bytes: usize,
}

impl Usage {
    fn add(&mut self, bytes: usize) {
        self.entries += 1;
        self.bytes = self.bytes.saturating_add(bytes);
    }

    fn remove(&mut self, bytes: usize) {
        self.entries -= 1;
        self.bytes -= bytes;
    }
}

fn path_len(p: &Path) -> usize {
    p.as_os_str().as_encoded_bytes().len()
}

fn map_slot<K, V>() -> usize {
    // Allow for unused B-tree slots and the links between nodes.
    2 * (size_of::<(K, V)>() + size_of::<usize>())
}

pub(super) const CONTENTS_OVERHEAD: usize = 2 * size_of::<usize>() + size_of::<Contents>();

fn contents_cost(bytes: &Contents) -> usize {
    CONTENTS_OVERHEAD + bytes.0.len()
}

fn entry_cost(key: &Key, entry: &Entry) -> usize {
    map_slot::<Key, Entry>()
        + key.0.len()
        + path_len(&entry.path)
        + entry.node.as_ref().map_or(0, |node| {
            node.backing.as_deref().map_or(0, path_len) + node.target.as_deref().map_or(0, path_len)
        })
}

fn override_cost(change: &Override) -> usize {
    map_slot::<Inode, Override>() + change.bytes.as_deref().map_or(0, contents_cost)
}

fn env_cost(key: &[u8], value_len: usize) -> usize {
    map_slot::<BString, Option<BString>>()
        .saturating_add(key.len())
        .saturating_add(value_len)
}

pub(super) struct State<H: Host + ?Sized> {
    base: Arc<H>,
    base_names: Mutex<BaseNames>,
    pub(super) paths: BTreeMap<Key, Entry>,
    pub(super) overrides: BTreeMap<Inode, Override>,
    pub(super) env: BTreeMap<BString, Option<BString>>,
    pub(super) policy: OverlayPathPolicy,
    usage: Usage,
    limit: usize,
    retired: Vec<Weak<Contents>>,
    next_ino: u64,
    last_time: Option<FileTime>,
}

impl<H: Host + ?Sized> State<H> {
    pub(super) fn new(base: Arc<H>, policy: OverlayPathPolicy, limit: usize) -> Self {
        Self {
            base,
            base_names: Mutex::default(),
            paths: BTreeMap::new(),
            overrides: BTreeMap::new(),
            env: BTreeMap::new(),
            policy,
            usage: Usage::default(),
            limit,
            retired: Vec::new(),
            next_ino: 0,
            last_time: None,
        }
    }

    pub(super) fn new_node(&mut self, kind: FileKind, now: FileTime) -> Node {
        self.next_ino += 1;
        Node {
            stat: Metadata {
                kind,
                mode: match kind {
                    FileKind::Dir => 0o755,
                    FileKind::Symlink => 0o777,
                    _ => 0o644,
                },
                len: 0,
                atime: now,
                mtime: now,
                dev: u64::MAX,
                ino: self.next_ino,
                nlink: if kind == FileKind::Dir { 2 } else { 1 },
            },
            backing: None,
            target: None,
        }
    }

    pub(super) fn tick(&mut self) -> FileTime {
        let now = self.base.now();
        let last = self.last_time.map_or(now, |last| last.max(now));
        let time = if last.nanos < 999_999_999 {
            FileTime {
                secs: last.secs,
                nanos: last.nanos + 1,
            }
        } else if last.secs < i64::MAX {
            FileTime {
                secs: last.secs + 1,
                nanos: 0,
            }
        } else {
            last
        };
        self.last_time = Some(time);
        time
    }

    fn base_node(&self, backing: PathBuf) -> HostResult<Option<Node>> {
        match self.base.metadata(&backing, Follow::No) {
            Ok(stat) => Ok(Some(Node {
                stat,
                backing: Some(Arc::from(backing)),
                target: None,
            })),
            Err(err) if err.kind == ErrorKind::NotFound => Ok(None),
            Err(err) => Err(err),
        }
    }

    fn base_name(&self, dir: &Path, name: &[u8], key: &[u8]) -> HostResult<BString> {
        if self.policy.case_sensitive {
            return Ok(name.into());
        }
        if let Some(entries) = self.base_names.lock().ok().and_then(|cache| {
            cache
                .directories
                .get(dir)
                .map(|entries| entries.get(key).cloned())
        }) {
            return Ok(entries.unwrap_or_else(|| name.into()));
        }

        let entries = match self.base.read_dir(dir) {
            Ok(entries) => entries,
            Err(err) if err.kind == ErrorKind::PermissionDenied => return Ok(name.into()),
            Err(err) => return Err(err),
        };
        let result = entries
            .iter()
            .find(|entry| self.policy.name_key(&entry.name) == key)
            .map_or_else(|| name.into(), |entry| entry.name.clone());
        if let Ok(mut cache) = self.base_names.lock()
            && !cache.directories.contains_key(dir)
            && cache.names.saturating_add(entries.len().max(1)) <= MAX_BASE_NAMES
        {
            let mut names = BTreeMap::new();
            let mut bytes = path_len(dir);
            for entry in entries {
                let key = self.policy.name_key(&entry.name);
                bytes = bytes
                    .saturating_add(key.len())
                    .saturating_add(entry.name.len());
                if cache.bytes.saturating_add(bytes) > MAX_BASE_NAME_BYTES {
                    return Ok(result);
                }
                names.entry(key).or_insert(entry.name);
            }
            // Only spelling is cached. Metadata and file contents still come from the base.
            if cache.bytes.saturating_add(bytes) <= MAX_BASE_NAME_BYTES {
                cache.bytes += bytes;
                cache.names += names.len().max(1);
                cache.directories.insert(dir.to_owned(), names);
            }
        }
        Ok(result)
    }

    fn root(&self, root: &[u8], windows: bool) -> HostResult<Resolved> {
        let mut path = path::to_native(root);
        let mut node = self.base_node(path.clone())?;
        if node.is_none() && !self.policy.case_sensitive && root.starts_with(b"//") {
            path = path::to_native(&self.policy.name_key(root));
            node = self.base_node(path.clone())?;
        }
        match node.as_ref().map(|node| node.stat.kind) {
            Some(FileKind::Dir) => Ok(Resolved {
                key: Key::root(self.policy, root),
                entry: Entry { path, node },
                parent: None,
                windows,
            }),
            None => Err(SyntheticError::NotFound.at(Some(&path))),
            _ => Err(SyntheticError::NotADirectory.at(Some(&path))),
        }
    }

    fn child(&self, dir: &Resolved, name: &[u8], listed: bool) -> HostResult<Resolved> {
        let folded = self.policy.name_key(name);
        let key = dir.key.child(&folded);
        let parent = dir.entry.node.clone();
        let virtual_path = child_path(&dir.entry.path, name);
        validate_length(&virtual_path)?;
        let entry = if let Some(entry) = self.paths.get(&key) {
            if entry.node.is_some() {
                entry.clone()
            } else {
                Entry {
                    path: virtual_path,
                    node: None,
                }
            }
        } else if let Some(backing) = parent.as_ref().and_then(|node| node.backing.as_deref()) {
            let name = if listed {
                name.into()
            } else {
                self.base_name(backing, name, &folded)?
            };
            let path = child_path(&dir.entry.path, &name);
            validate_length(&path)?;
            let backing = child_path(backing, &name);
            validate_length(&backing)?;
            Entry {
                path,
                node: self.base_node(backing)?,
            }
        } else {
            Entry {
                path: virtual_path,
                node: None,
            }
        };
        Ok(Resolved {
            key,
            entry,
            parent,
            windows: dir.windows,
        })
    }

    pub(super) fn resolve(
        &self,
        p: &Path,
        follow: Follow,
        missing_leaf: bool,
        mut new_dirs: Option<&mut NewDirectories>,
        rename_endpoint: bool,
    ) -> HostResult<Resolved> {
        let (mut parents, mut pending) = self.policy.with_absolute(p, |mut parsed| {
            let root = parsed
                .root
                .take()
                .ok_or_else(|| HostError::not_absolute(p.to_path_buf()))?;
            let mut parts = parsed.parts(new_dirs.is_some(), p)?;
            if rename_endpoint && matches!(parts.back(), Some(Part::Directory)) {
                parts.pop_back();
            }
            Ok((vec![self.root(&root, parsed.windows)?], parts))
        })?;
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
            let mut child = self.child(dir, &name, false)?;
            match child.entry.node.as_ref().map(|node| node.stat.kind) {
                None => {
                    if let Some(dirs) = new_dirs.as_deref_mut()
                        && (may_create || dirs.contains_key(&child.key))
                    {
                        let (entry, _) = dirs
                            .entry(child.key.clone())
                            .or_insert_with(|| (child.entry.clone(), child.parent.clone()));
                        child.entry = entry.clone();
                        parents.push(child);
                    } else if missing_leaf && pending.is_empty() {
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
                    let target = child.existing(p)?.target(self.base.as_ref(), p)?;
                    validate_length(&target)?;
                    let target = path::from_native(&target);
                    let mut parsed = self.policy.parse(&target, dir.windows);
                    if let Some(root) = parsed.root.take() {
                        parents = vec![self.root(&root, parsed.windows)?];
                    } else if parsed.windows
                        && target.first().is_some_and(|b| matches!(*b, b'/' | b'\\'))
                    {
                        parents.truncate(1);
                    } else if parsed.windows
                        && matches!(target.as_slice(), [letter, b':', ..] if letter.is_ascii_alphabetic())
                    {
                        return Err(SyntheticError::InvalidInput.at(Some(p)));
                    }
                    let mut parts = parsed.parts(false, p)?;
                    if parts.len() + pending.len() > MAX_PENDING_COMPONENTS {
                        return Err(budget_error(p));
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

    pub(super) fn existing(&self, p: &Path, follow: Follow) -> HostResult<Resolved> {
        self.resolve(p, follow, false, None, false)
    }

    pub(super) fn destination(&self, p: &Path, follow: Follow) -> HostResult<Resolved> {
        self.resolve(p, follow, true, None, false)
    }

    pub(super) fn descendants<'a>(
        &'a self,
        dir: &'a Key,
    ) -> impl Iterator<Item = (&'a Key, &'a Entry)> {
        self.paths
            .range((Excluded(dir.clone()), Unbounded))
            .take_while(move |(key, _)| key.is_under(dir))
    }

    pub(super) fn children(&self, dir: &Resolved, node: &Node) -> HostResult<Vec<DirEntry>> {
        let mut entries: BTreeMap<_, _> = match node.backing.as_deref() {
            Some(backing) => self
                .base
                .read_dir(backing)?
                .into_iter()
                .map(|entry| (self.policy.name_key(&entry.name), entry))
                .collect(),
            None => BTreeMap::new(),
        };
        for (key, entry) in self
            .descendants(&dir.key)
            .filter(|(key, _)| key.is_child_of(&dir.key))
        {
            let name = display_name(&entry.path);
            let folded = key.0.get(dir.key.0.len()..).unwrap_or_default();
            let folded = folded.strip_suffix(b"\0").unwrap_or_default();
            if let Some(node) = &entry.node {
                entries.insert(
                    folded.to_vec(),
                    DirEntry {
                        name,
                        kind: node.stat.kind,
                    },
                );
            } else {
                entries.remove(folded);
            }
        }
        let mut entries: Vec<_> = entries.into_values().collect();
        entries.sort_unstable_by(|a, b| a.name.cmp(&b.name));
        Ok(entries)
    }

    pub(super) fn stat(&self, node: &Node) -> Metadata {
        let mut stat = node.stat.clone();
        if let Some(change) = self.overrides.get(&node.inode()) {
            change.apply(&mut stat);
        }
        stat
    }

    pub(super) fn bytes(&self, node: &Node) -> Option<&Arc<Contents>> {
        self.overrides.get(&node.inode())?.bytes.as_ref()
    }

    pub(super) fn tombstone(&self, dest: &Resolved) -> HostResult<Option<Entry>> {
        let Some(parent) = dest
            .parent
            .as_ref()
            .and_then(|node| node.backing.as_deref())
        else {
            return Ok(None);
        };
        let name = display_name(&dest.entry.path);
        let base_name = self.base_name(parent, &name, &self.policy.name_key(&name))?;
        if self.base_node(child_path(parent, &base_name))?.is_none() {
            return Ok(None);
        }
        Ok(Some(Entry {
            path: dest.entry.path.clone(),
            node: None,
        }))
    }

    pub(super) fn remove_tree(&self, edit: &mut Edit, root: &Resolved, p: &Path) -> HostResult<()> {
        let mut pending = vec![Resolved {
            key: root.key.clone(),
            entry: root.entry.clone(),
            parent: None,
            windows: root.windows,
        }];
        let mut pending_bytes = entry_cost(&root.key, &root.entry);
        let mut visited = 0;
        while let Some(dir) = pending.pop() {
            pending_bytes -= entry_cost(&dir.key, &dir.entry);
            let node = dir.existing(p)?;
            edit.unlink(self, node);
            if node.stat.kind == FileKind::Dir {
                for child in self.children(&dir, node)? {
                    if child.name.is_empty()
                        || matches!(child.name.as_slice(), b"." | b"..")
                        || child.name.contains(&b'/')
                        || child.name.contains(&0)
                        || (dir.windows && child.name.contains(&b'\\'))
                    {
                        return Err(SyntheticError::InvalidInput.at(Some(p)));
                    }
                    let child = self.child(&dir, &child.name, true)?;
                    pending_bytes =
                        pending_bytes.saturating_add(entry_cost(&child.key, &child.entry));
                    if pending_bytes > self.limit || visited + pending.len() >= MAX_REMOVAL_ENTRIES
                    {
                        return Err(budget_error(p));
                    }
                    pending.push(child);
                }
            }
            visited += 1;
            if edit.overrides.len() > MAX_ENTRIES {
                return Err(budget_error(p));
            }
        }
        Ok(())
    }

    fn projection(&self, edit: &Edit) -> Usage {
        let mut usage = self.usage;
        for (key, entry) in &edit.paths {
            if let Some(old) = self.paths.get(key) {
                usage.remove(entry_cost(key, old));
            }
            if let Some(entry) = entry {
                usage.add(entry_cost(key, entry));
            }
        }
        for (inode, change) in &edit.overrides {
            if let Some(old) = self.overrides.get(inode) {
                usage.remove(override_cost(old));
            }
            if let Some(change) = change {
                usage.add(override_cost(change));
            }
        }
        usage
    }

    fn replaced_bytes<'a>(&'a self, edit: &'a Edit) -> impl Iterator<Item = &'a Arc<Contents>> {
        edit.overrides.iter().filter_map(|(inode, new)| {
            let old = self.overrides.get(inode)?.bytes.as_ref()?;
            let unchanged = new
                .as_ref()
                .and_then(|change| change.bytes.as_ref())
                .is_some_and(|new| Arc::ptr_eq(old, new));
            (!unchanged && Arc::strong_count(old) > 1 && !old.0.is_empty()).then_some(old)
        })
    }

    fn with_retired(&mut self, mut usage: Usage) -> Usage {
        self.retired.retain(|weak| {
            if let Some(bytes) = weak.upgrade() {
                usage.add(contents_cost(bytes.as_ref()) + size_of::<Weak<Contents>>());
                true
            } else {
                false
            }
        });
        usage
    }

    pub(super) fn check(&mut self, edit: &Edit, extra_bytes: usize, p: &Path) -> HostResult<()> {
        let projected = self.projection(edit);
        let mut usage = self.with_retired(projected);
        for bytes in self.replaced_bytes(edit) {
            usage.add(contents_cost(bytes.as_ref()) + size_of::<Weak<Contents>>());
        }
        if usage.entries > MAX_ENTRIES || usage.bytes.saturating_add(extra_bytes) > self.limit {
            return Err(budget_error(p));
        }
        Ok(())
    }

    pub(super) fn commit(&mut self, edit: Edit) {
        self.usage = self.projection(&edit);
        let retired: Vec<_> = self.replaced_bytes(&edit).map(Arc::downgrade).collect();
        self.retired.extend(retired);
        for (key, entry) in edit.paths {
            match entry {
                Some(entry) => {
                    self.paths.insert(key, entry);
                }
                None => {
                    self.paths.remove(&key);
                }
            }
        }
        for (inode, change) in edit.overrides {
            match change {
                Some(change) => {
                    self.overrides.insert(inode, change);
                }
                None => {
                    self.overrides.remove(&inode);
                }
            }
        }
    }

    pub(super) fn set_env(&mut self, key: BString, value: Option<&[u8]>) {
        let mut usage = self.usage;
        if let Some(old) = self.env.get(&key) {
            usage.remove(env_cost(&key, old.as_ref().map_or(0, |v| v.len())));
        }
        usage.add(env_cost(&key, value.map_or(0, <[u8]>::len)));
        let total = self.with_retired(usage);
        if total.entries <= MAX_ENTRIES && total.bytes <= self.limit {
            self.env.insert(key, value.map(BString::from));
            self.usage = usage;
        }
    }

    pub(super) fn limit(&self) -> usize {
        self.limit
    }
}
