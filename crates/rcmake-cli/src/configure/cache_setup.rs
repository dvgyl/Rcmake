use std::path::Path;

use rcmake_base::{BString, truth::is_on};
use rcmake_host::{FileKind, Follow, Host};
use rcmake_model::{Cache, CacheType};

use super::{Exit, GENERATOR};
use crate::cache;

pub(super) fn clear(host: &dyn Host, bin: &Path) -> rcmake_host::HostResult<()> {
    let ignore_missing = |result: rcmake_host::HostResult<()>| {
        result.or_else(|err| {
            if err.kind == std::io::ErrorKind::NotFound {
                Ok(())
            } else {
                Err(err)
            }
        })
    };
    ignore_missing(host.remove_file(&bin.join("CMakeCache.txt")))?;
    let files = bin.join("CMakeFiles");
    if host
        .metadata(&files, Follow::No)
        .is_ok_and(|m| m.kind == FileKind::Symlink)
    {
        ignore_missing(host.remove_file(&files))
    } else {
        ignore_missing(host.remove_dir_all(&files))
    }
}

pub(super) fn apply_environment_defaults(host: &dyn Host, cache: &mut Cache, has_cache: bool) {
    for (name, ty) in [
        (b"CMAKE_TOOLCHAIN_FILE".as_slice(), CacheType::Filepath),
        (b"CMAKE_INSTALL_PREFIX", CacheType::Path),
        (b"CMAKE_BUILD_TYPE", CacheType::String),
        (b"CMAKE_EXPORT_COMPILE_COMMANDS", CacheType::Bool),
        (b"CMAKE_COLOR_DIAGNOSTICS", CacheType::Bool),
        (b"CMAKE_CROSSCOMPILING_EMULATOR", CacheType::String),
    ] {
        if !has_cache
            && !cache.contains(name)
            && let Some(value) = host.env_get(name).filter(|v| !v.is_empty())
        {
            let value = if name == b"CMAKE_COLOR_DIAGNOSTICS" {
                BString::from(if is_on(&value) { "ON" } else { "OFF" })
            } else {
                value
            };
            cache::insert(cache, name, value, Some(ty));
        }
        if let Some(entry) = cache.get_mut(name)
            && entry.ty == CacheType::Uninitialized
        {
            entry.ty = ty;
        }
    }
    if cache
        .get(GENERATOR)
        .is_some_and(|g| g.value.windows(4).any(|window| window == b"Make"))
        && !cache.contains(b"CMAKE_COLOR_MAKEFILE")
    {
        cache::insert(
            cache,
            b"CMAKE_COLOR_MAKEFILE",
            "ON".into(),
            Some(CacheType::Bool),
        );
    }
}

pub(super) fn load(host: &dyn Host, bin: &Path) -> Result<Cache, Exit> {
    cache::load(host, bin).map_err(|err| {
        Exit::Message(format!(
            "Could not load cache from {}: {err}",
            bin.display()
        ))
    })
}
