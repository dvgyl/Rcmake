use rcmake_base::{BString, truth};
use rcmake_diag::{PolicyId, PolicyStatus};
use rcmake_eval::args::{KeywordParser, Slot};
use rcmake_eval::{CmdError, CmdResult, Cx, Flow};
use rcmake_expr::ExpandedArg;
use rcmake_model::CacheType;

use super::{bytes, cat};

const ARG_COUNT: &str = "called with incorrect number of arguments";
const CMP0077: PolicyId = PolicyId::new(77);
const CMP0102: PolicyId = PolicyId::new(102);

fn join_expanded(args: &[ExpandedArg]) -> BString {
    let Some((first, rest)) = args.split_first() else {
        return BString::default();
    };
    if rest.is_empty() {
        return first.value.clone();
    }
    let total = args
        .iter()
        .map(|arg| arg.value.len())
        .sum::<usize>()
        .saturating_add(rest.len());
    let mut out = Vec::with_capacity(total);
    out.extend_from_slice(&first.value);
    for arg in rest {
        out.push(b';');
        out.extend_from_slice(&arg.value);
    }
    BString::from(out)
}

fn wrapped<'a>(name: &'a [u8], prefix: &[u8]) -> Option<&'a [u8]> {
    name.strip_prefix(prefix)?.strip_suffix(b"}")
}

pub(super) fn set(cx: &mut Cx<'_>, args: &[ExpandedArg]) -> CmdResult {
    let [name, rest @ ..] = args else {
        return Err(CmdError::status(ARG_COUNT));
    };
    let name = bytes(name);
    if name.starts_with(b"ENV{") {
        let key = name
            .get(4..name.len().saturating_sub(1))
            .unwrap_or_default();
        if let [_, extra, ..] = rest {
            crate::author_warning(
                cx,
                cat(&[
                    b"Only the first value argument is used when setting an environment variable.  Argument '",
                    bytes(extra),
                    b"' and later are unused.",
                ]),
            );
        }
        let value = rest.first().map_or(b"".as_slice(), bytes);
        set_env_var(cx.host(), key, value, cfg!(windows));
    } else if let Some(key) = wrapped(name, b"CACHE{") {
        set_cache_keywords(cx, key, rest)?;
    } else {
        match rest {
            [] => cx.unset_var(name),
            [values @ .., tail] if bytes(tail) == b"PARENT_SCOPE" => {
                let value = (!values.is_empty()).then(|| join_expanded(values));
                cx.set_parent_scope(name, value);
            }
            _ => set_normal_or_cache(cx, name, rest)?,
        }
    }
    Ok(Flow::Normal)
}

fn set_env_var(host: &dyn rcmake_host::Host, key: &[u8], value: &[u8], windows: bool) {
    if value.is_empty() && windows {
        host.env_set(key, None);
    } else if !value.is_empty() || host.env_get(key).is_some() {
        host.env_set(key, Some(value));
    }
}

