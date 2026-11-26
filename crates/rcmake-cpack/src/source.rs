//! Stage source files, validate post-hook staged-tree containment, and apply CMake regular
//! expressions to source paths.

use std::collections::{BTreeMap, BTreeSet};
use std::io::ErrorKind;
use std::path::{Component, Path, PathBuf};

use rcmake_base::BString;
use rcmake_eval::Interp;
use rcmake_host::path::{from_native, from_native_cmake, to_native};
use rcmake_host::{CopyOptions, FileKind, Follow, Host, ReadRange};

use crate::config::{CpackConfig, absolute};
use crate::stage::{
    clear_output_file, ensure_dir, ensure_dir_with_mode, relative_install_path,
    staging_directory_mode, uses_destdir,
};
use crate::{CpackError, InstalledComponent, error};

const MAX_WALK_DEPTH: usize = 256;
const MAX_WALK_ENTRIES: usize = 100_000;
const MAX_SYMLINK_HOPS: usize = 40;

/// Policy governing symbolic links inside a validated payload tree.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SymlinkPolicy {
    /// Preserve payload symlinks whose targets (including chained links) remain strictly
    /// within the validated root.
    PreserveContained,
    /// Reject any symbolic link inside the payload tree (used for formats/targets that do
    /// not preserve POSIX symlinks, or for generated control trees).
    Reject,
}

/// Validate a portable relative payload path before joining it to a root directory.
///
/// Rejects empty paths, NUL bytes, parent traversals (`..`), root/prefix components, and
/// (when `windows_rules` is enabled) Windows alternate data streams, reserved device
/// names, and trailing dots or spaces.
pub(crate) fn validate_relative_path(path: &Path, windows_rules: bool) -> Result<(), CpackError> {
    let raw = from_native(path);
    if raw.is_empty() || raw.contains(&0) {
        return Err(error(format!("Invalid CPack relative path: {raw}")));
    }
    let mut saw_normal = false;
    for part in path.components() {
        match part {
            Component::Normal(segment) => {
                saw_normal = true;
                let bytes = from_native(Path::new(segment));
                if bytes.is_empty()
                    || bytes.contains(&0)
                    || bytes.contains(&b'/')
                    || bytes.contains(&b'\\')
                {
                    return Err(error(format!(
                        "Invalid CPack relative path component: {bytes}"
                    )));
                }
                if windows_rules {
                    if bytes.contains(&b':') {
                        return Err(error(format!(
                            "Invalid Windows stream or drive syntax in CPack path: {raw}"
                        )));
                    }
                    if bytes.ends_with(b".") || bytes.ends_with(b" ") {
                        return Err(error(format!(
                            "Invalid trailing dot or space in Windows CPack path: {raw}"
                        )));
                    }
                    if is_windows_reserved_name(&bytes) {
                        return Err(error(format!(
                            "Reserved Windows device name in CPack path: {raw}"
                        )));
                    }
                }
            }
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(error(format!(
                    "CPack relative path escapes root or is not relative: {raw}"
                )));
            }
        }
    }
    if !saw_normal {
        return Err(error(format!("Empty CPack relative path: {raw}")));
    }
    Ok(())
}

/// Return whether `name` is a reserved Windows device name (with or without extension).
pub(crate) fn is_windows_reserved_name(name: &[u8]) -> bool {
    let stem = name.split(|&b| b == b'.').next().unwrap_or(name);
    let upper = stem.to_ascii_uppercase();
    matches!(
        upper.as_slice(),
        b"CON"
            | b"PRN"
            | b"AUX"
            | b"NUL"
            | b"COM1"
            | b"COM2"
            | b"COM3"
            | b"COM4"
            | b"COM5"
            | b"COM6"
            | b"COM7"
            | b"COM8"
            | b"COM9"
            | b"LPT1"
            | b"LPT2"
            | b"LPT3"
            | b"LPT4"
            | b"LPT5"
            | b"LPT6"
            | b"LPT7"
            | b"LPT8"
            | b"LPT9"
    )
}

/// Verify that `root` is a real directory (no-follow) and return its freshly canonicalized path.
pub(crate) fn canonical_checked_root(host: &dyn Host, root: &Path) -> Result<PathBuf, CpackError> {
    let meta = host.metadata_fresh(root, Follow::No)?;
    if meta.kind != FileKind::Dir {
        return Err(error(format!(
            "Expected a staging directory at {}",
            root.display()
        )));
    }
    Ok(host.canonicalize_fresh(root)?)
}

