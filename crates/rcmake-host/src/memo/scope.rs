//! Object identities for path-scoped cache invalidation.
//!
//! A scope records each name lookup that a path resolution makes, as the device
//! and inode of the directory and the name that it reads. The scope also records
//! each symbolic link that the resolution follows and the final object.
//!
//! A host-mediated change binds or unbinds one name in one directory, changes
//! the listing of that directory, and can change the named object in place. A
//! cached query stays valid when its scope is independent of all three effects.
//!
//! Identities, and not spellings, make the comparison. Thus symbolic-link
//! aliases (`/tmp` and `/private/tmp`), case aliases, bind mounts, and hard
//! links all compare equal when they reach the same object. Names compare
//! without ASCII case, and a name with other bytes matches every name.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::io::ErrorKind;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use crate::{FileKind, Follow, Host, Metadata};

/// The maximum number of directory chains that the table keeps.
const MAX_DIRS: usize = 4096;
/// The maximum number of nested resolutions, which also stops link loops.
const MAX_DEPTH: u8 = 40;

/// The device and inode of a filesystem object.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Identity {
    dev: u64,
    ino: u64,
}

impl Identity {
    fn of(metadata: &Metadata) -> Option<Self> {
        (metadata.ino != 0).then_some(Self {
            dev: metadata.dev,
            ino: metadata.ino,
        })
    }
}

/// One name lookup in one directory.
#[derive(Clone, Debug)]
struct Lookup {
    dir: Identity,
    name: Box<[u8]>,
}

impl Lookup {
    /// Return `true` if a change to `other` can change this lookup.
    fn overlaps(&self, other: &Self) -> bool {
        self.dir == other.dir
            && (!self.name.is_ascii()
                || !other.name.is_ascii()
                || self.name.eq_ignore_ascii_case(&other.name))
    }
}

/// The resolution of one directory.
struct DirInfo {
    /// Every lookup that the resolution makes.
    lookups: Vec<Lookup>,
    /// Every link that the resolution follows.
    links: Vec<Identity>,
    /// The directory.
    id: Identity,
}

impl DirInfo {
    fn depends_on(&self, affected: &Affected) -> bool {
        self.lookups.iter().any(|lookup| {
            affected.changed.contains(&lookup.dir)
                || affected.names.iter().any(|name| name.overlaps(lookup))
        }) || self
            .links
            .iter()
            .chain([&self.id])
            .any(|identity| affected.changed.contains(identity))
    }
}

/// The objects that the resolution of one path depends on.
#[derive(Default)]
pub(super) struct Scope {
    /// The resolutions of the directories that hold looked-up names.
    dirs: Vec<Arc<DirInfo>>,
    /// The final lookups, one for the path and one for each followed link.
    lookups: Vec<Lookup>,
    /// Symbolic links that the resolution follows.
    links: Vec<Identity>,
    /// The final object, if it exists.
    object: Option<(Identity, FileKind)>,
}

impl Scope {
    /// Return `true` if a change in `affected` can change this resolution.
    fn depends_on(&self, affected: &Affected) -> bool {
        self.dirs.iter().any(|dir| dir.depends_on(affected))
            || self.lookups.iter().any(|lookup| {
                affected.changed.contains(&lookup.dir)
                    || affected.names.iter().any(|name| name.overlaps(lookup))
            })
            || self
                .links
                .iter()
                .any(|identity| affected.changed.contains(identity))
            || self.object.is_some_and(|(identity, _)| {
                affected.changed.contains(&identity) || affected.listings.contains(&identity)
            })
    }

    /// Return the effects of a change to the named object.
    fn affected(&self) -> Affected {
        Affected {
            names: self.lookups.clone(),
            listings: self.lookups.iter().map(|lookup| lookup.dir).collect(),
            changed: self
                .links
                .iter()
                .copied()
                .chain(self.object.map(|(identity, _)| identity))
                .collect(),
            // Every directory above the changed names, under every spelling:
            // the holding directories, their full resolutions, and the links.
            within: self
                .dirs
                .iter()
                .flat_map(|dir| {
                    dir.lookups
                        .iter()
                        .map(|lookup| lookup.dir)
                        .chain(dir.links.iter().copied())
                        .chain([dir.id])
                })
                .chain(self.lookups.iter().map(|lookup| lookup.dir))
                .chain(self.object.map(|(identity, _)| identity))
                .collect(),
        }
    }
}

