//! Private scratch staging, identical path recreation, File API query placement,
//! and clean paired process environments for `rcmake-verify`.

use std::io::{ErrorKind, Read};
use std::path::{Component, Path, PathBuf};

use rcmake_base::BString;
use rcmake_buildinfo::QUERY_FILES;
use rcmake_host::{FileKind, Follow, Host, WriteMode, path};

use super::protocol::{is_host_absolute, validate_source_relative};

const MAX_STAGE_FILES: usize = 20_000;
const MAX_STAGE_DEPTH: usize = 64;
const MAX_STAGE_FILE_BYTES: u64 = 64 * 1024 * 1024;
const MAX_STAGE_TOTAL_BYTES: u64 = 256 * 1024 * 1024;
const COPY_CHUNK_BYTES: usize = 64 * 1024;

/// Owned private scratch directory that cleans itself up via [`Self::cleanup`] or on drop.
pub(crate) struct ScratchWorkspace<'h> {
    host: &'h dyn Host,
    root: PathBuf,
    snapshot: PathBuf,
    work: PathBuf,
    staged_src: PathBuf,
    staged_bin: PathBuf,
    staged_home: PathBuf,
    staged_tmp: PathBuf,
    staged_xdg_cache: PathBuf,
    staged_xdg_config: PathBuf,
    staged_xdg_data: PathBuf,
    staged_appdata: PathBuf,
    staged_local_appdata: PathBuf,
    staged_ccache: PathBuf,
    staged_sccache: PathBuf,
    staged_rcmake_cache: PathBuf,
    cleaned: bool,
}

impl<'h> ScratchWorkspace<'h> {
    /// Allocate a private scratch parent strictly outside `source_dir` and capture
    /// a bounded pristine snapshot of `source_dir`.
    pub(crate) fn create(
        host: &'h dyn Host,
        source_dir: &Path,
        excluded_build_dir: Option<&Path>,
    ) -> Result<Self, String> {
        let source_canon = host.canonicalize(source_dir).map_err(|err| {
            format!(
                "Source directory {} is unreadable: {err}",
                source_dir.display()
            )
        })?;
        let base = temp_base_dir(host, &source_canon)?;
        let pid = host.system().pid;
        let nanos = host.monotonic().as_nanos();
        let mut root = None;
        for attempt in 0..64u32 {
            let candidate = base.join(format!("rcmake-verify-{pid}-{nanos}-{attempt}"));
            match host.create_private_dir(&candidate) {
                Ok(()) => {
                    root = Some(candidate);
                    break;
                }
                Err(err) if err.kind == ErrorKind::AlreadyExists => continue,
                Err(err) => {
                    return Err(format!(
                        "Could not create private verify scratch directory {}: {err}",
                        candidate.display()
                    ));
                }
            }
        }
        let Some(root) = root else {
            return Err("Could not allocate a unique private verify scratch directory.".into());
        };
        let canonical_root = host.canonicalize(&root).unwrap_or(root);
        if canonical_root == source_canon
            || canonical_root.starts_with(&source_canon)
            || source_canon.starts_with(&canonical_root)
        {
            let _ = host.remove_dir_all(&canonical_root);
            return Err(format!(
                "Verify scratch directory {} overlaps source directory {}. Set TMPDIR/TEMP outside the source tree.",
                canonical_root.display(),
                source_canon.display()
            ));
        }

        let snapshot = canonical_root.join("snapshot");
        let work = canonical_root.join("work");
        let staged_src = work.join("src");
        let staged_bin = work.join("build");
        let staged_home = work.join("home");
        let staged_tmp = work.join("tmp");
        let staged_xdg_cache = work.join("xdg-cache");
        let staged_xdg_config = work.join("xdg-config");
        let staged_xdg_data = work.join("xdg-data");
        let staged_appdata = work.join("appdata").join("roaming");
        let staged_local_appdata = work.join("appdata").join("local");
        let staged_ccache = work.join("ccache");
        let staged_sccache = work.join("sccache");
        let staged_rcmake_cache = work.join("rcmake-cache");

        let ws = Self {
            host,
            root: canonical_root,
            snapshot,
            work,
            staged_src,
            staged_bin,
            staged_home,
            staged_tmp,
            staged_xdg_cache,
            staged_xdg_config,
            staged_xdg_data,
            staged_appdata,
            staged_local_appdata,
            staged_ccache,
            staged_sccache,
            staged_rcmake_cache,
            cleaned: false,
        };

        let excluded_canon = excluded_build_dir.and_then(|dir| {
            host.canonicalize(dir)
                .ok()
                .or_else(|| Some(dir.to_path_buf()))
        });
        ws.host
            .create_dir_all(&ws.snapshot)
            .map_err(|err| format!("Could not create snapshot directory: {err}"))?;
        stage_tree(
            host,
            &source_canon,
            &ws.snapshot,
            excluded_canon.as_deref(),
            Some(&ws.root),
            true,
        )?;
        Ok(ws)
    }