/// Verify that every parent component from `root` down to `rel` is a real directory
/// inside `canon_root`, and validate the final component against `symlink_policy`.
pub(crate) fn verify_contained_path(
    host: &dyn Host,
    root: &Path,
    canon_root: &Path,
    rel: &Path,
    symlink_policy: SymlinkPolicy,
    windows_rules: bool,
) -> Result<FileKind, CpackError> {
    validate_relative_path(rel, windows_rules)?;
    let mut current = root.to_owned();
    let parts: Vec<_> = rel
        .components()
        .filter_map(|c| match c {
            Component::Normal(s) => Some(s),
            _ => None,
        })
        .collect();
    for (idx, part) in parts.iter().enumerate() {
        current.push(part);
        let is_last = idx + 1 == parts.len();
        let meta = host.metadata_fresh(&current, Follow::No).map_err(|err| {
            error(format!(
                "CPack cannot read the installed component file {}: {err}",
                current.display()
            ))
        })?;
        if !is_last {
            if meta.kind != FileKind::Dir {
                return Err(error(format!(
                    "CPack expected a directory at {}",
                    current.display()
                )));
            }
            let canon = host.canonicalize_fresh(&current)?;
            if !canon.starts_with(canon_root) {
                return Err(error(format!(
                    "CPack staged parent escapes root: {}",
                    current.display()
                )));
            }
            continue;
        }
        match meta.kind {
            FileKind::Symlink => {
                if symlink_policy == SymlinkPolicy::Reject {
                    return Err(error(format!(
                        "Symbolic link is not permitted in staged payload: {}",
                        current.display()
                    )));
                }
                validate_contained_symlink(host, root, canon_root, &current)?;
            }
            FileKind::Dir | FileKind::File => {
                let canon = host.canonicalize_fresh(&current)?;
                if !canon.starts_with(canon_root) {
                    return Err(error(format!(
                        "CPack staged entry escapes root: {}",
                        current.display()
                    )));
                }
            }
            FileKind::Other => {
                return Err(error(format!(
                    "Unsupported special file in CPack staging tree: {}",
                    current.display()
                )));
            }
        }
        return Ok(meta.kind);
    }
    Err(error(format!(
        "Empty CPack relative path: {}",
        rel.display()
    )))
}

/// Validate a symbolic link at `link_path` inside `root` (`canon_root`), following chained
/// links up to [`MAX_SYMLINK_HOPS`] and rejecting absolute targets, parent escapes,
/// symlinked intermediate directories outside `root`, and cycles.
pub(crate) fn validate_contained_symlink(
    host: &dyn Host,
    root: &Path,
    canon_root: &Path,
    link_path: &Path,
) -> Result<PathBuf, CpackError> {
    let mut hops_left = MAX_SYMLINK_HOPS;
    let mut visited = BTreeSet::new();
    validate_contained_symlink_inner(
        host,
        root,
        canon_root,
        link_path,
        &mut hops_left,
        &mut visited,
    )
}

