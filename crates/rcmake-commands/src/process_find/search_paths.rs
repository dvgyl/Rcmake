use std::path::{Component, Path, PathBuf};

use rcmake_base::{BString, list, path_lex::Lexical};
use rcmake_eval::Cx;
use rcmake_host::{FileKind, Follow, Metadata, path};

pub(super) const BUILD_ENV_SEPARATOR: u8 = if cfg!(windows) { b';' } else { b':' };

pub(super) fn cmake_paths(cx: &Cx<'_>, key: &[u8]) -> Vec<BString> {
    list::split(&cx.var_or_empty(key), list::Empty::Drop)
}

pub(super) fn split_env(value: &[u8], separator: u8) -> impl Iterator<Item = &[u8]> {
    value
        .split(move |byte| *byte == separator)
        .filter(|path| !path.is_empty())
}

pub(super) fn env_paths(cx: &Cx<'_>, key: &[u8], separator: u8) -> Vec<BString> {
    cx.host().env_get(key).map_or_else(Vec::new, |value| {
        split_env(&value, separator).map(BString::from).collect()
    })
}

pub(super) fn host_env_paths(cx: &Cx<'_>, key: &[u8]) -> Vec<BString> {
    env_paths(cx, key, host_env_separator(&cx.host().system().sysname))
}

fn host_env_separator(sysname: &[u8]) -> u8 {
    if sysname == b"Windows" { b';' } else { b':' }
}

pub(super) fn setting_enabled(cx: &Cx<'_>, key: &[u8]) -> bool {
    !cx.var_defined(key) || cx.var_is_on(key)
}

pub(super) fn resolved_lexical_path(cx: &Cx<'_>, value: &[u8]) -> PathBuf {
    search_path(cx, &cx.current_source_dir(), value)
}

pub(super) fn search_path(cx: &Cx<'_>, base: &Path, value: &[u8]) -> PathBuf {
    let native = path::to_native(value);
    let resolved = if let Some(rest) = value.strip_prefix(b"~/")
        && let Some(home) = cx.host().env_get(b"HOME").filter(|home| !home.is_empty())
    {
        path::to_native(&home).join(path::to_native(rest))
    } else {
        base.join(native)
    };
    let full = path::from_native_cmake(&resolved);
    path::to_native(&Lexical::new(&full).normalize())
}

pub(super) fn relative_suffix(value: &[u8]) -> PathBuf {
    path::to_native(value)
        .components()
        .filter(|part| matches!(part, Component::Normal(_) | Component::ParentDir))
        .collect()
}

pub(super) fn file_metadata(cx: &Cx<'_>, file: &Path) -> Option<Metadata> {
    cx.host()
        .metadata(file, Follow::Yes)
        .ok()
        .filter(|meta| meta.kind == FileKind::File)
}

pub(super) fn is_file(cx: &Cx<'_>, file: &Path) -> bool {
    file_metadata(cx, file).is_some()
}

pub(super) fn system_prefixes(cx: &Cx<'_>, use_install: bool) -> Vec<BString> {
    const USE_INSTALL_PREFIX: &[u8] = b"CMAKE_FIND_USE_INSTALL_PREFIX";
    let platform_skips_install = cx.var_is_on(b"CMAKE_FIND_NO_INSTALL_PREFIX");
    let install_prefixes = || {
        [b"CMAKE_INSTALL_PREFIX".as_slice(), b"CMAKE_STAGING_PREFIX"]
            .into_iter()
            .flat_map(|key| cmake_paths(cx, key))
    };

    let has_system_prefixes = cx.var_defined(b"CMAKE_SYSTEM_PREFIX_PATH");
    let mut prefixes = if has_system_prefixes {
        cmake_paths(cx, b"CMAKE_SYSTEM_PREFIX_PATH")
    } else {
        let mut defaults = vec!["/usr/local".into(), "/usr".into(), "/".into()];
        if cx.host().system().sysname.as_slice() == b"Darwin" {
            defaults.push("/opt/homebrew".into());
        }
        if use_install && !platform_skips_install {
            defaults.extend(install_prefixes());
        }
        defaults
    };

    if platform_skips_install {
        if use_install && cx.var_defined(USE_INSTALL_PREFIX) {
            prefixes.splice(..0, install_prefixes());
        }
    } else if !use_install && has_system_prefixes {
        remove_install_prefixes(cx, &mut prefixes);
    }
    prefixes
}

fn remove_install_prefixes(cx: &Cx<'_>, prefixes: &mut Vec<BString>) {
    // The platform records the occurrence of each prefix it added.
    let recorded_index = |kind| {
        let count_key = format!("_CMAKE_SYSTEM_PREFIX_PATH_{kind}_PREFIX_COUNT");
        let value_key = format!("_CMAKE_SYSTEM_PREFIX_PATH_{kind}_PREFIX_VALUE");
        let count = cx.var_or_empty(count_key.as_bytes());
        let offset = std::str::from_utf8(&count)
            .ok()?
            .parse::<usize>()
            .ok()?
            .checked_sub(1)?;
        let value = cx.var_or_empty(value_key.as_bytes());
        prefixes
            .iter()
            .enumerate()
            .filter(|(_, prefix)| **prefix == value)
            .nth(offset)
            .map(|(index, _)| index)
    };
    let install = recorded_index("INSTALL");
    let mut staging = recorded_index("STAGING");
    if let Some(index) = staging.filter(|_| staging == install) {
        // If both prefixes have the same value, the platform records the same count.
        staging = prefixes
            .split_at_checked(index)
            .and_then(|(earlier, rest)| {
                let value = rest.first()?;
                earlier.iter().rposition(|prefix| prefix == value)
            });
    }
    let mut indices = [install, staging];
    indices.sort_unstable();
    for index in indices.into_iter().rev().flatten() {
        prefixes.remove(index);
    }
}

#[cfg(test)]
mod tests {
    use super::{BUILD_ENV_SEPARATOR, host_env_separator, split_env};

    #[test]
    fn splits_environment_paths_for_the_selected_platform() {
        let windows = b";C:/one;;D:/two;";
        assert_eq!(
            split_env(windows, host_env_separator(b"Windows")).collect::<Vec<_>>(),
            [b"C:/one", b"D:/two"],
        );
        let unix = b":/one;name::/two:";
        assert_eq!(
            split_env(unix, host_env_separator(b"Darwin")).collect::<Vec<_>>(),
            [b"/one;name".as_slice(), b"/two"],
        );
        if cfg!(windows) {
            assert_ne!(BUILD_ENV_SEPARATOR, host_env_separator(b"PureOS"));
        } else {
            assert_ne!(BUILD_ENV_SEPARATOR, host_env_separator(b"Windows"));
        }
    }
}
