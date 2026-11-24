use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use rcmake_base::{BString, list, list_ops, truth};
use rcmake_diag::{PolicyId, PolicyStatus};
use rcmake_eval::{Cx, RunMode};
use rcmake_host::{FileKind, Follow, path};

use super::super::RootMode;
use super::super::search_paths::{
    cmake_paths, host_env_paths, is_file, resolved_lexical_path, search_path, split_env,
    system_prefixes,
};
use super::config::ConfigSearch;
use super::{PACKAGE_ROOT_SPELLINGS, text, trace::SearchTrace};

const FRAMEWORK_VARS_HEADING: &[u8] =
    b"CMAKE_FRAMEWORK_PATH and CMAKE_APPBUNDLE_PATH variables [CMAKE_FIND_USE_CMAKE_PATH].";
const PACKAGE_ROOT_HEADING: &[u8] =
    b"<PackageName>_ROOT CMake variable [CMAKE_FIND_USE_PACKAGE_ROOT_PATH].";

impl ConfigSearch<'_> {
    pub(super) fn prefixes(
        &self,
        cx: &mut Cx<'_>,
        package_roots: &[BString],
        mode: RootMode,
        trace: &mut SearchTrace,
    ) -> Vec<(PathBuf, Option<PathBuf>)> {
        let request = self.request;
        let mut groups = Vec::new();
        groups.push(vec![(
            BString::from("The internally managed CMAKE_FIND_PACKAGE_REDIRECTS_DIR."),
            redirects_directory(cx)
                .iter()
                .map(|dir| path::from_native(dir))
                .collect(),
        )]);
        if request.use_source(cx, b"PACKAGE_ROOT_PATH") {
            groups.push(vec![(PACKAGE_ROOT_HEADING.into(), package_roots.to_vec())]);
        }
        let apple_paths = [b"CMAKE_FRAMEWORK_PATH".as_slice(), b"CMAKE_APPBUNDLE_PATH"];
        if request.use_source(cx, b"CMAKE_PATH") {
            groups.push(vec![
                (
                    "CMAKE_PREFIX_PATH variable [CMAKE_FIND_USE_CMAKE_PATH].".into(),
                    cmake_paths(cx, b"CMAKE_PREFIX_PATH"),
                ),
                (
                    FRAMEWORK_VARS_HEADING.into(),
                    apple_paths
                        .iter()
                        .flat_map(|key| cmake_paths(cx, key))
                        .collect(),
                ),
            ]);
        }
        if request.use_source(cx, b"CMAKE_ENVIRONMENT_PATH") {
            let dir = text(&[self.name, b"_DIR"]);
            groups.push(vec![
                (
                    text(&[
                        b"Env variable ",
                        &dir,
                        b" [CMAKE_FIND_USE_CMAKE_ENVIRONMENT_PATH].",
                    ]),
                    host_env_paths(cx, &dir),
                ),
                (
                    "CMAKE_PREFIX_PATH env variable [CMAKE_FIND_USE_CMAKE_ENVIRONMENT_PATH]."
                        .into(),
                    host_env_paths(cx, b"CMAKE_PREFIX_PATH"),
                ),
                (
                    "CMAKE_FRAMEWORK_PATH and CMAKE_APPBUNDLE_PATH env variables [CMAKE_FIND_USE_CMAKE_ENVIRONMENT_PATH].".into(),
                    apple_paths.iter().flat_map(|key| host_env_paths(cx, key)).collect(),
                ),
            ]);
        }
        groups.push(vec![(
            "Paths specified by the find_package HINTS option.".into(),
            request.hints.clone(),
        )]);
        if request.use_source(cx, b"SYSTEM_ENVIRONMENT_PATH") {
            let paths = host_env_paths(cx, b"PATH").into_iter().map(|value| {
                let dir = path::to_native(&value);
                if (dir.ends_with("bin") || dir.ends_with("sbin"))
                    && let Some(parent) = dir.parent()
                {
                    return path::from_native(parent);
                }
                value
            });
            groups.push(vec![(
                "Standard system environment variables [CMAKE_FIND_USE_SYSTEM_ENVIRONMENT_PATH]."
                    .into(),
                paths.collect(),
            )]);
        }
        if request.use_source(cx, b"PACKAGE_REGISTRY")
            && (cx.var_defined(b"CMAKE_FIND_USE_PACKAGE_REGISTRY")
                || !cx.var_is_on(b"CMAKE_FIND_PACKAGE_NO_PACKAGE_REGISTRY"))
        {
            groups.push(vec![(
                "CMake User Package Registry [CMAKE_FIND_USE_PACKAGE_REGISTRY].".into(),
                user_registry(cx, self.name),
            )]);
        }
        if request.use_source(cx, b"CMAKE_SYSTEM_PATH") {
            groups.push(vec![(
                "CMake variables defined in the Platform file [CMAKE_FIND_USE_CMAKE_SYSTEM_PATH]."
                    .into(),
                self.system_prefixes(cx),
            )]);
        }
        if request.use_source(cx, b"SYSTEM_PACKAGE_REGISTRY")
            && (cx.var_defined(b"CMAKE_FIND_USE_SYSTEM_PACKAGE_REGISTRY")
                || !cx.var_is_on(b"CMAKE_FIND_PACKAGE_NO_SYSTEM_PACKAGE_REGISTRY"))
        {
            groups.push(vec![(
                "CMake System Package Registry [CMAKE_FIND_PACKAGE_NO_SYSTEM_PACKAGE_REGISTRY]."
                    .into(),
                Vec::new(),
            )]);
        }
        groups.push(vec![(
            "Paths specified by the find_package PATHS option.".into(),
            request.paths.clone(),
        )]);

        let mut seen = BTreeSet::new();
        let mut spellings = BTreeMap::new();
        let mut prefixes = Vec::new();
        for group in groups {
            let mut group_has_paths = false;
            for (heading, values) in group {
                let paths: Vec<_> = values
                    .into_iter()
                    .filter(|value| !value.is_empty() && !value.starts_with(b"[HKEY_"))
                    .filter_map(|value| {
                        let path = if heading == PACKAGE_ROOT_HEADING {
                            resolve_package_root(cx, &value)
                        } else {
                            resolved_lexical_path(cx, &value)
                        };
                        if !seen.insert(path.clone()) {
                            return None;
                        }
                        if heading == PACKAGE_ROOT_HEADING {
                            spellings.insert(path.clone(), path::to_native(&value));
                        }
                        Some(path)
                    })
                    .collect();
                if paths.is_empty() && group_has_paths {
                    trace.heading(&heading);
                } else {
                    trace.section(
                        &heading,
                        paths.iter().map(|path| spellings.get(path).unwrap_or(path)),
                    );
                }
                group_has_paths |= !paths.is_empty();
                prefixes.extend(paths);
            }
        }
        if !matches!(mode, RootMode::Never) {
            let roots = [
                b"CMAKE_FIND_ROOT_PATH".as_slice(),
                b"CMAKE_SYSROOT_COMPILE",
                b"CMAKE_SYSROOT_LINK",
                b"CMAKE_SYSROOT",
            ]
            .map(|key| (key, paths_from_vars(cx, &[key])));
            if roots.iter().any(|(_, paths)| !paths.is_empty()) {
                trace.heading(b"Prepending the following roots to each prefix:");
                for (key, paths) in roots {
                    trace.section(key, &paths);
                }
            }
        }
        mode.reroot(cx, prefixes)
            .into_iter()
            .map(|path| {
                let spelling = spellings.get(&path).cloned();
                (path, spelling)
            })
            .collect()
    }

    fn system_prefixes(&self, cx: &Cx<'_>) -> Vec<BString> {
        let use_install = self.request.use_source(cx, b"INSTALL_PREFIX");
        let mut prefixes = system_prefixes(cx, use_install);
        prefixes.extend(cmake_paths(cx, b"CMAKE_SYSTEM_FRAMEWORK_PATH"));
        prefixes.extend(cmake_paths(cx, b"CMAKE_SYSTEM_APPBUNDLE_PATH"));
        prefixes
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
                dirs.extend(cmake_children(cx, &package));
                dirs.push(package);
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

pub(super) fn redirects_directory(cx: &Cx<'_>) -> Option<PathBuf> {
    let configured = cx.var_or_empty(b"CMAKE_FIND_PACKAGE_REDIRECTS_DIR");
    if !configured.is_empty() {
        Some(resolved_lexical_path(cx, &configured))
    } else {
        (cx.mode() == RunMode::Configure).then(|| {
            let binary_dir = cx.var_or_empty(b"CMAKE_BINARY_DIR");
            cx.resolve_path(&binary_dir).join("CMakeFiles/pkgRedirects")
        })
    }
}

pub(super) fn package_roots(cx: &mut Cx<'_>, name: &[u8]) -> Vec<BString> {
    let mut roots = Vec::new();
    let preserved = text(&[name, b"_ROOT"]);
    let uppercase = text(&[&name.to_ascii_uppercase(), b"_ROOT"]);
    let policy = PolicyId::new(74);
    let values = |cx: &Cx<'_>, key: &[u8]| {
        [
            Some(cx.var_or_empty(key)).filter(|value| !value.is_empty()),
            cx.host().env_get(key).filter(|value| !value.is_empty()),
        ]
    };
    let [preserved_var, preserved_env] = values(cx, &preserved);
    match cx.policy(policy) {
        PolicyStatus::New => {
            let upper_policy = PolicyId::new(144);
            let [upper_var, upper_env] = if preserved == uppercase {
                [None, None]
            } else {
                let [var, env] = values(cx, &uppercase);
                [
                    var.filter(|value| Some(value) != preserved_var.as_ref()),
                    env.filter(|value| Some(value) != preserved_env.as_ref()),
                ]
            };
            let upper_status = cx.policy(upper_policy);
            let [upper_var, upper_env] = if upper_status == PolicyStatus::New {
                [upper_var, upper_env]
            } else {
                if upper_status == PolicyStatus::Warn {
                    warn_ignored_root(
                        cx,
                        upper_policy,
                        &uppercase,
                        [upper_var, upper_env],
                        b"For compatibility, find_package is ignoring the variable, but code in a .cmake module might still use it.",
                    );
                }
                [None, None]
            };
            for value in [preserved_var, upper_var].into_iter().flatten() {
                roots.extend(list::split(&value, list::Empty::Drop));
            }
            let separator = if cx.host().system().sysname == b"Windows" {
                b';'
            } else {
                b':'
            };
            for value in [preserved_env, upper_env].into_iter().flatten() {
                roots.extend(split_env(&value, separator).map(BString::from));
            }
        }
        PolicyStatus::Warn => warn_ignored_root(
            cx,
            policy,
            &preserved,
            [preserved_var, preserved_env],
            b"For compatibility, CMake is ignoring the variable.",
        ),
        PolicyStatus::Old => {}
    }
    roots.extend(cmake_paths(cx, PACKAGE_ROOT_SPELLINGS));
    roots
}

pub(super) fn resolve_package_root(cx: &Cx<'_>, root: &[u8]) -> PathBuf {
    search_path(cx, &cx.host().initial_cwd(), root)
}

fn warn_ignored_root(
    cx: &mut Cx<'_>,
    policy: PolicyId,
    key: &[u8],
    values: [Option<BString>; 2],
    explanation: &[u8],
) {
    let warned = text(&[
        b"_RCMAKE_FIND_PACKAGE_WARNED_",
        policy.to_string().as_bytes(),
        b"_",
        key,
    ]);
    if cx.global().props.get(&warned).is_some() {
        return;
    }
    let mut body = BString::default();
    for (source, value) in [b"CMake".as_slice(), b"Environment"]
        .into_iter()
        .zip(values)
    {
        if let Some(value) = value {
            body.extend_from_slice(&text(&[
                b"\n",
                source,
                b" variable ",
                key,
                b" is set to:\n\n  ",
                &value,
                b"\n",
            ]));
        }
    }
    if !body.is_empty() {
        body.push(b'\n');
        body.extend_from_slice(explanation);
        cx.global().props.set(warned, "1".into());
        crate::policy_misc::issue_warning(cx, policy, b"", &body);
    }
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
