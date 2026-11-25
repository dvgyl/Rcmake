//! Stage source files and apply CMake regular expressions to source paths.

use std::collections::BTreeSet;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use rcmake_eval::Interp;
use rcmake_host::path::{from_native_cmake, to_native};
use rcmake_host::{CopyOptions, FileKind, Follow, Host, ReadRange};

use crate::config::{CpackConfig, absolute, interpreter};
use crate::stage::{clear_output_file, ensure_dir, relative_install_path, uses_destdir};
use crate::{CpackError, error};

/// Copy the configured source directories and apply `CPACK_SOURCE_IGNORE_FILES`.
///
/// # Errors
///
/// Returns an error if a directory pair, regular expression, or file operation fails.
pub fn stage_sources(
    host: &Arc<dyn Host>,
    config: &CpackConfig,
    staging_dir: &Path,
) -> Result<(), CpackError> {
    let cwd = host.initial_cwd();
    let mut interp = interpreter(host, &cwd, &Default::default(), None)?;
    host.create_dir_all(staging_dir)?;
    copy_directories(host.as_ref(), config, &cwd, staging_dir, &mut interp)
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
    let mut remaining = 100_000;
    for [src, dest] in pairs {
        let src = host.canonicalize(&absolute(cwd, src))?;
        let dest = base.join(relative_install_path(dest)?);
        ensure_dir(host, &staging, &dest)?;
        copy_tree(
            host,
            &src,
            &dest,
            0,
            &mut remaining,
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
    let mut remaining = 100_000;
    copy_tree(
        host,
        source,
        dest,
        0,
        &mut remaining,
        Some(source),
        &mut |_, _| Ok(false),
    )
}

/// Copy the recorded `members` of the component at `source` into `dest`.
///
/// Missing ancestors of a member become plain directories. Identical files and
/// symbolic links that already exist in `dest` are accepted. Other existing entries
/// with the same name cause an error.
pub(crate) fn merge_recorded(
    host: &dyn Host,
    source: &Path,
    members: &BTreeSet<PathBuf>,
    dest: &Path,
) -> Result<(), CpackError> {
    let mut directories = Vec::new();
    for member in members {
        let input = source.join(member);
        let output = dest.join(member);
        let kind = host.metadata(&input, Follow::No)?.kind;
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
                clear_output_file(host, &output)?;
                host.symlink(&host.read_link(&input)?, &output)?;
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
        let metadata = host.metadata(input, Follow::No)?;
        host.set_permissions(output, metadata.mode & 0o7777)?;
        host.set_times(output, Some(metadata.atime), Some(metadata.mtime))?;
    }
    Ok(())
}

fn copy_tree(
    host: &dyn Host,
    source: &Path,
    dest: &Path,
    depth: usize,
    remaining: &mut usize,
    merge_root: Option<&Path>,
    ignored: &mut impl FnMut(&Path, FileKind) -> Result<bool, CpackError>,
) -> Result<(), CpackError> {
    if depth >= 256 {
        return Err(error("CPack source directory depth exceeds the limit"));
    }
    for entry in host.read_dir(source)? {
        *remaining = remaining
            .checked_sub(1)
            .ok_or_else(|| error("CPack source file count exceeds the limit"))?;
        let name = to_native(&entry.name);
        let input = source.join(&name);
        if entry.name.starts_with(b"._") || ignored(&input, entry.kind)? {
            continue;
        }
        let output = dest.join(name);
        if let Some(root) = merge_root
            && existing_component_entry(host, root, &input, &output, entry.kind)?
        {
            continue;
        }
        match entry.kind {
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
                clear_output_file(host, &output)?;
                host.symlink(&host.read_link(&input)?, &output)?;
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
                    ignored,
                )?;
                let metadata = host.metadata(&input, Follow::No)?;
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
    let existing = match host.metadata(output, Follow::No) {
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
                let meta = host.metadata(input, Follow::No)?;
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
