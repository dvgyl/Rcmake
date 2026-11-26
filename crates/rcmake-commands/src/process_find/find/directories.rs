use std::collections::HashSet;
use std::path::{Component, Path, PathBuf};

use rcmake_base::BString;
use rcmake_eval::{CmdError, Cx};
use rcmake_host::path;

use super::super::{
    find_package::PACKAGE_ROOTS,
    search_paths::{BUILD_ENV_SEPARATOR, cmake_paths, env_paths, relative_suffix, system_prefixes},
};
use super::{Kind, Options, use_default};

impl Kind {
    fn paths(self) -> SearchVariables {
        match self {
            Self::File | Self::Path => SearchVariables {
                cmake: b"CMAKE_INCLUDE_PATH",
                system: b"CMAKE_SYSTEM_INCLUDE_PATH",
                env: b"INCLUDE",
                prefix_subdirs: &["include"],
            },
            Self::Program => SearchVariables {
                cmake: b"CMAKE_PROGRAM_PATH",
                system: b"CMAKE_SYSTEM_PROGRAM_PATH",
                env: b"PATH",
                prefix_subdirs: &["bin", "sbin"],
            },
            Self::Library => SearchVariables {
                cmake: b"CMAKE_LIBRARY_PATH",
                system: b"CMAKE_SYSTEM_LIBRARY_PATH",
                env: b"LIB",
                prefix_subdirs: &["lib"],
            },
        }
    }
}

struct SearchVariables {
    cmake: &'static [u8],
    system: &'static [u8],
    env: &'static [u8],
    prefix_subdirs: &'static [&'static str],
}

pub(super) fn directories(
    cx: &Cx<'_>,
    kind: Kind,
    options: &Options,
) -> Result<Vec<PathBuf>, CmdError> {
    let variables = kind.paths();
    let mut paths = Vec::new();
    let framework = !matches!(kind, Kind::Program);

    if use_default(
        cx,
        options.no_defaults || options.no_package_root,
        b"CMAKE_FIND_USE_PACKAGE_ROOT_PATH",
    ) {
        add_prefixes(cx, &mut paths, cmake_paths(cx, PACKAGE_ROOTS), &variables);
    }
    if use_default(
        cx,
        options.no_defaults || options.no_cmake,
        b"CMAKE_FIND_USE_CMAKE_PATH",
    ) {
        add_prefixes(
            cx,
            &mut paths,
            cmake_paths(cx, b"CMAKE_PREFIX_PATH"),
            &variables,
        );
        paths.extend(
            cmake_paths(cx, variables.cmake)
                .iter()
                .map(|dir| cx.resolve_path(dir)),
        );
        if framework {
            add_framework_dirs(cx, &mut paths, cmake_paths(cx, b"CMAKE_FRAMEWORK_PATH"));
        } else if matches!(kind, Kind::Program) {
            add_framework_dirs(cx, &mut paths, cmake_paths(cx, b"CMAKE_APPBUNDLE_PATH"));
        }
    }
    if use_default(
        cx,
        options.no_defaults || options.no_cmake_env,
        b"CMAKE_FIND_USE_CMAKE_ENVIRONMENT_PATH",
    ) {
        add_prefixes(
            cx,
            &mut paths,
            env_paths(cx, b"CMAKE_PREFIX_PATH", BUILD_ENV_SEPARATOR),
            &variables,
        );
        paths.extend(
            env_paths(cx, variables.cmake, BUILD_ENV_SEPARATOR)
                .iter()
                .map(|dir| cx.resolve_path(dir)),
        );
        if framework {
            add_framework_dirs(
                cx,
                &mut paths,
                env_paths(cx, b"CMAKE_FRAMEWORK_PATH", BUILD_ENV_SEPARATOR),
            );
        } else if matches!(kind, Kind::Program) {
            add_framework_dirs(
                cx,
                &mut paths,
                env_paths(cx, b"CMAKE_APPBUNDLE_PATH", BUILD_ENV_SEPARATOR),
            );
        }
    }
    let expand = |inputs| {
        super::super::user_paths::expand(
            cx,
            inputs,
            options.registry_view.as_ref(),
            matches!(kind, Kind::Program),
        )
    };
    if !options.hints.is_empty() {
        paths.extend(
            expand(&options.hints)?
                .iter()
                .map(|dir| cx.resolve_path(dir)),
        );
    }
    if use_default(
        cx,
        options.no_defaults || options.no_system_env,
        b"CMAKE_FIND_USE_SYSTEM_ENVIRONMENT_PATH",
    ) {
        paths.extend(
            env_paths(cx, variables.env, BUILD_ENV_SEPARATOR)
                .iter()
                .map(|dir| cx.resolve_path(dir)),
        );
        if variables.env != b"PATH" {
            paths.extend(
                env_paths(cx, b"PATH", BUILD_ENV_SEPARATOR)
                    .iter()
                    .map(|dir| cx.resolve_path(dir)),
            );
        }
    }
    if use_default(
        cx,
        options.no_defaults || options.no_system,
        b"CMAKE_FIND_USE_CMAKE_SYSTEM_PATH",
    ) {
        add_prefixes(
            cx,
            &mut paths,
            system_prefixes(
                cx,
                use_default(cx, options.no_install, b"CMAKE_FIND_USE_INSTALL_PREFIX"),
            ),
            &variables,
        );
        paths.extend(
            cmake_paths(cx, variables.system)
                .iter()
                .map(|dir| cx.resolve_path(dir)),
        );
        if framework {
            add_framework_dirs(
                cx,
                &mut paths,
                cmake_paths(cx, b"CMAKE_SYSTEM_FRAMEWORK_PATH"),
            );
        } else if matches!(kind, Kind::Program) {
            add_framework_dirs(
                cx,
                &mut paths,
                cmake_paths(cx, b"CMAKE_SYSTEM_APPBUNDLE_PATH"),
            );
        }
    }
    if !options.paths.is_empty() {
        paths.extend(
            expand(&options.paths)?
                .iter()
                .map(|dir| cx.resolve_path(dir)),
        );
    }
    Ok(with_suffixes(paths, &options.suffixes))
}

