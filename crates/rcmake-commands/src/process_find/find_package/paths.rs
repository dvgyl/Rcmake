use std::path::{Path, PathBuf};

use rcmake_base::{BString, list_ops, truth};
use rcmake_diag::{PolicyId, PolicyStatus};
use rcmake_eval::Cx;
use rcmake_host::{FileKind, Follow, path};

use super::super::search_paths::{cmake_paths, host_env_paths, is_file, resolved_lexical_path};
use super::config::ConfigSearch;
use super::{PACKAGE_ROOTS, text};

impl ConfigSearch<'_> {
    pub(super) fn prefixes(&self, cx: &mut Cx<'_>, package_roots: &[BString]) -> Vec<PathBuf> {
        let request = self.request;
        let mut prefixes = Vec::new();
        if request.use_source(cx, b"PACKAGE_ROOT_PATH") {
            prefixes.extend_from_slice(package_roots);
        }
        let cmake_paths_keys: &[&[u8]] = &[
            b"CMAKE_PREFIX_PATH",
            b"CMAKE_FRAMEWORK_PATH",
            b"CMAKE_APPBUNDLE_PATH",
        ];
        if request.use_source(cx, b"CMAKE_PATH") {
            prefixes.extend(cmake_paths_keys.iter().flat_map(|key| cmake_paths(cx, key)));
        }
        if request.use_source(cx, b"CMAKE_ENVIRONMENT_PATH") {
            prefixes.extend(host_env_paths(cx, &text(&[self.name, b"_DIR"])));
            prefixes.extend(
                cmake_paths_keys
                    .iter()
                    .flat_map(|key| host_env_paths(cx, key)),
            );
        }
        prefixes.extend_from_slice(&request.hints);
        if request.use_source(cx, b"SYSTEM_ENVIRONMENT_PATH") {
            prefixes.extend(host_env_paths(cx, b"PATH").into_iter().map(|value| {
                let dir = path::to_native(&value);
                if (dir.ends_with("bin") || dir.ends_with("sbin"))
                    && let Some(parent) = dir.parent()
                {
                    return path::from_native(parent);
                }
                value
            }));
        }
        if request.use_source(cx, b"PACKAGE_REGISTRY")
            && (cx.var_defined(b"CMAKE_FIND_USE_PACKAGE_REGISTRY")
                || !cx.var_is_on(b"CMAKE_FIND_PACKAGE_NO_PACKAGE_REGISTRY"))
        {
            prefixes.extend(user_registry(cx, self.name));
        }
        if request.use_source(cx, b"CMAKE_SYSTEM_PATH") {
            if cx.var_defined(b"CMAKE_SYSTEM_PREFIX_PATH") {
                let install = cx.var_or_empty(b"CMAKE_INSTALL_PREFIX");
                let staging = cx.var_or_empty(b"CMAKE_STAGING_PREFIX");
                let use_install = request.use_source(cx, b"INSTALL_PREFIX");
                prefixes.extend(
                    cmake_paths(cx, b"CMAKE_SYSTEM_PREFIX_PATH")
                        .into_iter()
                        .filter(|value| use_install || (*value != install && *value != staging)),
                );
            } else {
                prefixes.extend(["/usr/local".into(), "/usr".into(), "/".into()]);
                if cx.host().system().sysname == b"Darwin" {
                    prefixes.push("/opt/homebrew".into());
                }
                if request.use_source(cx, b"INSTALL_PREFIX") {
                    prefixes.extend(cmake_paths(cx, b"CMAKE_INSTALL_PREFIX"));
                    prefixes.extend(cmake_paths(cx, b"CMAKE_STAGING_PREFIX"));
                }
            }
            prefixes.extend(cmake_paths(cx, b"CMAKE_SYSTEM_FRAMEWORK_PATH"));
            prefixes.extend(cmake_paths(cx, b"CMAKE_SYSTEM_APPBUNDLE_PATH"));
        }
        prefixes.extend_from_slice(&request.paths);
        let prefixes = prefixes
            .into_iter()
            .filter(|value| !value.is_empty() && !value.starts_with(b"[HKEY_"))
            .map(|value| resolved_lexical_path(cx, &value))
            .collect();
        request
            .root_mode
            .resolve(cx, b"CMAKE_FIND_ROOT_PATH_MODE_PACKAGE")
            .reroot(cx, prefixes)
    }

    pub(super) fn layout(&self, cx: &mut Cx<'_>, prefix: &Path) -> Vec<(PathBuf, bool)> {
        let mut dirs = Vec::new();
        let framework = cx.var_or_empty(b"CMAKE_FIND_FRAMEWORK");
        let bundle = cx.var_or_empty(b"CMAKE_FIND_APPBUNDLE");
        let apple = cx.host().system().sysname == b"Darwin";
        if framework == b"FIRST" || framework == b"ONLY" || framework.is_empty() && apple {
            self.frameworks(cx, prefix, &mut dirs);
        }
        if bundle == b"FIRST" || bundle == b"ONLY" || bundle.is_empty() && apple {
            self.bundles(cx, prefix, &mut dirs);
        }
        if framework != b"ONLY" && bundle != b"ONLY" {
            if !self.cps_names.is_empty() {
                let packages = self.cps_packages(cx, prefix);
                for package in &packages {
                    dirs.extend(cps_children(cx, package).into_iter().map(|dir| (dir, true)));
                }
                for package in &packages {
                    for version in children(cx, package, |_| true) {
                        dirs.extend(
                            cps_children(cx, &version)
                                .into_iter()
                                .map(|dir| (dir, true)),
                        );
                    }
                }
                self.cps_layout(cx, &cps_children(cx, prefix), &mut dirs);
            }

            let packages = self.children(cx, prefix);
            dirs.push((prefix.to_path_buf(), false));
            dirs.extend(
                cmake_children(cx, prefix)
                    .into_iter()
                    .map(|dir| (dir, false)),
            );
            dirs.extend(packages.iter().cloned().map(|dir| (dir, false)));
            for package in &packages {
                dirs.extend(
                    cmake_children(cx, package)
                        .into_iter()
                        .map(|dir| (dir, false)),
                );
            }
            for package in &packages {
                for cmake in cmake_children(cx, package) {
                    dirs.extend(
                        self.children(cx, &cmake)
                            .into_iter()
                            .map(|dir| (dir, false)),
                    );
                }
            }

            let lib_dirs = library_dirs(cx);
            if !self.cps_names.is_empty() {
                let lib_cps: Vec<_> = lib_dirs
                    .iter()
                    .map(|lib| prefix.join(lib).join("cps"))
                    .collect();
                self.cps_layout(cx, &lib_cps, &mut dirs);
            }
            let mut cmake_dirs = Vec::new();
            self.unix_layout(cx, prefix, &lib_dirs, &mut cmake_dirs);
            for package in packages {
                self.unix_layout(cx, &package, &lib_dirs, &mut cmake_dirs);
            }
            dirs.extend(cmake_dirs.into_iter().map(|dir| (dir, false)));
        }
        if framework == b"LAST" {
            self.frameworks(cx, prefix, &mut dirs);
        }
        if bundle == b"LAST" {
            self.bundles(cx, prefix, &mut dirs);
        }
        dirs
    }

    fn cps_packages(&self, cx: &Cx<'_>, dir: &Path) -> Vec<PathBuf> {
        children(cx, dir, |entry| {
            self.names
                .iter()
                .any(|name| name.eq_ignore_ascii_case(entry))
        })
    }

    fn cps_layout(&self, cx: &Cx<'_>, roots: &[PathBuf], dirs: &mut Vec<(PathBuf, bool)>) {
        let packages: Vec<_> = roots
            .iter()
            .flat_map(|root| self.cps_packages(cx, root))
            .collect();
        dirs.extend(packages.iter().cloned().map(|dir| (dir, true)));
        for package in packages {
            dirs.extend(
                children(cx, &package, |_| true)
                    .into_iter()
                    .map(|dir| (dir, true)),
            );
        }
        dirs.extend(roots.iter().cloned().map(|dir| (dir, true)));
    }

    fn unix_layout(
        &self,
        cx: &Cx<'_>,
        prefix: &Path,
        lib_dirs: &[PathBuf],
        dirs: &mut Vec<PathBuf>,
    ) {
        for lib in lib_dirs {
            let dir = prefix.join(lib);
            for cmake in cmake_children(cx, &dir) {
                dirs.extend(self.children(cx, &cmake));
            }
        }
        for lib in lib_dirs {
            let dir = prefix.join(lib);
            for package in self.children(cx, &dir) {
                dirs.push(package.clone());
                dirs.extend(cmake_children(cx, &package));
            }
        }
    }

    fn children(&self, cx: &Cx<'_>, dir: &Path) -> Vec<PathBuf> {
        let names: Vec<_> = self
            .names
            .iter()
            .map(|name| name.to_ascii_lowercase())
            .collect();
        children(cx, dir, |name| {
            let lowercase = name.to_ascii_lowercase();
            names.iter().any(|name| lowercase.starts_with(name))
        })
    }

    fn frameworks(&self, cx: &Cx<'_>, prefix: &Path, dirs: &mut Vec<(PathBuf, bool)>) {
        let frameworks = self.apple_children(cx, prefix, b".framework");
        for framework in &frameworks {
            if !self.cps_names.is_empty() {
                for version in children(cx, &framework.join("Versions"), |_| true) {
                    dirs.push((version.join("Resources/CPS"), true));
                }
                dirs.push((framework.join("Resources/CPS"), true));
            }
            let root = framework.join("Resources");
            dirs.extend([(root.clone(), false), (root.join("CMake"), false)]);
            for version in children(cx, &framework.join("Versions"), |_| true) {
                let root = version.join("Resources");
                dirs.extend([(root.clone(), false), (root.join("CMake"), false)]);
            }
        }
    }

    fn bundles(&self, cx: &Cx<'_>, prefix: &Path, dirs: &mut Vec<(PathBuf, bool)>) {
        for bundle in self.apple_children(cx, prefix, b".app") {
            let root = bundle.join("Contents/Resources");
            if !self.cps_names.is_empty() {
                dirs.push((root.join("CPS"), true));
            }
            dirs.extend([(root.clone(), false), (root.join("CMake"), false)]);
        }
    }

    fn apple_children(&self, cx: &Cx<'_>, prefix: &Path, suffix: &[u8]) -> Vec<PathBuf> {
        children(cx, prefix, |name| {
            name.strip_suffix(suffix).is_some_and(|stem| {
                self.names
                    .iter()
                    .any(|name| name.eq_ignore_ascii_case(stem))
            })
        })
    }
}