fn validate_contained_symlink_inner(
    host: &dyn Host,
    root: &Path,
    canon_root: &Path,
    link_path: &Path,
    hops_left: &mut usize,
    visited: &mut BTreeSet<PathBuf>,
) -> Result<PathBuf, CpackError> {
    let initial_target = host.read_link(link_path)?;
    let mut current_link = link_path.to_owned();
    let mut first = true;

    loop {
        if *hops_left == 0 {
            return Err(error(format!(
                "CPack payload symbolic link chain exceeds limit: {}",
                link_path.display()
            )));
        }
        *hops_left -= 1;
        if !visited.insert(current_link.clone()) {
            return Err(error(format!(
                "CPack payload symbolic link cycle detected: {}",
                link_path.display()
            )));
        }
        let target = if first {
            first = false;
            initial_target.clone()
        } else {
            host.read_link(&current_link)?
        };
        let raw_target = from_native(&target);
        if raw_target.is_empty()
            || raw_target.contains(&0)
            || target.is_absolute()
            || raw_target.starts_with(b"/")
            || raw_target.starts_with(b"\\")
        {
            return Err(error(format!(
                "CPack payload symbolic link escapes staging root: {} -> {}",
                current_link.display(),
                target.display()
            )));
        }
        let parent = current_link.parent().ok_or_else(|| {
            error(format!(
                "CPack payload symbolic link has no parent: {}",
                current_link.display()
            ))
        })?;
        let rel_parent = parent.strip_prefix(root).map_err(|_| {
            error(format!(
                "CPack payload symbolic link is outside root: {}",
                current_link.display()
            ))
        })?;
        let mut stack: Vec<std::ffi::OsString> = rel_parent
            .components()
            .filter_map(|c| match c {
                Component::Normal(s) => Some(s.to_os_string()),
                _ => None,
            })
            .collect();
        for comp in target.components() {
            match comp {
                Component::CurDir => {}
                Component::Normal(s) => stack.push(s.to_os_string()),
                Component::ParentDir => {
                    if stack.pop().is_none() {
                        return Err(error(format!(
                            "CPack payload symbolic link escapes staging root: {} -> {}",
                            current_link.display(),
                            target.display()
                        )));
                    }
                }
                Component::RootDir | Component::Prefix(_) => {
                    return Err(error(format!(
                        "CPack payload symbolic link uses absolute target: {} -> {}",
                        current_link.display(),
                        target.display()
                    )));
                }
            }
        }
        let mut resolved = root.to_owned();
        let mut next_link: Option<PathBuf> = None;
        for (idx, seg) in stack.iter().enumerate() {
            resolved.push(seg);
            let is_last = idx + 1 == stack.len();
            match host.metadata_fresh(&resolved, Follow::No) {
                Ok(meta) => {
                    if meta.kind == FileKind::Symlink {
                        if !is_last {
                            let rest: PathBuf =
                                stack.get(idx + 1..).unwrap_or(&[]).iter().collect();
                            let mid_target = host.read_link(&resolved)?;
                            let mid_parent = resolved.parent().unwrap_or(root);
                            let mid_rel = mid_parent.strip_prefix(root).map_err(|_| {
                                error(format!(
                                    "CPack symlink intermediate parent outside root: {}",
                                    resolved.display()
                                ))
                            })?;
                            let mut mid_stack: Vec<std::ffi::OsString> = mid_rel
                                .components()
                                .filter_map(|c| match c {
                                    Component::Normal(s) => Some(s.to_os_string()),
                                    _ => None,
                                })
                                .collect();
                            for c in mid_target.components().chain(rest.components()) {
                                match c {
                                    Component::CurDir => {}
                                    Component::Normal(s) => mid_stack.push(s.to_os_string()),
                                    Component::ParentDir => {
                                        if mid_stack.pop().is_none() {
                                            return Err(error(format!(
                                                "CPack payload symbolic link escapes staging root via {}: {}",
                                                resolved.display(),
                                                link_path.display()
                                            )));
                                        }
                                    }
                                    Component::RootDir | Component::Prefix(_) => {
                                        return Err(error(format!(
                                            "CPack payload symbolic link escapes staging root via absolute intermediate link: {}",
                                            resolved.display()
                                        )));
                                    }
                                }
                            }
                            let _ = validate_contained_symlink_inner(
                                host, root, canon_root, &resolved, hops_left, visited,
                            )?;
                            let mut combined = root.to_owned();
                            for part in &mid_stack {
                                combined.push(part);
                            }
                            if let Ok(m) = host.metadata_fresh(&combined, Follow::No) {
                                if m.kind == FileKind::Symlink {
                                    next_link = Some(combined);
                                    break;
                                }
                                let canon = host.canonicalize_fresh(&combined)?;
                                if !canon.starts_with(canon_root) {
                                    return Err(error(format!(
                                        "CPack payload symbolic link escapes staging root: {}",
                                        link_path.display()
                                    )));
                                }
                            }
                            return Ok(initial_target);
                        }
                        next_link = Some(resolved.clone());
                        break;
                    }
                    if !is_last && meta.kind != FileKind::Dir {
                        return Err(error(format!(
                            "CPack payload symbolic link traverses non-directory: {}",
                            resolved.display()
                        )));
                    }
                    let canon = host.canonicalize_fresh(&resolved)?;
                    if !canon.starts_with(canon_root) {
                        return Err(error(format!(
                            "CPack payload symbolic link escapes staging root: {}",
                            link_path.display()
                        )));
                    }
                }
                Err(err) if err.kind == ErrorKind::NotFound => {
                    return Ok(initial_target);
                }
                Err(err) => return Err(err.into()),
            }
        }
        match next_link {
            Some(link) => current_link = link,
            None => return Ok(initial_target),
        }
    }
}

