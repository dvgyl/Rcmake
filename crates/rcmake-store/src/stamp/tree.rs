// SPDX-License-Identifier: MIT OR Apache-2.0

use std::collections::BTreeSet;

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
    let mut key = KeyHasher::new(Domain::BlobV1);
    key.field(0, b"sdk-tree-root");
    key.field(1, graph.as_bytes());
    if host.metadata(root, Follow::No)?.kind == FileKind::Symlink {
        key.field(2, host.read_link(root)?.as_os_str().as_encoded_bytes());
    }
    Ok(key.finish())
}

pub(super) fn sdk_graph_digest(host: &dyn Host, canonical: &Path) -> HostResult<Digest> {
    sdk_graph(host, canonical, |_, _| {})
}

#[derive(Clone, Debug)]
pub(super) struct SdkLink {
    pub(super) path: PathBuf,
    pub(super) resolved: Option<(PathBuf, FileKind)>,
}

impl SdkLink {
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

pub(super) fn sdk_graph(
    host: &dyn Host,
    canonical: &Path,
    mut observe_link: impl FnMut(PathBuf, Option<(PathBuf, FileKind)>),
) -> HostResult<Digest> {
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
        let mut entries = host.read_dir(&directory)?;
        remaining = remaining
            .checked_sub(entries.len())
            .filter(|_| depth <= 32)
            .ok_or_else(|| HostError::unsupported("SDK search graph exceeds the listing limit."))?;
        entries.sort_unstable_by(|left, right| left.name.cmp(&right.name));
        key.field(3, directory.as_os_str().as_encoded_bytes());
        for entry in entries {
            let name = rcmake_host::path::to_native(&entry.name);
            if !matches!(name.components().next(), Some(Component::Normal(_)))
                || name.components().count() != 1
            {
                return Err(HostError::unsupported(
                    "Invalid SDK search directory entry.",
                ));
            }
            let child = directory.join(name);
            match entry.kind {
                FileKind::File => key.field(4, &entry.name),
                FileKind::Dir => {
                    key.field(5, &entry.name);
                    directories.push((child, depth + 1));
                }
                FileKind::Symlink => {
                    key.field(6, &entry.name);
                    let target = host.read_link(&child)?;
                    key.field(7, target.as_os_str().as_encoded_bytes());
                    let kind = match host.metadata(&child, Follow::Yes) {
                        Ok(stat) => stat.kind,
                        Err(error) if error.kind == ErrorKind::NotFound => {
                            key.field(8, b"missing");
                            observe_link(child, None);
                            continue;
                        }
                        Err(error) => return Err(error),
                    };
                    let resolved = host.canonicalize(&child)?;
                    if !resolved.starts_with(canonical)
                        || !link_stays_in_tree(canonical, &directory, &target)
                    {
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
                FileKind::Other => {
                    return Err(HostError::unsupported("Unsupported SDK search entry."));
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
