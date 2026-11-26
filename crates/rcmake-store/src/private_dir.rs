//! Refuse a cache directory that another user can change or replace.
//!
//! Callers resolve the configured path with [`canonical_path`] first and use the result
//! for every later operation, so no operation depends on a symbolic link.
//!
//! On POSIX hosts a private directory must be a real directory that the effective user
//! owns and that no group or other user can write. Each component of its path, from `/`
//! down, must be a real directory (not a symbolic link) owned by root or by the effective
//! user. A component that a group or other users can write must have the sticky bit. Then
//! no other user can rename, remove, or replace the directory or any component above it,
//! so the checked path keeps naming the checked directory. Every check reads fresh
//! metadata ([`Host::metadata_fresh`]).
//!
//! On Windows the host reports no owner and no access control list. A private directory
//! must resolve, through junctions and other reparse points, to a path inside the
//! resolved per-user profile (`%LOCALAPPDATA%` or `%USERPROFILE%`). The default access
//! control list of the profile admits only the user, administrators, and the system.

use std::io::{self, ErrorKind};
use std::path::{Component, Path, PathBuf};

use rcmake_host::{FileKind, Follow, Host, HostError, HostResult, Metadata};

use crate::io_at;

const STICKY: u32 = 0o1000;
const SHARED_WRITE: u32 = 0o022;

/// A directory that passed the privacy checks, bound to its file identity.
///
/// Use [`PrivateDir::recheck`] before each later use. The check reads fresh metadata of
/// the directory and requires the same device and inode, so a replaced directory fails.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PrivateDir {
    dev: u64,
    ino: u64,
    /// Effective user ID on POSIX hosts. `None` on Windows.
    uid: Option<u32>,
}

impl PrivateDir {
    /// Check that `dir` is still the directory that passed the checks, and return the
    /// metadata sample that the check used.
    ///
    /// One fresh sample gives the identity, the kind, the owner, and the mode. The
    /// components above `dir` passed the checks before, so only the effective user or
    /// root can replace them.
    ///
    /// # Errors
    ///
    /// Return a `NotFound` error if `dir` was removed, and a `PermissionDenied` error if it
    /// was replaced or changed.
    pub fn recheck(self, host: &dyn Host, dir: &Path) -> HostResult<Metadata> {
        let meta = host.metadata_fresh(dir, Follow::No)?;
        check_dir(dir, &meta, self.uid)?;
        if (meta.dev, meta.ino) != (self.dev, self.ino) {
            return Err(refuse(
                dir,
                "it was replaced after its first check".to_owned(),
            ));
        }
        Ok(meta)
    }

    /// Bind a directory that passed the checks to its file identity.
    fn admit(meta: &Metadata, uid: Option<u32>) -> Self {
        Self {
            dev: meta.dev,
            ino: meta.ino,
            uid,
        }
    }
}

/// Return the resolved path that names `path`, without symbolic links, junctions, or
/// other reparse points.
///
/// The deepest existing ancestor is canonicalized from fresh data
/// ([`Host::canonicalize_fresh`]), and the missing components are added again. The result
/// is not checked: pass it to [`verify_private_dir`] or [`ensure_private_dir`], and use it
/// for all later operations. On Windows the `\\?\` prefix of a drive or UNC path is
/// removed, and a path with a `.` or `..` component is returned unchanged, so that the
/// location check refuses it.
pub fn canonical_path(host: &dyn Host, path: &Path) -> PathBuf {
    if host.system().sysname != "Windows" {
        return resolve_existing(host, path);
    }
    if has_relative_component(path) {
        return path.to_owned();
    }
    without_verbatim_prefix(resolve_existing(host, path))
}

/// Return `true` if `path` has a `.` or `..` component.
fn has_relative_component(path: &Path) -> bool {
    path.components()
        .any(|component| matches!(component, Component::CurDir | Component::ParentDir))
}

/// Remove the Windows `\\?\` prefix from a drive path (`\\?\C:\...`) or a UNC path
/// (`\\?\UNC\server\share`), so that other programs accept the path.
fn without_verbatim_prefix(path: PathBuf) -> PathBuf {
    let Some(text) = path.to_str() else {
        return path;
    };
    if let Some(rest) = text.strip_prefix(r"\\?\UNC\") {
        return PathBuf::from(format!(r"\\{rest}"));
    }
    match text.strip_prefix(r"\\?\") {
        Some(rest) if rest.as_bytes().get(1) == Some(&b':') => PathBuf::from(rest),
        _ => path,
    }
}

/// Canonicalize the deepest existing ancestor of `path` and add the missing components.
fn resolve_existing(host: &dyn Host, path: &Path) -> PathBuf {
    let mut missing = Vec::new();
    for ancestor in path.ancestors() {
        if let Ok(mut resolved) = host.canonicalize_fresh(ancestor) {
            resolved.extend(missing.iter().rev());
            return resolved;
        }
        match ancestor.file_name() {
            Some(name) => missing.push(name),
            None => break,
        }
    }
    path.to_owned()
}

