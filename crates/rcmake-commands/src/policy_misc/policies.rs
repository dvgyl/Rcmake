use rcmake_base::{BString, ByteSlice, CMAKE_VERSION_STR, version::Version};
use rcmake_diag::generated_policies::POLICIES;
use rcmake_diag::{
    DiagAction, DiagCategory, Effect, MessageKind, PolicyError, PolicyId, PolicyStatus,
};
use rcmake_eval::{CmdError, CmdResult, Cx, Flow};
use rcmake_expr::ExpandedArg;

use super::{quoted, status, values};

const VERSION_ADVICE: &str = "Update the VERSION argument <min> value.  Or, use the <min>...<max> syntax to tell CMake that the project requires at least <min> but has been updated to work with policies introduced by <max> or earlier.";

pub(super) fn minimum_required(cx: &mut Cx<'_>, args: &[ExpandedArg]) -> CmdResult {
    let args = values(args);
    let mut version = None;
    let mut unknown = None;
    let mut iter = args.into_iter();
    while let Some(arg) = iter.next() {
        match arg {
            b"VERSION" => {
                version = Some(
                    iter.next()
                        .ok_or_else(|| status("called with no value for VERSION."))?,
                );
            }
            b"FATAL_ERROR" => {}
            _ => {
                unknown.get_or_insert(arg);
            }
        }
    }
    if let Some(arg) = unknown
        && !version.is_some_and(allows_future_options)
    {
        return Err(status(quoted(
            "called with unknown argument \"",
            arg,
            "\".",
        )));
    }
    match version {
        Some(raw) if !raw.is_empty() => apply_version(cx, raw, true),
        _ => Ok(Flow::Normal),
    }
}

pub(super) fn policy(cx: &mut Cx<'_>, args: &[ExpandedArg]) -> CmdResult {
    let values = values(args);
    match values.as_slice() {
        [] => return Err(status("requires at least one argument.")),
        [b"VERSION", raw] => return apply_version(cx, raw, false),
        [b"VERSION"] => return Err(status("VERSION not given an argument")),
        [b"VERSION", ..] => {
            return Err(status("VERSION given too many arguments"));
        }
        [b"SET", name, value] => {
            let id = policy_id(name, "SET")?;
            let state = match *value {
                b"NEW" => PolicyStatus::New,
                b"OLD" => PolicyStatus::Old,
                _ => {
                    return Err(status(quoted(
                        "SET given unrecognized policy status \"",
                        value,
                        "\"",
                    )));
                }
            };
            if let Err(error) = cx.set_policy(id, state) {
                return Err(policy_error(cx, error));
            }
            if state == PolicyStatus::Old && id.number() <= 161 {
                cx.issue(
                    MessageKind::DeprecationWarning,
                    Effect::None,
                    Some(DiagCategory::Deprecated),
                    format!("The OLD behavior for policy {id} will be removed from a future version of CMake.\n\nThe cmake-policies(7) manual explains that the OLD behaviors of all policies are deprecated and that a policy should be set to OLD only under specific short-term circumstances.  Projects should be ported to the NEW behavior and not rely on setting a policy to OLD.").into(),
                );
            }
        }
        [b"GET", name, var] => {
            let state = match cx.policy(policy_id(name, "GET")?) {
                PolicyStatus::New => "NEW",
                PolicyStatus::Old => "OLD",
                PolicyStatus::Warn => "",
            };
            cx.set_var(var, state.into());
        }
        [b"GET", name, var, b"PARENT_SCOPE"] => {
            let id = policy_id(name, "GET")?;
            let state = match cx.parent_policy(id) {
                Some(PolicyStatus::New) => "NEW",
                Some(PolicyStatus::Old) => "OLD",
                Some(PolicyStatus::Warn) | None => "",
            };
            cx.set_var(var, state.into());
        }
        [b"ISSUE_WARNING", name, options @ ..] => {
            let id = policy_id(name, "ISSUE_WARNING")?;
            let (mut preface, mut postface) = (Vec::new(), Vec::new());
            let mut destination: Option<&mut Vec<u8>> = None;
            for option in options {
                match *option {
                    b"PRE" => destination = Some(&mut preface),
                    b"POST" => destination = Some(&mut postface),
                    value => {
                        if let Some(destination) = &mut destination {
                            destination.extend_from_slice(value);
                        }
                    }
                }
            }
            issue_warning(cx, id, &preface, &postface);
        }
        [b"ISSUE_WARNING"] => {
            return Err(subcommand_error(
                args,
                "must be given at least one additional argument.",
            ));
        }
        [b"SET" | b"GET", ..] => {
            return Err(subcommand_error(
                args,
                "must be given exactly 2 additional arguments.",
            ));
        }
        [b"PUSH"] => cx.push_policy_scope(),
        [b"POP"] => cx.pop_policy_scope()?,
        [b"PUSH" | b"POP", ..] => {
            return Err(subcommand_error(
                args,
                "may not be given additional arguments.",
            ));
        }
        [op, ..] => return Err(status(quoted("given unknown first argument \"", op, "\""))),
    }
    Ok(Flow::Normal)
}