pub(super) fn package_roots(cx: &Cx<'_>, name: &[u8]) -> Vec<BString> {
    let mut roots = Vec::new();
    if cx.policy(PolicyId::new(74)) == PolicyStatus::New {
        let preserved = text(&[name, b"_ROOT"]);
        let uppercase = text(&[&name.to_ascii_uppercase(), b"_ROOT"]);
        let keys = [preserved.as_slice(), uppercase.as_slice()];
        let keys = if preserved != uppercase && cx.policy(PolicyId::new(144)) == PolicyStatus::New {
            &keys[..]
        } else {
            &keys[..1]
        };
        roots.extend(keys.iter().flat_map(|key| cmake_paths(cx, key)));
        roots.extend(keys.iter().flat_map(|key| host_env_paths(cx, key)));
    }
    roots.extend(cmake_paths(cx, PACKAGE_ROOTS));
    roots
}

pub(super) fn paths_from_vars(cx: &Cx<'_>, keys: &[&[u8]]) -> Vec<PathBuf> {
    keys.iter()
        .flat_map(|key| cmake_paths(cx, key))
        .map(|value| resolved_lexical_path(cx, &value))
        .collect()
}

fn user_registry(cx: &Cx<'_>, name: &[u8]) -> Vec<BString> {
    if cx.host().system().sysname == b"Windows" {
        return Vec::new();
    }
    let Some(home) = cx.host().env_get(b"HOME") else {
        return Vec::new();
    };
    let registry = cx
        .resolve_path(&home)
        .join(".cmake/packages")
        .join(path::to_native(name));
    let Ok(entries) = cx.host().read_dir(&registry) else {
        return Vec::new();
    };
    entries
        .into_iter()
        .filter_map(|entry| {
            let file = registry.join(path::to_native(&entry.name));
            let bytes = cx
                .host()
                .read(&file, rcmake_host::ReadRange::All { limit: 4096 })
                .ok()?;
            let line = bytes.split(|byte| *byte == b'\n').next()?.trim_ascii_end();
            let dir = path::to_native(line);
            if !dir.is_absolute() {
                return None;
            }
            if is_file(cx, &dir) {
                return dir.parent().map(path::from_native);
            }
            Some(line.into())
        })
        .collect()
}