    pub(crate) fn root(&self) -> &Path {
        &self.root
    }

    pub(crate) fn staged_src(&self) -> &Path {
        &self.staged_src
    }

    pub(crate) fn staged_bin(&self) -> &Path {
        &self.staged_bin
    }

    pub(crate) fn staged_home(&self) -> &Path {
        &self.staged_home
    }

    pub(crate) fn staged_tmp(&self) -> &Path {
        &self.staged_tmp
    }

    /// Recreate `<scratch>/work/{src,build,home,tmp,...}` identically from the pristine
    /// snapshot and place identical File API stateless queries into `<staged_bin>`.
    pub(crate) fn reset_for_run(&self, cwd_rel: Option<&BString>) -> Result<PathBuf, String> {
        if self.host.metadata(&self.work, Follow::No).is_ok() {
            self.host
                .remove_dir_all(&self.work)
                .map_err(|err| format!("Could not reset verify work tree: {err}"))?;
        }
        for dir in [
            &self.staged_src,
            &self.staged_bin,
            &self.staged_home,
            &self.staged_tmp,
            &self.staged_xdg_cache,
            &self.staged_xdg_config,
            &self.staged_xdg_data,
            &self.staged_appdata,
            &self.staged_local_appdata,
            &self.staged_ccache,
            &self.staged_sccache,
            &self.staged_rcmake_cache,
        ] {
            self.host
                .create_dir_all(dir)
                .map_err(|err| format!("Could not create {}: {err}", dir.display()))?;
        }
        stage_tree(
            self.host,
            &self.snapshot,
            &self.staged_src,
            None,
            None,
            false,
        )?;

        let query_dir = self.staged_bin.join(".cmake/api/v1/query");
        self.host
            .create_dir_all(&query_dir)
            .map_err(|err| format!("Could not create File API query directory: {err}"))?;
        for query in QUERY_FILES {
            self.host
                .write(&query_dir.join(query), b"", WriteMode::CreateNew)
                .map_err(|err| format!("Could not create File API query {query}: {err}"))?;
        }

        let staged_cwd = match cwd_rel.filter(|r| !r.is_empty()) {
            Some(rel) => {
                validate_source_relative(rel)?;
                let candidate = self.staged_src.join(path::to_native(rel));
                let src_canon = self
                    .host
                    .canonicalize(&self.staged_src)
                    .unwrap_or_else(|_| self.staged_src.clone());
                let cand_canon = self.host.canonicalize(&candidate).map_err(|err| {
                    format!(
                        "Working directory '{}' does not exist inside staged source: {err}",
                        String::from_utf8_lossy(rel)
                    )
                })?;
                if !cand_canon.starts_with(&src_canon) {
                    return Err(format!(
                        "Working directory '{}' escapes the staged source directory.",
                        String::from_utf8_lossy(rel)
                    ));
                }
                if !self
                    .host
                    .metadata(&cand_canon, Follow::Yes)
                    .is_ok_and(|meta| meta.kind == FileKind::Dir)
                {
                    return Err(format!(
                        "Working directory '{}' is not a directory inside the staged source.",
                        String::from_utf8_lossy(rel)
                    ));
                }
                cand_canon
            }
            None => self.staged_src.clone(),
        };
        Ok(staged_cwd)
    }

