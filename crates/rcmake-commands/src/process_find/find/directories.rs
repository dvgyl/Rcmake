use std::collections::BTreeSet;
use std::path::{Component, Path, PathBuf};

use rcmake_base::BString;
use rcmake_eval::Cx;
use rcmake_host::path;

use super::super::{
    find_package::PACKAGE_ROOTS,
    search_paths::{BUILD_ENV_SEPARATOR, cmake_paths, env_paths, relative_suffix},
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

pub(super) fn directories(cx: &Cx<'_>, kind: Kind, options: &Options) -> Vec<PathBuf> {
    let variables = kind.paths();
    let mut paths = Vec::new();
    let library = matches!(kind, Kind::Library);

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
        if library {
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
        if library {
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
    paths.extend(options.hints.iter().map(|dir| cx.resolve_path(dir)));
    if use_default(
        cx,
        options.no_defaults || options.no_system_env,
        b"CMAKE_FIND_USE_SYSTEM_ENVIRONMENT_PATH",
    ) {
        for key in [variables.env, b"PATH"] {
            paths.extend(
                env_paths(cx, key, BUILD_ENV_SEPARATOR)
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
        add_prefixes(cx, &mut paths, system_prefixes(cx, options), &variables);
        paths.extend(
            cmake_paths(cx, variables.system)
                .iter()
                .map(|dir| cx.resolve_path(dir)),
        );
        if library {
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
    paths.extend(options.paths.iter().map(|dir| cx.resolve_path(dir)));
    with_suffixes(paths, &options.suffixes)
}

fn add_framework_dirs(cx: &Cx<'_>, paths: &mut Vec<PathBuf>, values: Vec<BString>) {
    paths.extend(values.iter().map(|value| {
        if let Some(relative) = value.strip_prefix(b"~/")
            && let Some(home) = cx.host().env_get(b"HOME").filter(|home| !home.is_empty())
        {
            return cx.resolve_path(&home).join(path::to_native(relative));
        }
        cx.resolve_path(value)
    }));
}

fn system_prefixes(cx: &Cx<'_>, options: &Options) -> Vec<BString> {
    const USE_INSTALL_PREFIX: &[u8] = b"CMAKE_FIND_USE_INSTALL_PREFIX";
    let platform_skips_install = cx.var_is_on(b"CMAKE_FIND_NO_INSTALL_PREFIX");
    let use_install = use_default(cx, options.no_install, USE_INSTALL_PREFIX);
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

fn add_prefixes(
    cx: &Cx<'_>,
    paths: &mut Vec<PathBuf>,
    prefixes: Vec<BString>,
    variables: &SearchVariables,
) {
    let architecture = cx.var_or_empty(b"CMAKE_LIBRARY_ARCHITECTURE");
    for prefix in prefixes {
        let prefix = cx.resolve_path(&prefix);
        for dir in variables.prefix_subdirs {
            let subdir = prefix.join(dir);
            if !architecture.is_empty() && matches!(*dir, "include" | "lib") {
                paths.push(subdir.join(path::to_native(&architecture)));
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
    let mut seen = BTreeSet::new();
    let mut dirs = Vec::new();
    for dir in paths {
        for suffix in &suffixes {
            let path = normalize(&dir.join(suffix));
            if seen.insert(path.clone()) {
                dirs.push(path);
            }
        }
        let path = normalize(&dir);
        if seen.insert(path.clone()) {
            dirs.push(path);
        }
    }
    dirs
}

pub(super) fn normalize(input: &Path) -> PathBuf {
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