/// The possible effects of one change.
#[derive(Default)]
pub(super) struct Affected {
    /// Names that the change can bind, unbind, or bind again.
    names: Vec<Lookup>,
    /// Directories whose entries and times the change can change.
    listings: Vec<Identity>,
    /// Objects that the change can change in place.
    changed: Vec<Identity>,
    /// Every directory that holds a changed name, at any depth.
    within: Vec<Identity>,
}

impl Affected {
    /// Combine the effects of several changed paths.
    pub(super) fn merge(parts: Vec<Self>) -> Self {
        parts.into_iter().fold(Self::default(), |mut all, part| {
            all.names.extend(part.names);
            all.listings.extend(part.listings);
            all.changed.extend(part.changed);
            all.within.extend(part.within);
            all
        })
    }
}

/// The lazy state of the scope of one cached record.
#[derive(Default)]
pub(super) enum ScopeState {
    /// The scope is not calculated yet.
    #[default]
    Pending,
    /// The scope is known.
    Known(Scope),
    /// The scope is not available. Every change removes the record.
    Unknown,
}

impl ScopeState {
    /// Calculate a pending scope, then return `true` if the record stays valid.
    ///
    /// With `tree`, the record also depends on every name below the path.
    pub(super) fn survives<H: Host + ?Sized>(
        &mut self,
        dirs: &mut Dirs,
        base: &H,
        path: &Path,
        affected: &Affected,
        tree: bool,
    ) -> bool {
        if matches!(self, Self::Pending) {
            *self = dirs
                .resolve(base, path, 0)
                .map_or(Self::Unknown, Self::Known);
        }
        match self {
            Self::Known(scope) => {
                !scope.depends_on(affected)
                    && !(tree
                        && scope
                            .object
                            .is_some_and(|(identity, _)| affected.within.contains(&identity)))
            }
            Self::Pending | Self::Unknown => false,
        }
    }
}

/// Directory resolutions, keyed by exact spelling.
#[derive(Default)]
pub(super) struct Dirs {
    chains: BTreeMap<OsString, Arc<DirInfo>>,
}

impl Dirs {
    /// Remove every resolution.
    pub(super) fn clear(&mut self) {
        self.chains.clear();
    }

    /// Add the resolutions of `other` that this table does not have.
    pub(super) fn merge(&mut self, other: Self) {
        for (path, dir) in other.chains {
            if self.chains.len() >= MAX_DIRS {
                break;
            }
            self.chains.entry(path).or_insert(dir);
        }
    }

    /// Remove the resolutions that a change in `affected` can change.
    pub(super) fn retain_unaffected(&mut self, affected: &Affected) {
        self.chains.retain(|_, dir| !dir.depends_on(affected));
    }

    /// Return the effects of a change to `path`, before the change.
    ///
    /// Return `None` if the effects are not certain.
    pub(super) fn affected<H: Host + ?Sized>(&mut self, base: &H, path: &Path) -> Option<Affected> {
        let mut current = path;
        loop {
            if let Some(scope) = self.resolve(base, current, 0) {
                return Some(scope.affected());
            }
            // A change can create missing parent directories. Then the highest
            // missing ancestor is the first new name.
            match base.metadata(current, Follow::No) {
                Err(error) if error.kind == ErrorKind::NotFound => current = current.parent()?,
                _ => return None,
            }
        }
    }

