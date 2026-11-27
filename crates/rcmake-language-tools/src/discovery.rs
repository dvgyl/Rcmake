//! Bounded CMake listfile discovery and safe atomic file replacement over [`Host`].

#![forbid(unsafe_code)]

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use rcmake_host::{FileKind, FileTime, Follow, Host, Metadata, WriteMode};

use crate::config::ConfigCache;

/// Maximum number of discovered CMake files per run.
pub const MAX_DISCOVERED_FILES: usize = 50_000;
/// Maximum directory recursion depth during file discovery.
pub const MAX_DISCOVERY_DEPTH: usize = 64;
/// Maximum total directory entries visited during file discovery before stopping.
pub const MAX_VISITED_ENTRIES: usize = 200_000;

static TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Resource limits enforced during CMake listfile discovery.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DiscoveryLimits {
    /// Maximum matched CMake files returned.
    pub max_files: usize,
    /// Maximum directory recursion depth.
    pub max_depth: usize,
    /// Maximum total directory entries visited across all walked directories.
    pub max_visited_entries: usize,
}

impl Default for DiscoveryLimits {
    fn default() -> Self {
        Self {
            max_files: MAX_DISCOVERED_FILES,
            max_depth: MAX_DISCOVERY_DEPTH,
            max_visited_entries: MAX_VISITED_ENTRIES,
        }
    }
}

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
/// containing `CMakeLists.txt` (stopping at the first ancestor without `CMakeLists.txt`
/// once a `CMakeLists.txt` chain has started), falling back to `fallback_root`.
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
        } else if best_cmake.is_some() {
            break;
        }
        current = dir.parent();
    }
    best_cmake.unwrap_or_else(|| fallback_root.to_owned())
}

/// Directory-keyed cache for [`infer_project_root`].
#[derive(Clone, Debug, Default)]
pub struct ProjectRootCache {
    by_dir: BTreeMap<(PathBuf, PathBuf), PathBuf>,
}

impl ProjectRootCache {
    /// Resolve and cache the project root for `start_dir` with `fallback_root`.
    #[must_use]
    pub fn infer(&mut self, host: &dyn Host, start_dir: &Path, fallback_root: &Path) -> PathBuf {
        let key = (
            normalize_lexical(start_dir),
            normalize_lexical(fallback_root),
        );
        if let Some(cached) = self.by_dir.get(&key) {
            return cached.clone();
        }
        let resolved = infer_project_root(host, &key.0, &key.1);
        self.by_dir.insert(key, resolved.clone());
        resolved
    }

    /// Clear all cached project root mappings.
    pub fn clear(&mut self) {
        self.by_dir.clear();
    }
}

/// Return `true` if `candidate` matches any `.rcmake.toml` `exclude` pattern relative to `base_dir`.
#[must_use]
pub fn matches_exclude_patterns(base_dir: &Path, candidate: &Path, patterns: &[String]) -> bool {
    if patterns.is_empty() {
        return false;
    }
    let norm_base = normalize_lexical(base_dir);
    let norm_cand = normalize_lexical(candidate);
    let Ok(rel) = norm_cand.strip_prefix(&norm_base) else {
        return false;
    };
    let rel_posix: Vec<String> = rel
        .components()
        .filter_map(|c| match c {
            Component::Normal(s) => Some(s.to_string_lossy().into_owned()),
            _ => None,
        })
        .collect();
    if rel_posix.is_empty() {
        return false;
    }
    let rel_joined = rel_posix.join("/");
    let file_name = rel_posix.last().map(String::as_str).unwrap_or("");
    patterns
        .iter()
        .any(|pat| match_exclude_pattern(&rel_posix, &rel_joined, file_name, pat))
}