fn children(cx: &Cx<'_>, dir: &Path, matches: impl Fn(&[u8]) -> bool) -> Vec<PathBuf> {
    let mut entries = cx.host().read_dir(dir).unwrap_or_default();
    entries.retain(|entry| matches(&entry.name));
    let order = cx.var_or_empty(b"CMAKE_FIND_PACKAGE_SORT_ORDER");
    if order != b"NONE" {
        let ascending = cx.var_or_empty(b"CMAKE_FIND_PACKAGE_SORT_DIRECTION") == b"ASC";
        entries.sort_by(|a, b| {
            let order = if order == b"NAME" {
                a.name.cmp(&b.name)
            } else {
                list_ops::natural_cmp(&a.name, &b.name, false)
            };
            if ascending { order } else { order.reverse() }
        });
    }
    entries
        .into_iter()
        .filter_map(|entry| {
            let child = dir.join(path::to_native(&entry.name));
            let directory = entry.kind == FileKind::Dir
                || cx
                    .host()
                    .metadata(&child, Follow::Yes)
                    .is_ok_and(|meta| meta.kind == FileKind::Dir);
            directory.then_some(child)
        })
        .collect()
}

fn cps_children(cx: &Cx<'_>, dir: &Path) -> Vec<PathBuf> {
    children(cx, dir, |name| name.eq_ignore_ascii_case(b"cps"))
}

