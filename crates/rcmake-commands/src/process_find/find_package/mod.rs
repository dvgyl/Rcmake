use rcmake_base::{BString, list};
use rcmake_diag::{Effect, MessageKind, PolicyId, PolicyStatus};
use rcmake_eval::{CmdError, CmdResult, Cx, Flow, LogLevel};
use rcmake_expr::ExpandedArg;
use rcmake_host::{Stream, path};
use rcmake_syntax::ArgKind;

use super::search_paths::{cmake_paths, is_file};
use super::{cat, cps_import};
use config::ConfigSearch;
use log::{Mode, Reason, SearchLog, SearchReport};
use messages::report_missing;
use paths::{package_roots, redirects_directory, resolve_package_root};
use request::VersionRequest;
use trace::SearchTrace;

pub(super) use request::{Bindings, Request, version_bindings};

mod config;
mod log;
mod messages;
mod paths;
mod request;
mod trace;

const PROVIDER: &[u8] = b"_RCMAKE_FIND_PACKAGE_PROVIDER";
pub(super) const PACKAGE_ROOTS: &[u8] = b"_RCMAKE_FIND_PACKAGE_ROOTS";
const PACKAGE_ROOT_SPELLINGS: &[u8] = b"_RCMAKE_FIND_PACKAGE_ROOT_SPELLINGS";

pub(super) fn find_package(cx: &mut Cx<'_>, args: &[ExpandedArg]) -> CmdResult {
    find_package_with_options(cx, args, None, None)
}

pub(super) fn find_package_dependency(
    cx: &mut Cx<'_>,
    args: &[ExpandedArg],
    parent: &Request,
    needed: bool,
    diagnostics: &mut Vec<cps_import::PackageDiagnostic>,
) -> CmdResult {
    find_package_with_options(cx, args, Some((parent, needed)), Some(diagnostics))
}

fn find_package_with_options(
    cx: &mut Cx<'_>,
    args: &[ExpandedArg],
    parent: Option<(&Request, bool)>,
    dependency: Option<&mut Vec<cps_import::PackageDiagnostic>>,
) -> CmdResult {
    cx.check_package_find(false)?;
    let Some((package, parameters)) = args.split_first() else {
        return Err(CmdError::status(
            "called with incorrect number of arguments",
        ));
    };
    let name = &package.value;
    let mut request = Request::parse(parameters)?;
    if let Some((parent, needed)) = parent {
        request.inherit(cx, parent, needed);
    } else {
        request.snapshot_search(cx);
    }
    let unwind = request.unwind_include;
    cx.check_package_find(unwind)?;
    cx.project().package_lookups.insert(name.clone());
    let prefix = cx.var_normal_only(b"PACKAGE_PREFIX_DIR");
    let result = find_requested_package(cx, args, name, request, dependency);
    if cx.var_normal_only(b"PACKAGE_PREFIX_DIR") != prefix {
        if let Some(prefix) = prefix {
            cx.set_var(b"PACKAGE_PREFIX_DIR", prefix);
        } else {
            cx.unset_var(b"PACKAGE_PREFIX_DIR");
        }
    }
    if unwind && (result.is_err() || !cx.var_is_on(&cat(&[name, b"_FOUND"]))) {
        cx.unwind_package();
    }
    result
}

