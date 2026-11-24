use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use rcmake_base::{BString, list, truth};
use rcmake_cps::{CpsPackage, naming};
use rcmake_eval::{CmdError, Cx};
use rcmake_host::path;
use rcmake_model::{CacheEntry, CacheType};

use super::super::cps_import;
use super::super::search_paths::{is_file, relative_suffix};
use super::paths::paths_from_vars;
use super::{Bindings, Request, flag, text, version_bindings};

pub(super) struct ConfigSearch<'a> {
    pub(super) name: &'a [u8],
    pub(super) request: &'a Request,
    pub(super) names: Vec<BString>,
    pub(super) configs: Vec<BString>,
    redirect: Option<PathBuf>,
    visited: BTreeSet<(PathBuf, bool)>,
    pub(super) considered: Vec<(BString, BString, Option<BString>)>,
    pub(super) rejected_dependency: Option<cps_import::Rejection>,
    pub(super) cps_names: Vec<BString>,
}

pub(super) struct ConfigMatch {
    pub(super) file: BString,
    version: BString,
    cps: Option<Box<CpsPackage>>,
}

impl<'a> ConfigSearch<'a> {
    pub(super) fn new(name: &'a [u8], request: &'a Request) -> Self {
        let names = if request.names.is_empty() {
            vec![name.into()]
        } else {
            request.names.clone()
        };
        let configs = if request.configs.is_empty() {
            names
                .iter()
                .flat_map(|name| {
                    [
                        text(&[name, b"Config.cmake"]),
                        text(&[&name.to_ascii_lowercase(), b"-config.cmake"]),
                    ]
                })
                .collect()
        } else {
            request.configs.clone()
        };
        let cps_names = if request
            .config_options
            .iter()
            .any(|option| option == b"CONFIGS")
        {
            Vec::new()
        } else {
            names
                .iter()
                .flat_map(|name| {
                    [false, true].into_iter().filter_map(move |lower| {
                        if lower && !name.iter().any(u8::is_ascii_uppercase) {
                            return None;
                        }
                        naming::package_file_name(name, None, lower).ok()
                    })
                })
                .collect()
        };
        Self {
            name,
            request,
            names,
            cps_names,
            configs,
            redirect: None,
            visited: BTreeSet::new(),
            considered: Vec::new(),
            rejected_dependency: None,
        }
    }

    pub(super) fn redirect(&mut self, cx: &Cx<'_>, dir: &Path) -> bool {
        let exists = self.names.iter().any(|name| {
            [
                text(&[name, b"Config.cmake"]),
                text(&[&name.to_ascii_lowercase(), b"-config.cmake"]),
            ]
            .iter()
            .any(|file| is_file(cx, &dir.join(path::to_native(file))))
        });
        if exists {
            self.redirect = Some(dir.to_path_buf());
        }
        exists
    }

    pub(super) fn find(
        &mut self,
        cx: &mut Cx<'_>,
        package_roots: &[BString],
    ) -> Result<Option<ConfigMatch>, CmdError> {
        let dir = cx.var_or_empty(&text(&[self.name, b"_DIR"]));
        let preferred = self
            .redirect
            .clone()
            .or_else(|| (!dir.is_empty() && !truth::is_off(&dir)).then(|| cx.resolve_path(&dir)));
        if let Some(dir) = preferred {
            if self.redirect.is_none()
                && let Some(found) = self.in_directory(cx, &dir, true)?
            {
                return Ok(Some(found));
            }
            if let Some(found) = self.in_directory(cx, &dir, false)? {
                return Ok(Some(found));
            }
        }

        let ignored = paths_from_vars(cx, &[b"CMAKE_IGNORE_PATH", b"CMAKE_SYSTEM_IGNORE_PATH"]);
        let ignored_prefixes = paths_from_vars(
            cx,
            &[
                b"CMAKE_IGNORE_PREFIX_PATH",
                b"CMAKE_SYSTEM_IGNORE_PREFIX_PATH",
            ],
        );
        for prefix in self.prefixes(cx, package_roots) {
            if ignored_prefixes
                .iter()
                .any(|ignore| prefix.starts_with(ignore))
            {
                continue;
            }
            for (dir, cps) in self.layout(cx, &prefix) {
                let suffixes = self
                    .request
                    .suffixes
                    .iter()
                    .map(|suffix| dir.join(relative_suffix(suffix)));
                for dir in std::iter::once(dir.clone()).chain(suffixes) {
                    if !ignored.contains(&dir)
                        && let Some(found) = self.in_directory(cx, &dir, cps)?
                    {
                        return Ok(Some(found));
                    }
                }
            }
        }
        Ok(None)
    }

