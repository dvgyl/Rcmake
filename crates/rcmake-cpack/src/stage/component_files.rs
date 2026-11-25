//! Record the paths that one component install adds to a staging directory.

use std::collections::BTreeSet;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use rcmake_host::path::to_native;
use rcmake_host::{FileKind, Follow, Host};

use crate::{CpackError, error};

const MAX_ENTRIES: usize = 100_000;
const MAX_DEPTH: usize = 256;

/// List the paths below `dir` relative to `dir` without following symbolic links.
///
/// The list contains directories, files, symbolic links, and other entries. It skips
/// AppleDouble (`._*`) entries. A missing directory has no entries.
pub(super) fn snapshot(host: &dyn Host, dir: &Path) -> Result<BTreeSet<PathBuf>, CpackError> {
    let mut paths = BTreeSet::new();
    match host.metadata(dir, Follow::No) {
        Ok(meta) if meta.kind == FileKind::Dir => {}
        Ok(_) => {
            return Err(error(format!(
                "Expected a staging directory at {}",
                dir.display()
            )));
        }
        Err(err) if err.kind == ErrorKind::NotFound => return Ok(paths),
        Err(err) => return Err(err.into()),
    }
    let mut remaining = MAX_ENTRIES;
    walk(host, dir, Path::new(""), 0, &mut remaining, &mut paths)?;
    Ok(paths)
}

fn walk(
    host: &dyn Host,
    dir: &Path,
    relative: &Path,
    depth: usize,
    remaining: &mut usize,
    paths: &mut BTreeSet<PathBuf>,
) -> Result<(), CpackError> {
    if depth >= MAX_DEPTH {
        return Err(error("CPack staging directory depth exceeds the limit"));
    }
    for entry in host.read_dir(dir)? {
        *remaining = remaining
            .checked_sub(1)
            .ok_or_else(|| error("CPack staging file count exceeds the limit"))?;
        if entry.name.starts_with(b"._") {
            continue;
        }
        let name = to_native(&entry.name);
        let path = relative.join(&name);
        if entry.kind == FileKind::Dir {
            walk(host, &dir.join(&name), &path, depth + 1, remaining, paths)?;
        }
        paths.insert(path);
    }
    Ok(())
}