    /// Build a clean, isolated process environment for one engine run.
    ///
    /// Every supported user-profile, temp, XDG, compiler-cache, and rcmake-cache path
    /// points into freshly created directories under `<scratch>/work`.
    pub(crate) fn child_env(&self, for_rcmake: bool) -> Vec<(BString, BString)> {
        let mut env: Vec<(BString, BString)> = self
            .host
            .env_all()
            .into_iter()
            .filter(|(k, _)| {
                let upper = k.to_ascii_uppercase();
                !upper.starts_with(b"RCMAKE_")
                    && !matches!(
                        upper.as_slice(),
                        b"HOME"
                            | b"USERPROFILE"
                            | b"HOMEDRIVE"
                            | b"HOMEPATH"
                            | b"APPDATA"
                            | b"LOCALAPPDATA"
                            | b"TMPDIR"
                            | b"TEMP"
                            | b"TMP"
                            | b"XDG_CACHE_HOME"
                            | b"XDG_CONFIG_HOME"
                            | b"XDG_DATA_HOME"
                            | b"CMAKE_CONFIG_DIR"
                            | b"CCACHE_DIR"
                            | b"SCCACHE_DIR"
                            | b"PWD"
                            | b"OLDPWD"
                            | b"TZ"
                    )
            })
            .collect();

        let home = path::from_native(&self.staged_home);
        let tmp = path::from_native(&self.staged_tmp);
        let xdg_cache = path::from_native(&self.staged_xdg_cache);
        let xdg_config = path::from_native(&self.staged_xdg_config);
        let xdg_data = path::from_native(&self.staged_xdg_data);
        let appdata = path::from_native(&self.staged_appdata);
        let local_appdata = path::from_native(&self.staged_local_appdata);
        let cmake_config = path::from_native(&self.staged_xdg_config.join("cmake"));
        let ccache = path::from_native(&self.staged_ccache);
        let sccache = path::from_native(&self.staged_sccache);

        env.push((BString::from("HOME"), home.clone()));
        env.push((BString::from("USERPROFILE"), home));
        env.push((BString::from("APPDATA"), appdata));
        env.push((BString::from("LOCALAPPDATA"), local_appdata));
        env.push((BString::from("TMPDIR"), tmp.clone()));
        env.push((BString::from("TEMP"), tmp.clone()));
        env.push((BString::from("TMP"), tmp));
        env.push((BString::from("XDG_CACHE_HOME"), xdg_cache));
        env.push((BString::from("XDG_CONFIG_HOME"), xdg_config));
        env.push((BString::from("XDG_DATA_HOME"), xdg_data));
        env.push((BString::from("CMAKE_CONFIG_DIR"), cmake_config));
        env.push((BString::from("CCACHE_DIR"), ccache));
        env.push((BString::from("SCCACHE_DIR"), sccache));
        env.push((BString::from("TZ"), BString::from("UTC")));
        if for_rcmake {
            env.push((BString::from("RCMAKE_ORACLE_MODE"), BString::from("1")));
            env.push((
                BString::from("RCMAKE_CACHE_DIR"),
                path::from_native(&self.staged_rcmake_cache),
            ));
        }
        env.sort_by(|a, b| a.0.cmp(&b.0));
        env
    }

    /// Explicitly remove the private scratch tree and return any removal error.
    pub(crate) fn cleanup(mut self) -> Result<(), String> {
        self.cleaned = true;
        match self.host.remove_dir_all(&self.root) {
            Ok(()) => Ok(()),
            Err(err) if err.kind == ErrorKind::NotFound => Ok(()),
            Err(err) => Err(format!(
                "Could not remove verify scratch directory {}: {err}",
                self.root.display()
            )),
        }
    }
}