    fn in_directory(
        &mut self,
        cx: &mut Cx<'_>,
        dir: &Path,
        cps: bool,
    ) -> Result<Option<ConfigMatch>, CmdError> {
        if !self.visited.insert((dir.to_path_buf(), cps)) {
            return Ok(None);
        }
        if cps {
            return self.in_cps_directory(cx, dir);
        }
        for config in &self.configs {
            let file = dir.join(path::to_native(config));
            if !is_file(cx, &file) {
                continue;
            }
            let checked = check_version(cx, self.name, &file, self.request)?;
            let file = if cx.var_is_on(b"CMAKE_FIND_PACKAGE_RESOLVE_SYMLINKS") {
                cx.host().canonicalize(&file).unwrap_or(file)
            } else {
                file
            };
            let file = path::from_native(&file);
            let version = if checked.version.is_empty() {
                "unknown".into()
            } else {
                checked.version.clone()
            };
            self.considered.push((file.clone(), version, None));
            if checked.accepted {
                return Ok(Some(ConfigMatch {
                    file,
                    version: checked.version,
                    cps: None,
                }));
            }
        }
        Ok(None)
    }

    fn in_cps_directory(
        &mut self,
        cx: &mut Cx<'_>,
        dir: &Path,
    ) -> Result<Option<ConfigMatch>, CmdError> {
        let mut visited = BTreeSet::new();
        for filename in &self.cps_names {
            let file = dir.join(path::to_native(filename));
            if !is_file(cx, &file) {
                continue;
            }
            let file = if cx.var_is_on(b"CMAKE_FIND_PACKAGE_RESOLVE_SYMLINKS") {
                cx.host().canonicalize(&file).unwrap_or(file)
            } else {
                let entries = cx.host().read_dir(dir).unwrap_or_default();
                entries
                    .iter()
                    .find(|entry| entry.name == filename.as_slice())
                    .or_else(|| {
                        entries
                            .iter()
                            .find(|entry| entry.name.eq_ignore_ascii_case(filename))
                    })
                    .map_or(file, |entry| dir.join(path::to_native(&entry.name)))
            };
            if !visited.insert(file.clone()) {
                continue;
            }
            let mut found_version = BString::default();
            let result = match cps_import::read_root(cx, &file) {
                Ok(mut package) if package.name == self.name => {
                    found_version = package.version.clone().unwrap_or_default();
                    match cps_import::check_version(&package, self.request) {
                        Ok(()) => cps_import::prepare(cx, &file, &mut package, self.request)?
                            .map(|()| package),
                        Err(reason) => Err(reason.into()),
                    }
                }
                Ok(package) => Err(BString::from(format!(
                    "The file describes the package {:?}, which is not the requested package.",
                    package.name
                ))
                .into()),
                Err(_) => {
                    Err(BString::from("The package description file could not be read.").into())
                }
            };
            let file = path::from_native(&file);
            let considered_version = if found_version.is_empty() {
                "unknown".into()
            } else {
                found_version.clone()
            };
            match result {
                Ok(package) => {
                    self.considered
                        .push((file.clone(), considered_version, None));
                    return Ok(Some(ConfigMatch {
                        file,
                        version: found_version,
                        cps: Some(Box::new(package)),
                    }));
                }
                Err(rejection) => {
                    self.considered.push((
                        file,
                        considered_version,
                        Some(rejection.reason.clone()),
                    ));
                    if !rejection.diagnostics.is_empty() && self.rejected_dependency.is_none() {
                        self.rejected_dependency = Some(rejection);
                    }
                }
            }
        }
        Ok(None)
    }

    pub(super) fn load(
        &self,
        cx: &mut Cx<'_>,
        found_var: &[u8],
        matched: Option<ConfigMatch>,
    ) -> Result<bool, CmdError> {
        let dir = text(&[self.name, b"_DIR"]);
        let config = text(&[self.name, b"_CONFIG"]);
        let message = text(&[self.name, b"_NOT_FOUND_MESSAGE"]);
        let value = matched
            .as_ref()
            .and_then(|found| path::to_native(&found.file).parent().map(path::from_native))
            .unwrap_or_else(|| text(&[&dir, b"-NOTFOUND"]));
        let current = cx.cache().get(&dir).cloned();
        cx.cache().insert(
            dir.clone(),
            CacheEntry {
                value: value.clone(),
                ty: CacheType::Path,
                help: text(&[
                    b"The directory containing a CMake configuration file for ",
                    self.name,
                    b".",
                ]),
                advanced: current.as_ref().is_some_and(|entry| entry.advanced),
                props: current.map_or_else(Default::default, |entry| entry.props),
            },
        );
        if cx.var_normal_only(&dir).is_some() {
            cx.set_var(&dir, value);
        }

        if let Some(found) = matched {
            let mut vars = Bindings::default();
            let prefix = text(&[self.name, b"_VERSION"]);
            for suffix in [b"".as_slice(), b"_MAJOR", b"_MINOR", b"_PATCH", b"_TWEAK"] {
                cx.unset_var(&text(&[&prefix, suffix]));
            }
            if found.version.is_empty() {
                cx.set_var(&text(&[&prefix, b"_COUNT"]), "0".into());
            } else {
                if let Some(package) = &found.cps {
                    cps_import::version_bindings(package, &prefix, &mut vars);
                } else {
                    version_bindings(&prefix, &found.version, &mut vars);
                }
                vars.apply(cx);
            }
            cx.unset_var(found_var);
            cx.unset_var(&message);
            if let Some(package) = found.cps {
                cps_import::import(cx, &package, &found.file)?;
                for (component, entry) in &package.components {
                    let supported = cps_import::target_kind(&entry.ty).is_some();
                    cx.set_var(
                        &text(&[self.name, b"_", component, b"_FOUND"]),
                        flag(supported).into(),
                    );
                }
                for (component, _) in &self.request.components {
                    if !package.components.contains_key(component) {
                        cx.set_var(&text(&[self.name, b"_", component, b"_FOUND"]), "0".into());
                    }
                }
            } else {
                cx.include_file(
                    &path::to_native(&found.file),
                    self.request.include_options(),
                )?;
            }
            if cx.fatal_occurred() {
                return Err(CmdError::Reported);
            }
            let found_package =
                !cx.var_defined(found_var) || !truth::is_off(&cx.var_or_empty(found_var));
            if found_package {
                cx.set_var(&config, found.file);
            } else {
                cx.unset_var(&config);
            }
            cx.set_var(found_var, flag(found_package).into());
        } else {
            cx.unset_var(&config);
            cx.set_var(found_var, "0".into());
            if self
                .considered
                .iter()
                .any(|(_, _, reason)| reason.is_some())
            {
                for (component, _) in &self.request.components {
                    cx.set_var(&text(&[self.name, b"_", component, b"_FOUND"]), "0".into());
                }
            }
        }
        cx.set_var(
            &text(&[self.name, b"_CONSIDERED_CONFIGS"]),
            list::join(self.considered.iter().map(|(path, _, _)| path)),
        );
        cx.set_var(
            &text(&[self.name, b"_CONSIDERED_VERSIONS"]),
            list::join(self.considered.iter().map(|(_, version, _)| version)),
        );
        Ok(cx.var_is_on(found_var))
    }
}

