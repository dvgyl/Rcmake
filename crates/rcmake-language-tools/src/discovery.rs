//! Bounded CMake listfile discovery and safe atomic file replacement over [`Host`].

#![forbid(unsafe_code)]

use std::collections::BTreeSet;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use rcmake_host::{FileKind, FileTime, Follow, Host, Metadata, WriteMode};

/// Maximum number of discovered CMake files per run.
pub const MAX_DISCOVERED_FILES: usize = 50_000;
/// Maximum directory recursion depth during file discovery.
pub const MAX_DISCOVERY_DEPTH: usize = 64;

static TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// File discovery error.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiscoveryError {
    /// Path where the failure occurred.
    pub path: PathBuf,
    /// Human-readable error message.
    pub message: String,
}

/// Captured file stamp used to detect intervening edits before atomic replacement.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FileStamp {
    /// File byte length.
    pub len: u64,
    /// Modification time.
    pub mtime: FileTime,
    /// Change time, if supplied by the host.
    pub ctime: Option<FileTime>,
    /// Device ID.
    pub dev: u64,
    /// Inode number.
    pub ino: u64,
    /// POSIX mode bits.
    pub mode: u32,
}

impl FileStamp {
    /// Extract a stamp from host [`Metadata`].
    #[must_use]
    pub const fn from_metadata(meta: &Metadata) -> Self {
        Self {
            len: meta.len,
            mtime: meta.mtime,
            ctime: meta.ctime,
            dev: meta.dev,
            ino: meta.ino,
            mode: meta.mode,
        }
    }

    /// Return `true` if `other` matches the identity and modification timestamps of `self`.
    #[must_use]
    pub fn matches_identity(&self, other: &Self) -> bool {
        self.len == other.len
            && self.mtime == other.mtime
            && self.ctime == other.ctime
            && self.dev == other.dev
            && self.ino == other.ino
    }
}

/// Normalize a path lexically (collapsing `.` and `..` without touching the filesystem).
#[must_use]
pub fn normalize_lexical(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if !matches!(
                    out.components().next_back(),
                    None | Some(Component::RootDir | Component::Prefix(_) | Component::ParentDir)
                ) {
                    out.pop();
                }
            }
            other => out.push(other),
        }
    }
    out
}

/// Resolve an operand relative to `cwd` and normalize it lexically.
#[must_use]
pub fn resolve_operand(cwd: &Path, operand: &Path) -> PathBuf {
    let joined = if operand.is_absolute() {
        operand.to_path_buf()
    } else {
        cwd.join(operand)
    };
    normalize_lexical(&joined)
}

/// Infer a project root boundary for `.rcmake.toml` lookup from a starting directory.
///
/// Walks upward looking for a VCS marker (`.git`) or the highest contiguous ancestor
/// containing `CMakeLists.txt`, falling back to `fallback_root`.
#[must_use]
pub fn infer_project_root(host: &dyn Host, start_dir: &Path, fallback_root: &Path) -> PathBuf {
    let mut best_cmake: Option<PathBuf> = None;
    let mut current = Some(start_dir);
    let mut depth = 0usize;
    while let Some(dir) = current {
        if depth >= MAX_DISCOVERY_DEPTH {
            break;
        }
        depth += 1;
        if host.metadata(&dir.join(".git"), Follow::No).is_ok() {
            return dir.to_owned();
        }
        if host
            .metadata(&dir.join("CMakeLists.txt"), Follow::Yes)
            .is_ok_and(|meta| meta.kind == FileKind::File)
        {
            best_cmake = Some(dir.to_owned());
        }
        current = dir.parent();
    }
    best_cmake.unwrap_or_else(|| fallback_root.to_owned())
}

/// Discover CMake listfiles from CLI operands (or `cwd` if `operands` is empty).
///
/// - Explicit file operands are included directly without extension filtering.
/// - Directory operands are walked iteratively for `CMakeLists.txt` and `*.cmake`.
/// - Skips VCS metadata (`.git`, `.hg`, `.svn`, `.bzr`), build trees (directories
///   containing `CMakeCache.txt` or `CMakeFiles`), AppleDouble `._*` entries,
///   and directory symlinks.
/// - Returns deduplicated paths in deterministic sorted order.
pub fn discover_files(
    host: &dyn Host,
    cwd: &Path,
    operands: &[PathBuf],
) -> Result<Vec<PathBuf>, DiscoveryError> {
    let targets: Vec<PathBuf> = if operands.is_empty() {
        vec![normalize_lexical(cwd)]
    } else {
        operands.iter().map(|op| resolve_operand(cwd, op)).collect()
    };

    let mut seen_canonical = BTreeSet::new();
    let mut files = Vec::new();

    for target in targets {
        let meta = host
            .metadata(&target, Follow::Yes)
            .map_err(|err| DiscoveryError {
                path: target.clone(),
                message: format!("Cannot access path: {err}"),
            })?;
        match meta.kind {
            FileKind::File => {
                record_file(host, target, &mut seen_canonical, &mut files)?;
            }
            FileKind::Dir => {
                walk_directory(host, &target, &mut seen_canonical, &mut files)?;
            }
            _ => {
                return Err(DiscoveryError {
                    path: target,
                    message: "Path is neither a regular file nor a directory.".into(),
                });
            }
        }
    }

    files.sort();
    Ok(files)
}