fn match_exclude_pattern(
    rel_segments: &[String],
    rel_joined: &str,
    file_name: &str,
    raw_pattern: &str,
) -> bool {
    let mut pat = raw_pattern.trim().replace('\\', "/");
    while let Some(rest) = pat.strip_prefix("./") {
        pat = rest.to_owned();
    }
    while let Some(rest) = pat.strip_prefix('/') {
        pat = rest.to_owned();
    }
    let trimmed = pat.trim_end_matches('/');
    if trimmed.is_empty() {
        return false;
    }
    let has_slash = trimmed.contains('/');
    let has_glob = trimmed.contains('*') || trimmed.contains('?');
    if !has_slash {
        if !has_glob {
            return rel_segments.iter().any(|seg| seg == trimmed);
        }
        return glob_match_segment(trimmed, file_name)
            || rel_segments
                .iter()
                .any(|seg| glob_match_segment(trimmed, seg));
    }
    let pat_segs: Vec<&str> = trimmed.split('/').filter(|s| !s.is_empty()).collect();
    if pat_segs.is_empty() {
        return false;
    }
    if glob_match_segments(&pat_segs, rel_segments) {
        return true;
    }
    // A path pattern without trailing wildcard also excludes children beneath that directory.
    if !has_glob
        && rel_joined
            .strip_prefix(trimmed)
            .is_some_and(|rest| rest.starts_with('/'))
    {
        return true;
    }
    false
}

fn glob_match_segments(pat: &[&str], path: &[String]) -> bool {
    let n = path.len();
    let mut dp = vec![false; n + 1];
    if let Some(first) = dp.first_mut() {
        *first = true;
    }
    for &p in pat {
        let mut next = vec![false; n + 1];
        if p == "**" {
            let mut seen = false;
            for (dst, &src) in next.iter_mut().zip(dp.iter()) {
                seen |= src;
                *dst = seen;
            }
        } else {
            for ((dst, &prev), seg) in next.iter_mut().skip(1).zip(dp.iter()).zip(path.iter()) {
                if prev && glob_match_segment(p, seg) {
                    *dst = true;
                }
            }
        }
        dp = next;
    }
    if dp.last().copied().unwrap_or(false) {
        return true;
    }
    // Also match a directory prefix when pattern ends with `/**` and candidate IS that directory.
    if pat.len() >= 2
        && pat.last() == Some(&"**")
        && let Some(prefix) = pat.get(..pat.len() - 1)
    {
        return glob_match_segments(prefix, path);
    }
    false
}

fn glob_match_segment(pat: &str, text: &str) -> bool {
    let t: Vec<char> = text.chars().collect();
    let n = t.len();
    let mut dp = vec![false; n + 1];
    if let Some(first) = dp.first_mut() {
        *first = true;
    }
    for pc in pat.chars() {
        let mut next = vec![false; n + 1];
        if pc == '*' {
            let mut seen = false;
            for (dst, &src) in next.iter_mut().zip(dp.iter()) {
                seen |= src;
                *dst = seen;
            }
        } else {
            for ((dst, &prev), &tc) in next.iter_mut().skip(1).zip(dp.iter()).zip(t.iter()) {
                if prev && (pc == '?' || pc == tc) {
                    *dst = true;
                }
            }
        }
        dp = next;
    }
    dp.last().copied().unwrap_or(false)
}

/// Discover CMake listfiles from CLI operands (or `cwd` if `operands` is empty) using default
/// [`DiscoveryLimits`].
///
/// Policy:
/// - Explicit file operands are included directly without extension filtering. Explicit
///   symbolic links to regular files are returned so read-only modes (`--check`, `--diff`,
///   `lint`) can inspect their targets; in-place writers refuse to overwrite symbolic links.
/// - Directory operands are walked iteratively for `CMakeLists.txt` and `*.cmake`.
/// - Recursive walks skip VCS metadata (`.git`, `.hg`, `.svn`, `.bzr`), build trees
///   (directories containing `CMakeCache.txt` or `CMakeFiles`), AppleDouble `._*` entries,
///   and all symbolic links (both directory and file symlinks).
/// - Returns deduplicated paths in deterministic sorted order.
pub fn discover_files(
    host: &dyn Host,
    cwd: &Path,
    operands: &[PathBuf],
) -> Result<Vec<PathBuf>, DiscoveryError> {
    discover_files_with_limits(host, cwd, operands, &DiscoveryLimits::default())
}

