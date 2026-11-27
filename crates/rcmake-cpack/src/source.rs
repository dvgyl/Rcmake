//! Stage source files, validate post-hook staged-tree containment, and apply CMake regular
//! expressions to source paths.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::ffi::OsString;
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

/// Verify that `root` is still a real directory (no-follow) whose freshly canonicalized path
/// equals the pre-hook `expected_canon` root.
pub(crate) fn verify_expected_root(
    host: &dyn Host,
    root: &Path,
    expected_canon: &Path,
) -> Result<PathBuf, CpackError> {
    let actual = canonical_checked_root(host, root)?;
    if actual != expected_canon {
        return Err(error(format!(
            "CPack directory changed canonical root: {} (expected {}, got {})",
            root.display(),
            expected_canon.display(),
            actual.display()
        )));
    }
    Ok(actual)
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

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum PathStep {
    Normal(OsString),
    Parent,
}

fn parse_relative_steps(target: &Path, link_path: &Path) -> Result<Vec<PathStep>, CpackError> {
    let raw_target = from_native(target);
    if raw_target.is_empty()
        || raw_target.contains(&0)
        || target.is_absolute()
        || raw_target.starts_with(b"/")
        || raw_target.starts_with(b"\\")
    {
        return Err(error(format!(
            "CPack payload symbolic link escapes staging root: {} -> {}",
            link_path.display(),
            target.display()
        )));
    }
    let mut steps = Vec::new();
    for comp in target.components() {
        match comp {
            Component::CurDir => {}
            Component::Normal(seg) => steps.push(PathStep::Normal(seg.to_os_string())),
            Component::ParentDir => steps.push(PathStep::Parent),
            Component::RootDir | Component::Prefix(_) => {
                return Err(error(format!(
                    "CPack payload symbolic link uses absolute target: {} -> {}",
                    link_path.display(),
                    target.display()
                )));
            }
        }
    }
    Ok(steps)
}

/// Validate a symbolic link at `link_path` inside `root` (`canon_root`), resolving each path
/// component and symbolic link in order before applying subsequent `..` traversals. Rejects
/// absolute targets, parent escapes at any step, symlinked parents outside `root`, cycles,
/// and chains exceeding [`MAX_SYMLINK_HOPS`].
pub(crate) fn validate_contained_symlink(
    host: &dyn Host,
    root: &Path,
    canon_root: &Path,
    link_path: &Path,
) -> Result<PathBuf, CpackError> {
    let initial_target = host.read_link(link_path)?;
    let initial_steps = parse_relative_steps(&initial_target, link_path)?;

    let parent = link_path.parent().ok_or_else(|| {
        error(format!(
            "CPack payload symbolic link has no parent: {}",
            link_path.display()
        ))
    })?;
    let rel_parent = parent.strip_prefix(root).map_err(|_| {
        error(format!(
            "CPack payload symbolic link is outside root: {}",
            link_path.display()
        ))
    })?;

    let mut stack: Vec<OsString> = Vec::new();
    let mut parent_check = root.to_owned();
    for comp in rel_parent.components() {
        if let Component::Normal(seg) = comp {
            parent_check.push(seg);
            let meta = host.metadata_fresh(&parent_check, Follow::No)?;
            if meta.kind != FileKind::Dir
                || !host
                    .canonicalize_fresh(&parent_check)?
                    .starts_with(canon_root)
            {
                return Err(error(format!(
                    "CPack payload symbolic link parent escapes staging root: {}",
                    parent_check.display()
                )));
            }
            stack.push(seg.to_os_string());
        }
    }

    let mut pending: VecDeque<PathStep> = initial_steps.into();
    let mut hops_left = MAX_SYMLINK_HOPS.saturating_sub(1);
    let mut seen_states: BTreeSet<(PathBuf, VecDeque<PathStep>)> = BTreeSet::new();
    seen_states.insert((link_path.to_path_buf(), VecDeque::new()));

    while let Some(step) = pending.pop_front() {
        match step {
            PathStep::Parent => {
                if stack.pop().is_none() {
                    return Err(error(format!(
                        "CPack payload symbolic link escapes staging root: {} -> {}",
                        link_path.display(),
                        initial_target.display()
                    )));
                }
            }
            PathStep::Normal(seg) => {
                let mut candidate = root.to_owned();
                for part in &stack {
                    candidate.push(part);
                }
                candidate.push(&seg);
                match host.metadata_fresh(&candidate, Follow::No) {
                    Ok(meta) => match meta.kind {
                        FileKind::Symlink => {
                            if !seen_states.insert((candidate.clone(), pending.clone())) {
                                return Err(error(format!(
                                    "CPack payload symbolic link cycle detected: {}",
                                    link_path.display()
                                )));
                            }
                            if hops_left == 0 {
                                return Err(error(format!(
                                    "CPack payload symbolic link chain exceeds limit: {}",
                                    link_path.display()
                                )));
                            }
                            hops_left -= 1;
                            let next_target = host.read_link(&candidate)?;
                            let next_steps = parse_relative_steps(&next_target, &candidate)?;
                            for next in next_steps.into_iter().rev() {
                                pending.push_front(next);
                            }
                        }
                        FileKind::Dir => {
                            let canon = host.canonicalize_fresh(&candidate)?;
                            if !canon.starts_with(canon_root) {
                                return Err(error(format!(
                                    "CPack payload symbolic link escapes staging root: {}",
                                    link_path.display()
                                )));
                            }
                            stack.push(seg);
                        }
                        FileKind::File => {
                            let canon = host.canonicalize_fresh(&candidate)?;
                            if !canon.starts_with(canon_root) {
                                return Err(error(format!(
                                    "CPack payload symbolic link escapes staging root: {}",
                                    link_path.display()
                                )));
                            }
                            if !pending.is_empty() {
                                return Err(error(format!(
                                    "CPack payload symbolic link traverses non-directory: {}",
                                    candidate.display()
                                )));
                            }
                            stack.push(seg);
                        }
                        FileKind::Other => {
                            return Err(error(format!(
                                "Unsupported special file in CPack staging tree: {}",
                                candidate.display()
                            )));
                        }
                    },
                    Err(err) if err.kind == ErrorKind::NotFound => {
                        stack.push(seg);
                    }
                    Err(err) => return Err(err.into()),
                }
            }
        }
    }

    let mut final_path = root.to_owned();
    for part in &stack {
        final_path.push(part);
    }
    match host.metadata_fresh(&final_path, Follow::No) {
        Ok(_) => {
            let canon = host.canonicalize_fresh(&final_path)?;
            if !canon.starts_with(canon_root) {
                return Err(error(format!(
                    "CPack payload symbolic link escapes staging root: {}",
                    link_path.display()
                )));
            }
        }
        Err(err) if err.kind == ErrorKind::NotFound => {}
        Err(err) => return Err(err.into()),
    }
    Ok(initial_target)
}

/// Re-validate the staged tree after pre-build hooks run against the pre-hook canonical staging
/// root.
///
/// For component installs (`!members.is_empty()`), validates every component's pre-hook root
/// and scanned directory against its captured canonical root and walks the full live tree under
/// each component root. For monolithic installs, validates the full `staging_root` tree.
pub(crate) fn validate_staged_tree(
    host: &dyn Host,
    staging_root: &Path,
    expected_canon_staging: &Path,
    members: &[InstalledComponent],
    symlink_policy: SymlinkPolicy,
    windows_rules: bool,
) -> Result<(), CpackError> {
    let canon_staging = verify_expected_root(host, staging_root, expected_canon_staging)?;
    if members.is_empty() {
        return validate_dir_subtree(
            host,
            staging_root,
            &canon_staging,
            staging_root,
            symlink_policy,
            windows_rules,
        );
    }
    for component in members {
        for staged in &component.staged_roots {
            let rel_root = staged.root.strip_prefix(staging_root).map_err(|_| {
                error(format!(
                    "CPack component staging root escapes staging directory: {}",
                    staged.root.display()
                ))
            })?;
            if !rel_root.as_os_str().is_empty() {
                verify_contained_path(
                    host,
                    staging_root,
                    &canon_staging,
                    rel_root,
                    SymlinkPolicy::Reject,
                    false,
                )?;
            }
            let canon_root = if !staged.canon_root.as_os_str().is_empty() {
                verify_expected_root(host, &staged.root, &staged.canon_root)?
            } else {
                canonical_checked_root(host, &staged.root)?
            };
            if !canon_root.starts_with(&canon_staging) {
                return Err(error(format!(
                    "CPack component root escapes staging directory: {}",
                    staged.root.display()
                )));
            }

            let rel_scanned = staged.scanned.strip_prefix(&staged.root).map_err(|_| {
                error(format!(
                    "CPack component scanned root escapes component directory: {}",
                    staged.scanned.display()
                ))
            })?;
            if !rel_scanned.as_os_str().is_empty() {
                verify_contained_path(
                    host,
                    &staged.root,
                    &canon_root,
                    rel_scanned,
                    SymlinkPolicy::Reject,
                    false,
                )?;
            }
            let canon_scanned = if !staged.canon_scanned.as_os_str().is_empty() {
                verify_expected_root(host, &staged.scanned, &staged.canon_scanned)?
            } else {
                canonical_checked_root(host, &staged.scanned)?
            };
            if !canon_scanned.starts_with(&canon_root) {
                return Err(error(format!(
                    "CPack component scanned root escapes component directory: {}",
                    staged.scanned.display()
                )));
            }

            validate_dir_subtree(
                host,
                &staged.root,
                &canon_root,
                &staged.root,
                symlink_policy,
                windows_rules,
            )?;
        }
    }
    Ok(())
}

/// Validate every directory, file, and symbolic link below `start` against `(root, canon_root)`.
pub(crate) fn validate_dir_subtree(
    host: &dyn Host,
    root: &Path,
    canon_root: &Path,
    start: &Path,
    symlink_policy: SymlinkPolicy,
    windows_rules: bool,
) -> Result<(), CpackError> {
    let mut remaining = MAX_WALK_ENTRIES;
    let mut seen_case = BTreeMap::new();
    validate_dir_walk(
        host,
        root,
        canon_root,
        start,
        Path::new(""),
        0,
        &mut remaining,
        symlink_policy,
        windows_rules,
        &mut seen_case,
    )
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
    copy_checked_tree_with_canon(
        host,
        source,
        &canon_source,
        dest,
        symlink_policy,
        windows_rules,
    )
}

/// Copy a validated tree from `source` into `dest` after verifying `source` still resolves
/// to `expected_canon_source`.
pub(crate) fn copy_checked_tree_expected(
    host: &dyn Host,
    source: &Path,
    expected_canon_source: &Path,
    dest: &Path,
    symlink_policy: SymlinkPolicy,
    windows_rules: bool,
) -> Result<(), CpackError> {
    let canon_source = verify_expected_root(host, source, expected_canon_source)?;
    copy_checked_tree_with_canon(
        host,
        source,
        &canon_source,
        dest,
        symlink_policy,
        windows_rules,
    )
}

fn copy_checked_tree_with_canon(
    host: &dyn Host,
    source: &Path,
    canon_source: &Path,
    dest: &Path,
    symlink_policy: SymlinkPolicy,
    windows_rules: bool,
) -> Result<(), CpackError> {
    validate_dir_subtree(
        host,
        source,
        canon_source,
        source,
        symlink_policy,
        windows_rules,
    )?;
    let mut remaining = MAX_WALK_ENTRIES;
    copy_tree(
        host,
        source,
        dest,
        0,
        &mut remaining,
        Some(source),
        Some((source, canon_source, symlink_policy)),
        &mut |_, _| Ok(false),
    )
}

/// Copy a validated tree from `source` over `dest`. A file or symbolic link of `source`
/// replaces an entry of `dest` at the same relative path, as a later component install
/// into a shared directory does.
pub(crate) fn overlay_checked_tree(
    host: &dyn Host,
    source: &Path,
    dest: &Path,
    symlink_policy: SymlinkPolicy,
    windows_rules: bool,
) -> Result<(), CpackError> {
    let canon_source = canonical_checked_root(host, source)?;
    validate_dir_subtree(
        host,
        source,
        &canon_source,
        source,
        symlink_policy,
        windows_rules,
    )?;
    let mut remaining = MAX_WALK_ENTRIES;
    copy_tree(
        host,
        source,
        dest,
        0,
        &mut remaining,
        None,
        Some((source, &canon_source, symlink_policy)),
        &mut |_, _| Ok(false),
    )
}

/// Validate a payload at `root` after all of its copies and before a tool reads it.
///
/// A merge can complete the target of a symbolic link with an entry from another
/// source root, so a check of each source root alone does not prove that the links
/// of the assembled tree stay inside it.
pub(crate) fn validate_assembled_payload(
    host: &dyn Host,
    root: &Path,
    symlink_policy: SymlinkPolicy,
    windows_rules: bool,
) -> Result<(), CpackError> {
    let canon_root = canonical_checked_root(host, root)?;
    validate_dir_subtree(host, root, &canon_root, root, symlink_policy, windows_rules)
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
    expected_canon_source: &Path,
    members: &BTreeSet<PathBuf>,
    dest: &Path,
) -> Result<(), CpackError> {
    merge_recorded_checked(
        host,
        source,
        expected_canon_source,
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
    expected_canon_source: &Path,
    members: &BTreeSet<PathBuf>,
    dest: &Path,
    symlink_policy: SymlinkPolicy,
    windows_rules: bool,
) -> Result<(), CpackError> {
    let canon_source = verify_expected_root(host, source, expected_canon_source)?;
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