fn record_file(
    host: &dyn Host,
    path: PathBuf,
    seen: &mut BTreeSet<PathBuf>,
    files: &mut Vec<PathBuf>,
) -> Result<(), DiscoveryError> {
    if files.len() >= MAX_DISCOVERED_FILES {
        return Err(DiscoveryError {
            path,
            message: format!(
                "Discovered file count exceeds the {MAX_DISCOVERED_FILES} file limit."
            ),
        });
    }
    let key = host.canonicalize(&path).unwrap_or_else(|_| path.clone());
    if seen.insert(key) {
        files.push(path);
    }
    Ok(())
}

fn walk_directory(
    host: &dyn Host,
    root: &Path,
    seen: &mut BTreeSet<PathBuf>,
    files: &mut Vec<PathBuf>,
) -> Result<(), DiscoveryError> {
    let mut stack = vec![(root.to_path_buf(), 0usize)];
    while let Some((dir, depth)) = stack.pop() {
        if depth > MAX_DISCOVERY_DEPTH {
            continue;
        }
        let mut entries = host.read_dir(&dir).map_err(|err| DiscoveryError {
            path: dir.clone(),
            message: format!("Cannot read directory: {err}"),
        })?;
        entries.sort_by(|a, b| a.name.cmp(&b.name));

        // Skip build trees identified by CMakeCache.txt or a CMakeFiles directory,
        // unless this is the user's root directory (where an in-source build may live,
        // in which case we still skip CMakeFiles below).
        if depth > 0 && is_build_tree(&entries) {
            continue;
        }

        let mut subdirs = Vec::new();
        for entry in entries {
            let name = entry.name.as_slice();
            if name.starts_with(b"._") {
                continue;
            }
            let child = dir.join(rcmake_host::path::to_native(name));
            match entry.kind {
                FileKind::Dir => {
                    if is_skipped_dir_name(name) {
                        continue;
                    }
                    subdirs.push((child, depth + 1));
                }
                FileKind::File => {
                    if is_cmake_listfile_name(name) {
                        record_file(host, child, seen, files)?;
                    }
                }
                // Never follow directory symlinks; skip file symlinks in recursive walk as well
                // unless they are non-directory CMake files — check via lstat/stat carefully.
                FileKind::Symlink => {
                    if is_cmake_listfile_name(name)
                        && host
                            .metadata(&child, Follow::Yes)
                            .is_ok_and(|meta| meta.kind == FileKind::File)
                    {
                        record_file(host, child, seen, files)?;
                    }
                }
                FileKind::Other => {}
            }
        }
        // Push in reverse so lexicographically earlier directories are popped first.
        for subdir in subdirs.into_iter().rev() {
            stack.push(subdir);
        }
    }
    Ok(())
}

fn is_build_tree(entries: &[rcmake_host::DirEntry]) -> bool {
    entries.iter().any(|entry| {
        (entry.name.as_slice() == b"CMakeCache.txt" && entry.kind == FileKind::File)
            || (entry.name.as_slice() == b"CMakeFiles" && entry.kind == FileKind::Dir)
    })
}

fn is_skipped_dir_name(name: &[u8]) -> bool {
    matches!(
        name,
        b".git" | b".hg" | b".svn" | b".bzr" | b"CMakeFiles" | b"Testing"
    )
}

/// Return `true` if a filename matches `CMakeLists.txt` or `*.cmake`.
#[must_use]
pub fn is_cmake_listfile_name(name: &[u8]) -> bool {
    name == b"CMakeLists.txt" || (name.len() > 6 && name.ends_with(b".cmake"))
}

