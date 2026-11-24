use rcmake_base::BString;
use rcmake_eval::{CmdError, Cx, RunMode};
use rcmake_host::path;
use rcmake_model::{DirId, PropScope, TargetBinding, TargetId};

use crate::dirs::{find_directory, normalized_path, scoped_test_directory};

use super::quoted;

const DIRECTORY_ERROR: &str = "DIRECTORY scope provided but requested directory was not found.  This could be because the directory argument was invalid or, it is valid but has not been processed yet.";

pub(super) fn source_scope(cx: &mut Cx<'_>, dir: DirId, source: &[u8]) -> PropScope {
    let resolved = normalized_path(&cx.resolve_path(source));
    let mut key = path::from_native(&resolved);
    let binary = normalized_path(&cx.current_binary_dir().join(path::to_native(source)));
    let binary = path::from_native(&binary);
    let known = &mut cx.project().dir_mut(dir).source_props;
    if !known.contains_key(&key) {
        if known.contains_key(&binary) {
            key = binary;
        } else if resolved.extension().is_none()
            && let Some(existing) = known.keys().find(|candidate| {
                [key.as_slice(), binary.as_slice()].iter().any(|prefix| {
                    candidate
                        .strip_prefix(*prefix)
                        .is_some_and(|suffix| suffix.starts_with(b".") && !suffix.contains(&b'/'))
                })
            })
        {
            key = existing.clone();
        }
    }
    known.entry(key.clone()).or_default();
    PropScope::Source { dir, path: key }
}

pub(super) fn single_name<'a>(scope: &[u8], names: &[&'a [u8]]) -> Result<&'a [u8], CmdError> {
    match names {
        [name] => Ok(name),
        [] => Err(CmdError::status(quoted(
            "not given name for ",
            scope,
            " scope.",
        ))),
        [_, extra, ..] => Err(CmdError::status(quoted(
            "given invalid argument \"",
            extra,
            "\".",
        ))),
    }
}

pub(super) fn test_names<'a, 'b>(
    cx: &mut Cx<'_>,
    names: &'a [&'b [u8]],
) -> Result<(&'a [&'b [u8]], DirId), CmdError> {
    match names {
        [tests @ .., b"DIRECTORY", directory] => Ok((tests, scoped_test_directory(cx, directory)?)),
        [.., b"DIRECTORY"] => Err(CmdError::status(
            "called with incorrect number of arguments no value provided to the DIRECTORY option",
        )),
        _ => Ok((names, cx.current_dir_id())),
    }
}

pub(super) fn source_scopes(
    cx: &mut Cx<'_>,
    mut names: &[&[u8]],
    query: bool,
) -> Result<Vec<PropScope>, CmdError> {
    let is_option = |word: &[u8]| matches!(word, b"DIRECTORY" | b"TARGET_DIRECTORY");
    let section_end = |words: &[&[u8]]| {
        words
            .iter()
            .position(|word| is_option(word))
            .unwrap_or(words.len())
    };
    let (sources, rest) = names.split_at(section_end(names));
    names = rest;
    if query {
        single_name(b"SOURCE", sources)?;
    }
    let mut dirs = Vec::new();
    while let Some((option, rest)) = names.split_first() {
        let (selected, tail) = rest.split_at(section_end(rest));
        if selected.is_empty() {
            return Err(CmdError::status(quoted(
                "called with incorrect number of arguments no value provided to the ",
                option,
                " option",
            )));
        }
        if query {
            single_name(option, selected)?;
        }
        for name in selected {
            let dir = source_directory(cx, option, name)?;
            if !dirs.contains(&dir) {
                dirs.push(dir);
            }
        }
        names = tail;
    }
    if dirs.is_empty() {
        dirs.push(cx.current_dir_id());
    }
    let mut scopes = Vec::new();
    for dir in dirs {
        scopes.extend(sources.iter().map(|source| source_scope(cx, dir, source)));
    }
    Ok(scopes)
}

pub(super) fn source_directory(
    cx: &mut Cx<'_>,
    option: &[u8],
    value: &[u8],
) -> Result<DirId, CmdError> {
    if option == b"DIRECTORY" {
        if cx.mode() != RunMode::Script
            && let Some(dir) = find_directory(cx, value)
        {
            return Ok(dir);
        }
        return Err(CmdError::status(quoted(
            "given non-existent DIRECTORY ",
            value,
            "",
        )));
    }
    let target = find_target(cx, value)
        .ok()
        .and_then(|id| cx.project().target(id));
    target.map(|target| target.dir).ok_or_else(|| {
        CmdError::status(quoted(
            "given non-existent target for TARGET_DIRECTORY ",
            value,
            "",
        ))
    })
}

pub(super) fn find_target(cx: &mut Cx<'_>, target: &[u8]) -> Result<TargetId, CmdError> {
    let dir = cx.current_dir_id();
    cx.project().find_target(dir, target).ok_or_else(|| {
        CmdError::status(quoted(
            "could not find TARGET ",
            target,
            ".  Perhaps it has not yet been created.",
        ))
    })
}

pub(super) fn writable_target(cx: &mut Cx<'_>, name: &[u8]) -> Result<TargetId, CmdError> {
    let dir = cx.current_dir_id();
    if matches!(
        cx.project().lookup_target(dir, name),
        Some(TargetBinding::Alias(_))
    ) {
        return Err(CmdError::status("can not be used on an ALIAS target."));
    }
    find_target(cx, name)
}

pub(super) fn alias_value(cx: &mut Cx<'_>, target: &[u8], name: &[u8]) -> Option<BString> {
    let dir = cx.current_dir_id();
    let model = cx.project();
    let TargetBinding::Alias(id) = model.lookup_target(dir, target)? else {
        return None;
    };
    let target = model.target(id)?;
    match name {
        b"ALIASED_TARGET" => Some(target.name.clone()),
        b"ALIAS_GLOBAL" => Some(if target.is_global() { "TRUE" } else { "FALSE" }.into()),
        _ => None,
    }
}

pub(super) fn check_directory(cx: &mut Cx<'_>, names: &[&[u8]]) -> Result<DirId, CmdError> {
    match names {
        [] => Ok(cx.current_dir_id()),
        [name] => find_directory(cx, name).ok_or_else(|| CmdError::status(DIRECTORY_ERROR)),
        _ => Err(CmdError::status(
            "given more than one name for DIRECTORY scope.",
        )),
    }
}
