use rcmake_base::{BString, list};
use rcmake_diag::{Effect, MessageKind};
use rcmake_eval::{CmdError, Cx};
use rcmake_model::{FileSetId, FileSetKind, PropScope, TargetId, UsageScope};

use crate::targets::{find_file_set, replace_file_set_paths, require_sycl, scope_name};

use super::set::{Update, update_scoped};
use super::{model_value, quoted};

pub(super) fn file_set_selection<'a, 'b>(
    names: &'a [&'b [u8]],
) -> Result<(&'a [&'b [u8]], &'b [u8]), CmdError> {
    let Some(index) = names.iter().position(|word| *word == b"TARGET") else {
        return Err(CmdError::status("required TARGET option is missing"));
    };
    let (sets, tail) = names.split_at(index);
    match tail {
        [_, target] => Ok((sets, target)),
        [_] => Err(CmdError::status(
            "called with incorrect number of arguments no value provided to the TARGET option",
        )),
        [_, _, extra, ..] => Err(CmdError::status(quoted(
            "given invalid argument \"",
            extra,
            "\".",
        ))),
        [] => Err(CmdError::status("required TARGET option is missing")),
    }
}

pub(super) fn file_set_target(cx: &mut Cx<'_>, target: &[u8]) -> Result<TargetId, CmdError> {
    let dir = cx.current_dir_id();
    cx.project()
        .find_target(dir, target)
        .ok_or_else(|| CmdError::status(quoted("given non-existent TARGET ", target, "")))
}

pub(super) fn file_set_id(
    cx: &mut Cx<'_>,
    target_id: TargetId,
    target: &[u8],
    set: &[u8],
) -> Result<FileSetId, CmdError> {
    find_file_set(cx.project(), target_id, set).ok_or_else(|| {
        CmdError::status(format!(
            "could not find FILE_SET {} for TARGET {}.  Perhaps it has not yet been created.",
            BString::from(set),
            BString::from(target)
        ))
    })
}

pub(super) fn set_file_set(
    cx: &mut Cx<'_>,
    id: FileSetId,
    name: &[u8],
    value: Option<BString>,
    update: Update,
) -> Result<(), CmdError> {
    let model = cx.project();
    let set = model
        .file_set(id)
        .ok_or_else(|| CmdError::status("file set does not exist"))?;
    match name {
        b"TYPE" | b"SCOPE" => {
            let target = model
                .target(set.target)
                .ok_or_else(|| CmdError::status("target does not exist"))?;
            let body = format!(
                "{} property is read-only for the file set \"{} of the target \"{}\"\n",
                BString::from(name),
                set.name,
                target.name
            );
            cx.issue(MessageKind::Error, Effect::SetError, None, body.into());
            Ok(())
        }
        b"BASE_DIRS" | b"SOURCES" | b"INTERFACE_SOURCES" => {
            if (name == b"SOURCES" && set.scope == UsageScope::Interface)
                || (name == b"INTERFACE_SOURCES" && set.scope == UsageScope::Private)
                || (value.is_none() && !matches!(update, Update::Set))
            {
                return Ok(());
            }
            replace_file_set_paths(
                cx,
                id,
                name == b"BASE_DIRS",
                value,
                !matches!(update, Update::Set),
            )
        }
        b"LANGUAGE" => {
            if !matches!(update, Update::Set) {
                return Err(crate::command_error(
                    cx,
                    "LANGUAGE file set property does not support appending values.",
                ));
            }
            if value
                .as_ref()
                .is_some_and(|value| value.windows(2).any(|pair| pair == b"$<"))
            {
                return Err(crate::command_error(
                    cx,
                    "LANGUAGE file set property does not support generator expressions.",
                ));
            }
            match set.kind {
                FileSetKind::Headers => {
                    require_sycl(cx, "LANGUAGE property on a HEADERS file set")?
                }
                FileSetKind::CxxModules => {
                    return Err(crate::command_error(
                        cx,
                        "LANGUAGE property may only be set on a SOURCES, HEADERS, or custom rule file set.",
                    ));
                }
                FileSetKind::Sources => {}
            }
            update_scoped(cx, &PropScope::FileSet(id), name, value, update)
        }
        _ => update_scoped(cx, &PropScope::FileSet(id), name, value, update),
    }
}

pub(super) fn file_set_value(
    cx: &mut Cx<'_>,
    id: FileSetId,
    name: &[u8],
    inherited: bool,
) -> Option<BString> {
    let set = cx.project().file_set(id)?;
    let entries = match name {
        b"TYPE" => return Some(set.kind.as_str().into()),
        b"SCOPE" => return Some(scope_name(set.scope).into()),
        b"BASE_DIRS" => &set.base_dirs,
        b"SOURCES" if set.scope != UsageScope::Interface => &set.files,
        b"INTERFACE_SOURCES" if set.scope != UsageScope::Private => &set.files,
        b"SOURCES" | b"INTERFACE_SOURCES" => return None,
        _ => return model_value(cx, &PropScope::FileSet(id), name, inherited),
    };
    (!entries.is_empty()).then(|| list::join(entries.iter().map(|entry| &entry.value)))
}
