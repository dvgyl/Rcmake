use std::collections::BTreeSet;
use std::path::Path;

use rcmake_base::{BString, ByteSlice as _, version};
use rcmake_diag::{Effect, MessageKind};
use rcmake_eval::{CmdError, CmdResult, Cx, IncludeOptions};
use rcmake_expr::ExpandedArg;
use rcmake_host::path;

use super::super::search_paths::setting_enabled;
use super::super::{RootMode, text};

#[derive(Default)]
pub(in crate::process_find) struct Request {
    pub(in crate::process_find) version: Option<VersionRequest>,
    pub(in crate::process_find) exact: bool,
    pub(super) quiet: bool,
    pub(super) required: bool,
    pub(super) required_by: Option<BString>,
    pub(super) optional: bool,
    pub(super) module: bool,
    pub(in crate::process_find) global: bool,
    pub(super) no_policy_scope: bool,
    pub(super) bypass_provider: bool,
    pub(super) unwind_include: bool,
    pub(super) registry_view: Option<BString>,
    pub(in crate::process_find) components: Vec<(BString, bool)>,
    pub(super) config_options: Vec<BString>,
    pub(super) names: Vec<BString>,
    pub(super) configs: Vec<BString>,
    pub(super) hints: Vec<BString>,
    pub(super) paths: Vec<BString>,
    pub(super) suffixes: Vec<BString>,
    pub(super) log_hints: Vec<BString>,
    pub(super) log_paths_args: Vec<BString>,
    pub(super) log_suffixes: Vec<BString>,
    disabled_sources: BTreeSet<&'static [u8]>,
    no_defaults: bool,
    pub(super) root_mode: RootMode,
    pub(super) sort_order: BString,
    pub(super) sort_ascending: bool,
}

#[derive(Clone, Copy)]
enum Values {
    None,
    Components(bool),
    Names,
    Configs,
    Hints,
    Paths,
    Suffixes,
}

impl Request {
    pub(super) fn parse(args: &[ExpandedArg]) -> Result<Self, CmdError> {
        let mut request = Self::default();
        let mut values = Values::None;
        let mut args = args.iter();
        while let Some(ExpandedArg { value, .. }) = args.next() {
            let key = value.as_slice();
            match key {
                b"EXACT" => request.exact = true,
                b"QUIET" => request.quiet = true,
                b"MODULE" => request.module = true,
                b"GLOBAL" => request.global = true,
                b"NO_POLICY_SCOPE" => request.no_policy_scope = true,
                b"BYPASS_PROVIDER" => request.bypass_provider = true,
                b"UNWIND_INCLUDE" => request.unwind_include = true,
                b"REQUIRED" | b"OPTIONAL" => {
                    request.required |= key == b"REQUIRED";
                    request.optional |= key == b"OPTIONAL";
                    values = Values::Components(true);
                    continue;
                }
                b"COMPONENTS" | b"OPTIONAL_COMPONENTS" => {
                    values = Values::Components(key == b"COMPONENTS");
                    continue;
                }
                b"REGISTRY_VIEW" => {
                    let Some(view) = args.next().map(|arg| &arg.value) else {
                        return Err(CmdError::status(
                            "missing required argument for REGISTRY_VIEW",
                        ));
                    };
                    if !super::super::registry_path::valid_view(view) {
                        return Err(CmdError::status(text(&[
                            b"given invalid value for REGISTRY_VIEW: ",
                            view,
                        ])));
                    }
                    request.registry_view = Some(view.clone());
                }
                b"NAMES" | b"CONFIGS" | b"HINTS" | b"PATHS" | b"PATH_SUFFIXES" => {
                    request.config_options.push(value.clone());
                    values = match key {
                        b"NAMES" => Values::Names,
                        b"CONFIGS" => Values::Configs,
                        b"HINTS" => Values::Hints,
                        b"PATHS" => Values::Paths,
                        _ => Values::Suffixes,
                    };
                    continue;
                }
                b"CONFIG" | b"NO_MODULE" | b"NO_CMAKE_BUILDS_PATH" => {
                    request.config_options.push(value.clone());
                }
                b"NO_DEFAULT_PATH" => {
                    request.config_options.push(value.clone());
                    request.no_defaults = true;
                }
                key if let Some(mode) = RootMode::option(key) => {
                    request.config_options.push(value.clone());
                    request.root_mode = mode;
                }
                _ => {
                    if let Some(source) = search_source(key) {
                        request.config_options.push(value.clone());
                        request.disabled_sources.insert(source);
                    } else {
                        request.add_value(values, value)?;
                        continue;
                    }
                }
            };
            values = Values::None;
        }

        if request.required && request.optional {
            return Err(CmdError::status("cannot be both REQUIRED and OPTIONAL"));
        }
        if request.module && !request.config_options.is_empty() {
            let options = list_lines(request.config_options.iter());
            return Err(CmdError::status(text(&[
                b"given options exclusive to Module mode:\n\n  MODULE\n\nand options exclusive to Config mode:\n",
                &options,
                b"\nThe options are incompatible.",
            ])));
        }
        let required: BTreeSet<_> = request
            .components
            .iter()
            .filter_map(|(name, required)| required.then_some(name))
            .collect();
        let overlap = list_lines(request.components.iter().filter_map(|(name, mandatory)| {
            (!mandatory && required.contains(name)).then_some(name)
        }));
        if !overlap.is_empty() {
            return Err(CmdError::status(text(&[
                b"called with components that are both required and optional:\n",
                &overlap,
                b"\n",
            ])));
        }
        Ok(request)
    }