fn cmake_children(cx: &Cx<'_>, dir: &Path) -> Vec<PathBuf> {
    children(cx, dir, |name| name.eq_ignore_ascii_case(b"cmake"))
}

fn library_dirs(cx: &mut Cx<'_>) -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    let arch = cx.var_or_empty(b"CMAKE_LIBRARY_ARCHITECTURE");
    if !arch.is_empty() {
        dirs.push(Path::new("lib").join(path::to_native(&arch)));
    }
    let pointer_size = cx.var_or_empty(b"CMAKE_SIZEOF_VOID_P");
    let x32 = cx.var_is_on(b"CMAKE_INTERNAL_PLATFORM_ABI_X32");
    let props = &cx.global().props;
    for (enabled, prop, dir) in [
        (
            pointer_size == b"8",
            b"FIND_LIBRARY_USE_LIB64_PATHS".as_slice(),
            "lib64",
        ),
        (
            pointer_size == b"4",
            b"FIND_LIBRARY_USE_LIB32_PATHS",
            "lib32",
        ),
        (x32, b"FIND_LIBRARY_USE_LIBX32_PATHS", "libx32"),
    ] {
        if enabled && props.get(prop).is_some_and(|value| truth::is_on(value)) {
            dirs.push(dir.into());
        }
    }
    dirs.extend(["lib".into(), "share".into()]);
    dirs
}