    /// Resolve `path` and return the objects that its resolution depends on.
    pub(super) fn resolve<H: Host + ?Sized>(
        &mut self,
        base: &H,
        path: &Path,
        depth: u8,
    ) -> Option<Scope> {
        if depth > MAX_DEPTH || !plain(path) {
            return None;
        }
        let Some(parent) = path.parent() else {
            let metadata = base.metadata(path, Follow::Yes).ok()?;
            return Some(Scope {
                object: Some((Identity::of(&metadata)?, metadata.kind)),
                ..Scope::default()
            });
        };
        let dir = self.dir(base, parent, depth + 1)?;
        let lookup = Lookup {
            dir: dir.id,
            name: path.file_name()?.as_encoded_bytes().into(),
        };
        match base.metadata(path, Follow::No) {
            Err(error) if error.kind == ErrorKind::NotFound => Some(Scope {
                dirs: vec![dir],
                lookups: vec![lookup],
                ..Scope::default()
            }),
            Err(_) => None,
            Ok(link) if link.kind == FileKind::Symlink => {
                let target = base.read_link(path).ok()?;
                let target = link_target(parent, &target)?;
                let mut scope = self.resolve(base, &target, depth + 1)?;
                scope.dirs.push(dir);
                scope.lookups.push(lookup);
                scope.links.push(Identity::of(&link)?);
                Some(scope)
            }
            Ok(metadata) => Some(Scope {
                dirs: vec![dir],
                lookups: vec![lookup],
                links: Vec::new(),
                object: Some((Identity::of(&metadata)?, metadata.kind)),
            }),
        }
    }

    /// Return the resolution of a directory.
    fn dir<H: Host + ?Sized>(&mut self, base: &H, path: &Path, depth: u8) -> Option<Arc<DirInfo>> {
        if let Some(dir) = self.chains.get(path.as_os_str()) {
            return Some(Arc::clone(dir));
        }
        if self.chains.len() >= MAX_DIRS {
            return None;
        }
        let scope = self.resolve(base, path, depth)?;
        let (id, FileKind::Dir) = scope.object? else {
            return None;
        };
        let mut lookups = scope.lookups;
        let mut links = scope.links;
        for dir in &scope.dirs {
            lookups.extend(dir.lookups.iter().cloned());
            links.extend(&dir.links);
            links.push(dir.id);
        }
        let dir = Arc::new(DirInfo { lookups, links, id });
        self.chains
            .insert(path.as_os_str().to_owned(), Arc::clone(&dir));
        Some(dir)
    }
}

/// Return `true` for an absolute path in normal form.
///
/// A dot component, a repeated separator, or a trailing separator gives `false`.
fn plain(path: &Path) -> bool {
    path.is_absolute()
        && path.as_os_str().len() <= 4096
        && path.components().all(|component| {
            matches!(
                component,
                Component::Prefix(_) | Component::RootDir | Component::Normal(_)
            )
        })
        && path.components().collect::<PathBuf>().as_os_str() == path.as_os_str()
}

/// Return the path of a link target. Reject a target with dot components.
fn link_target(parent: &Path, target: &Path) -> Option<PathBuf> {
    let path = if target.is_absolute() {
        target.to_path_buf()
    } else {
        if target
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
        {
            return None;
        }
        parent.join(target)
    };
    plain(&path).then_some(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_only_normal_absolute_spellings() {
        // Use an absolute root for the platform.
        let root = if cfg!(windows) { "C:\\work" } else { "/work" };
        let sep = std::path::MAIN_SEPARATOR;
        let path = |rest: &str| PathBuf::from(format!("{root}{rest}"));
        for spelling in [path(""), path(&format!("{sep}file"))] {
            assert!(plain(&spelling), "{}", spelling.display());
        }
        for spelling in [
            PathBuf::from("work"),
            path(&format!("{sep}")),
            path(&format!("{sep}.{sep}file")),
            path(&format!("{sep}{sep}file")),
            path(&format!("{sep}..{sep}file")),
        ] {
            assert!(!plain(&spelling), "{}", spelling.display());
        }
        let work = path("");
        assert_eq!(
            link_target(&work, &Path::new("dir").join("file")),
            Some(work.join("dir").join("file"))
        );
        assert_eq!(link_target(&work, &Path::new("..").join("file")), None);
        assert_eq!(link_target(&work, &Path::new(".").join("file")), None);
        assert_eq!(
            link_target(&work, &path(&format!("{sep}a{sep}..{sep}b"))),
            None
        );
    }
}