const MAX_PREALLOC: usize = 4096;

fn add_framework_dirs(cx: &Cx<'_>, paths: &mut Vec<PathBuf>, values: Vec<BString>) {
    if values.is_empty() {
        return;
    }
    let mut home: Option<Option<PathBuf>> = None;
    paths.reserve(values.len().min(MAX_PREALLOC));
    paths.extend(values.iter().map(|value| {
        if let Some(relative) = value.strip_prefix(b"~/") {
            let resolved = home.get_or_insert_with(|| {
                cx.host()
                    .env_get(b"HOME")
                    .filter(|home| !home.is_empty())
                    .map(|home| cx.resolve_path(&home))
            });
            if let Some(home) = resolved {
                return home.join(path::to_native(relative));
            }
        }
        cx.resolve_path(value)
    }));
}

fn add_prefixes(
    cx: &Cx<'_>,
    paths: &mut Vec<PathBuf>,
    prefixes: Vec<BString>,
    variables: &SearchVariables,
) {
    if prefixes.is_empty() {
        return;
    }
    let architecture = cx.var_or_empty(b"CMAKE_LIBRARY_ARCHITECTURE");
    let arch_native = (!architecture.is_empty()).then(|| path::to_native(&architecture));
    paths.reserve(
        prefixes
            .len()
            .saturating_mul(variables.prefix_subdirs.len() + 1)
            .min(MAX_PREALLOC),
    );
    for prefix in prefixes {
        let prefix = cx.resolve_path(&prefix);
        for dir in variables.prefix_subdirs {
            let subdir = prefix.join(dir);
            if let Some(arch) = &arch_native
                && matches!(*dir, "include" | "lib")
            {
                paths.push(subdir.join(arch));
            }
            paths.push(subdir);
        }
        paths.push(prefix);
    }
}

fn with_suffixes(paths: Vec<PathBuf>, suffixes: &[BString]) -> Vec<PathBuf> {
    let suffixes: Vec<_> = suffixes
        .iter()
        .filter(|s| !s.is_empty())
        .map(|s| relative_suffix(s))
        .collect();
    let capacity = paths
        .len()
        .saturating_mul(suffixes.len() + 1)
        .min(MAX_PREALLOC);
    let mut seen = HashSet::with_capacity(capacity);
    let mut dirs = Vec::with_capacity(capacity);
    let mut push = |path: PathBuf| {
        if !seen.contains(&path) {
            seen.insert(path.clone());
            dirs.push(path);
        }
    };
    if suffixes.is_empty() {
        for dir in paths {
            push(normalize_owned(dir));
        }
    } else {
        for dir in paths {
            for suffix in &suffixes {
                push(join_normalized(&dir, suffix));
            }
            push(normalize_owned(dir));
        }
    }
    dirs
}

pub(super) fn strip_trailing_slashes(path: &[u8]) -> &[u8] {
    let mut trimmed = path;
    while trimmed.len() > 1 {
        let is_sep =
            trimmed.last() == Some(&b'/') || (cfg!(windows) && trimmed.last() == Some(&b'\\'));
        if !is_sep {
            break;
        }
        if trimmed.len() == 3
            && trimmed.first().is_some_and(u8::is_ascii_alphabetic)
            && trimmed.get(1) == Some(&b':')
        {
            break;
        }
        if cfg!(windows) && (trimmed == b"//" || trimmed == br"\\") {
            break;
        }
        let Some(next) = trimmed.get(..trimmed.len() - 1) else {
            break;
        };
        trimmed = next;
    }
    trimmed
}

pub(super) fn join_preserving_parent(dir: &Path, child: &Path) -> PathBuf {
    let joined = dir.join(child);
    let raw = joined.as_os_str().as_encoded_bytes();
    if raw.len() <= 1 || !matches!(raw.last(), Some(b'/' | b'\\')) {
        return joined;
    }
    let bytes = path::from_native(&joined);
    let trimmed = strip_trailing_slashes(&bytes);
    if trimmed.len() == bytes.len() {
        joined
    } else {
        path::to_native(trimmed)
    }
}

#[cfg(not(windows))]
fn is_clean(input: &Path) -> bool {
    let bytes = input.as_os_str().as_encoded_bytes();
    if bytes.is_empty() || bytes == b"/" {
        return true;
    }
    if bytes.ends_with(b"/") {
        return false;
    }
    let body = bytes.strip_prefix(b"/").unwrap_or(bytes);
    body.split(|&byte| byte == b'/')
        .all(|segment| !segment.is_empty() && segment != b"." && segment != b"..")
}

#[cfg(windows)]
fn is_clean(_input: &Path) -> bool {
    false
}

pub(super) fn normalize_owned(input: PathBuf) -> PathBuf {
    if is_clean(&input) {
        input
    } else {
        normalize(&input)
    }
}

pub(super) fn join_normalized(dir: &Path, child: &Path) -> PathBuf {
    normalize_owned(dir.join(child))
}

fn normalize(input: &Path) -> PathBuf {
    let mut path = PathBuf::new();
    for component in input.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                path.pop();
            }
            _ => path.push(component),
        }
    }
    path
}