fn policy_id(name: &[u8], op: &str) -> Result<PolicyId, CmdError> {
    PolicyId::parse(name).ok_or_else(|| {
        status(quoted(
            &format!("{op} given policy \""),
            name,
            "\" which is not known to this version of CMake.",
        ))
    })
}

fn issue_warning(cx: &mut Cx<'_>, id: PolicyId, preface: &[u8], postface: &[u8]) {
    let summary = match id.number() {
        133 => "The CPack module disables SLA by default in the CPack DragNDrop Generator.",
        161 => "CPACK_PRODUCTBUILD_DOMAINS defaults to true.",
        172 => {
            "The CPack module enables per-machine installation by default in the CPack WIX Generator."
        }
        _ => return cx.issue_policy_warning(id, preface, postface),
    };
    let message = format!(
        "Policy {id} is not set: {summary}  Run \"cmake --help-policy {id}\" for policy details.  Use the cmake_policy command to set the policy and suppress this warning."
    );
    let body = [preface, message.as_bytes(), postface]
        .into_iter()
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join(b"\n".as_slice());
    cx.issue(
        MessageKind::AuthorWarning,
        Effect::None,
        Some(DiagCategory::Policy),
        body.into(),
    );
}

fn parse_version(raw: &[u8]) -> Option<Version> {
    let raw = raw.trim_ascii_start();
    let mut end = 0;
    let mut parts = 0;
    let mut rest = raw;
    while parts < 4 {
        let digits = rest.iter().take_while(|byte| byte.is_ascii_digit()).count();
        if digits == 0 {
            break;
        }
        end += digits;
        parts += 1;
        let Some(next) = raw.get(end..)?.strip_prefix(b".") else {
            break;
        };
        if parts == 4 || !next.first().is_some_and(u8::is_ascii_digit) {
            break;
        }
        end += 1;
        rest = next;
    }
    if parts < 2 {
        return None;
    }
    Version::parse_strict(raw.get(..end)?)
}

fn running_version() -> Version {
    Version::from_tuple(
        rcmake_base::CMAKE_VERSION_MAJOR,
        rcmake_base::CMAKE_VERSION_MINOR,
        rcmake_base::CMAKE_VERSION_PATCH,
    )
}

fn allows_future_options(raw: &[u8]) -> bool {
    let policy_version = raw
        .find(b"...")
        .and_then(|at| raw.get(at + 3..))
        .unwrap_or(raw);
    parse_version(policy_version).is_some_and(|version| version > running_version())
}