/// Check that the existing directory `dir` is private to the current user.
///
/// On POSIX hosts `dir` must be a canonical path from [`canonical_path`].
///
/// # Errors
///
/// Return a `NotFound` error if `dir` or a component above it does not exist. Return a
/// `PermissionDenied` error that names the problem if `dir` or a component fails a check,
/// if a component cannot be read, or if the host cannot report the current user.
pub fn verify_private_dir(host: &dyn Host, dir: &Path) -> HostResult<PrivateDir> {
    let uid = current_user(host, dir)?;
    match uid {
        Some(uid) => {
            if let Some(parent) = dir.parent() {
                check_chain(host, dir, parent, uid)?;
            }
        }
        None => check_windows_location(host, dir)?,
    }
    let meta = host.metadata_fresh(dir, Follow::No)?;
    check_dir(dir, &meta, uid)?;
    Ok(PrivateDir::admit(&meta, uid))
}

/// Create `dir` and its missing components so that only the current user can use them.
///
/// The walk starts at `/`. Each existing or new component is checked before anything is
/// created below it, also when another process creates it first. Each new directory
/// starts at mode `0o700` or narrower. An existing `dir` keeps its mode and must pass
/// [`verify_private_dir`].
///
/// # Errors
///
/// Return a `PermissionDenied` error if a component or `dir` fails a check, and the host
/// error if a directory cannot be created.
pub fn ensure_private_dir(host: &dyn Host, dir: &Path) -> HostResult<PrivateDir> {
    match verify_private_dir(host, dir) {
        Err(error) if error.kind == ErrorKind::NotFound => {}
        result => return result,
    }
    let uid = current_user(host, dir)?;
    let mut current = PathBuf::new();
    for component in dir.components() {
        match component {
            Component::Prefix(_) => {
                current.push(component.as_os_str());
                continue;
            }
            Component::RootDir => current.push(component.as_os_str()),
            Component::Normal(name) => {
                current.push(name);
                match host.metadata_fresh(&current, Follow::No) {
                    Ok(_) => {}
                    Err(error) if error.kind == ErrorKind::NotFound => {
                        match host.create_private_dir(&current) {
                            // Another process created it first. The check below covers it.
                            Ok(())
                            | Err(HostError {
                                kind: ErrorKind::AlreadyExists,
                                ..
                            }) => {}
                            Err(error) => return Err(error),
                        }
                    }
                    Err(error) => return Err(lookup_refusal(dir, &current, &error)),
                }
            }
            Component::CurDir | Component::ParentDir => return Err(relative_refusal(dir)),
        }
        match uid {
            Some(uid) if current.as_path() != dir => check_ancestor(host, dir, &current, uid)?,
            Some(_) => {}
            // On Windows each component of the resolved path must still resolve to itself,
            // so no junction appeared since the path was resolved.
            None if matches!(component, Component::Normal(_)) => {
                check_windows_component(host, dir, &current)?;
            }
            None => {}
        }
    }
    verify_private_dir(host, dir)
}

/// Return the effective user ID, `None` on Windows, or an error if a POSIX host cannot
/// report it.
fn current_user(host: &dyn Host, dir: &Path) -> HostResult<Option<u32>> {
    if host.system().sysname == "Windows" {
        return Ok(None);
    }
    host.effective_uid().map(Some).ok_or_else(|| {
        refuse(
            dir,
            "the host cannot report the current user, so the owner cannot be checked".to_owned(),
        )
    })
}

/// Check the directory itself: kind, owner, and mode.
fn check_dir(dir: &Path, meta: &Metadata, uid: Option<u32>) -> HostResult<()> {
    if meta.kind != FileKind::Dir {
        return Err(refuse(dir, "it is not a directory".to_owned()));
    }
    let Some(uid) = uid else {
        return Ok(());
    };
    match meta.owner {
        Some(owner) if owner == uid => {}
        Some(_) => {
            return Err(refuse(
                dir,
                "it is owned by another user. Set RCMAKE_CACHE_DIR to a directory that you own"
                    .to_owned(),
            ));
        }
        None => {
            return Err(refuse(dir, "the host cannot report its owner".to_owned()));
        }
    }
    if meta.mode & SHARED_WRITE != 0 {
        let shown = dir.display();
        return Err(refuse(
            dir,
            format!("it is writable by group or others. Run: chmod 700 {shown}"),
        ));
    }
    Ok(())
}