    pub(super) fn snapshot_search(&mut self, cx: &Cx<'_>) {
        self.root_mode = self
            .root_mode
            .resolve(cx, b"CMAKE_FIND_ROOT_PATH_MODE_PACKAGE");
        let order = b"CMAKE_FIND_PACKAGE_SORT_ORDER";
        self.sort_order = if cx.var_defined(order) {
            cx.var_or_empty(order)
        } else {
            "NATURAL".into()
        };
        let direction = b"CMAKE_FIND_PACKAGE_SORT_DIRECTION";
        self.sort_ascending = cx.var_defined(direction) && cx.var_or_empty(direction) != b"DEC";
    }

    pub(super) fn inherit(&mut self, cx: &Cx<'_>, parent: &Self, needed: bool) {
        self.no_defaults = parent.no_defaults;
        self.disabled_sources.clone_from(&parent.disabled_sources);
        self.suffixes.clone_from(&parent.suffixes);
        self.root_mode = parent.root_mode;
        self.registry_view.clone_from(&parent.registry_view);
        self.sort_order.clone_from(&parent.sort_order);
        self.sort_ascending = parent.sort_ascending;
        self.global = parent.global || cx.var_is_on(b"CMAKE_FIND_PACKAGE_TARGETS_GLOBAL");
        self.quiet = parent.quiet;
        self.required = needed && parent.required;
        self.optional = !self.required;
    }

    pub(super) fn apply_requiredness(&mut self, cx: &mut Cx<'_>, name: &[u8]) {
        if !self.required && !self.optional && cx.var_is_on(b"CMAKE_FIND_REQUIRED") {
            self.required = true;
            self.required_by = Some("CMAKE_FIND_REQUIRED".into());
        }
        let package_setting = text(&[b"CMAKE_REQUIRE_FIND_PACKAGE_", name]);
        if cx.var_is_on(&package_setting) {
            if self.required {
                cx.issue(
                    MessageKind::Warning,
                    Effect::None,
                    None,
                    text(&[
                        b"for module ",
                        name,
                        b" already called with REQUIRED, thus ",
                        &package_setting,
                        b" has no effect.",
                    ]),
                );
            } else {
                self.required = true;
                self.required_by = Some(package_setting);
            }
        }
    }

    fn add_value(&mut self, values: Values, value: &BString) -> Result<(), CmdError> {
        match values {
            Values::Components(required) => self.components.push((value.clone(), required)),
            Values::Names => self.names.push(value.clone()),
            Values::Configs => {
                let filename = path::to_native(value);
                if filename.file_name() != Some(filename.as_os_str()) || !value.ends_with(b".cmake")
                {
                    return Err(CmdError::status(text(&[
                        b"given CONFIGS option followed by invalid file name \"", value,
                        b"\".  The names given must be file names without a path and with a \".cmake\" extension.",
                    ])));
                }
                self.configs.push(value.clone());
            }
            Values::Hints | Values::Paths => {
                let dest = if matches!(values, Values::Hints) {
                    &mut self.hints
                } else {
                    &mut self.paths
                };
                let raw = if matches!(values, Values::Hints) {
                    &mut self.log_hints
                } else {
                    &mut self.log_paths_args
                };
                raw.push(value.clone());
                dest.push(value.clone());
            }
            Values::Suffixes => {
                self.suffixes.push(value.clone());
                let suffix = value.strip_prefix(b"/").unwrap_or(value);
                let suffix = suffix.strip_suffix(b"/").unwrap_or(suffix);
                if !suffix.is_empty() {
                    self.log_suffixes.push(suffix.into());
                }
            }
            Values::None if self.version.is_none() => {
                self.version = Some(VersionRequest::parse(value)?)
            }
            Values::None => return Err(invalid_argument(value)),
        }
        Ok(())
    }

