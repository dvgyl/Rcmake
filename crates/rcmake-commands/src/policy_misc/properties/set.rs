use rcmake_base::{BString, list, truth};
use rcmake_diag::{Effect, MessageKind};
use rcmake_eval::{CmdError, CmdResult, Cx, Flow};
use rcmake_expr::ExpandedArg;
use rcmake_model::{AppendMode, CacheType, ModelError, PropOrigin, PropScope, TargetId};

use super::file_set::{file_set_id, file_set_selection, file_set_target, set_file_set};
use super::scope::{
    SourceSelection, check_directory, select_source, source_scopes, test_names, writable_target,
};
use super::{known_features, quoted, values};

pub(in crate::policy_misc) fn set_directory(cx: &mut Cx<'_>, args: &[ExpandedArg]) -> CmdResult {
    let args = values(args);
    let [b"PROPERTIES", pairs @ ..] = args.as_slice() else {
        return Err(CmdError::status("Wrong number of arguments"));
    };
    let dir = PropScope::Directory(cx.current_dir_id());
    for [name, value] in property_pairs(pairs)? {
        update_scoped(cx, &dir, name, Some((*value).into()), Update::Set)?;
    }
    Ok(Flow::Normal)
}

pub(in crate::policy_misc) fn set_sources(cx: &mut Cx<'_>, args: &[ExpandedArg]) -> CmdResult {
    let args = values(args);
    let Some(split) = args.iter().position(|arg| *arg == b"PROPERTIES") else {
        return Err(CmdError::status(
            "called with incorrect number of arguments",
        ));
    };
    let (sources, tail) = args.split_at(split);
    let pairs = property_pairs(tail.get(1..).unwrap_or_default())?;
    for scope in source_scopes(cx, sources, false)? {
        for [name, value] in pairs {
            update_source(cx, &scope, name, Some((*value).into()), Update::Set)?;
        }
    }
    Ok(Flow::Normal)
}

fn property_pairs<'a, 'b>(args: &'a [&'b [u8]]) -> Result<&'a [[&'b [u8]; 2]], CmdError> {
    let (pairs, extra) = args.as_chunks::<2>();
    if !extra.is_empty() {
        return Err(CmdError::status(
            "called with incorrect number of arguments",
        ));
    }
    Ok(pairs)
}

#[derive(Clone, Copy)]
pub(super) enum Update {
    Set,
    Append,
    AppendString,
}

impl Update {
    fn value(self, old: &[u8], new: &[u8]) -> BString {
        match self {
            Self::Set => new.into(),
            Self::Append if !old.is_empty() && !new.is_empty() => list::join([old, new]),
            _ => [old, new].concat().into(),
        }
    }
}

pub(in crate::policy_misc) fn set(cx: &mut Cx<'_>, args: &[ExpandedArg]) -> CmdResult {
    let args = values(args);
    let Some((scope, rest)) = args.split_first() else {
        return Err(CmdError::status(
            "called with incorrect number of arguments",
        ));
    };
    let mut words = rest.splitn(2, |arg| *arg == b"PROPERTY");
    let mut names = words.next().unwrap_or_default().to_vec();
    let Some([name, tail @ ..]) = words.next() else {
        return Err(CmdError::status("not given a PROPERTY <name> argument."));
    };
    let mut update = Update::Set;
    names.retain(|arg| match *arg {
        b"APPEND" => {
            update = Update::Append;
            false
        }
        b"APPEND_STRING" => {
            update = Update::AppendString;
            false
        }
        _ => true,
    });
    let tail = match (*scope, tail.split_last()) {
        (b"INSTALL", Some((last, values))) if *last == b"APPEND" => {
            update = Update::Append;
            values
        }
        _ => tail,
    };
    let value = (!tail.is_empty()).then(|| list::join(tail));
    match *scope {
        b"GLOBAL" if names.is_empty() => {
            if !matches!(
                *name,
                b"IN_TRY_COMPILE" | b"ENABLED_LANGUAGES" | b"GENERATOR_IS_MULTI_CONFIG"
            ) && known_features(name).is_none()
            {
                update_scoped(cx, &PropScope::Global, name, value, update)?;
            }
        }
        b"GLOBAL" => return Err(CmdError::status("given names for GLOBAL scope.")),
        b"DIRECTORY" => {
            let dir = PropScope::Directory(check_directory(cx, &names)?);
            update_scoped(cx, &dir, name, value, update)?;
        }
        b"SOURCE" => {
            for scope in source_scopes(cx, &names, false)? {
                update_source(cx, &scope, name, value.clone(), update)?;
            }
        }
        b"FILE_SET" => {
            let (sets, target) = file_set_selection(&names)?;
            let target_id = file_set_target(cx, target)?;
            for set in sets {
                let id = file_set_id(cx, target_id, target, set)?;
                set_file_set(cx, id, name, value.clone(), update)?;
            }
        }
        b"TARGET" => {
            let targets = names
                .iter()
                .map(|target| writable_target(cx, target, name))
                .collect::<Result<Vec<_>, _>>()?;
            for target in targets {
                if crate::targets::check_target_property(cx, target, name)
                    || !crate::targets::validate_target_property_value(
                        cx,
                        target,
                        name,
                        value
                            .as_ref()
                            .map_or(b"".as_slice(), |value| value.as_slice()),
                        !matches!(update, Update::Set),
                    )
                {
                    continue;
                }
                if *name == b"IMPORTED_GLOBAL" {
                    set_imported_global(cx, target, value.as_ref(), update)?;
                } else {
                    let scope = PropScope::Target(target);
                    update_scoped(cx, &scope, name, value.clone(), update)?;
                    crate::targets::schedule_target_property_validation(cx, name);
                }
            }
        }
        b"TEST" => {
            let (tests, dir) = test_names(cx, &names)?;
            let missing = tests
                .iter()
                .filter(|test| cx.project().find_test(dir, test).is_none())
                .flat_map(|test| [b"\n  ".as_slice(), *test])
                .flatten()
                .copied()
                .collect::<Vec<_>>();
            if !missing.is_empty() {
                return Err(CmdError::status(
                    [
                        b"given TEST names that do not exist:\n".as_slice(),
                        &missing,
                    ]
                    .concat(),
                ));
            }
            for test in tests {
                let scope = PropScope::Test {
                    dir,
                    name: (*test).into(),
                };
                update_scoped(cx, &scope, name, value.clone(), update)?;
            }
        }
        b"INSTALL" => {
            for file in names {
                let scope = PropScope::Install { file: file.into() };
                update_scoped(cx, &scope, name, value.clone(), update)?;
            }
        }
        b"CACHE" => {
            for entry_name in names {
                set_cache(cx, entry_name, name, value.clone(), update)?;
            }
        }
        _ => return Err(CmdError::status(quoted("given invalid scope ", scope, "."))),
    }
    Ok(Flow::Normal)
}

