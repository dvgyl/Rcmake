use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use rcmake_base::{BString, ByteSlice as _, list, list_ops, truth, version};
use rcmake_cps::{CpsPackage, naming};
use rcmake_diag::{DiagCategory, Effect, MessageKind, PolicyId, PolicyStatus};
use rcmake_eval::{CmdError, CmdResult, Cx, Flow, IncludeOptions};
use rcmake_expr::ExpandedArg;
use rcmake_host::{FileKind, Follow, path};
use rcmake_model::{CacheEntry, CacheType};
use rcmake_syntax::ArgKind;

use super::{cps_import, diagnostic, text};

const PROVIDER: &[u8] = b"_RCMAKE_FIND_PACKAGE_PROVIDER";
const PACKAGE_ROOTS: &[u8] = b"_RCMAKE_FIND_PACKAGE_ROOTS";

#[derive(Clone, Copy, Default)]
enum RootMode {
    #[default]
    Default,
    Both,
    Only,
    Never,
}

#[derive(Default)]
pub(super) struct Request {
    pub(super) version: Option<VersionRequest>,
    pub(super) exact: bool,
    quiet: bool,
    required: bool,
    optional: bool,
    module: bool,
    pub(super) global: bool,
    no_policy_scope: bool,
    bypass_provider: bool,
    registry_view: Option<BString>,
    pub(super) components: Vec<(BString, bool)>,
    config_options: Vec<BString>,
    names: Vec<BString>,
    configs: Vec<BString>,
    hints: Vec<BString>,
    paths: Vec<BString>,
    suffixes: Vec<BString>,
    disabled_sources: BTreeSet<&'static [u8]>,
    no_defaults: bool,
    root_mode: RootMode,
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
    fn parse(cx: &mut Cx<'_>, args: &[ExpandedArg]) -> Result<Self, CmdError> {
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
                        return Err(status(b"given REGISTRY_VIEW without a value"));
                    };
                    if !matches!(
                        view.as_slice(),
                        b"64" | b"32" | b"64_32" | b"32_64" | b"HOST" | b"TARGET" | b"BOTH"
                    ) {
                        return Err(status(&text(&[
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
                b"CMAKE_FIND_ROOT_PATH_BOTH"
                | b"ONLY_CMAKE_FIND_ROOT_PATH"
                | b"NO_CMAKE_FIND_ROOT_PATH" => {
                    request.config_options.push(value.clone());
                    request.root_mode = match key {
                        b"CMAKE_FIND_ROOT_PATH_BOTH" => RootMode::Both,
                        b"ONLY_CMAKE_FIND_ROOT_PATH" => RootMode::Only,
                        _ => RootMode::Never,
                    };
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
            return Err(status(b"cannot be both REQUIRED and OPTIONAL."));
        }
        if request.module && !request.config_options.is_empty() {
            let options = list_lines(request.config_options.iter());
            return Err(status(&text(&[
                b"given options exclusive to Module mode:\n\n    MODULE\n\nand options exclusive to Config mode:\n",
                &options,
                b"\nThe options are incompatible.\n",
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
            return Err(status(&text(&[
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
                    return Err(status(&text(&[
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
                        dest.extend(env_paths(cx, &key.value));
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

    fn use_source(&self, cx: &Cx<'_>, source: &[u8]) -> bool {
        !self.no_defaults
            && !self.disabled_sources.contains(source)
            && use_setting(cx, &text(&[b"CMAKE_FIND_USE_", source]))
    }

    fn include_options(&self) -> IncludeOptions {
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

fn status(message: &[u8]) -> CmdError {
    CmdError::Status(text(&[b"find_package ", message]))
}

fn invalid_argument(value: &[u8]) -> CmdError {
    status(&text(&[b"called with invalid argument \"", value, b"\""]))
}

fn list_lines<'a>(values: impl IntoIterator<Item = &'a BString>) -> BString {
    let mut out = BString::default();
    for value in values {
        out.extend_from_slice(b"\n    ");
        out.extend_from_slice(value);
    }
    if !out.is_empty() {
        out.push(b'\n');
    }
    out
}

#[derive(Clone)]
pub(super) struct VersionRequest {
    full: BString,
    pub(super) min: BString,
    pub(super) max: Option<BString>,
    pub(super) exclusive: bool,
}

impl VersionRequest {
    fn parse(full: &BString) -> Result<Self, CmdError> {
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
                return Err(status(b"specified version range is empty.\n"));
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

    fn bindings(&self, prefix: &[u8], vars: &mut Bindings) {
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

pub(super) fn version_bindings(prefix: &[u8], value: &[u8], vars: &mut Bindings) {
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
pub(super) struct Bindings(Vec<(BString, BString)>);

impl Bindings {
    pub(super) fn add(&mut self, name: BString, value: impl Into<BString>) {
        self.0.push((name, value.into()));
    }

    fn apply(self, cx: &mut Cx<'_>) {
        for (name, value) in self.0 {
            cx.set_var(&name, value);
        }
    }

    fn scoped<T>(self, cx: &mut Cx<'_>, run: impl FnOnce(&mut Cx<'_>) -> T) -> T {
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

pub(super) fn find_package(cx: &mut Cx<'_>, args: &[ExpandedArg]) -> CmdResult {
    let Some((package, parameters)) = args.split_first() else {
        return Err(status(b"called with incorrect number of arguments"));
    };
    let name = &package.value;
    let mut request = Request::parse(cx, parameters)?;
    let find = |suffix| text(&[name, b"_FIND_", suffix]);
    if cx.var_is_on(&find(b"MODULE")) {
        if request.version.is_none() && cx.var_defined(&find(b"VERSION")) {
            request.version = Some(VersionRequest::parse(
                &cx.var_or_empty(&find(b"VERSION_COMPLETE")),
            )?);
            request.exact |= cx.var_is_on(&find(b"VERSION_EXACT"));
        }
        if request.components.is_empty() {
            request.components = cmake_paths(cx, &find(b"COMPONENTS"))
                .into_iter()
                .map(|component| {
                    let required = cx.var_is_on(&text(&[name, b"_FIND_REQUIRED_", &component]));
                    (component, required)
                })
                .collect();
        }
    }
    if request.exact {
        match &request.version {
            Some(version) if version.max.is_some() => {
                return Err(status(b"EXACT cannot be specified with a version range."));
            }
            None => {
                cx.issue(
                    MessageKind::AuthorWarning,
                    Effect::None,
                    Some(DiagCategory::Author),
                    "Ignoring EXACT since no version is requested.".into(),
                );
                request.exact = false;
            }
            Some(_) => {}
        }
    }
    request.required |= cx.var_is_on(&text(&[b"CMAKE_REQUIRE_FIND_PACKAGE_", name]))
        || !request.optional && cx.var_is_on(b"CMAKE_FIND_REQUIRED");

    let found_var = text(&[name, b"_FOUND"]);
    let disable = text(&[b"CMAKE_DISABLE_FIND_PACKAGE_", name]);
    if cx.var_is_on(&disable) {
        cx.set_var(&found_var, "0".into());
        if request.required {
            return Err(diagnostic(
                cx,
                text(&[
                    b"find_package for module ",
                    name,
                    b" called with REQUIRED, but\n",
                    &disable,
                    b" is enabled.  A REQUIRED package cannot be disabled.\n",
                ]),
            ));
        }
        return Ok(Flow::Normal);
    }

    let first_target = cx.project().targets().count();
    if !request.bypass_provider
        && let Some(provider) = cx.global().props.get(PROVIDER).cloned()
    {
        let mut call = Vec::with_capacity(args.len() + 1);
        call.push(ExpandedArg {
            value: "FIND_PACKAGE".into(),
            kind: ArgKind::Bracket,
            line: cx.current_line(),
        });
        call.extend_from_slice(args);
        cx.call_command(&provider, call)?;
        if cx.var_is_on(&found_var) {
            return finish(cx, name, &request, first_target, true);
        }
    }

    let mut vars = Bindings::default();
    vars.add("CMAKE_FIND_PACKAGE_NAME".into(), name.clone());
    let package_roots = package_roots(cx, name);
    vars.add(PACKAGE_ROOTS.into(), list::join(package_roots.iter()));
    vars.add(
        find(b"COMPONENTS"),
        list::join(request.components.iter().map(|(name, _)| name)),
    );
    for (component, required) in &request.components {
        vars.add(
            text(&[name, b"_FIND_REQUIRED_", component]),
            flag(*required),
        );
    }
    if let Some(version) = &request.version {
        version.bindings(&find(b"VERSION"), &mut vars);
        vars.add(find(b"VERSION_EXACT"), flag(request.exact));
    }
    if request.quiet {
        vars.add(find(b"QUIETLY"), "1");
    }
    if request.required {
        vars.add(find(b"REQUIRED"), "1");
    }
    if let Some(registry_view) = &request.registry_view {
        vars.add(find(b"REGISTRY_VIEW"), registry_view.clone());
    }
    let found = vars.scoped(cx, |cx| {
        search(cx, name, &found_var, &request, &package_roots)
    });
    match found {
        Ok(found) => finish(cx, name, &request, first_target, found),
        Err(err) => {
            record_result(cx, name, cx.var_is_on(&found_var));
            Err(err)
        }
    }
}

fn flag(value: bool) -> &'static str {
    if value { "1" } else { "0" }
}

fn finish(
    cx: &mut Cx<'_>,
    name: &[u8],
    request: &Request,
    first_target: usize,
    found: bool,
) -> CmdResult {
    record_result(cx, name, found);
    if request.global || cx.var_is_on(b"CMAKE_FIND_PACKAGE_TARGETS_GLOBAL") {
        let dir = cx.current_dir_id();
        let project = cx.project();
        let imported: Vec<_> = project
            .targets()
            .skip(first_target)
            .filter_map(|(id, target)| {
                (target.dir == dir && target.is_imported() && !target.is_global()).then_some(id)
            })
            .collect();
        for id in imported {
            project
                .set_imported_global(id)
                .map_err(|err| CmdError::Status(err.to_string().into()))?;
        }
    }
    Ok(Flow::Normal)
}

fn record_result(cx: &mut Cx<'_>, name: &[u8], found: bool) {
    for (property, include) in [
        (b"PACKAGES_FOUND".as_slice(), found),
        (b"PACKAGES_NOT_FOUND", !found),
    ] {
        let props = &mut cx.global().props;
        let old = props.get(property).cloned().unwrap_or_default();
        let mut values = list::split(&old, list::Empty::Drop);
        let listed = values.iter().any(|value| value == name);
        if include && !listed {
            values.push(name.into());
        } else if !include && listed {
            values.retain(|value| value != name);
        } else {
            continue;
        }
        props.set(property.into(), list::join(values.iter()));
    }
}

fn search(
    cx: &mut Cx<'_>,
    name: &[u8],
    found_var: &[u8],
    request: &Request,
    package_roots: &[BString],
) -> Result<bool, CmdError> {
    let mut config = ConfigSearch::new(name, request);
    let redirected = cx.var_or_empty(b"CMAKE_FIND_PACKAGE_REDIRECTS_DIR");
    let redirected = !redirected.is_empty() && config.redirect(cx, &cx.resolve_path(&redirected));
    let prefer_config = !request.module && cx.var_is_on(b"CMAKE_FIND_PACKAGE_PREFER_CONFIG");
    let try_module = request.config_options.is_empty() && !redirected;
    if try_module
        && !prefer_config
        && let Some(found) = load_module(cx, name, found_var, request)?
    {
        return Ok(found);
    }

    if !request.module || redirected {
        let matched = config.find(cx, package_roots)?;
        let config_file = matched.as_ref().map(|found| found.file.clone());
        let found = config.load(cx, found_var, matched)?;
        if !found
            && try_module
            && prefer_config
            && let Some(found) = load_module(cx, name, found_var, request)?
        {
            return Ok(found);
        }
        if let Some(file) = config_file {
            if !found {
                let reason = cx.var_or_empty(&text(&[name, b"_NOT_FOUND_MESSAGE"]));
                let message = text(&[
                    b"Found package configuration file:\n\n  ",
                    &file,
                    b"\n\nbut it set ",
                    found_var,
                    b" to FALSE so package \"",
                    name,
                    b"\" is considered to be NOT FOUND.",
                    if reason.is_empty() {
                        b""
                    } else {
                        b"\nReason given by package:\n\n"
                    },
                    &reason,
                ]);
                report_missing(cx, request, message)?;
            }
            return Ok(found);
        }
    } else {
        cx.set_var(found_var, "0".into());
    }

    let message = if request.module && !redirected {
        text(&[
            b"No \"Find",
            name,
            b".cmake\" found in CMAKE_MODULE_PATH.\n",
        ])
    } else {
        config.missing_message(try_module)
    };
    report_missing(cx, request, message)?;
    Ok(false)
}

fn report_missing(cx: &mut Cx<'_>, request: &Request, message: BString) -> Result<(), CmdError> {
    if request.required {
        return Err(diagnostic(cx, message));
    }
    if !request.quiet && !request.optional {
        cx.issue(MessageKind::Warning, Effect::None, None, message);
    }
    Ok(())
}

fn load_module(
    cx: &mut Cx<'_>,
    name: &[u8],
    found_var: &[u8],
    request: &Request,
) -> Result<Option<bool>, CmdError> {
    let filename = path::to_native(&text(&[b"Find", name, b".cmake"]));
    let mut dirs = cmake_paths(cx, b"CMAKE_MODULE_PATH");
    dirs.push(text(&[&cx.var_or_empty(b"CMAKE_ROOT"), b"/Modules"]));
    let module = dirs
        .into_iter()
        .map(|dir| cx.resolve_path(&dir).join(&filename))
        .find(|file| is_file(cx, file));
    let Some(module) = module else {
        return Ok(None);
    };
    let mut vars = Bindings::default();
    vars.add(text(&[name, b"_FIND_MODULE"]), "1");
    vars.scoped(cx, |cx| cx.include_file(&module, request.include_options()))?;
    if cx.fatal_occurred() {
        return Err(CmdError::Reported);
    }
    if !cx.var_defined(found_var) {
        let uppercase = text(&[&name.to_ascii_uppercase(), b"_FOUND"]);
        let found = cx.var_is_on(&uppercase);
        cx.set_var(found_var, flag(found).into());
    }
    Ok(Some(cx.var_is_on(found_var)))
}

struct ConfigSearch<'a> {
    name: &'a [u8],
    request: &'a Request,
    names: Vec<BString>,
    configs: Vec<BString>,
    redirect: Option<PathBuf>,
    visited: BTreeSet<(PathBuf, bool)>,
    considered: Vec<(BString, BString, Option<BString>)>,
    cps_names: Vec<BString>,
}

struct ConfigMatch {
    file: BString,
    version: BString,
    cps: Option<Box<CpsPackage>>,
}

impl<'a> ConfigSearch<'a> {
    fn new(name: &'a [u8], request: &'a Request) -> Self {
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
        }
    }

    fn redirect(&mut self, cx: &Cx<'_>, dir: &Path) -> bool {
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

    fn find(
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
                let suffixes = self.request.suffixes.iter().map(|suffix| {
                    let suffix: PathBuf = path::to_native(suffix)
                        .components()
                        .filter(|part| {
                            matches!(
                                part,
                                std::path::Component::Normal(_) | std::path::Component::ParentDir
                            )
                        })
                        .collect();
                    dir.join(suffix)
                });
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
                        Err(reason) => Err(reason),
                    }
                }
                Ok(package) => Err(format!(
                    "The file describes the package {:?}, which is not the requested package.",
                    package.name
                )
                .into()),
                Err(reason) => Err(reason),
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
                Err(reason) => self
                    .considered
                    .push((file, considered_version, Some(reason))),
            }
        }
        Ok(None)
    }

    fn load(
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

    fn missing_message(&self, tried_module: bool) -> BString {
        let name = self.name;
        let preface = if tried_module {
            text(&[
                b"By not providing \"Find", name,
                b".cmake\" in CMAKE_MODULE_PATH this project has asked CMake to find a package configuration file provided by \"",
                name, b"\", but CMake did not find one.\n\n",
            ])
        } else {
            BString::default()
        };
        let requested = self
            .request
            .version
            .as_ref()
            .map_or_else(BString::default, |v| {
                let kind: &[u8] = if self.request.exact {
                    b"exactly matches requested version "
                } else if v.max.is_some() {
                    b"is compatible with requested version range "
                } else {
                    b"is compatible with requested version "
                };
                text(&[b" that ", kind, b"\"", &v.full, b"\""])
            });
        if !self.considered.is_empty() {
            let requested: &[u8] = if requested.is_empty()
                && self
                    .considered
                    .iter()
                    .any(|(_, _, reason)| reason.is_some())
            {
                b" that is compatible with requested version \"\""
            } else {
                &requested
            };
            let files: Vec<_> = self
                .considered
                .iter()
                .map(|(file, version, reason)| {
                    let mut entry = text(&[file, b", version: ", version]);
                    if let Some(reason) = reason {
                        entry.extend_from_slice(b"\n      ");
                        entry.extend_from_slice(reason);
                    }
                    entry
                })
                .collect();
            return text(&[
                &preface,
                b"Could not find a configuration file for package \"",
                name,
                b"\"",
                requested,
                b".\n\nThe following configuration files were considered but not accepted:\n",
                &list_lines(&files),
            ]);
        }
        let mut configs = self.cps_names.clone();
        configs.extend_from_slice(&self.configs);
        let requested = self
            .request
            .version
            .as_ref()
            .map_or_else(BString::default, |v| {
                text(&[b" (requested version ", &v.full, b")"])
            });
        text(&[
            &preface,
            b"Could not find a package configuration file provided by \"",
            name,
            b"\"",
            &requested,
            b" with any of the following names:\n",
            &list_lines(&configs),
            b"\nAdd the installation prefix of \"",
            name,
            b"\" to CMAKE_PREFIX_PATH or set \"",
            name,
            b"_DIR\" to a directory containing one of the above files.  If \"",
            name,
            b"\" provides a separate development package or SDK, be sure it has been installed.\n",
        ])
    }
}

impl ConfigSearch<'_> {
    fn prefixes(&self, cx: &mut Cx<'_>, package_roots: &[BString]) -> Vec<PathBuf> {
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
            prefixes.extend(env_paths(cx, &text(&[self.name, b"_DIR"])));
            prefixes.extend(cmake_paths_keys.iter().flat_map(|key| env_paths(cx, key)));
        }
        prefixes.extend_from_slice(&request.hints);
        if request.use_source(cx, b"SYSTEM_ENVIRONMENT_PATH") {
            prefixes.extend(env_paths(cx, b"PATH").into_iter().map(|value| {
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
            .map(|value| full_path(cx, &value))
            .collect();
        reroot(cx, request.root_mode, prefixes)
    }

    fn layout(&self, cx: &mut Cx<'_>, prefix: &Path) -> Vec<(PathBuf, bool)> {
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

fn is_file(cx: &Cx<'_>, file: &Path) -> bool {
    cx.host()
        .metadata(file, Follow::Yes)
        .is_ok_and(|meta| meta.kind == FileKind::File)
}

fn cmake_paths(cx: &Cx<'_>, key: &[u8]) -> Vec<BString> {
    list::split(&cx.var_or_empty(key), list::Empty::Drop)
}

fn env_paths(cx: &Cx<'_>, key: &[u8]) -> Vec<BString> {
    let separator = if cx.host().system().sysname == b"Windows" {
        b';'
    } else {
        b':'
    };
    cx.host().env_get(key).map_or_else(Vec::new, |value| {
        value
            .split(|byte| *byte == separator)
            .filter(|part| !part.is_empty())
            .map(BString::from)
            .collect()
    })
}

fn use_setting(cx: &Cx<'_>, key: &[u8]) -> bool {
    !cx.var_defined(key) || cx.var_is_on(key)
}

fn package_roots(cx: &Cx<'_>, name: &[u8]) -> Vec<BString> {
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
        roots.extend(keys.iter().flat_map(|key| env_paths(cx, key)));
    }
    roots.extend(cmake_paths(cx, PACKAGE_ROOTS));
    roots
}

fn full_path(cx: &Cx<'_>, value: &[u8]) -> PathBuf {
    let full = path::from_native(&cx.resolve_path(value));
    path::to_native(&rcmake_base::path_lex::Lexical::new(&full).normalize())
}

fn paths_from_vars(cx: &Cx<'_>, keys: &[&[u8]]) -> Vec<PathBuf> {
    keys.iter()
        .flat_map(|key| cmake_paths(cx, key))
        .map(|value| full_path(cx, &value))
        .collect()
}

fn reroot(cx: &Cx<'_>, mode: RootMode, prefixes: Vec<PathBuf>) -> Vec<PathBuf> {
    let mode = match mode {
        RootMode::Default => match cx
            .var_or_empty(b"CMAKE_FIND_ROOT_PATH_MODE_PACKAGE")
            .as_slice()
        {
            b"ONLY" => RootMode::Only,
            b"NEVER" => RootMode::Never,
            _ => RootMode::Both,
        },
        mode => mode,
    };
    if matches!(mode, RootMode::Never) {
        return prefixes;
    }
    let roots = paths_from_vars(
        cx,
        &[
            b"CMAKE_FIND_ROOT_PATH",
            b"CMAKE_SYSROOT_COMPILE",
            b"CMAKE_SYSROOT_LINK",
            b"CMAKE_SYSROOT",
        ],
    );
    if roots.is_empty() {
        return prefixes;
    }
    let staging = cmake_paths(cx, b"CMAKE_STAGING_PREFIX")
        .first()
        .map(|value| full_path(cx, value));
    let mut dirs = Vec::new();
    for root in roots {
        for prefix in &prefixes {
            if prefix.starts_with(&root)
                || staging.as_ref().is_some_and(|dir| prefix.starts_with(dir))
            {
                dirs.push(prefix.clone());
            } else {
                let relative: PathBuf = prefix
                    .components()
                    .filter(|part| matches!(part, std::path::Component::Normal(_)))
                    .collect();
                dirs.push(root.join(relative));
            }
        }
    }
    if matches!(mode, RootMode::Both) {
        dirs.extend(prefixes);
    }
    dirs
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