fn set_normal_or_cache(cx: &mut Cx<'_>, name: &[u8], args: &[ExpandedArg]) -> Result<(), CmdError> {
    if let [single] = args
        && bytes(single) != b"CACHE"
    {
        cx.set_var(name, single.value.clone());
        return Ok(());
    }
    let (values, ty, help, force) = match args {
        [.., arg] if bytes(arg) == b"CACHE" => {
            return Err(CmdError::status(
                "given invalid arguments for CACHE mode: missing type and docstring",
            ));
        }
        [.., arg, _] if bytes(arg) == b"CACHE" => {
            return Err(CmdError::status(
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
                return Err(CmdError::status(
                    "given invalid arguments: FORCE specified without CACHE",
                ));
            }
            (values, bytes(ty), bytes(help), false)
        }
        [.., last] if bytes(last) == b"FORCE" && args.len() >= 4 => {
            return Err(CmdError::status(
                "given invalid arguments: FORCE specified without CACHE",
            ));
        }
        _ => {
            cx.set_var(name, join_expanded(args));
            return Ok(());
        }
    };
    let ty = cache_type(ty).unwrap_or_else(|| {
        crate::author_warning(
            cx,
            cat(&[b"implicitly converting '", ty, b"' to 'STRING' type."]),
        );
        CacheType::String
    });
    assign_cache(cx, name, join_expanded(values), ty, help, force);
    Ok(())
}

fn cache_type(value: &[u8]) -> Option<CacheType> {
    CacheType::parse(value).filter(|ty| ty.as_str().as_bytes() == value)
}

fn set_cache_keywords(cx: &mut Cx<'_>, name: &[u8], args: &[ExpandedArg]) -> Result<(), CmdError> {
    let Some(value_index) = args.iter().position(|arg| bytes(arg) == b"VALUE") else {
        return Err(CmdError::status("Required argument 'VALUE' is missing."));
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
        return Err(crate::command_error(cx, errors));
    }
    if !parsed.unparsed().is_empty() {
        let unsupported = parsed.unparsed().join(b";".as_slice());
        return Err(CmdError::status(cat(&[
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
        join_expanded(value),
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
        if entry.ty == CacheType::Uninitialized && entry.props.get(Cx::CACHE_PLACEHOLDER).is_none()
        {
            if !force {
                value.clone_from(&entry.value);
            }
            if matches!(ty, CacheType::Path | CacheType::Filepath) {
                value = cx.cache_paths(&value);
            }
        }
    }
    if cx.write_cache(name, value, ty, help) {
        cx.clear_cache_shadow(name);
    }
}

pub(super) fn unset(cx: &mut Cx<'_>, args: &[ExpandedArg]) -> CmdResult {
    let [name, rest @ ..] = args else {
        return Err(CmdError::status(ARG_COUNT));
    };
    if rest.len() > 1 {
        return Err(CmdError::status(ARG_COUNT));
    }
    let name = bytes(name);
    if let Some(key) = wrapped(name, b"ENV{") {
        cx.host().env_set(key, None);
    } else if let Some(key) = wrapped(name, b"CACHE{") {
        cx.remove_cache_entry(key);
    } else {
        match rest.first().map(bytes) {
            None => cx.unset_var(name),
            Some(b"CACHE") => {
                cx.remove_cache_entry(name);
            }
            Some(b"PARENT_SCOPE") => cx.set_parent_scope(name, None),
            Some(_) => return Err(CmdError::status("called with an invalid second argument")),
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
        if entry.props.get(Cx::CACHE_PLACEHOLDER).is_none() {
            value.clone_from(&entry.value);
        }
    }
    cx.write_cache(name, value, CacheType::Bool, bytes(help));
    cx.clear_cache_shadow(name);
    if policy != PolicyStatus::New {
        if normal && policy == PolicyStatus::Warn {
            cx.issue_policy_warning(
                CMP0077,
                b"",
                &cat(&[
                    b"For compatibility with older versions of CMake, option is clearing the normal variable '",
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
    CmdError::status(cat(&[
        ARG_COUNT.as_bytes(),
        b": ",
        &values.join(b" ".as_slice()),
    ]))
}

pub(super) fn mark_as_advanced(cx: &mut Cx<'_>, args: &[ExpandedArg]) -> CmdResult {
    let [first, rest @ ..] = args else {
        return Err(CmdError::status(ARG_COUNT));
    };
    let (advanced, overwrite, names) = match bytes(first) {
        b"CLEAR" => (false, true, rest),
        b"FORCE" => (true, true, rest),
        _ => (true, false, args),
    };
    let create = cx.policy(CMP0102) != PolicyStatus::New;
    for name in names.iter().map(bytes) {
        if create && !cx.cache().contains(name) {
            cx.write_cache(name, BString::default(), CacheType::Uninitialized, b"");
            if let Some(entry) = cx.cache().get_mut(name) {
                entry
                    .props
                    .set(Cx::CACHE_PLACEHOLDER.into(), BString::default());
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

#[cfg(test)]
mod unit_tests {
    use rcmake_host::{Host, PureHost};

    use super::set_env_var;

    #[test]
    fn windows_empty_env_set_removes_variable() {
        let host = PureHost::default();
        host.env_set(b"SET_THEN_EMPTY", Some(b"set"));
        set_env_var(&host, b"SET_THEN_EMPTY", b"", true);
        assert_eq!(host.env_get(b"SET_THEN_EMPTY"), None);

        set_env_var(&host, b"UNSET_THEN_EMPTY", b"", true);
        assert_eq!(host.env_get(b"UNSET_THEN_EMPTY"), None);

        host.env_set(b"POSIX_EMPTY", Some(b"set"));
        set_env_var(&host, b"POSIX_EMPTY", b"", false);
        assert_eq!(host.env_get(b"POSIX_EMPTY"), Some("".into()));
    }
}