fn find_requested_package(
    cx: &mut Cx<'_>,
    args: &[ExpandedArg],
    name: &BString,
    mut request: Request,
    dependency: Option<&mut Vec<cps_import::PackageDiagnostic>>,
) -> CmdResult {
    let find = |suffix| cat(&[name, b"_FIND_", suffix]);
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
                    let required = cx.var_is_on(&cat(&[name, b"_FIND_REQUIRED_", &component]));
                    (component, required)
                })
                .collect();
        }
    }
    if request.exact {
        match &request.version {
            Some(version) if version.max.is_some() => {
                return Err(CmdError::status(
                    "EXACT cannot be specified with a version range.",
                ));
            }
            None => {
                crate::author_warning(cx, "Ignoring EXACT since no version is requested.");
                request.exact = false;
            }
            Some(_) => {}
        }
    }
    request.apply_requiredness(cx, name);

    let found_var = cat(&[name, b"_FOUND"]);
    let mut log = SearchLog::new(cx, name, &found_var, &request);
    let outer_roots = cmake_paths(cx, PACKAGE_ROOT_SPELLINGS);
    let disable = cat(&[b"CMAKE_DISABLE_FIND_PACKAGE_", name]);
    if cx.var_is_on(&disable) {
        let require = cat(&[b"CMAKE_REQUIRE_FIND_PACKAGE_", name]);
        if cx.var_is_on(&require) {
            cx.issue(
                MessageKind::Error,
                Effect::SetError,
                None,
                cat(&[
                    b"find_package for module ",
                    name,
                    b" was made REQUIRED with ",
                    &require,
                    b" but ",
                    &disable,
                    b" is enabled.  A REQUIRED package cannot be disabled.",
                ]),
            );
        } else if request.required {
            cx.issue(
                MessageKind::Error,
                Effect::SetError,
                None,
                cat(&[
                    b"find_package for module ",
                    name,
                    b" called with REQUIRED, but\n",
                    &disable,
                    b" is enabled.  A REQUIRED package cannot be disabled.\n",
                ]),
            );
        }
        log.emit(cx, name, &request, &outer_roots);
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
        if let Err(error) = cx.call_command(&provider, call) {
            log.emit(cx, name, &request, &outer_roots);
            return Err(error);
        }
        let path = cat(&[b"dependency_provider::", &provider]);
        if cx.var_is_on(&found_var) {
            log.selected(path, Mode::Provider, BString::default());
            log.emit(cx, name, &request, &outer_roots);
            return finish(cx, name, &request, first_target, true);
        }
        log.candidate(
            || path,
            Mode::Provider,
            Reason::NotFound,
            BString::default(),
        );
    }

    let mut vars = Bindings::default();
    vars.add("CMAKE_FIND_PACKAGE_NAME".into(), name.clone());
    vars.add("CMAKE_FIND_REQUIRED".into(), "");
    let mut package_roots = package_roots(cx, name);
    log::bind_root_count(cx, &mut vars, package_roots.len());
    package_roots.extend(outer_roots.iter().cloned());
    vars.add(
        PACKAGE_ROOTS.into(),
        list::join(
            package_roots
                .iter()
                .map(|root| path::from_native_cmake(&resolve_package_root(cx, root))),
        ),
    );
    vars.add(
        PACKAGE_ROOT_SPELLINGS.into(),
        list::join(package_roots.iter()),
    );
    vars.add(
        find(b"COMPONENTS"),
        list::join(request.components.iter().map(|(name, _)| name)),
    );
    for (component, required) in &request.components {
        vars.add(cat(&[name, b"_FIND_REQUIRED_", component]), flag(*required));
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
        let mut report = SearchReport {
            trace: SearchTrace::new(cx, name),
            log,
        };
        let result = search(
            cx,
            name,
            &found_var,
            &request,
            &package_roots,
            dependency,
            &mut report,
        );
        report.log.emit(cx, name, &request, &outer_roots);
        report.trace.emit(cx);
        result
    });
    match found {
        Ok(found) => finish(cx, name, &request, first_target, found),
        Err(err) => {
            record_result(cx, name, &request, cx.var_is_on(&found_var));
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
    record_result(cx, name, request, found);
    let version = cx.var(&cat(&[name, b"_VERSION"]));
    let global = request.global || cx.var_is_on(b"CMAKE_FIND_PACKAGE_TARGETS_GLOBAL");
    let dir = cx.current_dir_id();
    let project = cx.project();
    let imported: Vec<_> = project
        .targets()
        .skip(first_target)
        .filter(|(_, target)| target.dir == dir && target.is_imported())
        .map(|(id, _)| id)
        .collect();
    for id in imported {
        if let Some(target) = project.target_mut(id)
            && found
            && target.props.get(b"_RCMAKE_FIND_PACKAGE_NAME").is_none()
        {
            target
                .props
                .set("_RCMAKE_FIND_PACKAGE_NAME".into(), name.into());
            if let Some(version) = version.as_ref().filter(|version| !version.is_empty()) {
                target
                    .props
                    .set("_RCMAKE_FIND_PACKAGE_VERSION".into(), version.clone());
            }
            if let Some(requested) = &request.version {
                target.props.set(
                    "_RCMAKE_FIND_PACKAGE_REQUESTED_VERSION".into(),
                    requested.full.clone(),
                );
            }
        }
        if global {
            project
                .set_imported_global(id)
                .map_err(|err| CmdError::Status(err.to_string().into()))?;
        }
    }
    Ok(Flow::Normal)
}

fn record_result(cx: &mut Cx<'_>, name: &[u8], request: &Request, found: bool) {
    let properties = &mut cx.global().props;
    properties.set(
        cat(&[b"_CMAKE_", name, b"_QUIET"]),
        if request.quiet { "TRUE" } else { "FALSE" }.into(),
    );
    properties.set(
        cat(&[b"_CMAKE_", name, b"_TRANSITIVE_DEPENDENCY"]),
        "False".into(),
    );
    let version = request
        .version
        .as_ref()
        .map_or_else(BString::default, |version| {
            if version.full.windows(3).any(|part| part == b"...") {
                version.full.clone()
            } else {
                cat(&[if request.exact { b"== " } else { b">= " }, &version.full])
            }
        });
    properties.set(cat(&[b"_CMAKE_", name, b"_REQUIRED_VERSION"]), version);
    if request.required {
        properties.set(cat(&[b"_CMAKE_", name, b"_TYPE"]), "REQUIRED".into());
    }
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
    mut dependency: Option<&mut Vec<cps_import::PackageDiagnostic>>,
    report: &mut SearchReport,
) -> Result<bool, CmdError> {
    let lookup_names = if request.names.is_empty() {
        [name].as_slice().to_vec()
    } else {
        request.names.iter().map(|name| name.as_slice()).collect()
    };
    for package in lookup_names {
        let key = cat(&[
            b"_FetchContent_",
            &package.to_ascii_lowercase(),
            b"_override_find_package",
        ]);
        if cx
            .global()
            .props
            .get(&key)
            .is_some_and(|value| rcmake_base::truth::is_on(value))
        {
            cx.call_command(
                b"FetchContent_MakeAvailable",
                vec![ExpandedArg {
                    value: package.into(),
                    kind: ArgKind::Unquoted,
                    line: cx.current_line(),
                }],
            )?;
        }
        if let Some(directory) = redirects_directory(cx) {
            let lower = cat(&[&package.to_ascii_lowercase(), b"-config.cmake"]);
            let mixed = cat(&[package, b"Config.cmake"]);
            if [lower, mixed]
                .iter()
                .any(|file| is_file(cx, &directory.join(path::to_native(file))))
            {
                break;
            }
        }
    }
    let mut config = ConfigSearch::new(name, request);
    let redirected = redirects_directory(cx).is_some_and(|dir| config.redirect(cx, &dir));
    let prefer_config = !request.module && cx.var_is_on(b"CMAKE_FIND_PACKAGE_PREFER_CONFIG");
    let try_module = request.config_options.is_empty() && !redirected;
    if try_module && !prefer_config {
        if let Some(found) = load_module(cx, name, found_var, request, report)? {
            return Ok(found);
        }
        if !request.module && cx.var_is_on(b"CMAKE_FIND_PACKAGE_WARN_NO_MODULE") {
            messages::warn_config_fallback(cx, name);
        }
    }

    if !request.module || redirected || !cx.var_is_on(b"CMAKE_FIND_PACKAGE_PREFER_CONFIG") {
        report.log.config_search(&config, redirected);
    }
    if !request.module || redirected {
        let matched = config.find(cx, package_roots, report)?;
        if let Some(found) = &matched {
            report
                .log
                .selected(found.file.clone(), found.mode(), found.version.clone());
        }
        let config_file = matched.as_ref().map(|found| found.file.clone());
        let found = config.load(cx, found_var, matched)?;
        if !found
            && try_module
            && prefer_config
            && let Some(found) = load_module(cx, name, found_var, request, report)?
        {
            return Ok(found);
        }
        if let Some(file) = config_file {
            if found && let Some(diagnostics) = dependency.as_deref_mut() {
                let dir = cx.current_dir_id();
                let targets = request
                    .components
                    .iter()
                    .map(|(component, _)| cat(&[name, b"::", component]))
                    .filter(|target| cx.project().find_target(dir, target).is_none())
                    .collect::<Vec<_>>();
                if !targets.is_empty() {
                    diagnostics.push(cps_import::PackageDiagnostic::new(cat(&[
                        b"Found package configuration file:\n  ",
                        &file,
                        b"\nbut the following required targets were not found: ",
                        &list::join(targets.iter()),
                    ])));
                    cx.set_var(found_var, "0".into());
                    return Ok(false);
                }
            }
            if !found {
                let reason = cx.var_or_empty(&cat(&[name, b"_NOT_FOUND_MESSAGE"]));
                let message = cat(&[
                    b"Found package configuration file:\n  ",
                    &file,
                    b"\nbut it set ",
                    found_var,
                    b" to FALSE so package \"",
                    name,
                    b"\" is considered to be NOT FOUND.",
                    if reason.is_empty() {
                        b""
                    } else {
                        b"\nReason given by package:\n"
                    },
                    &reason,
                ]);
                if let Some(diagnostics) = dependency {
                    diagnostics.push(cps_import::PackageDiagnostic::new(message));
                } else {
                    report_missing(cx, request, message)?;
                }
            }
            return Ok(found);
        }
    } else {
        cx.set_var(found_var, "0".into());
    }

    let message = if request.module && !redirected {
        cat(&[b"No \"Find", name, b".cmake\" found in CMAKE_MODULE_PATH."])
    } else {
        config.missing_message(try_module)
    };
    if !try_module && !request.module && !request.required && config.considered.is_empty() {
        if !request.quiet && cx.log_level() >= LogLevel::Status {
            cx.host().write_out(
                Stream::Stdout,
                &cat(&[
                    b"-- Could NOT find ",
                    name,
                    b" (missing: ",
                    name,
                    b"_DIR)\n",
                ]),
            );
        }
    } else if let Some(diagnostics) = dependency {
        diagnostics.push(cps_import::PackageDiagnostic::new(message));
    } else {
        let result = report_missing(cx, request, message);
        if request.module && (request.required || !request.quiet && !request.optional) {
            messages::warn_module_path(cx, name);
        }
        result?;
    }
    Ok(false)
}

fn load_module(
    cx: &mut Cx<'_>,
    name: &[u8],
    found_var: &[u8],
    request: &Request,
    report: &mut SearchReport,
) -> Result<Option<bool>, CmdError> {
    let filename = cat(&[b"Find", name, b".cmake"]);
    let native_name = path::to_native(&filename);
    let mut considered = Vec::new();
    let mut module = None;
    for dir in cmake_paths(cx, b"CMAKE_MODULE_PATH") {
        let candidate = path::to_native(&dir).join(&native_name);
        let resolved = cx.resolve_path(&path::from_native(&candidate));
        if is_file(cx, &resolved) {
            module = Some(resolved);
            break;
        }
        considered.push(candidate);
    }
    let policy = match name {
        b"Qt" => Some(PolicyId::new(84)),
        b"Dart" => Some(PolicyId::new(145)),
        b"CUDA" => Some(PolicyId::new(146)),
        b"PythonInterp" | b"PythonLibs" => Some(PolicyId::new(148)),
        b"Boost" => Some(PolicyId::new(167)),
        b"GCCXML" => Some(PolicyId::new(188)),
        b"CABLE" => Some(PolicyId::new(191)),
        _ => None,
    };
    let root = cx.var_or_empty(b"CMAKE_ROOT");
    let bundled = cx
        .resolve_path(&cat(&[&root, b"/Modules"]))
        .join(&native_name);
    let bundled = if is_file(cx, &bundled) {
        Some(bundled)
    } else {
        considered.push(bundled);
        None
    };
    report.trace.module(
        &filename,
        &considered,
        module.as_deref().or(bundled.as_deref()),
    );
    if module.is_none()
        && let Some(bundled) = bundled
    {
        if let Some(policy) = policy {
            match cx.policy(policy) {
                PolicyStatus::New => return Ok(None),
                PolicyStatus::Warn => cx.issue_policy_warning(policy, b"", b""),
                PolicyStatus::Old => {}
            }
        }
        module = Some(bundled);
    }
    let Some(module) = module else {
        return Ok(None);
    };
    let mut vars = Bindings::default();
    vars.add(cat(&[name, b"_FIND_MODULE"]), "1");
    let result = vars.scoped(cx, |cx| request.include_file(cx, &module));
    let defined = cx.var_defined(found_var);
    if defined && !cx.var_is_on(found_var) {
        report.log.candidate(
            || path::from_native_cmake(&module),
            Mode::Module,
            Reason::NotFound,
            cx.var_or_empty(&cat(&[name, b"_NOT_FOUND_MESSAGE"])),
        );
    } else {
        report.log.selected(
            path::from_native_cmake(&module),
            Mode::Module,
            cx.var_or_empty(&cat(&[name, b"_VERSION"])),
        );
    }
    result?;
    if cx.fatal_occurred() {
        return Err(CmdError::Reported);
    }
    if !defined {
        let uppercase = cat(&[&name.to_ascii_uppercase(), b"_FOUND"]);
        let found = cx.var_is_on(&uppercase);
        cx.set_var(found_var, flag(found).into());
    }
    let found = cx.var_is_on(found_var);
    if defined && !found {
        report.trace.module_not_found(found_var);
    }
    Ok(Some(found))
}
