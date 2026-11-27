//! Private scratch staging, identical path recreation, File API query placement,
//! and clean paired process environments for `rcmake-verify`.

use std::io::ErrorKind;
use std::path::{Component, Path, PathBuf};

use rcmake_base::BString;
use rcmake_buildinfo::QUERY_FILES;
use rcmake_host::{CopyOptions, FileKind, Follow, Host, WriteMode, path};

const MAX_STAGE_FILES: usize = 20_000;
const MAX_STAGE_DEPTH: usize = 64;
const MAX_STAGE_FILE_BYTES: u64 = 64 * 1024 * 1024;
const MAX_STAGE_TOTAL_BYTES: u64 = 256 * 1024 * 1024;

/// Owned private scratch directory that cleans itself up unconditionally on drop.
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
    staged_rcmake_cache: PathBuf,
}

impl<'h> ScratchWorkspace<'h> {
    /// Allocate a private scratch parent and capture a bounded pristine snapshot of `source_dir`.
    pub(crate) fn create(
        host: &'h dyn Host,
        source_dir: &Path,
        excluded_build_dir: Option<&Path>,
    ) -> Result<Self, String> {
        let base = temp_base_dir(host)?;
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
        let snapshot = canonical_root.join("snapshot");
        let work = canonical_root.join("work");
        let staged_src = work.join("src");
        let staged_bin = work.join("build");
        let staged_home = work.join("home");
        let staged_tmp = work.join("tmp");
        let staged_xdg_cache = work.join("xdg-cache");
        let staged_xdg_config = work.join("xdg-config");
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
            staged_rcmake_cache,
        };

        let excluded_canon = excluded_build_dir.and_then(|dir| {
            host.canonicalize(dir)
                .ok()
                .or_else(|| Some(dir.to_path_buf()))
        });
        ws.host
            .create_dir_all(&ws.snapshot)
            .map_err(|err| format!("Could not create snapshot directory: {err}"))?;
        let mut budget = StageBudget::default();
        copy_tree_bounded(
            host,
            source_dir,
            source_dir,
            &ws.snapshot,
            excluded_canon.as_deref(),
            0,
            &mut budget,
            true,
        )?;
        Ok(ws)
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
            &self.staged_rcmake_cache,
        ] {
            self.host
                .create_dir_all(dir)
                .map_err(|err| format!("Could not create {}: {err}", dir.display()))?;
        }
        let mut budget = StageBudget::default();
        copy_tree_bounded(
            self.host,
            &self.snapshot,
            &self.snapshot,
            &self.staged_src,
            None,
            0,
            &mut budget,
            false,
        )?;

        // Place identical File API queries before configure.
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
                let candidate = self.staged_src.join(path::to_native(rel));
                if self
                    .host
                    .metadata(&candidate, Follow::Yes)
                    .is_ok_and(|meta| meta.kind == FileKind::Dir)
                {
                    candidate
                } else {
                    self.staged_src.clone()
                }
            }
            None => self.staged_src.clone(),
        };
        Ok(staged_cwd)
    }

    /// Build a clean, identical process environment for both engines.
    ///
    /// For `rcmake`, sets `RCMAKE_ORACLE_MODE=1` (forcing the compatibility persona)
    /// and isolates `RCMAKE_CACHE_DIR` inside scratch so probe cache state never leaks.
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
        let cmake_config = path::from_native(&self.staged_xdg_config.join("cmake"));

        env.push((BString::from("HOME"), home));
        env.push((BString::from("TMPDIR"), tmp.clone()));
        env.push((BString::from("TEMP"), tmp.clone()));
        env.push((BString::from("TMP"), tmp));
        env.push((BString::from("XDG_CACHE_HOME"), xdg_cache));
        env.push((BString::from("XDG_CONFIG_HOME"), xdg_config));
        env.push((BString::from("CMAKE_CONFIG_DIR"), cmake_config));
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
}

impl Drop for ScratchWorkspace<'_> {
    fn drop(&mut self) {
        let _ = self.host.remove_dir_all(&self.root);
    }
}

