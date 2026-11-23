use rcmake_base::{BString, list, truth};
use rcmake_diag::{DiagCategory, Effect, MessageKind, PolicyId, PolicyStatus};
use rcmake_eval::args::{KeywordParser, Slot};
use rcmake_eval::{CmdError, CmdResult, Cx, Flow};
use rcmake_expr::ExpandedArg;
use rcmake_host::path::{from_native, to_native};
use rcmake_model::{CacheEntry, CacheType};

use super::{bytes, cat, status};

const ARG_COUNT: &str = "called with incorrect number of arguments";
const CMP0077: PolicyId = PolicyId::new(77);
const CMP0102: PolicyId = PolicyId::new(102);
const CMP0126: PolicyId = PolicyId::new(126);
// This flag distinguishes a legacy placeholder from an untyped command-line value.
const PLACEHOLDER: &[u8] = b"\0mark_as_advanced_placeholder";

fn wrapped<'a>(name: &'a [u8], prefix: &[u8]) -> Option<&'a [u8]> {
    name.strip_prefix(prefix)?.strip_suffix(b"}")
}

pub(super) fn set(cx: &mut Cx<'_>, args: &[ExpandedArg]) -> CmdResult {
    let [name, rest @ ..] = args else {
        return Err(status(ARG_COUNT));
    };
    let name = bytes(name);
    if name.starts_with(b"ENV{") {
        let key = name
            .get(4..name.len().saturating_sub(1))
            .unwrap_or_default();
        if let [_, extra, ..] = rest {
            cx.issue(
                MessageKind::AuthorWarning,
                Effect::None,
                Some(DiagCategory::Author),
                cat(&[
                    b"Only the first value argument is used when setting an environment variable.  Argument '",
                    bytes(extra),
                    b"' and later are unused.",
                ]),
            );
        }
        let value = rest.first().map_or(b"".as_slice(), bytes);
        if !value.is_empty() || cx.host().env_get(key).is_some() {
            cx.host().env_set(key, Some(value));
        }
    } else if let Some(key) = wrapped(name, b"CACHE{") {
        set_cache_keywords(cx, key, rest)?;
    } else {
        match rest {
            [] => cx.unset_var(name),
            [values @ .., tail] if bytes(tail) == b"PARENT_SCOPE" => {
                let value = (!values.is_empty()).then(|| list::join(values.iter().map(bytes)));
                cx.set_parent_scope(name, value);
            }
            _ => set_normal_or_cache(cx, name, rest)?,
        }
    }
    Ok(Flow::Normal)
}

fn set_normal_or_cache(cx: &mut Cx<'_>, name: &[u8], args: &[ExpandedArg]) -> Result<(), CmdError> {
    let (values, ty, help, force) = match args {
        [.., arg] if bytes(arg) == b"CACHE" => {
            return Err(status(
                "given invalid arguments for CACHE mode: missing type and docstring",
            ));
        }
        [.., arg, _] if bytes(arg) == b"CACHE" => {
            return Err(status(
                "given invalid arguments for CACHE mode: missing type or docstring",
            ));
        }
        [values @ .., keyword, ty, help, force]
            if bytes(keyword) == b"CACHE" && bytes(force) == b"FORCE" =>
        {
            (values, bytes(ty), bytes(help), true)
        }
        [values @ .., keyword, ty, help] if bytes(keyword) == b"CACHE" => {
            if bytes(help) == b"FORCE" {
                return Err(status(
                    "given invalid arguments: FORCE specified without CACHE",
                ));
            }
            (values, bytes(ty), bytes(help), false)
        }
        [.., last] if bytes(last) == b"FORCE" && args.len() >= 4 => {
            return Err(status(
                "given invalid arguments: FORCE specified without CACHE",
            ));
        }
        _ => {
            cx.set_var(name, list::join(args.iter().map(bytes)));
            return Ok(());
        }
    };
    let ty = cache_type(ty).unwrap_or_else(|| {
        cx.issue(
            MessageKind::AuthorWarning,
            Effect::None,
            Some(DiagCategory::Author),
            cat(&[b"implicitly converting '", ty, b"' to 'STRING' type."]),
        );
        CacheType::String
    });
    assign_cache(
        cx,
        name,
        list::join(values.iter().map(bytes)),
        ty,
        help,
        force,
    );
    Ok(())
}

fn cache_type(value: &[u8]) -> Option<CacheType> {
    CacheType::parse(value).filter(|ty| ty.as_str().as_bytes() == value)
}

