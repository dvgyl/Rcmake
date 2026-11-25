use rcmake_base::{BString, list};
use rcmake_eval::{Cx, RunMode};

use super::super::search_paths::{cmake_paths, host_env_paths};
use super::super::{RootMode, text};
use super::{Request, config::ConfigSearch, trace::SearchTrace};

const ROOT_COUNTS: &[u8] = b"\0RCMAKE_FIND_PACKAGE_ROOT_COUNTS";

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Mode {
    Any,
    Module,
    Config,
    Cps,
    Provider,
}

impl Mode {
    fn label(self) -> &'static [u8] {
        match self {
            Self::Any => b"none?",
            Self::Module => b"module",
            Self::Config => b"config",
            Self::Cps => b"cps",
            Self::Provider => b"provider",
        }
    }
}

pub(super) enum Reason {
    NoExist,
    Ignored,
    InsufficientVersion,
    InsufficientComponents,
    Error,
    NotFound,
}

impl Reason {
    fn label(&self) -> &'static [u8] {
        match self {
            Self::NoExist => b"no_exist",
            Self::Ignored => b"ignored",
            Self::InsufficientVersion => b"insufficient_version",
            Self::InsufficientComponents => b"insufficient_components",
            Self::Error => b"error",
            Self::NotFound => b"not_found",
        }
    }
}

struct Candidate {
    path: BString,
    mode: Mode,
    reason: Reason,
    message: BString,
}

pub(super) struct SearchReport {
    pub(super) trace: SearchTrace,
    pub(super) log: SearchLog,
}