/// Check each component of `start` from `/` down for the directory `dir`.
fn check_chain(host: &dyn Host, dir: &Path, start: &Path, uid: u32) -> HostResult<()> {
    let mut current = PathBuf::new();
    for component in start.components() {
        match component {
            Component::Prefix(_) => {
                current.push(component.as_os_str());
                continue;
            }
            Component::RootDir => current.push(component.as_os_str()),
            Component::Normal(name) => current.push(name),
            Component::CurDir | Component::ParentDir => return Err(relative_refusal(dir)),
        }
        check_ancestor(host, dir, &current, uid)?;
    }
    Ok(())
}

/// Check one component above `dir` from one fresh sample without following a link.
///
/// A missing component returns its `NotFound` error unchanged.
fn check_ancestor(host: &dyn Host, dir: &Path, path: &Path, uid: u32) -> HostResult<()> {
    let meta = match host.metadata_fresh(path, Follow::No) {
        Ok(meta) => meta,
        Err(error) if error.kind == ErrorKind::NotFound => return Err(error),
        Err(error) => return Err(lookup_refusal(dir, path, &error)),
    };
    let shown = path.display();
    let owner = meta.owner.ok_or_else(|| {
        refuse(
            dir,
            format!("the host cannot report the owner of its ancestor {shown}"),
        )
    })?;
    if owner != 0 && owner != uid {
        return Err(refuse(
            dir,
            format!(
                "its ancestor {shown} is owned by another user, who can replace it. Set RCMAKE_CACHE_DIR to a private directory"
            ),
        ));
    }
    match meta.kind {
        FileKind::Dir if meta.mode & SHARED_WRITE == 0 || meta.mode & STICKY != 0 => Ok(()),
        FileKind::Dir => Err(refuse(
            dir,
            format!(
                "its ancestor {shown} is writable by group or others and does not have the sticky bit. Run: chmod go-w {shown}"
            ),
        )),
        FileKind::Symlink => Err(refuse(
            dir,
            format!(
                "its ancestor {shown} is a symbolic link that changed after the path was resolved"
            ),
        )),
        FileKind::File | FileKind::Other => Err(refuse(
            dir,
            format!("its ancestor {shown} is not a directory"),
        )),
    }
}

/// Require a Windows directory that resolves to a path inside the resolved user profile.
fn check_windows_location(host: &dyn Host, dir: &Path) -> HostResult<()> {
    if has_relative_component(dir) {
        return Err(relative_refusal(dir));
    }
    // Canonicalization follows junctions and other reparse points to their targets.
    let resolved = resolve_existing(host, dir);
    let target = lowercase(&resolved);
    let inside = [b"LOCALAPPDATA".as_slice(), b"USERPROFILE".as_slice()]
        .into_iter()
        .filter_map(|name| host.env_get(name))
        .map(|value| rcmake_host::path::to_native(&value))
        .filter(|base| base.is_absolute())
        .filter_map(|base| host.canonicalize_fresh(&base).ok())
        .any(|base| target.starts_with(lowercase(&base)));
    if !inside {
        return Err(refuse(
            dir,
            "it does not resolve to a path inside the user profile, so its access control list is not known to be private. Set RCMAKE_CACHE_DIR to a directory inside %LOCALAPPDATA%"
                .to_owned(),
        ));
    }
    // The operations use `dir`, so `dir` must be the resolved path that passed the check.
    if lowercase(&without_verbatim_prefix(resolved)) != lowercase(dir) {
        return Err(refuse(
            dir,
            "it resolves to another path through a junction or link".to_owned(),
        ));
    }
    Ok(())
}

/// Require that the existing Windows component `path` of `dir` resolves to itself.
fn check_windows_component(host: &dyn Host, dir: &Path, path: &Path) -> HostResult<()> {
    let resolved = host
        .canonicalize_fresh(path)
        .map_err(|error| lookup_refusal(dir, path, &error))?;
    if lowercase(&without_verbatim_prefix(resolved)) == lowercase(path) {
        return Ok(());
    }
    let shown = path.display();
    Err(refuse(
        dir,
        format!("its component {shown} resolves to another path through a junction or link"),
    ))
}

/// Lowercase a Windows path for a case-insensitive comparison.
fn lowercase(path: &Path) -> PathBuf {
    PathBuf::from(path.to_string_lossy().to_lowercase())
}

fn relative_refusal(dir: &Path) -> HostError {
    refuse(dir, "its path has a `.` or `..` component".to_owned())
}

fn lookup_refusal(dir: &Path, path: &Path, error: &HostError) -> HostError {
    let shown = path.display();
    refuse(
        dir,
        format!("its ancestor {shown} cannot be checked ({error})"),
    )
}

/// Return a permission error for `dir` with the reason `reason`.
fn refuse(dir: &Path, reason: String) -> HostError {
    let shown = dir.display();
    let message = format!("Refusing cache directory {shown}: {reason}.");
    io_at(io::Error::new(ErrorKind::PermissionDenied, message), dir)
}