fn set_imported_global(
    cx: &mut Cx<'_>,
    id: TargetId,
    value: Option<&BString>,
    update: Update,
) -> Result<(), CmdError> {
    let dir = cx.current_dir_id();
    let project = cx.project();
    let target = project
        .target(id)
        .ok_or_else(|| CmdError::status(ModelError::UnknownObject.to_string()))?;
    let name = target.name.clone();
    let target_dir = target.dir;
    let invalid_directory =
        target.dir != dir && target.is_imported() && matches!(update, Update::Set);
    let error = if !target.is_imported() {
        Some(format!(
            "IMPORTED_GLOBAL property can't be set on non-imported targets(\"{name}\")\n"
        ))
    } else if !matches!(update, Update::Set) {
        Some(format!(
            "IMPORTED_GLOBAL property can't be appended, only set on imported targets (\"{name}\")\n"
        ))
    } else if !value.is_some_and(|value| rcmake_base::truth::is_on(value)) {
        Some(format!(
            "IMPORTED_GLOBAL property can't be set to FALSE on targets (\"{name}\")\n"
        ))
    } else {
        project
            .set_imported_global(id)
            .map_err(|err| CmdError::status(err.to_string()))?;
        None
    };
    if let Some(error) = error {
        if invalid_directory {
            cx.issue_directory_error(target_dir, error.into());
        } else {
            cx.issue_in_directory(MessageKind::Error, Effect::SetError, None, error.into());
        }
    }
    if invalid_directory {
        cx.issue_in_directory(MessageKind::Error, Effect::SetError, None, format!(
            "Attempt to promote imported target \"{name}\" to global scope (by setting IMPORTED_GLOBAL) which is not built in this directory."
        ).into());
    }
    Ok(())
}

fn update_source(
    cx: &mut Cx<'_>,
    selection: &SourceSelection<'_>,
    name: &[u8],
    value: Option<BString>,
    update: Update,
) -> Result<(), CmdError> {
    if name == b"GENERATED"
        && super::generated::set(
            cx,
            selection.dir,
            selection.source,
            selection.scoped,
            value
                .as_ref()
                .map_or(b"".as_slice(), |value| value.as_slice()),
            update,
        )
    {
        return Ok(());
    }
    if let Some(scope) = select_source(cx, selection.dir, selection.source, selection.scoped, true)
    {
        update_scoped(cx, &scope, name, value, update)?;
    }
    Ok(())
}

