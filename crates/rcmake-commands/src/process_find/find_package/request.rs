use std::collections::BTreeSet;

use rcmake_base::{BString, ByteSlice as _, version};
use rcmake_eval::{CmdError, Cx, IncludeOptions};
use rcmake_expr::ExpandedArg;
use rcmake_host::path;

use super::super::search_paths::{host_env_paths, setting_enabled};
use super::super::{RootMode, text};

#[derive(Default)]
pub(in crate::process_find) struct Request {
    pub(in crate::process_find) version: Option<VersionRequest>,
    pub(in crate::process_find) exact: bool,
    pub(super) quiet: bool,
    pub(super) required: bool,
    pub(super) optional: bool,
    pub(super) module: bool,
    pub(in crate::process_find) global: bool,
    no_policy_scope: bool,
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
    disabled_sources: BTreeSet<&'static [u8]>,
    no_defaults: bool,
    pub(super) root_mode: RootMode,
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
    pub(super) fn parse(cx: &mut Cx<'_>, args: &[ExpandedArg]) -> Result<Self, CmdError> {
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
                    if !matches!(
                        view.as_slice(),
                        b"64" | b"32" | b"64_32" | b"32_64" | b"HOST" | b"TARGET" | b"BOTH"
                    ) {
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
                        request.add_value(cx, values, value, &mut args)?;
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
                b"given options exclusive to Module mode:\n\n    MODULE\n\nand options exclusive to Config mode:\n",
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

    fn add_value<'a>(
        &mut self,
        cx: &Cx<'_>,
        values: Values,
        value: &BString,
        args: &mut impl Iterator<Item = &'a ExpandedArg>,
    ) -> Result<(), CmdError> {
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
                if value == b"ENV" {
                    if let Some(key) = args.next() {
                        dest.extend(host_env_paths(cx, &key.value));
                    }
                } else {
                    dest.push(value.clone());
                }
            }
            Values::Suffixes => self.suffixes.push(value.clone()),
            Values::None if self.version.is_none() => {
                self.version = Some(VersionRequest::parse(value)?)
            }
            Values::None => return Err(invalid_argument(value)),
        }
        Ok(())
    }

    pub(super) fn use_source(&self, cx: &Cx<'_>, source: &[u8]) -> bool {
        !self.no_defaults
            && !self.disabled_sources.contains(source)
            && setting_enabled(cx, &text(&[b"CMAKE_FIND_USE_", source]))
    }

    pub(super) fn include_options(&self) -> IncludeOptions {
        IncludeOptions {
            no_policy_scope: self.no_policy_scope,
            ..Default::default()
        }
    }
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
pub(in crate::process_find) struct Bindings(pub(super) Vec<(BString, BString)>);

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