fn set_cache_keywords(cx: &mut Cx<'_>, name: &[u8], args: &[ExpandedArg]) -> Result<(), CmdError> {
    let Some(value_index) = args.iter().position(|arg| bytes(arg) == b"VALUE") else {
        return Err(status("Required argument 'VALUE' is missing."));
    };
    let (options, value) = args.split_at(value_index + 1);
    let parsed = KeywordParser::new()
        .keyword("TYPE", Slot::Single)
        .keyword("HELP", Slot::MultiNonEmpty)
        .keyword("FORCE", Slot::Flag)
        .keyword("VALUE", Slot::Flag)
        .parse(options);
    let ty = parsed.single("TYPE").and_then(|ty| cache_type(ty));
    let mut errors = parsed
        .report_error_text()
        .map_or_else(Vec::new, |text| text.to_vec());
    if let Some(invalid) = parsed.single("TYPE").filter(|_| ty.is_none()) {
        errors.extend_from_slice(&cat(&[
            b"Error after keyword \"TYPE\":\n  invalid value: \"",
            invalid,
            b"\"\n",
        ]));
    }
    if !errors.is_empty() {
        cx.issue(
            MessageKind::Error,
            Effect::Fatal,
            None,
            BString::from(errors),
        );
        return Err(CmdError::Reported);
    }
    if !parsed.unparsed().is_empty() {
        let unsupported = parsed.unparsed().join(b";".as_slice());
        return Err(status(cat(&[
            b"Called with unsupported argument(s): ",
            &unsupported,
            b".",
        ])));
    }
    let existing = cx.cache().get(name);
    let ty = ty
        .or_else(|| existing.map(|entry| entry.ty))
        .unwrap_or(CacheType::String);
    let help = if parsed.seen("HELP") {
        BString::from(parsed.multi("HELP").concat())
    } else {
        existing.map_or_else(BString::default, |entry| entry.help.clone())
    };
    assign_cache(
        cx,
        name,
        list::join(value.iter().map(bytes)),
        ty,
        &help,
        parsed.flag("FORCE"),
    );
    Ok(())
}

fn assign_cache(
    cx: &mut Cx<'_>,
    name: &[u8],
    mut value: BString,
    ty: CacheType,
    help: &[u8],
    force: bool,
) {
    let force = force || ty == CacheType::Internal;
    if let Some(entry) = cx.cache().get(name) {
        if entry.ty != CacheType::Uninitialized && !force {
            return;
        }
        if entry.ty == CacheType::Uninitialized && entry.props.get(PLACEHOLDER).is_none() {
            if !force {
                value.clone_from(&entry.value);
            }
            if matches!(ty, CacheType::Path | CacheType::Filepath) {
                value = cache_paths(cx, &value);
            }
        }
    }
    write_cache(cx, name, value, ty, help);
    clear_cache_shadow(cx, name);
}

fn clear_cache_shadow(cx: &mut Cx<'_>, name: &[u8]) {
    let policy = cx.policy(CMP0126);
    if policy != PolicyStatus::New {
        if policy == PolicyStatus::Warn
            && cx.var_is_on(b"CMAKE_POLICY_WARNING_CMP0126")
            && cx.var_normal_only(name).is_some()
        {
            cx.issue_policy_warning(
                CMP0126,
                b"",
                &cat(&[
                    b"\nFor compatibility with older versions of CMake, normal variable \"",
                    name,
                    b"\" will be removed from the current scope.",
                ]),
            );
        }
        cx.unset_var(name);
    }
}

fn cache_paths(cx: &Cx<'_>, value: &[u8]) -> BString {
    let directory = cx.host().initial_cwd();
    let home = cx.host().env_get(b"HOME").filter(|home| !home.is_empty());
    list::join(
        list::split(value, list::Empty::Drop)
            .into_iter()
            .map(|path| {
                if truth::is_off(&path) {
                    return path;
                }
                let path = match (home.as_ref(), path.as_slice()) {
                    (Some(home), b"~") => home.clone(),
                    (Some(home), path) if path.starts_with(b"~/") => {
                        cat(&[home, path.get(1..).unwrap_or_default()])
                    }
                    _ => path,
                };
                let path = directory.join(to_native(&path));
                let mut clean = std::path::PathBuf::new();
                for component in path.components() {
                    match component {
                        std::path::Component::ParentDir => {
                            clean.pop();
                        }
                        _ => clean.push(component),
                    }
                }
                from_native(&clean)
            }),
    )
}