    pub(super) fn log_paths(&self, cx: &Cx<'_>) -> Vec<(&'static [u8], bool)> {
        let enabled = |source: &'static [u8]| {
            !self.disabled_sources.contains(source)
                && setting_enabled(cx, &text(&[b"CMAKE_FIND_USE_", source]))
        };
        let registry = |source: &'static [u8], legacy| {
            let key = text(&[b"CMAKE_FIND_USE_", source]);
            enabled(source) && (cx.var_defined(&key) || !cx.var_is_on(legacy))
        };
        vec![
            (b"CMAKE_PATH", !self.no_defaults),
            (
                b"CMAKE_ENVIRONMENT_PATH",
                enabled(b"CMAKE_ENVIRONMENT_PATH"),
            ),
            (
                b"SYSTEM_ENVIRONMENT_PATH",
                enabled(b"SYSTEM_ENVIRONMENT_PATH"),
            ),
            (b"CMAKE_SYSTEM_PATH", enabled(b"CMAKE_SYSTEM_PATH")),
            (b"INSTALL_PREFIX", enabled(b"INSTALL_PREFIX")),
            (b"PACKAGE_ROOT_PATH", enabled(b"PACKAGE_ROOT_PATH")),
            (
                b"CMAKE_PACKAGE_REGISTRY",
                registry(
                    b"PACKAGE_REGISTRY",
                    b"CMAKE_FIND_PACKAGE_NO_PACKAGE_REGISTRY".as_slice(),
                ),
            ),
            (
                b"SYSTEM_PACKAGE_REGISTRY",
                registry(
                    b"SYSTEM_PACKAGE_REGISTRY",
                    b"CMAKE_FIND_PACKAGE_NO_SYSTEM_PACKAGE_REGISTRY",
                ),
            ),
        ]
    }

    pub(super) fn use_source(&self, cx: &Cx<'_>, source: &[u8]) -> bool {
        !self.no_defaults
            && !self.disabled_sources.contains(source)
            && setting_enabled(cx, &text(&[b"CMAKE_FIND_USE_", source]))
    }

    pub(super) fn include_file(&self, cx: &mut Cx<'_>, path: &Path) -> CmdResult {
        include_package_file(cx, path, !self.no_policy_scope)
    }
}

pub(super) fn include_package_file(cx: &mut Cx<'_>, path: &Path, policy_scope: bool) -> CmdResult {
    // Start the policy scope before the include records its stack depth.
    if policy_scope {
        cx.push_policy_scope();
    }
    let result = cx.include_package_file(
        path,
        IncludeOptions {
            no_policy_scope: true,
            ..Default::default()
        },
    );
    if policy_scope {
        cx.pop_policy_scope()?;
    }
    result
}

fn search_source(key: &[u8]) -> Option<&'static [u8]> {
    Some(match key {
        b"NO_PACKAGE_ROOT_PATH" => b"PACKAGE_ROOT_PATH",
        b"NO_CMAKE_PATH" => b"CMAKE_PATH",
        b"NO_CMAKE_ENVIRONMENT_PATH" => b"CMAKE_ENVIRONMENT_PATH",
        b"NO_SYSTEM_ENVIRONMENT_PATH" => b"SYSTEM_ENVIRONMENT_PATH",
        b"NO_CMAKE_PACKAGE_REGISTRY" => b"PACKAGE_REGISTRY",
        b"NO_CMAKE_SYSTEM_PATH" => b"CMAKE_SYSTEM_PATH",
        b"NO_CMAKE_INSTALL_PREFIX" => b"INSTALL_PREFIX",
        b"NO_CMAKE_SYSTEM_PACKAGE_REGISTRY" => b"SYSTEM_PACKAGE_REGISTRY",
        _ => return None,
    })
}

fn invalid_argument(value: &[u8]) -> CmdError {
    CmdError::status(text(&[b"called with invalid argument \"", value, b"\""]))
}