impl Drop for ScratchWorkspace<'_> {
    fn drop(&mut self) {
        if !self.cleaned {
            let _ = self.host.remove_dir_all(&self.root);
        }
    }
}

fn temp_base_dir(host: &dyn Host, source_canon: &Path) -> Result<PathBuf, String> {
    let windows = host.system().sysname.eq_ignore_ascii_case(b"Windows");
    let mut candidates: Vec<(PathBuf, bool)> = Vec::new();
    for key in [b"TMPDIR".as_slice(), b"TEMP", b"TMP"] {
        if let Some(val) = host.env_get(key).filter(|v| !v.is_empty())
            && is_host_absolute(host, &val)
        {
            candidates.push((path::to_native(&val), false));
        }
    }
    if windows {
        if let Some(local) = host.env_get(b"LOCALAPPDATA").filter(|v| !v.is_empty())
            && is_host_absolute(host, &local)
        {
            candidates.push((path::to_native(&local).join("Temp"), true));
        }
        candidates.push((PathBuf::from("C:/Windows/Temp"), true));
    } else {
        candidates.push((PathBuf::from("/tmp"), true));
    }

    let mut saw_inside_source = false;
    for (cand, allow_create) in candidates {
        if allow_create {
            if host.create_dir_all(&cand).is_err() {
                continue;
            }
        } else if !host
            .metadata(&cand, Follow::Yes)
            .is_ok_and(|meta| meta.kind == FileKind::Dir)
        {
            continue;
        }
        let canon = host.canonicalize(&cand).unwrap_or(cand);
        if canon == source_canon || canon.starts_with(source_canon) {
            saw_inside_source = true;
            continue;
        }
        return Ok(canon);
    }

    if saw_inside_source {
        return Err(format!(
            "No temporary scratch directory exists outside source directory {}. Set TMPDIR/TEMP to a directory outside the source tree.",
            source_canon.display()
        ));
    }
    Err("Could not locate a writable temporary directory for verify scratch staging.".into())
}

#[derive(Default)]
struct StageBudget {
    files: usize,
    bytes: u64,
}

struct DeferredSymlink {
    target: PathBuf,
    dst: PathBuf,
}

fn stage_tree(
    host: &dyn Host,
    source_root: &Path,
    dst_root: &Path,
    excluded: Option<&Path>,
    scratch_root: Option<&Path>,
    validate_symlinks: bool,
) -> Result<(), String> {
    let mut budget = StageBudget::default();
    let mut symlinks = Vec::new();
    copy_tree_bounded(
        host,
        source_root,
        source_root,
        dst_root,
        excluded,
        scratch_root,
        0,
        &mut budget,
        validate_symlinks,
        &mut symlinks,
    )?;
    create_deferred_symlinks(host, symlinks)
}