fn apply_version(cx: &mut Cx<'_>, raw: &[u8], required: bool) -> CmdResult {
    let (minimum, maximum) = match raw.find(b"...") {
        Some(at) => {
            let min = raw.get(..at).unwrap_or_default();
            let max = raw.get(at + 3..).unwrap_or_default();
            if min.is_empty() || max.is_empty() {
                return Err(status(quoted(
                    "VERSION \"",
                    raw,
                    "\" does not have a version on both sides of \"...\".",
                )));
            }
            (min, Some(max))
        }
        None => (raw, None),
    };
    let minimum_version = parse_version(minimum).ok_or_else(|| {
        if required {
            status(quoted("could not parse VERSION \"", minimum, "\"."))
        } else {
            invalid_version(cx, "policy version", minimum)
        }
    })?;
    if minimum_version > running_version() {
        let body = if required {
            quoted(
                "CMake ",
                minimum,
                &format!(" or higher is required.  You are running version {CMAKE_VERSION_STR}"),
            )
        } else {
            quoted(
                "An attempt was made to set the policy version of CMake to \"",
                minimum,
                "\" which is greater than this version of CMake.  This is not allowed because the greater version may have new policies not known to this CMake.  You may need a newer CMake version to build this project.",
            )
        };
        return Err(fatal(cx, body));
    }

    if !required && minimum_version < Version::from_tuple(2, 4, 0) {
        return Err(fatal(cx, "Compatibility with CMake < 2.4 is not supported by CMake >= 3.0.  For compatibility with older versions please use any CMake 2.8.x release or lower.".into()));
    }
    let mut effective = if let Some(max) = maximum {
        let version =
            parse_version(max).ok_or_else(|| invalid_version(cx, "policy max version", max))?;
        if version < minimum_version {
            return Err(version_error(
                cx,
                quoted(
                    "Policy VERSION range \"",
                    raw,
                    "\" specifies a later minimum than maximum.",
                ),
            ));
        }
        version
    } else {
        minimum_version
    };
    let floor_name = b"CMAKE_POLICY_VERSION_MINIMUM";
    let floor = cx.var_or_empty(floor_name);
    if !floor.is_empty() {
        let version = parse_version(&floor)
            .ok_or_else(|| invalid_version(cx, "CMAKE_POLICY_VERSION_MINIMUM", &floor))?;
        effective = effective.max(version);
    }
    // Older install(EXPORT) files set policy version 2.6 in a policy scope.
    let legacy_export = !required
        && (effective.major(), effective.minor(), effective.patch()) == (2, 6, 0)
        && cx.parent_policy(PolicyId::new(66)).is_some();
    if legacy_export {
        effective = Version::from_tuple(3, 5, 0);
    }
    let result = if required {
        cx.set_cmake_minimum_required(minimum, &effective)
    } else {
        cx.apply_policy_version(&effective)
    };
    result.map_err(|error| policy_error(cx, error))?;
    for policy in POLICIES {
        if cx.policy(policy.id) != PolicyStatus::Warn {
            continue;
        }
        let name = format!("CMAKE_POLICY_DEFAULT_{}", policy.id);
        let value = cx.var_or_empty(name.as_bytes());
        let state = match value.as_slice() {
            b"" => continue,
            b"NEW" => PolicyStatus::New,
            b"OLD" => PolicyStatus::Old,
            _ => {
                return Err(fatal(
                    cx,
                    quoted(
                        &format!("{name} has value \""),
                        &value,
                        "\" but must be \"OLD\", \"NEW\", or \"\" (empty).",
                    ),
                ));
            }
        };
        cx.set_policy(policy.id, state)
            .map_err(|error| policy_error(cx, error))?;
    }
    if !legacy_export && effective < Version::from_tuple(3, 10, 0) {
        cx.issue(
            MessageKind::DeprecationWarning,
            Effect::None,
            Some(DiagCategory::Deprecated),
            format!("Compatibility with CMake < 3.10 will be removed from a future version of CMake.\n\n{VERSION_ADVICE}").into(),
        );
    }
    Ok(Flow::Normal)
}

fn invalid_version(cx: &mut Cx<'_>, field: &str, raw: &[u8]) -> CmdError {
    version_error(
        cx,
        quoted(
            &format!("Invalid {field} value \""),
            raw,
            "\".  A numeric major.minor[.patch[.tweak]] must be given.",
        ),
    )
}