fn temp_base_dir(host: &dyn Host) -> Result<PathBuf, String> {
    for key in [b"TMPDIR".as_slice(), b"TEMP", b"TMP"] {
        if let Some(val) = host.env_get(key).filter(|v| !v.is_empty()) {
            let p = path::to_native(&val);
            if p.is_absolute()
                && host
                    .metadata(&p, Follow::Yes)
                    .is_ok_and(|meta| meta.kind == FileKind::Dir)
            {
                return Ok(p);
            }
        }
    }
    let fallback = if host.system().sysname.eq_ignore_ascii_case(b"Windows") {
        PathBuf::from("C:/Windows/Temp")
    } else {
        PathBuf::from("/tmp")
    };
    host.create_dir_all(&fallback).map_err(|err| {
        format!(
            "Could not access temporary directory {}: {err}",
            fallback.display()
        )
    })?;
    Ok(fallback)
}

#[derive(Default)]
struct StageBudget {
    files: usize,
    bytes: u64,
}

#[allow(clippy::too_many_arguments)]
fn copy_tree_bounded(
    host: &dyn Host,
    source_root: &Path,
    src_dir: &Path,
    dst_dir: &Path,
    excluded: Option<&Path>,
    depth: usize,
    budget: &mut StageBudget,
    validate_symlinks: bool,
) -> Result<(), String> {
    if depth > MAX_STAGE_DEPTH {
        return Err(format!(
            "Source tree exceeds verify staging depth limit ({MAX_STAGE_DEPTH})."
        ));
    }
    let mut entries = host
        .read_dir(src_dir)
        .map_err(|err| format!("Could not list {}: {err}", src_dir.display()))?;
    entries.sort_by(|a, b| a.name.cmp(&b.name));

    for entry in entries {
        if entry.name.starts_with(b"._") {
            continue;
        }
        let child_src = src_dir.join(path::to_native(&entry.name));
        if let Some(excl) = excluded
            && (child_src == excl
                || host
                    .canonicalize(&child_src)
                    .ok()
                    .is_some_and(|canon| canon == excl))
        {
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
                    depth + 1,
                    budget,
                    validate_symlinks,
                )?;
            }
            FileKind::File => {
                let meta = host
                    .metadata(&child_src, Follow::No)
                    .map_err(|err| format!("Could not stat {}: {err}", child_src.display()))?;
                if meta.len > MAX_STAGE_FILE_BYTES {
                    return Err(format!(
                        "Source file {} exceeds verify staging file limit ({MAX_STAGE_FILE_BYTES} bytes).",
                        child_src.display()
                    ));
                }
                budget.bytes = budget.bytes.saturating_add(meta.len);
                if budget.bytes > MAX_STAGE_TOTAL_BYTES {
                    return Err(format!(
                        "Source tree exceeds verify staging total size limit ({MAX_STAGE_TOTAL_BYTES} bytes)."
                    ));
                }
                host.copy_file(
                    &child_src,
                    &child_dst,
                    CopyOptions {
                        preserve_permissions: true,
                        preserve_mtime: false,
                    },
                )
                .map_err(|err| format!("Could not stage {}: {err}", child_src.display()))?;
            }
            FileKind::Symlink => {
                let target = host.read_link(&child_src).map_err(|err| {
                    format!("Could not read symlink {}: {err}", child_src.display())
                })?;
                if validate_symlinks {
                    check_internal_symlink(host, source_root, &child_src, &target)?;
                }
                host.symlink(&target, &child_dst).map_err(|err| {
                    format!("Could not stage symlink {}: {err}", child_src.display())
                })?;
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

fn check_internal_symlink(
    host: &dyn Host,
    source_root: &Path,
    link_path: &Path,
    target: &Path,
) -> Result<(), String> {
    if target.is_absolute() {
        return Err(format!(
            "Source symlink {} points to absolute path {} outside the relocatable source tree.",
            link_path.display(),
            target.display()
        ));
    }
    // Lexically check that the relative target does not escape `source_root`.
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