/// Atomically replace `path` via a same-directory temporary file and rename.
///
/// Preserves POSIX permissions and refuses replacement if fresh metadata shows
/// an intervening edit since `expected_stamp` was captured.
pub fn write_atomic_checked(
    host: &dyn Host,
    path: &Path,
    expected_stamp: &FileStamp,
    bytes: &[u8],
) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| format!("Path has no parent directory: {}", path.display()))?;

    let before = host
        .metadata_fresh(path, Follow::Yes)
        .map_err(|err| format!("Failed to stat {}: {err}", path.display()))?;
    if !expected_stamp.matches_identity(&FileStamp::from_metadata(&before)) {
        return Err(format!(
            "Refusing to overwrite {}: file changed on disk during formatting.",
            path.display()
        ));
    }

    let pid = host.system().pid;
    let mut temp_path = None;
    for _ in 0..32 {
        let seq = TMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let candidate = parent.join(format!(".rcmake-fmt.{pid}.{seq}.tmp"));
        match host.write(&candidate, bytes, WriteMode::CreateNew) {
            Ok(()) => {
                temp_path = Some(candidate);
                break;
            }
            Err(err) if err.kind == std::io::ErrorKind::AlreadyExists => continue,
            Err(err) => {
                return Err(format!(
                    "Failed to create temporary file in {}: {err}",
                    parent.display()
                ));
            }
        }
    }
    let temp = temp_path.ok_or_else(|| {
        format!(
            "Failed to allocate a unique temporary file in {}.",
            parent.display()
        )
    })?;

    let mode = expected_stamp.mode & 0o7777;
    if mode != 0
        && let Err(err) = host.set_permissions(&temp, mode)
    {
        let _ = host.remove_file(&temp);
        return Err(format!(
            "Failed to preserve permissions on {}: {err}",
            path.display()
        ));
    }

    let check = host.metadata_fresh(path, Follow::Yes).map_err(|err| {
        let _ = host.remove_file(&temp);
        format!("Failed to re-stat {}: {err}", path.display())
    })?;
    if !expected_stamp.matches_identity(&FileStamp::from_metadata(&check)) {
        let _ = host.remove_file(&temp);
        return Err(format!(
            "Refusing to overwrite {}: file changed on disk before rename.",
            path.display()
        ));
    }

    if let Err(err) = host.rename(&temp, path) {
        let _ = host.remove_file(&temp);
        return Err(format!("Failed to replace {}: {err}", path.display()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use rcmake_host::PureHost;

    use super::*;

    #[test]
    fn discovery_skips_vcs_build_trees_appledouble_and_dir_symlinks() {
        let host = PureHost::default();
        host.seed_file(Path::new("/work/CMakeLists.txt"), b"project(A)\n")
            .unwrap();
        host.seed_file(Path::new("/work/cmake/Helpers.cmake"), b"set(X 1)\n")
            .unwrap();
        host.seed_file(Path::new("/work/._CMakeLists.txt"), b"junk")
            .unwrap();
        host.seed_file(Path::new("/work/cmake/._Helpers.cmake"), b"junk")
            .unwrap();
        host.seed_file(Path::new("/work/.git/CMakeLists.txt"), b"ignore")
            .unwrap();
        host.seed_file(Path::new("/work/build/CMakeCache.txt"), b"cache")
            .unwrap();
        host.seed_file(Path::new("/work/build/Generated.cmake"), b"ignore")
            .unwrap();
        host.seed_file(Path::new("/work/CMakeFiles/Temp.cmake"), b"ignore")
            .unwrap();
        host.seed_file(Path::new("/elsewhere/CMakeLists.txt"), b"external")
            .unwrap();
        host.symlink(Path::new("/elsewhere"), Path::new("/work/linked_dir"))
            .unwrap();

        let found = discover_files(&host, Path::new("/work"), &[]).unwrap();
        assert_eq!(
            found,
            vec![
                PathBuf::from("/work/CMakeLists.txt"),
                PathBuf::from("/work/cmake/Helpers.cmake"),
            ]
        );
    }

    #[test]
    fn atomic_write_preserves_permissions_and_detects_intervening_edits() {
        let host = PureHost::default();
        let file = Path::new("/work/CMakeLists.txt");
        host.seed_file(file, b"set(A 1)\n").unwrap();
        host.set_permissions(file, 0o750).unwrap();
        let stamp = FileStamp::from_metadata(&host.metadata_fresh(file, Follow::Yes).unwrap());

        write_atomic_checked(&host, file, &stamp, b"set(A 2)\n").unwrap();
        assert_eq!(
            host.read(file, rcmake_host::ReadRange::All { limit: 1024 })
                .unwrap(),
            b"set(A 2)\n"
        );
        let new_meta = host.metadata_fresh(file, Follow::Yes).unwrap();
        assert_eq!(new_meta.mode & 0o777, 0o750);

        // Using the stale stamp must now fail without overwriting.
        let err = write_atomic_checked(&host, file, &stamp, b"set(A 3)\n").unwrap_err();
        assert!(err.contains("changed on disk"));
        assert_eq!(
            host.read(file, rcmake_host::ReadRange::All { limit: 1024 })
                .unwrap(),
            b"set(A 2)\n"
        );
    }
}
