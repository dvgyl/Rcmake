use std::path::Path;

use rcmake_base::{BString, path_lex::Lexical, truth::is_on};
use rcmake_host::{
    FileKind, Follow, Host, Stream,
    path::{from_native_cmake, to_native},
};
use rcmake_model::{Cache, CacheType};

use super::{Exit, GENERATOR, HOME_DIR, matches_glob};
use crate::{cache, options::CacheEdit};

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
        (b"CMAKE_DISABLE_PRECOMPILE_HEADERS", CacheType::Bool),
    ] {
        if (!has_cache || name == b"CMAKE_DISABLE_PRECOMPILE_HEADERS")
            && !cache.contains(name)
            && let Some(value) = host.env_get(name).filter(|v| !v.is_empty())
        {
            let value = if name == b"CMAKE_COLOR_DIAGNOSTICS" {
                BString::from(if is_on(&value) { "ON" } else { "OFF" })
            } else {
                value
            };
            cache::insert(cache, name, value, Some(ty));
            if name == b"CMAKE_DISABLE_PRECOMPILE_HEADERS"
                && let Some(entry) = cache.get_mut(name)
            {
                entry.help = "Default value for DISABLE_PRECOMPILE_HEADERS of targets.".into();
            }
        }
        if let Some(entry) = cache.get_mut(name) {
            if entry.ty == CacheType::Uninitialized {
                entry.ty = ty;
            }
            if name == b"CMAKE_TOOLCHAIN_FILE" {
                let path = Lexical::new(&entry.value);
                if path.absolute() {
                    entry.value = path.normalize();
                }
            }
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

const TOOLCHAIN_FILE: &[u8] = b"CMAKE_TOOLCHAIN_FILE";

pub(super) fn refresh_toolchain(
    host: &dyn Host,
    source: &Path,
    build: &Path,
    edits: &[CacheEdit],
    cache: &mut Cache,
) -> Result<(), Exit> {
    if !cache.contains(HOME_DIR) {
        return Ok(());
    }
    let selection = edits.iter().rev().find_map(|edit| match edit {
        CacheEdit::Define(definition) if definition.name == TOOLCHAIN_FILE => {
            Some(Some(definition.value.as_slice()))
        }
        CacheEdit::Remove(pattern) if matches_glob(pattern, TOOLCHAIN_FILE) => Some(Some(b"")),
        CacheEdit::Preload(_) => Some(None),
        _ => None,
    });
    let Some(Some(selected)) = selection else {
        return Ok(());
    };
    let previous = cache
        .get(TOOLCHAIN_FILE)
        .map_or(b"".as_slice(), |entry| entry.value.as_slice());
    let resolve = |value: &[u8]| {
        if value.is_empty() {
            return BString::default();
        }
        let path = to_native(value);
        let absolute = if path.is_absolute() {
            path
        } else {
            let relative = build.join(&path);
            if super::is_file(host, &relative) {
                relative
            } else {
                source.join(path)
            }
        };
        let resolved = host.canonicalize(&absolute).unwrap_or(absolute);
        Lexical::new(&from_native_cmake(&resolved)).normalize()
    };
    let selected = resolve(selected);
    if selected == resolve(previous) {
        return Ok(());
    }
    clear(host, build).map_err(|error| {
        Exit::Message(format!(
            "Could not remove the cache in {}: {error}",
            build.display()
        ))
    })?;
    let environment = cache
        .get(SYSTEM_ENVIRONMENT_ID)
        .map(|entry| entry.value.clone());
    *cache = Cache::default();
    if let Some(environment) = environment {
        cache::insert(
            cache,
            SYSTEM_ENVIRONMENT_ID,
            environment,
            Some(CacheType::Internal),
        );
    }
    host.write_out(
        Stream::Stderr,
        format!(
            "You have changed variables that require your cache to be deleted.\nConfigure will be re-run and you may have to reset some variables.\nThe following variables have changed:\nCMAKE_TOOLCHAIN_FILE= {selected}\n\n"
        ).as_bytes(),
    );
    Ok(())
}

const SYSTEM_ENVIRONMENT_ID: &[u8] = b"CMAKE_SYSTEM_ENVIRONMENT_ID";

pub(super) fn apply_environment_identity(
    host: &dyn Host,
    bin: &Path,
    cache: &mut Cache,
) -> Result<(), Exit> {
    let current = host.env_get(SYSTEM_ENVIRONMENT_ID).unwrap_or_default();
    if let Some(previous) = cache.get(SYSTEM_ENVIRONMENT_ID) {
        let previous = &previous.value;
        if *previous == current {
            return Ok(());
        }
        let action = host
            .env_get(b"CMAKE_SYSTEM_ENVIRONMENT_ACTION")
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| "WARN".into());
        let advice = match action.as_slice() {
            b"IGNORE" => return Ok(()),
            b"WARN" => {
                "The configure results are probably outdated.  Consider running cmake with --fresh, removing the CMakeCache.txt file and CMakeFiles directory, or choosing a different binary directory."
            }
            b"REFRESH" => "The cache will be refreshed automatically.",
            _ => {
                let message = format!("Unsupported CMAKE_SYSTEM_ENVIRONMENT_ACTION '{action}'");
                let message = rcmake_diag::format_paragraphs(message.as_bytes(), 2, 77);
                host.write_out(
                    Stream::Stderr,
                    format!("CMake Error:\n{message}\n\n").as_bytes(),
                );
                return Err(Exit::Code(1));
            }
        };
        let warning = format!(
            "CMAKE_SYSTEM_ENVIRONMENT_ID: {current}\n\nDoes not match the previous value: {previous}\n\n{advice}"
        );
        let message = rcmake_diag::format_paragraphs(warning.as_bytes(), 2, 77);
        host.write_out(
            rcmake_host::Stream::Stderr,
            format!("CMake Warning:\n{message}\n\n\n").as_bytes(),
        );
        if action == b"WARN" {
            return Ok(());
        }
        clear(host, bin).map_err(|error| {
            Exit::Message(format!(
                "Could not remove the cache in {}: {error}",
                bin.display()
            ))
        })?;
        *cache = Cache::default();
    }
    cache::insert(
        cache,
        SYSTEM_ENVIRONMENT_ID,
        current,
        Some(CacheType::Internal),
    );
    Ok(())
}

/// Cache entry that records the project file name (`cmake --project-file`).
pub(super) const LIST_FILE_NAME: &[u8] = b"CMAKE_LIST_FILE_NAME";

/// Select the project file name from `--project-file` or the existing cache.
///
/// A different name than the cached name is an error. A name other than
/// `CMakeLists.txt` gives the development-only warning.
pub(super) fn select_list_file(
    host: &dyn Host,
    given: Option<&BString>,
    saved: &Cache,
) -> Result<BString, Exit> {
    let previous = saved.get(LIST_FILE_NAME).map(|entry| &entry.value);
    if let (Some(given), Some(previous)) = (given, previous)
        && given != previous
    {
        return Err(Exit::Message(format!(
            "CMakeLists filename : \"{}\"\nDoes not match the previous: \"{}\"\nEither remove the CMakeCache.txt file and CMakeFiles directory or choose a different binary directory.",
            String::from_utf8_lossy(given),
            String::from_utf8_lossy(previous)
        )));
    }
    let name = given
        .or(previous)
        .cloned()
        .unwrap_or_else(|| rcmake_eval::DEFAULT_LIST_FILE_NAME.into());
    if name.as_slice() != rcmake_eval::DEFAULT_LIST_FILE_NAME.as_bytes() {
        crate::options::warn(
            host,
            "This project has been configured with a project file other than\n  CMakeLists.txt.  This feature is intended for temporary use during\n  development and not for publication of a final product.",
        );
    }
    Ok(name)
}

#[cfg(test)]
mod tests {
    use rcmake_host::PureHost;

    use super::*;

    #[test]
    fn refresh_uses_the_new_system_environment_identity() -> rcmake_host::HostResult<()> {
        let work = Path::new(if cfg!(windows) { "C:/work" } else { "/work" });
        let host = PureHost::new(work.into(), work.join("cmake"));
        let build = work.join("build");
        host.create_dir_all(&build.join("CMakeFiles"))?;
        host.seed_file(&build.join("CMakeCache.txt"), b"cache")?;
        let mut cache = Cache::default();
        cache::insert(
            &mut cache,
            SYSTEM_ENVIRONMENT_ID,
            "original".into(),
            Some(CacheType::Internal),
        );
        cache::insert(
            &mut cache,
            b"SAMPLE",
            "saved".into(),
            Some(CacheType::String),
        );
        host.env_set(SYSTEM_ENVIRONMENT_ID, Some(b"changed"));

        assert!(apply_environment_identity(&host, &build, &mut cache).is_ok());
        assert!(cache.contains(b"SAMPLE"));
        assert!(
            cache
                .get(SYSTEM_ENVIRONMENT_ID)
                .is_some_and(|entry| entry.value == "original")
        );
        host.env_set(b"CMAKE_SYSTEM_ENVIRONMENT_ACTION", Some(b"invalid"));
        assert!(matches!(
            apply_environment_identity(&host, &build, &mut cache),
            Err(Exit::Code(1))
        ));
        assert!(cache.contains(b"SAMPLE"));
        assert!(host.stream_bytes(Stream::Stderr).ends_with(
            b"CMake Error:\n  Unsupported CMAKE_SYSTEM_ENVIRONMENT_ACTION 'invalid'\n\n\n"
        ));
        host.env_set(b"CMAKE_SYSTEM_ENVIRONMENT_ACTION", Some(b"REFRESH"));
        assert!(apply_environment_identity(&host, &build, &mut cache).is_ok());
        assert!(!cache.contains(b"SAMPLE"));
        assert!(
            cache
                .get(SYSTEM_ENVIRONMENT_ID)
                .is_some_and(|entry| entry.value == "changed")
        );
        assert!(
            host.metadata(&build.join("CMakeCache.txt"), Follow::No)
                .is_err()
        );
        Ok(())
    }
}