/// Apply `update` to a model-owned property scope.
pub(super) fn update_scoped(
    cx: &mut Cx<'_>,
    scope: &PropScope,
    name: &[u8],
    value: Option<BString>,
    update: Update,
) -> Result<(), CmdError> {
    if matches!(scope, PropScope::Directory(_))
        && matches!(
            name,
            b"SUBDIRECTORIES"
                | b"BUILDSYSTEM_TARGETS"
                | b"IMPORTED_TARGETS"
                | b"PARENT_DIRECTORY"
                | b"SOURCE_DIR"
                | b"BINARY_DIR"
                | b"LISTFILE_STACK"
                | b"TESTS"
        )
    {
        return Ok(());
    }
    if let PropScope::Target(id) = scope
        && matches!(name, b"BINARY_DIR" | b"IMPORTED" | b"SOURCE_DIR")
        && let Some(target) = cx.project().target_mut(*id)
    {
        if let Some(value) = value {
            let previous = target
                .props
                .get(name)
                .map_or(b"".as_slice(), |value| value.as_slice());
            target
                .props
                .set(name.into(), update.value(previous, &value));
        } else if matches!(update, Update::Set) {
            target.props.unset(name);
        }
        return Ok(());
    }
    let value = if matches!(scope, PropScope::Target(_))
        && name == b"SOURCES"
        && !matches!(update, Update::AppendString)
    {
        value.map(|value| list::join(list::split(&value, list::Empty::Drop)))
    } else {
        value
    };
    let backtrace = cx.backtrace();
    let origin = PropOrigin {
        dir: cx.current_dir_id(),
        backtrace: &backtrace,
    };
    let project = cx.project();
    let result = match (update, value) {
        (Update::Set, value) => project.set_prop(scope, name, value, origin),
        (Update::Append, Some(value)) => {
            project.append_prop(scope, name, &value, AppendMode::List, origin)
        }
        (Update::AppendString, Some(value)) => {
            project.append_prop(scope, name, &value, AppendMode::String, origin)
        }
        (_, None) => Ok(()),
    };
    match result {
        Ok(()) => {
            if let (PropScope::Test { dir, name: test }, b"WORKING_DIRECTORY") = (scope, name) {
                let value = project.get_prop(scope, name);
                if let Some(id) = project.find_test(*dir, test)
                    && let Some(entry) = project.test_mut(id)
                {
                    entry.working_directory = value;
                }
            }
            Ok(())
        }
        Err(ModelError::ReadOnly) => Err(crate::command_error(
            cx,
            quoted("", name, " property is read-only\n"),
        )),
        Err(other) => Err(CmdError::status(other.to_string())),
    }
}

fn set_cache(
    cx: &mut Cx<'_>,
    entry: &[u8],
    name: &[u8],
    value: Option<BString>,
    update: Update,
) -> Result<(), CmdError> {
    let ty = cx.cache().get(entry).map(|entry| entry.ty).ok_or_else(|| {
        CmdError::status(quoted(
            "could not find CACHE variable ",
            entry,
            ".  Perhaps it has not yet been created.",
        ))
    })?;
    match name {
        b"VALUE" | b"HELPSTRING" | b"STRINGS" => {}
        b"TYPE" => {
            let new = value.as_ref().map_or(b"".as_slice(), AsRef::as_ref);
            let new = update.value(ty.as_str().as_bytes(), new);
            if !CacheType::parse(&new).is_some_and(|ty| ty.as_str().as_bytes() == new.as_slice()) {
                return Err(CmdError::status(quoted(
                    "given invalid CACHE entry TYPE \"",
                    &new,
                    "\"",
                )));
            }
        }
        b"ADVANCED" => {
            if let Some(new) = &value
                && !truth::is_on(new)
                && !truth::is_off(new)
            {
                return Err(CmdError::status(quoted(
                    "given non-boolean value \"",
                    new,
                    "\" for CACHE property \"ADVANCED\".",
                )));
            }
        }
        _ => {
            return Err(CmdError::status(quoted(
                "given invalid CACHE property ",
                name,
                ".  Settable CACHE properties are: ADVANCED, HELPSTRING, STRINGS, TYPE, and VALUE.",
            )));
        }
    }
    check_cache_bytes(cx, entry, name, value.as_ref(), update)?;
    update_scoped(cx, &PropScope::Cache(entry.into()), name, value, update)
}

/// Refuse a cache property write that would pass `limits.max_cache_bytes`, before the
/// entry changes.
fn check_cache_bytes(
    cx: &mut Cx<'_>,
    entry: &[u8],
    name: &[u8],
    value: Option<&BString>,
    update: Update,
) -> Result<(), CmdError> {
    let Some(new) = value else {
        return Ok(());
    };
    let limit = cx.limits().max_cache_bytes;
    let cache = cx.cache();
    let (old_len, key_len) = cache.get(entry).map_or((0, 0), |cached| match name {
        b"VALUE" => (cached.value.len(), 0),
        b"HELPSTRING" => (cached.help.len(), 0),
        b"TYPE" => (0, 0),
        _ => cached
            .props
            .get(name)
            .map_or((0, name.len()), |old| (old.len(), 0)),
    });
    let new_len = match update {
        Update::Set => new.len(),
        Update::Append if old_len > 0 && !new.is_empty() => {
            old_len.saturating_add(1).saturating_add(new.len())
        }
        Update::Append | Update::AppendString => old_len.saturating_add(new.len()),
    };
    let projected = cache
        .bytes()
        .saturating_add(cache.reserved_bytes())
        .saturating_sub(old_len)
        .saturating_add(new_len)
        .saturating_add(key_len);
    if projected > limit {
        cx.set_fatal();
        return Err(CmdError::status(format!(
            "Cache storage would exceed the {limit}-byte limit."
        )));
    }
    Ok(())
}