pub(super) struct SearchLog {
    debug: bool,
    enabled: bool,
    previous: Option<bool>,
    names: Option<Vec<BString>>,
    configs: Vec<(BString, &'static [u8])>,
    candidates: Vec<Candidate>,
    selected: Option<(BString, Mode, BString)>,
    root_mode: RootMode,
    paths: Vec<(&'static [u8], bool)>,
    global: bool,
    use_modules: bool,
    outer_root_counts: BString,
}

impl SearchLog {
    pub(super) fn new(cx: &mut Cx<'_>, name: &[u8], found_var: &[u8], request: &Request) -> Self {
        let debug = cx.find_package_debug(name);
        let enabled = cx.mode() == RunMode::Configure
            && (debug || !cx.var_is_on(b"CMAKE_FIND_DEBUG_MODE_NO_IMPLICIT_CONFIGURE_LOG"));
        let cache = if request.module {
            None
        } else {
            cx.cache()
                .get(&text(&[name, b"_DIR"]))
                .map(|entry| entry.value != b"NOTFOUND" && !entry.value.ends_with(b"-NOTFOUND"))
        };
        let previous = if cache == Some(true) || !request.config_options.is_empty() {
            cache
        } else {
            cx.var(found_var)
                .map(|value| rcmake_base::truth::is_on(&value))
                .or(cache)
        };
        let paths = request.log_paths(cx);
        Self {
            debug,
            enabled,
            previous,
            names: None,
            configs: Vec::new(),
            candidates: Vec::new(),
            selected: None,
            root_mode: request
                .root_mode
                .resolve(cx, b"CMAKE_FIND_ROOT_PATH_MODE_PACKAGE"),
            paths,
            global: request.global || cx.var_is_on(b"CMAKE_FIND_PACKAGE_TARGETS_GLOBAL"),
            use_modules: request.config_options.is_empty(),
            outer_root_counts: cx.var_or_empty(ROOT_COUNTS),
        }
    }

    pub(super) fn config_search(&mut self, config: &ConfigSearch<'_>, redirected: bool) {
        if !self.enabled {
            return;
        }
        self.use_modules &= !redirected;
        self.names = Some(config.names.clone());
        self.configs = config
            .descriptions
            .iter()
            .map(|(file, mode)| {
                (
                    file.clone(),
                    if *mode == Mode::Cps {
                        b"cps".as_slice()
                    } else {
                        b"cmake"
                    },
                )
            })
            .collect();
    }

    pub(super) fn candidate(
        &mut self,
        path: impl FnOnce() -> BString,
        mode: Mode,
        reason: Reason,
        message: BString,
    ) {
        if self.enabled {
            self.candidates.push(Candidate {
                path: path(),
                mode,
                reason,
                message,
            });
        }
    }

    pub(super) fn selected(&mut self, path: BString, mode: Mode, version: BString) {
        if self.enabled {
            self.selected = Some((path, mode, version));
        }
    }

    pub(super) fn emit(self, cx: &mut Cx<'_>, name: &[u8], request: &Request, roots: &[BString]) {
        // The transition tracks whether a file was selected, even if its script sets FOUND false.
        let selected = self.selected.is_some();
        let implicit = !request.module
            && self.previous != Some(selected)
            && !self
                .selected
                .as_ref()
                .is_some_and(|(_, mode, _)| *mode == Mode::Module)
            && self.candidates.iter().any(|candidate| {
                matches!(candidate.mode, Mode::Config | Mode::Cps | Mode::Provider)
            });
        if !self.enabled || !(self.debug || implicit) {
            return;
        }

        let suffixes: Vec<_> = std::iter::once(BString::default())
            .chain(request.log_suffixes.iter().cloned())
            .collect();
        let mut remaining = roots;
        let mut package_stack = Vec::new();
        for count in list::split(&self.outer_root_counts, list::Empty::Drop) {
            let Some(count) = std::str::from_utf8(&count)
                .ok()
                .and_then(|value| value.parse().ok())
            else {
                break;
            };
            let Some((group, rest)) = remaining.split_at_checked(count) else {
                break;
            };
            if !group.is_empty() {
                package_stack.push(group);
            }
            remaining = rest;
        }

        cx.log_configure_find_package(|cx, event| {
            event.name = name;
            event
                .components
                .extend(request.components.iter().map(|(component, required)| {
                    (
                        component.as_slice(),
                        *required,
                        cx.var_is_on(&text(&[name, b"_", component, b"_FOUND"])),
                    )
                }));
            event.configs.extend(
                self.configs
                    .iter()
                    .map(|(file, kind)| (file.as_slice(), *kind)),
            );
            if let Some(version) = &request.version {
                event.version.extend([
                    (b"version".as_slice(), version.min.as_slice()),
                    (b"version_complete", version.full.as_slice()),
                ]);
                if version.max.is_some() {
                    event.version.extend([
                        (b"min".as_slice(), b"INCLUDE".as_slice()),
                        (
                            b"max",
                            if version.exclusive {
                                b"EXCLUDE"
                            } else {
                                b"INCLUDE"
                            },
                        ),
                    ]);
                }
            }
            event.exact = request.exact;
            event.required = match request.required_by.as_deref().map(Vec::as_slice) {
                Some(b"CMAKE_FIND_REQUIRED") => b"required_from_find_variable",
                Some(_) => b"required_from_package_variable",
                None if request.required => b"required_explicit",
                None if request.optional => b"optional_explicit",
                None => b"optional",
            };
            event.flags.extend([
                (b"quiet".as_slice(), request.quiet),
                (b"global", self.global),
                (b"policy_scope", !request.no_policy_scope),
                (b"bypass_provider", request.bypass_provider),
            ]);
            event.lists.extend([
                (b"hints".as_slice(), request.log_hints.as_slice()),
                (b"names", self.names.as_deref().unwrap_or(&request.names)),
                (b"search_paths", request.log_paths_args.as_slice()),
                (b"path_suffixes", suffixes.as_slice()),
            ]);
            event.registry_view = request.registry_view.as_ref().map(|view| view.as_slice());
            event.paths = self.paths.clone();
            event.root_mode = match self.root_mode {
                RootMode::Default | RootMode::Both => b"BOTH",
                RootMode::Never => b"NEVER",
                RootMode::Only => b"ONLY",
            };
            event
                .candidates
                .extend(self.candidates.iter().map(|candidate| {
                    (
                        candidate.path.as_slice(),
                        candidate.mode.label(),
                        candidate.reason.label(),
                        candidate.message.as_slice(),
                    )
                }));
            event.found = self
                .selected
                .as_ref()
                .map(|(path, mode, version)| (path.as_slice(), mode.label(), version.as_slice()));
            event.package_stack = package_stack;
            for key in [
                b"CMAKE_PACKAGE_PATH".as_slice(),
                b"CMAKE_PREFIX_PATH",
                b"CMAKE_FRAMEWORK_PATH",
            ] {
                event.context_list(key, cmake_paths(cx, key));
            }
            for key in [
                b"CMAKE_PACKAGE_PATH".as_slice(),
                b"CMAKE_PREFIX_PATH",
                b"CMAKE_FRAMEWORK_PATH",
                b"PATH",
            ] {
                event.context_list(&text(&[b"ENV{", key, b"}"]), host_env_paths(cx, key));
            }
            for key in [b"CMAKE_INSTALL_PREFIX".as_slice(), b"CMAKE_STAGING_PREFIX"] {
                if let Some(value) = cx.var(key) {
                    event.context_string(key, value);
                }
            }
            for key in [
                b"CMAKE_SYSTEM_PREFIX_PATH".as_slice(),
                b"CMAKE_SYSTEM_PACKAGE_PATH",
            ] {
                event.context_list(key, cmake_paths(cx, key));
            }
            for key in [
                b"CMAKE_SYSROOT".as_slice(),
                b"CMAKE_SYSROOT_COMPILE",
                b"CMAKE_SYSROOT_LINK",
                b"CMAKE_FIND_ROOT_PATH",
            ] {
                if let Some(value) = cx.var(key) {
                    event.context_string(key, value);
                }
            }
            for key in [
                b"CMAKE_IGNORE_PATH".as_slice(),
                b"CMAKE_IGNORE_PREFIX_PATH",
                b"CMAKE_SYSTEM_IGNORE_PATH",
                b"CMAKE_SYSTEM_IGNORE_PREFIX_PATH",
                b"CMAKE_SYSTEM_FRAMEWORK_PATH",
            ] {
                event.context_list(key, cmake_paths(cx, key));
            }
            if self.use_modules {
                event.context_list(b"CMAKE_MODULE_PATH", cmake_paths(cx, b"CMAKE_MODULE_PATH"));
            }
        });
    }
}

pub(super) fn bind_root_count(cx: &Cx<'_>, vars: &mut super::Bindings, count: usize) {
    if count > 0 {
        let state = cx.var_or_empty(ROOT_COUNTS);
        vars.add(
            ROOT_COUNTS.into(),
            text(&[count.to_string().as_bytes(), b";", &state]),
        );
    }
}
