use std::collections::BTreeSet;
use std::path::{Component, Path, PathBuf};

use rcmake_base::BString;
use rcmake_eval::Cx;
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

pub(super) fn directories(cx: &Cx<'_>, kind: Kind, options: &Options) -> Vec<PathBuf> {
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