#[allow(clippy::too_many_arguments)]
fn copy_tree_bounded(
    host: &dyn Host,
    source_root: &Path,
    src_dir: &Path,
    dst_dir: &Path,
    excluded: Option<&Path>,
    scratch_root: Option<&Path>,
    depth: usize,
    budget: &mut StageBudget,
    validate_symlinks: bool,
    symlinks: &mut Vec<DeferredSymlink>,
) -> Result<(), String> {
    if depth > MAX_STAGE_DEPTH {
        return Err(format!(
            "Source tree exceeds verify staging depth limit ({MAX_STAGE_DEPTH})."
        ));
    }
    let mut entries = host
        .read_dir(src_dir)
        .map_err(|err| format!("Could not list {}: {err}", src_dir.display()))?;
    let remaining_slots = MAX_STAGE_FILES.saturating_sub(budget.files);
    if entries
        .iter()
        .filter(|e| !e.name.starts_with(b"._"))
        .count()
        > remaining_slots
    {
        return Err(format!(
            "Source tree exceeds verify staging file limit ({MAX_STAGE_FILES} entries)."
        ));
    }
    entries.sort_by(|a, b| a.name.cmp(&b.name));

    for entry in entries {
        if entry.name.starts_with(b"._") {
            continue;
        }
        let child_src = src_dir.join(path::to_native(&entry.name));
        let matches_excluded = |target: &Path| {
            child_src == target
                || host
                    .canonicalize(&child_src)
                    .ok()
                    .is_some_and(|canon| canon == target)
        };
        if excluded.is_some_and(matches_excluded) || scratch_root.is_some_and(matches_excluded) {
            continue;
        }
        budget.files += 1;
        if budget.files > MAX_STAGE_FILES {
            return Err(format!(
                "Source tree exceeds verify staging file limit ({MAX_STAGE_FILES} entries)."
            ));
        }
        let child_dst = dst_dir.join(path::to_native(&entry.name));
        match entry.kind {
            FileKind::Dir => {
                host.create_dir_all(&child_dst)
                    .map_err(|err| format!("Could not create {}: {err}", child_dst.display()))?;
                copy_tree_bounded(
                    host,
                    source_root,
                    &child_src,
                    &child_dst,
                    excluded,
                    scratch_root,
                    depth + 1,
                    budget,
                    validate_symlinks,
                    symlinks,
                )?;
            }
            FileKind::File => {
                copy_file_bounded(host, &child_src, &child_dst, budget)?;
            }
            FileKind::Symlink => {
                let target = host.read_link(&child_src).map_err(|err| {
                    format!("Could not read symlink {}: {err}", child_src.display())
                })?;
                if validate_symlinks {
                    check_internal_symlink(host, source_root, &child_src, &target)?;
                }
                // Defer symlink creation until all directories and regular files exist so
                // Windows directory symlinks resolve their destination kind accurately.
                symlinks.push(DeferredSymlink {
                    target,
                    dst: child_dst,
                });
            }
            FileKind::Other => {
                return Err(format!(
                    "Unsupported special file in source tree: {}",
                    child_src.display()
                ));
            }
        }
    }
    Ok(())
}

/// Stream `src` into `dst` while enforcing both per-file and cumulative byte budgets
/// on the actual bytes read, preventing a concurrently growing file from exceeding limits.
fn copy_file_bounded(
    host: &dyn Host,
    src: &Path,
    dst: &Path,
    budget: &mut StageBudget,
) -> Result<(), String> {
    let pre_meta = host
        .metadata_fresh(src, Follow::No)
        .map_err(|err| format!("Could not stat {}: {err}", src.display()))?;
    if pre_meta.kind != FileKind::File {
        return Err(format!(
            "Source entry {} changed to a non-regular file during staging.",
            src.display()
        ));
    }
    if pre_meta.len > MAX_STAGE_FILE_BYTES {
        return Err(format!(
            "Source file {} exceeds verify staging file limit ({MAX_STAGE_FILE_BYTES} bytes).",
            src.display()
        ));
    }
    if budget.bytes.saturating_add(pre_meta.len) > MAX_STAGE_TOTAL_BYTES {
        return Err(format!(
            "Source tree exceeds verify staging total size limit ({MAX_STAGE_TOTAL_BYTES} bytes)."
        ));
    }

    let (mut reader, open_meta) = host
        .open_read_with_metadata(src)
        .map_err(|err| format!("Could not open {} for staging: {err}", src.display()))?;
    let mode = open_meta.as_ref().map_or(pre_meta.mode, |meta| meta.mode);

    let mut buf = vec![0u8; COPY_CHUNK_BYTES];
    let mut file_bytes = 0u64;
    let mut created = false;
    loop {
        let n = match reader.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(err) if err.kind() == ErrorKind::Interrupted => continue,
            Err(err) => {
                if created {
                    let _ = host.remove_file(dst);
                }
                return Err(format!(
                    "Could not read {} during staging: {err}",
                    src.display()
                ));
            }
        };
        let chunk = buf.get(..n).unwrap_or(&[]);
        file_bytes = file_bytes.saturating_add(n as u64);
        if file_bytes > MAX_STAGE_FILE_BYTES {
            if created {
                let _ = host.remove_file(dst);
            }
            return Err(format!(
                "Source file {} grew beyond verify staging file limit ({MAX_STAGE_FILE_BYTES} bytes).",
                src.display()
            ));
        }
        budget.bytes = budget.bytes.saturating_add(n as u64);
        if budget.bytes > MAX_STAGE_TOTAL_BYTES {
            if created {
                let _ = host.remove_file(dst);
            }
            return Err(format!(
                "Source tree exceeded verify staging total size limit ({MAX_STAGE_TOTAL_BYTES} bytes) while copying {}.",
                src.display()
            ));
        }
        let write_mode = if created {
            WriteMode::Append
        } else {
            WriteMode::CreateNew
        };
        if let Err(err) = host.write(dst, chunk, write_mode) {
            if created {
                let _ = host.remove_file(dst);
            }
            return Err(format!("Could not stage {}: {err}", src.display()));
        }
        created = true;
    }

    if !created {
        host.write(dst, b"", WriteMode::CreateNew)
            .map_err(|err| format!("Could not stage {}: {err}", src.display()))?;
    }
    let _ = host.set_permissions(dst, mode);
    Ok(())
}

