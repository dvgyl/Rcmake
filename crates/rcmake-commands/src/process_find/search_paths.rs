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
    let full = path::from_native(&cx.resolve_path(value));
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
