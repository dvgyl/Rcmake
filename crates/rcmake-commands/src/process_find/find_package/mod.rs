use rcmake_base::{BString, list};
use rcmake_diag::{DiagCategory, Effect, MessageKind, PolicyId, PolicyStatus};
use rcmake_eval::{CmdError, CmdResult, Cx, Flow, LogLevel};
use rcmake_expr::ExpandedArg;
use rcmake_host::{Stream, path};
use rcmake_syntax::ArgKind;

use super::search_paths::{cmake_paths, is_file};
use super::{cps_import, diagnostic, text};
use config::ConfigSearch;
use messages::report_missing;
use paths::{package_roots, redirects_directory};
use request::VersionRequest;
use trace::SearchTrace;

pub(super) use request::{Bindings, Request, version_bindings};

mod config;
mod messages;
mod paths;
mod request;
mod trace;

const PROVIDER: &[u8] = b"_RCMAKE_FIND_PACKAGE_PROVIDER";
pub(super) const PACKAGE_ROOTS: &[u8] = b"_RCMAKE_FIND_PACKAGE_ROOTS";

pub(super) fn find_package(cx: &mut Cx<'_>, args: &[ExpandedArg]) -> CmdResult {
    find_package_with_diagnostics(cx, args, None)
}

pub(super) fn find_package_with_diagnostics(
    cx: &mut Cx<'_>,
    args: &[ExpandedArg],
    dependency: Option<&mut Vec<cps_import::PackageDiagnostic>>,
) -> CmdResult {
    let Some((package, parameters)) = args.split_first() else {
        return Err(CmdError::status(
            "called with incorrect number of arguments",
        ));
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
                return Err(CmdError::status(
                    "EXACT cannot be specified with a version range.",
                ));
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
        let mut trace = SearchTrace::new(cx, name);
        let result = search(
            cx,
            name,
            &found_var,
            &request,
            &package_roots,
            dependency,
            &mut trace,
        );
        trace.emit(cx);
        result
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
    let version = cx.var(&text(&[name, b"_VERSION"]));
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
    mut dependency: Option<&mut Vec<cps_import::PackageDiagnostic>>,
    trace: &mut SearchTrace,
) -> Result<bool, CmdError> {
    let mut config = ConfigSearch::new(name, request);
    let redirected = redirects_directory(cx).is_some_and(|dir| config.redirect(cx, &dir));
    let prefer_config = !request.module && cx.var_is_on(b"CMAKE_FIND_PACKAGE_PREFER_CONFIG");
    let try_module = request.config_options.is_empty() && !redirected;
    if try_module
        && !prefer_config
        && let Some(found) = load_module(cx, name, found_var, request, trace)?
    {
        return Ok(found);
    }

    if !request.module || redirected {
        let matched = config.find(cx, package_roots, trace)?;
        let config_file = matched.as_ref().map(|found| found.file.clone());
        let found = config.load(cx, found_var, matched)?;
        if !found
            && try_module
            && prefer_config
            && let Some(found) = load_module(cx, name, found_var, request, trace)?
        {
            return Ok(found);
        }
        if let Some(file) = config_file {
            if found && let Some(diagnostics) = dependency.as_deref_mut() {
                let dir = cx.current_dir_id();
                let targets = request
                    .components
                    .iter()
                    .map(|(component, _)| text(&[name, b"::", component]))
                    .filter(|target| cx.project().find_target(dir, target).is_none())
                    .collect::<Vec<_>>();
                if !targets.is_empty() {
                    diagnostics.push(cps_import::PackageDiagnostic::new(text(&[
                        b"Found package configuration file:\n\n  ",
                        &file,
                        b"\n\nbut the following required targets were not found: ",
                        &list::join(targets.iter()),
                    ])));
                    cx.set_var(found_var, "0".into());
                    return Ok(false);
                }
            }
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
        text(&[b"No \"Find", name, b".cmake\" found in CMAKE_MODULE_PATH."])
    } else {
        config.missing_message(try_module)
    };
    if let Some(diagnostics) = dependency {
        if let Some(rejection) = config.rejected_dependency {
            diagnostics.extend(rejection.diagnostics);
        } else {
            diagnostics.push(cps_import::PackageDiagnostic::new(message));
        }
    } else if request.required
        && let Some(rejection) = config.rejected_dependency
    {
        for diagnostic in rejection.diagnostics {
            diagnostic.emit(cx);
        }
        report_missing(cx, request, rejection.reason)?;
    } else if !try_module && !request.module && !request.required && config.considered.is_empty() {
        if !request.quiet && cx.log_level() >= LogLevel::Status {
            cx.host().write_out(
                Stream::Stdout,
                &text(&[
                    b"-- Could NOT find ",
                    name,
                    b" (missing: ",
                    name,
                    b"_DIR)\n",
                ]),
            );
        }
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
    trace: &mut SearchTrace,
) -> Result<Option<bool>, CmdError> {
    let filename = text(&[b"Find", name, b".cmake"]);
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
        .resolve_path(&text(&[&root, b"/Modules"]))
        .join(&native_name);
    let bundled = if is_file(cx, &bundled) {
        Some(bundled)
    } else {
        considered.push(bundled);
        None
    };
    trace.module(
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