/// Re-validate the staged tree after pre-build hooks run (Audit Finding #9).
///
/// For component installs (`!members.is_empty()`), validates every recorded component
/// root and every recorded member path (including all intermediate parent directories and
/// symlink targets) plus any live tree entries under each component root. For monolithic
/// installs, validates the full `staging_root` tree.
pub(crate) fn validate_staged_tree(
    host: &dyn Host,
    staging_root: &Path,
    members: &[InstalledComponent],
    symlink_policy: SymlinkPolicy,
    windows_rules: bool,
) -> Result<(), CpackError> {
    let canon_staging = canonical_checked_root(host, staging_root)?;
    if members.is_empty() {
        let mut remaining = MAX_WALK_ENTRIES;
        let mut seen_case = BTreeMap::new();
        validate_dir_walk(
            host,
            staging_root,
            &canon_staging,
            staging_root,
            Path::new(""),
            0,
            &mut remaining,
            symlink_policy,
            windows_rules,
            &mut seen_case,
        )?;
        return Ok(());
    }
    for component in members {
        for staged in &component.staged_roots {
            let rel_scanned = staged.scanned.strip_prefix(staging_root).map_err(|_| {
                error(format!(
                    "CPack component staging root escapes staging directory: {}",
                    staged.scanned.display()
                ))
            })?;
            if !rel_scanned.as_os_str().is_empty() {
                verify_contained_path(
                    host,
                    staging_root,
                    &canon_staging,
                    rel_scanned,
                    SymlinkPolicy::Reject,
                    false,
                )?;
            }
            let canon_root = canonical_checked_root(host, &staged.scanned)?;
            if !canon_root.starts_with(&canon_staging) {
                return Err(error(format!(
                    "CPack component root escapes staging directory: {}",
                    staged.scanned.display()
                )));
            }
            for member in &staged.files {
                verify_contained_path(
                    host,
                    &staged.scanned,
                    &canon_root,
                    member,
                    symlink_policy,
                    windows_rules,
                )?;
            }
            let mut remaining = MAX_WALK_ENTRIES;
            let mut seen_case = BTreeMap::new();
            validate_dir_walk(
                host,
                &staged.scanned,
                &canon_root,
                &staged.scanned,
                Path::new(""),
                0,
                &mut remaining,
                symlink_policy,
                windows_rules,
                &mut seen_case,
            )?;
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn validate_dir_walk(
    host: &dyn Host,
    root: &Path,
    canon_root: &Path,
    current: &Path,
    rel: &Path,
    depth: usize,
    remaining: &mut usize,
    symlink_policy: SymlinkPolicy,
    windows_rules: bool,
    seen_case: &mut BTreeMap<BString, PathBuf>,
) -> Result<(), CpackError> {
    if depth >= MAX_WALK_DEPTH {
        return Err(error("CPack staging directory depth exceeds the limit"));
    }
    let meta = host.metadata_fresh(current, Follow::No)?;
    if meta.kind != FileKind::Dir {
        return Err(error(format!(
            "CPack expected a directory at {}",
            current.display()
        )));
    }
    if !host.canonicalize_fresh(current)?.starts_with(canon_root) {
        return Err(error(format!(
            "CPack staging directory escapes root: {}",
            current.display()
        )));
    }
    let mut entries = host.read_dir(current)?;
    entries.sort_unstable_by(|a, b| a.name.cmp(&b.name));
    for entry in entries {
        *remaining = remaining
            .checked_sub(1)
            .ok_or_else(|| error("CPack staging file count exceeds the limit"))?;
        if entry.name.starts_with(b"._") {
            continue;
        }
        let name = to_native(&entry.name);
        let entry_rel = rel.join(&name);
        validate_relative_path(&entry_rel, windows_rules)?;
        if windows_rules {
            let folded = BString::from(from_native_cmake(&entry_rel).to_ascii_lowercase());
            if let Some(prev) = seen_case.insert(folded, entry_rel.clone())
                && prev != entry_rel
            {
                return Err(error(format!(
                    "Case-insensitive path collision in CPack staging tree: {} and {}",
                    prev.display(),
                    entry_rel.display()
                )));
            }
        }
        let entry_path = current.join(&name);
        let fresh = host.metadata_fresh(&entry_path, Follow::No)?;
        match fresh.kind {
            FileKind::Dir => {
                validate_dir_walk(
                    host,
                    root,
                    canon_root,
                    &entry_path,
                    &entry_rel,
                    depth + 1,
                    remaining,
                    symlink_policy,
                    windows_rules,
                    seen_case,
                )?;
            }
            FileKind::File => {
                if !host
                    .canonicalize_fresh(&entry_path)?
                    .starts_with(canon_root)
                {
                    return Err(error(format!(
                        "CPack staged file escapes root: {}",
                        entry_path.display()
                    )));
                }
            }
            FileKind::Symlink => {
                if symlink_policy == SymlinkPolicy::Reject {
                    return Err(error(format!(
                        "Symbolic link is not permitted in staged payload: {}",
                        entry_path.display()
                    )));
                }
                validate_contained_symlink(host, root, canon_root, &entry_path)?;
            }
            FileKind::Other => {
                return Err(error(format!(
                    "Unsupported special file in CPack staging tree: {}",
                    entry_path.display()
                )));
            }
        }
    }
    Ok(())
}

pub(crate) fn copy_directories(
    host: &dyn Host,
    config: &CpackConfig,
    cwd: &Path,
    staging: &Path,
    interp: &mut Interp,
) -> Result<(), CpackError> {
    let directories = config.list(b"CPACK_INSTALLED_DIRECTORIES");
    let (pairs, remainder) = directories.as_chunks::<2>();
    if !remainder.is_empty() {
        return Err(error(
            "CPACK_INSTALLED_DIRECTORIES requires pairs of source and destination directories",
        ));
    }
    if directories.is_empty() {
        return Ok(());
    }
    let patterns = if config.get(b"CPACK_IGNORE_FILES").is_some() {
        config.list(b"CPACK_IGNORE_FILES")
    } else {
        config.list(b"CPACK_SOURCE_IGNORE_FILES")
    };
    let regexes = patterns
        .iter()
        .map(|pattern| {
            interp.cx().regex(pattern).map_err(|err| {
                error(format!(
                    "Invalid CPack ignore expression \"{pattern}\": {err:?}"
                ))
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    let steps_per_path = interp.cx().limits().regex_steps;
    let ignored = |path: &Path, kind: FileKind| -> Result<bool, CpackError> {
        let mut budget = steps_per_path;
        let mut name = from_native_cmake(path);
        if kind == FileKind::Dir {
            name.push(b'/');
        }
        for regex in &regexes {
            if regex
                .find_at(&name, 0, Default::default(), &mut budget)
                .map_err(|err| error(err.to_string()))?
                .is_some()
            {
                return Ok(true);
            }
        }
        Ok(false)
    };
    let staging = host.canonicalize(staging)?;
    let excluded = staging
        .ancestors()
        .find(|path| {
            path.file_name()
                .is_some_and(|name| name == "_CPack_Packages")
        })
        .unwrap_or(&staging);
    // Without DESTDIR staging, directory pairs install below the packaging prefix.
    let mut base = staging.clone();
    if !uses_destdir(config) {
        base.extend(
            relative_install_path(
                config
                    .get(b"CPACK_PACKAGING_INSTALL_PREFIX")
                    .unwrap_or_default(),
            )?
            .iter(),
        );
    }
    let directory_mode = staging_directory_mode(config)?;
    let mut remaining = MAX_WALK_ENTRIES;
    for [src, dest] in pairs {
        let src = host.canonicalize(&absolute(cwd, src))?;
        let dest = base.join(relative_install_path(dest)?);
        ensure_dir_with_mode(host, &staging, &dest, directory_mode)?;
        copy_tree(
            host,
            &src,
            &dest,
            0,
            &mut remaining,
            None,
            None,
            &mut |path, kind| Ok(path.starts_with(excluded) || ignored(path, kind)?),
        )?;
    }
    Ok(())
}

pub(crate) fn merge_component(
    host: &dyn Host,
    source: &Path,
    dest: &Path,
) -> Result<(), CpackError> {
    copy_checked_tree(host, source, dest, SymlinkPolicy::PreserveContained, false)
}

/// Copy a validated tree from `source` into `dest`, checking every parent and payload
/// symlink against `source` and rejecting conflicting existing files in `dest`.
pub(crate) fn copy_checked_tree(
    host: &dyn Host,
    source: &Path,
    dest: &Path,
    symlink_policy: SymlinkPolicy,
    windows_rules: bool,
) -> Result<(), CpackError> {
    let canon_source = canonical_checked_root(host, source)?;
    let mut remaining = MAX_WALK_ENTRIES;
    let mut seen_case = BTreeMap::new();
    validate_dir_walk(
        host,
        source,
        &canon_source,
        source,
        Path::new(""),
        0,
        &mut remaining,
        symlink_policy,
        windows_rules,
        &mut seen_case,
    )?;
    let mut remaining = MAX_WALK_ENTRIES;
    copy_tree(
        host,
        source,
        dest,
        0,
        &mut remaining,
        Some(source),
        Some((source, &canon_source, symlink_policy)),
        &mut |_, _| Ok(false),
    )
}

/// Copy the recorded `members` of the component at `source` into `dest`.
///
/// Validates every parent component and symlink target against `source` before copying.
/// Missing ancestors of a member become plain directories. Identical files and
/// symbolic links that already exist in `dest` are accepted. Other existing entries
/// with the same name cause an error.
pub(crate) fn merge_recorded(
    host: &dyn Host,
    source: &Path,
    members: &BTreeSet<PathBuf>,
    dest: &Path,
) -> Result<(), CpackError> {
    merge_recorded_checked(
        host,
        source,
        members,
        dest,
        SymlinkPolicy::PreserveContained,
        false,
    )
}

/// Checked variant of [`merge_recorded`] enforcing `symlink_policy` and optional Windows
/// path rules across every parent component and payload symlink.
pub(crate) fn merge_recorded_checked(
    host: &dyn Host,
    source: &Path,
    members: &BTreeSet<PathBuf>,
    dest: &Path,
    symlink_policy: SymlinkPolicy,
    windows_rules: bool,
) -> Result<(), CpackError> {
    let canon_source = canonical_checked_root(host, source)?;
    let mut directories = Vec::new();
    let mut seen_case = BTreeMap::new();
    for member in members {
        if windows_rules {
            let folded = BString::from(from_native_cmake(member).to_ascii_lowercase());
            if let Some(prev) = seen_case.insert(folded, member.clone())
                && prev != *member
            {
                return Err(error(format!(
                    "Case-insensitive path collision in CPack component: {} and {}",
                    prev.display(),
                    member.display()
                )));
            }
        }
        let kind = verify_contained_path(
            host,
            source,
            &canon_source,
            member,
            symlink_policy,
            windows_rules,
        )?;
        let input = source.join(member);
        let output = dest.join(member);
        if let Some(parent) = output.parent() {
            ensure_dir(host, dest, parent)?;
        }
        if existing_component_entry(host, source, &input, &output, kind)? {
            continue;
        }
        match kind {
            FileKind::File => {
                clear_output_file(host, &output)?;
                host.copy_file(
                    &input,
                    &output,
                    CopyOptions {
                        preserve_permissions: true,
                        preserve_mtime: true,
                    },
                )?;
            }
            FileKind::Symlink => {
                let target = validate_contained_symlink(host, source, &canon_source, &input)?;
                clear_output_file(host, &output)?;
                host.symlink(&target, &output)?;
            }
            FileKind::Dir => {
                ensure_dir(host, dest, &output)?;
                directories.push((input, output));
            }
            FileKind::Other => {}
        }
    }
    // Children change the times of a directory, so apply directory metadata last.
    for (input, output) in directories.iter().rev() {
        let metadata = host.metadata_fresh(input, Follow::No)?;
        host.set_permissions(output, metadata.mode & 0o7777)?;
        host.set_times(output, Some(metadata.atime), Some(metadata.mtime))?;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn copy_tree(
    host: &dyn Host,
    source: &Path,
    dest: &Path,
    depth: usize,
    remaining: &mut usize,
    merge_root: Option<&Path>,
    containment: Option<(&Path, &Path, SymlinkPolicy)>,
    ignored: &mut impl FnMut(&Path, FileKind) -> Result<bool, CpackError>,
) -> Result<(), CpackError> {
    if depth >= MAX_WALK_DEPTH {
        return Err(error("CPack source directory depth exceeds the limit"));
    }
    if let Some((_, canon_root, _)) = containment {
        let meta = host.metadata_fresh(source, Follow::No)?;
        if meta.kind != FileKind::Dir || !host.canonicalize_fresh(source)?.starts_with(canon_root) {
            return Err(error(format!(
                "CPack source directory escapes root: {}",
                source.display()
            )));
        }
    }
    let mut entries = host.read_dir(source)?;
    entries.sort_unstable_by(|a, b| a.name.cmp(&b.name));
    for entry in entries {
        *remaining = remaining
            .checked_sub(1)
            .ok_or_else(|| error("CPack source file count exceeds the limit"))?;
        let name = to_native(&entry.name);
        let input = source.join(&name);
        if entry.name.starts_with(b"._") || ignored(&input, entry.kind)? {
            continue;
        }
        let kind = if containment.is_some() {
            host.metadata_fresh(&input, Follow::No)?.kind
        } else {
            entry.kind
        };
        let output = dest.join(name);
        if let Some(root) = merge_root
            && existing_component_entry(host, root, &input, &output, kind)?
        {
            continue;
        }
        match kind {
            FileKind::File => {
                if let Some((_, canon_root, _)) = containment
                    && !host.canonicalize_fresh(&input)?.starts_with(canon_root)
                {
                    return Err(error(format!(
                        "CPack staged file escapes root: {}",
                        input.display()
                    )));
                }
                clear_output_file(host, &output)?;
                host.copy_file(
                    &input,
                    &output,
                    CopyOptions {
                        preserve_permissions: true,
                        preserve_mtime: true,
                    },
                )?;
            }
            FileKind::Symlink => {
                let target = if let Some((root, canon_root, policy)) = containment {
                    if policy == SymlinkPolicy::Reject {
                        return Err(error(format!(
                            "Symbolic link is not permitted in staged payload: {}",
                            input.display()
                        )));
                    }
                    validate_contained_symlink(host, root, canon_root, &input)?
                } else {
                    host.read_link(&input)?
                };
                clear_output_file(host, &output)?;
                host.symlink(&target, &output)?;
            }
            FileKind::Dir => {
                ensure_dir(host, dest, &output)?;
                copy_tree(
                    host,
                    &input,
                    &output,
                    depth + 1,
                    remaining,
                    merge_root,
                    containment,
                    ignored,
                )?;
                let metadata = host.metadata_fresh(&input, Follow::No)?;
                host.set_permissions(&output, metadata.mode & 0o7777)?;
                host.set_times(&output, Some(metadata.atime), Some(metadata.mtime))?;
            }
            FileKind::Other => {}
        }
    }
    Ok(())
}

fn existing_component_entry(
    host: &dyn Host,
    root: &Path,
    input: &Path,
    output: &Path,
    kind: FileKind,
) -> Result<bool, CpackError> {
    let existing = match host.metadata_fresh(output, Follow::No) {
        Ok(meta) => meta,
        Err(err) if err.kind == ErrorKind::NotFound => return Ok(false),
        Err(err) => return Err(err.into()),
    };
    if existing.kind == kind {
        match kind {
            FileKind::Dir => return Ok(false),
            FileKind::Symlink if host.read_link(input)? == host.read_link(output)? => {
                return Ok(true);
            }
            FileKind::File => {
                let meta = host.metadata_fresh(input, Follow::No)?;
                if existing.len == meta.len && same_file_contents(host, input, output, meta.len)? {
                    return Ok(true);
                }
            }
            _ => {}
        }
    }
    Err(error(format!(
        "ERROR The data in files with the same filename is different: {}",
        input.strip_prefix(root).unwrap_or(input).display()
    )))
}

fn same_file_contents(
    host: &dyn Host,
    first: &Path,
    second: &Path,
    size: u64,
) -> Result<bool, CpackError> {
    const CHUNK_SIZE: u64 = 1024 * 1024;
    let mut offset = 0;
    while offset < size {
        let len = (size - offset).min(CHUNK_SIZE);
        let range = ReadRange::Slice {
            offset,
            len: Some(len),
        };
        let bytes = host.read(first, range)?;
        if bytes.len() as u64 != len || bytes != host.read(second, range)? {
            return Ok(false);
        }
        offset += len;
    }
    Ok(true)
}