fn version_error(cx: &mut Cx<'_>, message: BString) -> CmdError {
    cx.issue(MessageKind::Error, Effect::SetError, None, message);
    CmdError::Reported
}

fn policy_error(cx: &mut Cx<'_>, error: PolicyError) -> CmdError {
    let message = match error {
        PolicyError::VersionBelowMinimum => format!(
            "{error}\n\n{VERSION_ADVICE}\n\nOr, add -DCMAKE_POLICY_VERSION_MINIMUM=3.5 to try configuring anyway."
        ),
        PolicyError::RemovedOldBehavior(id) => {
            let version = id.info().map_or((0, 0, 0), |info| info.introduced);
            format!(
                "Policy {id} may not be set to OLD behavior because this version of CMake no longer supports it.  The policy was introduced in CMake version {}.{}.{}, and use of NEW behavior is now required.\n\nPlease either update your CMakeLists.txt files to conform to the new behavior or use an older version of CMake that still supports the old behavior.  Run cmake --help-policy {id} for more information.",
                version.0, version.1, version.2
            )
        }
        PolicyError::UnknownPolicy(_) => error.to_string(),
    };
    fatal(cx, message.into())
}

fn fatal(cx: &mut Cx<'_>, message: BString) -> CmdError {
    cx.issue(MessageKind::Error, Effect::Fatal, None, message);
    CmdError::Reported
}

pub(super) fn diagnostic(cx: &mut Cx<'_>, args: &[ExpandedArg]) -> CmdResult {
    let values = values(args);
    match values.as_slice() {
        [] => return Err(status("requires at least one argument.")),
        [b"PUSH"] => cx.push_diag_scope(),
        [b"POP"] => cx.pop_diag_scope()?,
        [b"PUSH" | b"POP", ..] => {
            return Err(subcommand_error(
                args,
                "may not be given additional arguments.",
            ));
        }
        [op @ b"GET", cat, var] => {
            let cat = category(op, cat)?;
            let value = cx.diag_state().get(cat).as_str();
            cx.set_var(var, value.into());
        }
        [b"GET", ..] => {
            return Err(subcommand_error(
                args,
                "must be given exactly 2 additional arguments.",
            ));
        }
        [
            op @ (b"SET" | b"PROMOTE" | b"DEMOTE"),
            cat,
            action,
            flags @ ..,
        ] if flags.len() <= 1 => {
            let cat = category(op, cat)?;
            let op_text = op.to_str_lossy();
            let action = DiagAction::parse(action).ok_or_else(|| {
                status(quoted(
                    &format!("{op_text} given unrecognized diagnostic action \""),
                    action,
                    "\".",
                ))
            })?;
            let recurse = match flags {
                [] => *op != b"SET",
                [b"RECURSE"] => true,
                [b"NO_RECURSE"] => false,
                [other, ..] => {
                    return Err(status(quoted(
                        &format!("{op_text} given unknown option \""),
                        other,
                        "\".",
                    )));
                }
            };
            cx.update_diag_state(|state| match *op {
                b"SET" => state.set(cat, action, recurse),
                b"PROMOTE" => state.promote(cat, action, recurse),
                _ => state.demote(cat, action, recurse),
            });
        }
        [b"SET" | b"PROMOTE" | b"DEMOTE", ..] => {
            return Err(subcommand_error(
                args,
                "must be given exactly 2 or 3 additional arguments.",
            ));
        }
        [op, ..] => return Err(status(quoted("given unknown first argument \"", op, "\""))),
    }
    Ok(Flow::Normal)
}

fn category(op: &[u8], cat: &[u8]) -> Result<DiagCategory, CmdError> {
    let op = op.to_str_lossy();
    DiagCategory::parse(cat).ok_or_else(|| {
        status(quoted(
            &format!("{op} given unrecognized diagnostic category \""),
            cat,
            "\".",
        ))
    })
}

fn subcommand_error(args: &[ExpandedArg], message: &str) -> CmdError {
    let op = args.first().map_or(b"".as_slice(), |a| a.value.as_slice());
    status([op, b" ", message.as_bytes()].concat())
}