fn write_cache(cx: &mut Cx<'_>, name: &[u8], value: BString, ty: CacheType, help: &[u8]) {
    if let Some(entry) = cx.cache().get_mut(name) {
        entry.value = value;
        entry.ty = ty;
        entry.help = BString::from(help);
        entry.props.unset(PLACEHOLDER);
    } else {
        cx.cache().insert(
            name.into(),
            CacheEntry {
                value,
                ty,
                help: help.into(),
                advanced: false,
                props: Default::default(),
            },
        );
    }
}

pub(super) fn unset(cx: &mut Cx<'_>, args: &[ExpandedArg]) -> CmdResult {
    let [name, rest @ ..] = args else {
        return Err(status(ARG_COUNT));
    };
    if rest.len() > 1 {
        return Err(status(ARG_COUNT));
    }
    let name = bytes(name);
    if let Some(key) = wrapped(name, b"ENV{") {
        cx.host().env_set(key, None);
    } else if let Some(key) = wrapped(name, b"CACHE{") {
        cx.cache().remove(key);
    } else {
        match rest.first().map(bytes) {
            None => cx.unset_var(name),
            Some(b"CACHE") => {
                cx.cache().remove(name);
            }
            Some(b"PARENT_SCOPE") => cx.set_parent_scope(name, None),
            Some(_) => return Err(status("called with an invalid second argument")),
        }
    }
    Ok(Flow::Normal)
}

pub(super) fn option(cx: &mut Cx<'_>, args: &[ExpandedArg]) -> CmdResult {
    let [name, help, initial @ ..] = args else {
        return Err(option_count(args));
    };
    if initial.len() > 1 {
        return Err(option_count(args));
    }
    let name = bytes(name);
    let policy = cx.policy(CMP0077);
    let normal = cx.var_normal_only(name).is_some();
    if policy == PolicyStatus::New && normal {
        return Ok(Flow::Normal);
    }
    let on = initial.first().is_some_and(|arg| truth::is_on(bytes(arg)));
    let mut value = BString::from(if on { "ON" } else { "OFF" });
    if let Some(entry) = cx.cache().get_mut(name) {
        if entry.ty != CacheType::Uninitialized {
            entry.help.clone_from(&help.value);
            return Ok(Flow::Normal);
        }
        if entry.props.get(PLACEHOLDER).is_none() {
            value.clone_from(&entry.value);
        }
    }
    write_cache(cx, name, value, CacheType::Bool, bytes(help));
    clear_cache_shadow(cx, name);
    if policy != PolicyStatus::New {
        if normal && policy == PolicyStatus::Warn {
            cx.issue_policy_warning(
                CMP0077,
                b"",
                &cat(&[
                    b"\nFor compatibility with older versions of CMake, option is clearing the normal variable '",
                    name,
                    b"'.",
                ]),
            );
        }
        cx.unset_var(name);
    }
    Ok(Flow::Normal)
}

fn option_count(args: &[ExpandedArg]) -> CmdError {
    let values: Vec<_> = args.iter().map(bytes).collect();
    status(cat(&[
        ARG_COUNT.as_bytes(),
        b": ",
        &values.join(b" ".as_slice()),
    ]))
}

pub(super) fn mark_as_advanced(cx: &mut Cx<'_>, args: &[ExpandedArg]) -> CmdResult {
    let [first, rest @ ..] = args else {
        return Err(status(ARG_COUNT));
    };
    let (advanced, overwrite, names) = match bytes(first) {
        b"CLEAR" => (false, true, rest),
        b"FORCE" => (true, true, rest),
        _ => (true, false, args),
    };
    let create = cx.policy(CMP0102) != PolicyStatus::New;
    for name in names.iter().map(bytes) {
        if create && !cx.cache().contains(name) {
            write_cache(cx, name, BString::default(), CacheType::Uninitialized, b"");
            if let Some(entry) = cx.cache().get_mut(name) {
                entry.props.set(PLACEHOLDER.into(), BString::default());
            }
        }
        if let Some(entry) = cx.cache().get_mut(name)
            && (overwrite || entry.props.get(b"ADVANCED").is_none() && !entry.advanced)
        {
            entry.advanced = advanced;
            entry
                .props
                .set("ADVANCED".into(), if advanced { "1" } else { "0" }.into());
        }
    }
    Ok(Flow::Normal)
}