fn create_deferred_symlinks(
    host: &dyn Host,
    mut pending: Vec<DeferredSymlink>,
) -> Result<(), String> {
    if pending.is_empty() {
        return Ok(());
    }
    // Resolve chained relative symlinks in dependency order so intermediate targets
    // already exist when Windows chooses directory vs file symlink kind.
    for _ in 0..MAX_STAGE_DEPTH {
        if pending.is_empty() {
            return Ok(());
        }
        let mut next = Vec::new();
        let mut progressed = false;
        for link in pending {
            let resolved = link
                .dst
                .parent()
                .map_or_else(|| link.target.clone(), |p| p.join(&link.target));
            if host.metadata(&resolved, Follow::No).is_ok() {
                host.symlink(&link.target, &link.dst).map_err(|err| {
                    format!("Could not stage symlink {}: {err}", link.dst.display())
                })?;
                progressed = true;
            } else {
                next.push(link);
            }
        }
        pending = next;
        if !progressed {
            break;
        }
    }
    for link in pending {
        host.symlink(&link.target, &link.dst)
            .map_err(|err| format!("Could not stage symlink {}: {err}", link.dst.display()))?;
    }
    Ok(())
}

fn check_internal_symlink(
    host: &dyn Host,
    source_root: &Path,
    link_path: &Path,
    target: &Path,
) -> Result<(), String> {
    if is_host_absolute(host, &path::from_native(target)) || target.is_absolute() {
        return Err(format!(
            "Source symlink {} points to absolute path {} outside the relocatable source tree.",
            link_path.display(),
            target.display()
        ));
    }
    let rel_parent = link_path
        .parent()
        .and_then(|p| p.strip_prefix(source_root).ok())
        .unwrap_or(Path::new(""));
    let mut depth: isize = 0;
    for comp in rel_parent.components().chain(target.components()) {
        match comp {
            Component::Normal(_) => depth += 1,
            Component::ParentDir => {
                depth -= 1;
                if depth < 0 {
                    return Err(format!(
                        "Source symlink {} escapes the source directory via '{}'.",
                        link_path.display(),
                        target.display()
                    ));
                }
            }
            Component::CurDir => {}
            Component::RootDir | Component::Prefix(_) => {
                return Err(format!(
                    "Source symlink {} escapes the source directory.",
                    link_path.display()
                ));
            }
        }
    }
    if let Ok(resolved) = host.canonicalize(link_path)
        && let Ok(root_canon) = host.canonicalize(source_root)
        && !resolved.starts_with(&root_canon)
    {
        return Err(format!(
            "Source symlink {} resolves outside the source directory to {}.",
            link_path.display(),
            resolved.display()
        ));
    }
    Ok(())
}