/// Discover CMake listfiles with explicit [`DiscoveryLimits`].
pub fn discover_files_with_limits(
    host: &dyn Host,
    cwd: &Path,
    operands: &[PathBuf],
    limits: &DiscoveryLimits,
) -> Result<Vec<PathBuf>, DiscoveryError> {
    discover_files_inner(host, cwd, operands, limits, None)
}

/// Discover CMake listfiles honoring `.rcmake.toml` `exclude` patterns via cached
/// project-root and configuration resolution.
pub fn discover_files_with_config(
    host: &dyn Host,
    cwd: &Path,
    operands: &[PathBuf],
    limits: &DiscoveryLimits,
    config_cache: &mut ConfigCache,
    root_cache: &mut ProjectRootCache,
) -> Result<Vec<PathBuf>, DiscoveryError> {
    discover_files_inner(
        host,
        cwd,
        operands,
        limits,
        Some((config_cache, root_cache)),
    )
}

fn discover_files_inner(
    host: &dyn Host,
    cwd: &Path,
    operands: &[PathBuf],
    limits: &DiscoveryLimits,
    mut exclude_ctx: Option<(&mut ConfigCache, &mut ProjectRootCache)>,
) -> Result<Vec<PathBuf>, DiscoveryError> {
    let targets: Vec<PathBuf> = if operands.is_empty() {
        vec![normalize_lexical(cwd)]
    } else {
        operands.iter().map(|op| resolve_operand(cwd, op)).collect()
    };

    let mut seen_canonical = BTreeSet::new();
    let mut files = Vec::new();
    let mut visited_entries = 0usize;

    for target in targets {
        if visited_entries >= limits.max_visited_entries {
            return Err(DiscoveryError {
                path: target,
                message: format!(
                    "Discovery visited-entry count exceeds the {} entry limit.",
                    limits.max_visited_entries
                ),
            });
        }
        visited_entries += 1;
        let meta = host
            .metadata(&target, Follow::Yes)
            .map_err(|err| DiscoveryError {
                path: target.clone(),
                message: format!("Cannot access path: {err}"),
            })?;
        match meta.kind {
            FileKind::File => {
                record_file(host, target, limits, &mut seen_canonical, &mut files)?;
            }
            FileKind::Dir => {
                walk_directory(
                    host,
                    cwd,
                    &target,
                    limits,
                    &mut visited_entries,
                    &mut seen_canonical,
                    &mut files,
                    exclude_ctx.as_mut(),
                )?;
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
    limits: &DiscoveryLimits,
    seen: &mut BTreeSet<PathBuf>,
    files: &mut Vec<PathBuf>,
) -> Result<(), DiscoveryError> {
    if files.len() >= limits.max_files {
        return Err(DiscoveryError {
            path,
            message: format!(
                "Discovered file count exceeds the {} file limit.",
                limits.max_files
            ),
        });
    }
    let key = host.canonicalize(&path).unwrap_or_else(|_| path.clone());
    if seen.insert(key) {
        files.push(path);
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn walk_directory(
    host: &dyn Host,
    cwd: &Path,
    root: &Path,
    limits: &DiscoveryLimits,
    visited_entries: &mut usize,
    seen: &mut BTreeSet<PathBuf>,
    files: &mut Vec<PathBuf>,
    exclude_ctx: Option<&mut (&mut ConfigCache, &mut ProjectRootCache)>,
) -> Result<(), DiscoveryError> {
    let mut stack = vec![(root.to_path_buf(), 0usize)];
    while let Some((dir, depth)) = stack.pop() {
        if depth > limits.max_depth {
            continue;
        }
        let exclude_rule: Option<(PathBuf, Vec<String>)> =
            if let Some(&mut (ref mut config_cache, ref mut root_cache)) = exclude_ctx {
                let boundary = root_cache.infer(host, &dir, cwd);
                let resolved =
                    config_cache
                        .resolve(host, &dir, Some(&boundary))
                        .map_err(|err| DiscoveryError {
                            path: err.path.clone().unwrap_or_else(|| dir.clone()),
                            message: err.to_string(),
                        })?;
                if !resolved.config.exclude.is_empty() {
                    let base_dir = resolved
                        .config_path
                        .as_deref()
                        .and_then(Path::parent)
                        .unwrap_or(&dir)
                        .to_path_buf();
                    Some((base_dir, resolved.config.exclude))
                } else {
                    None
                }
            } else {
                None
            };
        let remaining = limits.max_visited_entries.saturating_sub(*visited_entries);
        let (mut entries, truncated) =
            host.read_dir_bounded(&dir, remaining)
                .map_err(|err| DiscoveryError {
                    path: dir.clone(),
                    message: format!("Cannot read directory: {err}"),
                })?;
        // Charge the visited-entry budget before sorting or retaining any entry from `dir`.
        let next_visited = visited_entries.saturating_add(entries.len());
        if truncated || next_visited > limits.max_visited_entries {
            return Err(DiscoveryError {
                path: dir,
                message: format!(
                    "Discovery visited-entry count exceeds the {} entry limit.",
                    limits.max_visited_entries
                ),
            });
        }
        *visited_entries = next_visited;
        entries.sort_by(|a, b| a.name.cmp(&b.name));

        // Skip nested build trees while keeping an in-source root directory walkable
        // (CMakeFiles inside the root is still excluded below).
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
            if let Some((base_dir, patterns)) = &exclude_rule
                && matches_exclude_patterns(base_dir, &child, patterns)
            {
                continue;
            }
            match entry.kind {
                FileKind::Dir => {
                    if is_skipped_dir_name(name) {
                        continue;
                    }
                    subdirs.push((child, depth + 1));
                }
                FileKind::File => {
                    if is_cmake_listfile_name(name) {
                        record_file(host, child, limits, seen, files)?;
                    }
                }
                // Recursive walks skip all symbolic links (directory and file links) to avoid
                // directory cycles and silent link destruction on in-place writes.
                FileKind::Symlink | FileKind::Other => {}
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
/// Preserves POSIX permissions, refuses to replace symbolic links or non-regular files
/// (using non-following `Follow::No` metadata checks before staging and again immediately
/// before rename), and refuses replacement if fresh metadata shows an intervening edit
/// since `expected_stamp` was captured.
///
/// # Concurrency and platform race limits
///
/// [`Host::rename`] performs an unconditional atomic path replacement (neither POSIX
/// `rename(2)` nor Windows `MoveFileExW` provides kernel compare-and-swap against the
/// destination inode/metadata). Re-checking `metadata_fresh(path, Follow::No)` immediately
/// before `rename` catches intervening edits, inode replacements, and symlink swaps up to
/// that check, while a residual TOCTOU window remains between the final stat and `rename`
/// on hosts without atomic target-identity verification inside rename.
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
        .metadata_fresh(path, Follow::No)
        .map_err(|err| format!("Failed to stat {}: {err}", path.display()))?;
    verify_regular_target_stamp(path, &before, expected_stamp, "during formatting")?;

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

    let check = host.metadata_fresh(path, Follow::No).map_err(|err| {
        let _ = host.remove_file(&temp);
        format!("Failed to re-stat {}: {err}", path.display())
    })?;
    if let Err(msg) = verify_regular_target_stamp(path, &check, expected_stamp, "before rename") {
        let _ = host.remove_file(&temp);
        return Err(msg);
    }

    if let Err(err) = host.rename(&temp, path) {
        let _ = host.remove_file(&temp);
        return Err(format!("Failed to replace {}: {err}", path.display()));
    }
    Ok(())
}

fn verify_regular_target_stamp(
    path: &Path,
    meta: &Metadata,
    expected_stamp: &FileStamp,
    phase: &str,
) -> Result<(), String> {
    if meta.kind == FileKind::Symlink {
        return Err(format!(
            "Refusing to overwrite symbolic link {} in place; format the target file directly.",
            path.display()
        ));
    }
    if meta.kind != FileKind::File {
        return Err(format!(
            "Refusing to overwrite {}: path is not a regular file.",
            path.display()
        ));
    }
    if !expected_stamp.matches_identity(&FileStamp::from_metadata(meta)) {
        return Err(format!(
            "Refusing to overwrite {}: file changed on disk {phase}.",
            path.display()
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use core::time::Duration;
    use std::sync::atomic::AtomicBool;

    use rcmake_host::{
        CopyOptions, DirEntry, FileTime, HostError, HostResult, PureHost, ReadRange, RunOutput,
        RunRequest, Stream, SystemInfo, TimeZone,
    };

    use super::*;

    /// Host wrapper that runs a one-shot hook when a `CreateNew` temp file is created,
    /// simulating a concurrent editor save or symlink swap between staging and rename.
    struct RacingHost<F> {
        inner: PureHost,
        fired: AtomicBool,
        on_stage: F,
    }

    impl<F: Fn(&PureHost) + Send + Sync> RacingHost<F> {
        fn new(inner: PureHost, on_stage: F) -> Self {
            Self {
                inner,
                fired: AtomicBool::new(false),
                on_stage,
            }
        }
    }

    impl<F: Fn(&PureHost) + Send + Sync> Host for RacingHost<F> {
        fn metadata(&self, p: &Path, follow: Follow) -> HostResult<Metadata> {
            self.inner.metadata(p, follow)
        }
        fn metadata_fresh(&self, p: &Path, follow: Follow) -> HostResult<Metadata> {
            self.inner.metadata_fresh(p, follow)
        }
        fn read(&self, p: &Path, range: ReadRange) -> HostResult<Vec<u8>> {
            self.inner.read(p, range)
        }
        fn write(&self, p: &Path, data: &[u8], mode: WriteMode) -> HostResult<()> {
            self.inner.write(p, data, mode)?;
            if mode == WriteMode::CreateNew && !self.fired.swap(true, Ordering::SeqCst) {
                (self.on_stage)(&self.inner);
            }
            Ok(())
        }
        fn read_dir(&self, p: &Path) -> HostResult<Vec<DirEntry>> {
            self.inner.read_dir(p)
        }
        fn create_dir_all(&self, p: &Path) -> HostResult<()> {
            self.inner.create_dir_all(p)
        }
        fn remove_file(&self, p: &Path) -> HostResult<()> {
            self.inner.remove_file(p)
        }
        fn remove_dir(&self, p: &Path) -> HostResult<()> {
            self.inner.remove_dir(p)
        }
        fn remove_dir_all(&self, p: &Path) -> HostResult<()> {
            self.inner.remove_dir_all(p)
        }
        fn rename(&self, from: &Path, to: &Path) -> HostResult<()> {
            self.inner.rename(from, to)
        }
        fn copy_file(&self, from: &Path, to: &Path, opts: CopyOptions) -> HostResult<()> {
            self.inner.copy_file(from, to, opts)
        }
        fn symlink(&self, target: &Path, link: &Path) -> HostResult<()> {
            self.inner.symlink(target, link)
        }
        fn hard_link(&self, src: &Path, dst: &Path) -> HostResult<()> {
            self.inner.hard_link(src, dst)
        }
        fn read_link(&self, p: &Path) -> HostResult<PathBuf> {
            self.inner.read_link(p)
        }
        fn canonicalize(&self, p: &Path) -> HostResult<PathBuf> {
            self.inner.canonicalize(p)
        }
        fn set_permissions(&self, p: &Path, mode: u32) -> HostResult<()> {
            self.inner.set_permissions(p, mode)
        }
        fn set_times(
            &self,
            p: &Path,
            atime: Option<FileTime>,
            mtime: Option<FileTime>,
        ) -> HostResult<()> {
            self.inner.set_times(p, atime, mtime)
        }
        fn env_get(&self, key: &[u8]) -> Option<rcmake_base::BString> {
            self.inner.env_get(key)
        }
        fn env_set(&self, key: &[u8], value: Option<&[u8]>) {
            self.inner.env_set(key, value);
        }
        fn env_all(&self) -> Vec<(rcmake_base::BString, rcmake_base::BString)> {
            self.inner.env_all()
        }
        fn run(
            &self,
            req: &RunRequest,
            on_output: &mut dyn FnMut(Stream, &[u8]),
        ) -> HostResult<RunOutput> {
            self.inner.run(req, on_output)
        }
        fn write_out(&self, stream: Stream, bytes: &[u8]) {
            self.inner.write_out(stream, bytes);
        }
        fn read_stdin(&self, limit: u64) -> HostResult<Vec<u8>> {
            self.inner.read_stdin(limit)
        }
        fn is_terminal(&self, stream: Stream) -> bool {
            self.inner.is_terminal(stream)
        }
        fn now(&self) -> FileTime {
            self.inner.now()
        }
        fn monotonic(&self) -> Duration {
            self.inner.monotonic()
        }
        fn sleep(&self, duration: Duration) {
            self.inner.sleep(duration);
        }
        fn time_zone(&self) -> TimeZone {
            self.inner.time_zone()
        }
        fn current_exe(&self) -> HostResult<PathBuf> {
            self.inner.current_exe()
        }
        fn initial_cwd(&self) -> PathBuf {
            self.inner.initial_cwd()
        }
        fn system(&self) -> SystemInfo {
            self.inner.system()
        }
        fn rename_noreplace(&self, from: &Path, to: &Path) -> HostResult<()> {
            self.inner.rename_noreplace(from, to)
        }
        fn effective_uid(&self) -> Option<u32> {
            self.inner.effective_uid()
        }
        fn read_with_origin(
            &self,
            p: &Path,
            range: ReadRange,
            origin: &Path,
        ) -> HostResult<Vec<u8>> {
            self.inner.read_with_origin(p, range, origin)
        }
        fn lock_file(
            &self,
            path: &Path,
            timeout: Option<Duration>,
        ) -> HostResult<Box<dyn rcmake_host::FileLockGuard>> {
            let _ = (path, timeout);
            Err(HostError::unsupported("unused in test"))
        }
    }

    #[test]
    fn discovery_skips_vcs_build_trees_appledouble_and_all_symlinks() {
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
        host.seed_file(Path::new("/elsewhere/Target.cmake"), b"set(Y 2)\n")
            .unwrap();
        host.symlink(Path::new("/elsewhere"), Path::new("/work/linked_dir"))
            .unwrap();
        host.symlink(
            Path::new("/elsewhere/Target.cmake"),
            Path::new("/work/linked.cmake"),
        )
        .unwrap();

        let found = discover_files(&host, Path::new("/work"), &[]).unwrap();
        assert_eq!(
            found,
            vec![
                PathBuf::from("/work/CMakeLists.txt"),
                PathBuf::from("/work/cmake/Helpers.cmake"),
            ]
        );

        // Explicit symlink operand is still accepted by discovery so read-only tools can inspect it.
        let direct =
            discover_files(&host, Path::new("/work"), &[PathBuf::from("linked.cmake")]).unwrap();
        assert_eq!(direct, vec![PathBuf::from("/work/linked.cmake")]);
    }

    #[test]
    fn discovery_enforces_visited_entry_limit_before_retaining() {
        let host = PureHost::default();
        for idx in 0..25 {
            host.seed_file(
                &PathBuf::from(format!("/work/file_{idx:02}.txt")),
                b"non-cmake",
            )
            .unwrap();
        }
        host.seed_file(Path::new("/work/CMakeLists.txt"), b"project(A)\n")
            .unwrap();

        let tight = DiscoveryLimits {
            max_visited_entries: 10,
            ..DiscoveryLimits::default()
        };
        let err = discover_files_with_limits(&host, Path::new("/work"), &[], &tight).unwrap_err();
        assert!(err.message.contains("visited-entry"), "got {}", err.message);
    }

    #[test]
    fn atomic_write_preserves_permissions_refuses_symlinks_and_detects_concurrent_replacement() {
        let host = PureHost::default();
        let file = Path::new("/work/CMakeLists.txt");
        host.seed_file(file, b"set(A 1)\n").unwrap();
        host.set_permissions(file, 0o750).unwrap();
        let stamp = FileStamp::from_metadata(&host.metadata_fresh(file, Follow::No).unwrap());

        write_atomic_checked(&host, file, &stamp, b"set(A 2)\n").unwrap();
        assert_eq!(
            host.read(file, ReadRange::All { limit: 1024 }).unwrap(),
            b"set(A 2)\n"
        );
        let new_meta = host.metadata_fresh(file, Follow::No).unwrap();
        assert_eq!(new_meta.mode & 0o777, 0o750);

        // Stale stamp fails without overwriting.
        let err = write_atomic_checked(&host, file, &stamp, b"set(A 3)\n").unwrap_err();
        assert!(err.contains("changed on disk"));
        assert_eq!(
            host.read(file, ReadRange::All { limit: 1024 }).unwrap(),
            b"set(A 2)\n"
        );

        // Direct symlink path is refused even if caller supplied target metadata.
        let link = Path::new("/work/linked.cmake");
        host.symlink(file, link).unwrap();
        let target_stamp =
            FileStamp::from_metadata(&host.metadata_fresh(link, Follow::Yes).unwrap());
        let sym_err = write_atomic_checked(&host, link, &target_stamp, b"set(A 9)\n").unwrap_err();
        assert!(sym_err.contains("symbolic link"), "got {sym_err}");
        assert_eq!(
            host.metadata_fresh(link, Follow::No).unwrap().kind,
            FileKind::Symlink
        );
        assert_eq!(
            host.read(file, ReadRange::All { limit: 1024 }).unwrap(),
            b"set(A 2)\n"
        );

        // Concurrent editor replacement mid-flight (after temp staging, before rename).
        let race_inner = PureHost::default();
        race_inner.seed_file(file, b"set(A 1)\n").unwrap();
        let race_stamp =
            FileStamp::from_metadata(&race_inner.metadata_fresh(file, Follow::No).unwrap());
        let race_host = RacingHost::new(race_inner, |h| {
            h.seed_file(
                Path::new("/work/CMakeLists.txt.new"),
                b"set(CONCURRENT 1)\n",
            )
            .unwrap();
            h.rename(Path::new("/work/CMakeLists.txt.new"), file)
                .unwrap();
        });
        let race_err =
            write_atomic_checked(&race_host, file, &race_stamp, b"set(A 2)\n").unwrap_err();
        assert!(race_err.contains("before rename"), "got {race_err}");
        assert_eq!(
            race_host
                .read(file, ReadRange::All { limit: 1024 })
                .unwrap(),
            b"set(CONCURRENT 1)\n"
        );
        let remaining = race_host.read_dir(Path::new("/work")).unwrap();
        assert!(
            !remaining
                .iter()
                .any(|e| e.name.as_slice().starts_with(b".rcmake-fmt.")),
            "temporary file leaked: {remaining:?}"
        );

        // Concurrent swap to a symbolic link mid-flight must also fail cleanly without replacing the link.
        let sym_race_inner = PureHost::default();
        sym_race_inner.seed_file(file, b"set(A 1)\n").unwrap();
        sym_race_inner
            .seed_file(Path::new("/work/other.cmake"), b"set(OTHER 1)\n")
            .unwrap();
        let sym_race_stamp =
            FileStamp::from_metadata(&sym_race_inner.metadata_fresh(file, Follow::No).unwrap());
        let sym_race_host = RacingHost::new(sym_race_inner, |h| {
            h.remove_file(file).unwrap();
            h.symlink(Path::new("/work/other.cmake"), file).unwrap();
        });
        let sym_race_err =
            write_atomic_checked(&sym_race_host, file, &sym_race_stamp, b"set(A 2)\n").unwrap_err();
        assert!(sym_race_err.contains("symbolic link"), "got {sym_race_err}");
        assert_eq!(
            sym_race_host.metadata_fresh(file, Follow::No).unwrap().kind,
            FileKind::Symlink
        );
    }

    #[test]
    fn infer_project_root_stops_at_first_ancestor_without_cmakelists_and_caches() {
        let host = PureHost::default();
        // Unrelated parent CMakeLists.txt separated by an intermediate directory with none.
        host.seed_file(Path::new("/home/CMakeLists.txt"), b"project(Stray)\n")
            .unwrap();
        host.create_dir_all(Path::new("/home/workspace")).unwrap();
        host.seed_file(
            Path::new("/home/workspace/proj/CMakeLists.txt"),
            b"project(Proj)\n",
        )
        .unwrap();
        host.seed_file(
            Path::new("/home/workspace/proj/sub/CMakeLists.txt"),
            b"add_library(s INTERFACE)\n",
        )
        .unwrap();
        host.seed_file(
            Path::new("/home/workspace/proj/cmake/Helper.cmake"),
            b"set(X 1)\n",
        )
        .unwrap();

        let fallback = Path::new("/fallback");
        assert_eq!(
            infer_project_root(&host, Path::new("/home/workspace/proj/sub"), fallback),
            PathBuf::from("/home/workspace/proj")
        );
        // Subdirectory without its own CMakeLists.txt climbs into the project, then stops at workspace.
        assert_eq!(
            infer_project_root(&host, Path::new("/home/workspace/proj/cmake"), fallback),
            PathBuf::from("/home/workspace/proj")
        );

        let mut cache = ProjectRootCache::default();
        let first = cache.infer(&host, Path::new("/home/workspace/proj/sub"), fallback);
        assert_eq!(first, PathBuf::from("/home/workspace/proj"));
        // Cached entry returns the memoized root without re-walking until cleared.
        host.seed_file(
            Path::new("/home/workspace/CMakeLists.txt"),
            b"project(NewParent)\n",
        )
        .unwrap();
        assert_eq!(
            cache.infer(&host, Path::new("/home/workspace/proj/sub"), fallback),
            PathBuf::from("/home/workspace/proj")
        );
        cache.clear();
        assert_eq!(
            cache.infer(&host, Path::new("/home/workspace/proj/sub"), fallback),
            PathBuf::from("/home")
        );
    }

    #[test]
    fn discovery_honors_rcmake_toml_exclude_patterns() {
        let host = PureHost::default();
        host.seed_file(
            Path::new("/work/.rcmake.toml"),
            b"exclude = [\"third_party/**\", \"gen/*.cmake\", \"skip.cmake\"]\n",
        )
        .unwrap();
        host.seed_file(Path::new("/work/CMakeLists.txt"), b"project(A)\n")
            .unwrap();
        host.seed_file(Path::new("/work/src/CMakeLists.txt"), b"set(A 1)\n")
            .unwrap();
        host.seed_file(Path::new("/work/src/skip.cmake"), b"set(SKIP 1)\n")
            .unwrap();
        host.seed_file(
            Path::new("/work/third_party/lib/CMakeLists.txt"),
            b"project(ThirdParty)\n",
        )
        .unwrap();
        host.seed_file(Path::new("/work/gen/auto.cmake"), b"set(G 1)\n")
            .unwrap();

        let mut cfg_cache = ConfigCache::default();
        let mut root_cache = ProjectRootCache::default();
        let found = discover_files_with_config(
            &host,
            Path::new("/work"),
            &[],
            &DiscoveryLimits::default(),
            &mut cfg_cache,
            &mut root_cache,
        )
        .unwrap();
        assert_eq!(
            found,
            vec![
                PathBuf::from("/work/CMakeLists.txt"),
                PathBuf::from("/work/src/CMakeLists.txt"),
            ]
        );
    }
}