pub(super) fn list_lines<'a>(values: impl IntoIterator<Item = &'a BString>) -> BString {
    let mut out = BString::default();
    for value in values {
        out.extend_from_slice(b"\n  ");
        out.extend_from_slice(value);
    }
    if !out.is_empty() {
        out.push(b'\n');
    }
    out
}

#[derive(Clone)]
pub(in crate::process_find) struct VersionRequest {
    pub(super) full: BString,
    pub(in crate::process_find) min: BString,
    pub(in crate::process_find) max: Option<BString>,
    pub(in crate::process_find) exclusive: bool,
}

impl VersionRequest {
    pub(super) fn parse(full: &BString) -> Result<Self, CmdError> {
        if let Some((min, max)) = full.split_once_str(b"...") {
            let bound = max.strip_prefix(b"<");
            let max = bound.unwrap_or(max);
            let exclusive = bound.is_some();
            if version::Version::parse_strict(min).is_none()
                || version::Version::parse_strict(max).is_none()
            {
                return Err(invalid_argument(full));
            }
            let order = version::compare(min, max);
            if order.is_gt() || exclusive && order.is_eq() {
                return Err(CmdError::status("specified version range is empty."));
            }
            Ok(Self {
                full: full.clone(),
                min: min.into(),
                max: Some(max.into()),
                exclusive,
            })
        } else {
            version::Version::parse_strict(full).ok_or_else(|| invalid_argument(full))?;
            Ok(Self {
                full: full.clone(),
                min: full.clone(),
                max: None,
                exclusive: false,
            })
        }
    }

    pub(super) fn bindings(&self, prefix: &[u8], vars: &mut Bindings) {
        version_bindings(prefix, &self.min, vars);
        vars.add(text(&[prefix, b"_COMPLETE"]), self.full.clone());
        if let Some(max) = &self.max {
            vars.add(text(&[prefix, b"_RANGE"]), self.full.clone());
            vars.add(text(&[prefix, b"_RANGE_MIN"]), "INCLUDE");
            vars.add(
                text(&[prefix, b"_RANGE_MAX"]),
                if self.exclusive { "EXCLUDE" } else { "INCLUDE" },
            );
            version_bindings(&text(&[prefix, b"_MIN"]), &self.min, vars);
            version_bindings(&text(&[prefix, b"_MAX"]), max, vars);
        }
    }
}

pub(in crate::process_find) fn version_bindings(prefix: &[u8], value: &[u8], vars: &mut Bindings) {
    vars.add(prefix.into(), BString::from(value));
    let numeric = value
        .split(|byte| !byte.is_ascii_digit() && *byte != b'.')
        .next()
        .unwrap_or_default();
    let mut items = numeric
        .split(|byte| *byte == b'.')
        .take_while(|item| !item.is_empty());
    let mut count = 0;
    for label in [b"_MAJOR".as_slice(), b"_MINOR", b"_PATCH", b"_TWEAK"] {
        let parsed = items
            .next()
            .and_then(|item| std::str::from_utf8(item).ok())
            .and_then(|item| item.parse::<u32>().ok());
        count += usize::from(parsed.is_some());
        vars.add(text(&[prefix, label]), parsed.unwrap_or(0).to_string());
    }
    vars.add(text(&[prefix, b"_COUNT"]), count.to_string());
}

#[derive(Default)]
pub(in crate::process_find) struct Bindings(Vec<(BString, BString)>);

impl Bindings {
    pub(in crate::process_find) fn add(&mut self, name: BString, value: impl Into<BString>) {
        self.0.push((name, value.into()));
    }

    pub(super) fn apply(self, cx: &mut Cx<'_>) {
        for (name, value) in self.0 {
            cx.set_var(&name, value);
        }
    }

    pub(super) fn scoped<T>(self, cx: &mut Cx<'_>, run: impl FnOnce(&mut Cx<'_>) -> T) -> T {
        let mut saved = Vec::with_capacity(self.0.len());
        for (name, value) in self.0 {
            let previous = cx.var_normal_only(&name);
            cx.set_var(&name, value);
            saved.push((name, previous));
        }
        let result = run(cx);
        for (name, value) in saved.into_iter().rev() {
            if let Some(value) = value {
                cx.set_var(&name, value);
            } else {
                cx.unset_var(&name);
            }
        }
        result
    }
}