struct VersionCheck {
    accepted: bool,
    version: BString,
}

fn check_version(
    cx: &mut Cx<'_>,
    name: &[u8],
    config: &Path,
    request: &Request,
) -> Result<VersionCheck, CmdError> {
    let config_path = path::from_native(config);
    let stem = config_path.strip_suffix(b".cmake").unwrap_or(&config_path);
    let version_file = [b"-version.cmake".as_slice(), b"Version.cmake"]
        .into_iter()
        .map(|suffix| path::to_native(&text(&[stem, suffix])))
        .find(|file| is_file(cx, file));
    let Some(version_file) = version_file else {
        return Ok(VersionCheck {
            accepted: request.version.is_none(),
            version: BString::default(),
        });
    };

    let mut code = b"block(SCOPE_FOR VARIABLES PROPAGATE PACKAGE_VERSION PACKAGE_VERSION_COMPATIBLE PACKAGE_VERSION_EXACT PACKAGE_VERSION_UNSUITABLE)\nunset(PACKAGE_VERSION)\nunset(PACKAGE_VERSION_COMPATIBLE)\nunset(PACKAGE_VERSION_EXACT)\nunset(PACKAGE_VERSION_UNSUITABLE)\n".to_vec();
    let mut inputs = Bindings::default();
    inputs.add("PACKAGE_FIND_NAME".into(), BString::from(name));
    if let Some(version) = &request.version {
        version.bindings(b"PACKAGE_FIND_VERSION", &mut inputs);
    } else {
        version_bindings(b"PACKAGE_FIND_VERSION", b"", &mut inputs);
        inputs.add("PACKAGE_FIND_VERSION_COMPLETE".into(), "");
    }
    for (key, value) in inputs.0 {
        code.extend_from_slice(b"set(");
        code.extend_from_slice(&key);
        code.push(b' ');
        write_argument(&mut code, &value);
        code.extend_from_slice(b")\n");
    }
    code.extend_from_slice(b"include(");
    write_argument(&mut code, &path::from_native(&version_file));
    code.extend_from_slice(b")\nendblock()\n");

    let mut outputs = Bindings::default();
    for key in [
        "PACKAGE_VERSION",
        "PACKAGE_VERSION_COMPATIBLE",
        "PACKAGE_VERSION_EXACT",
        "PACKAGE_VERSION_UNSUITABLE",
    ] {
        outputs.add(key.into(), "");
    }
    outputs.scoped(cx, |cx| {
        cx.eval_code(&code)?;
        if cx.fatal_occurred() {
            return Err(CmdError::Reported);
        }
        let exact = cx.var_is_on(b"PACKAGE_VERSION_EXACT");
        let compatible = cx.var_is_on(b"PACKAGE_VERSION_COMPATIBLE");
        let accepted = !cx.var_is_on(b"PACKAGE_VERSION_UNSUITABLE")
            && (request.version.is_none() || exact || !request.exact && compatible);
        Ok(VersionCheck {
            accepted,
            version: cx.var_or_empty(b"PACKAGE_VERSION"),
        })
    })
}

fn write_argument(code: &mut Vec<u8>, value: &[u8]) {
    code.push(b'"');
    for &byte in value {
        if matches!(byte, b'"' | b'$' | b'\\') {
            code.push(b'\\');
        }
        code.push(byte);
    }
    code.push(b'"');
}
