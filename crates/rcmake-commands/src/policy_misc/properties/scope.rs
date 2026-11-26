use rcmake_base::BString;
use rcmake_eval::{CmdError, Cx, RunMode};
use rcmake_host::{Follow, path};
use rcmake_model::{DirId, PropScope, TargetBinding, TargetId};

use crate::custom::{SOURCE_BINARY_ALTERNATIVE, bind_source, record_source_order};
use crate::dirs::{find_directory, normalized_path, scoped_test_directory};

use super::quoted;

const DIRECTORY_ERROR: &str = "DIRECTORY scope provided but requested directory was not found.  This could be because the directory argument was invalid or, it is valid but has not been processed yet.";
pub(super) const RESOLVED_SOURCE_LOCATION: &[u8] = b"\0rcmake-source-location";
const ABSOLUTE_SOURCE_PATH: &[u8] = b"\0rcmake-source-absolute";

pub(super) fn source_scope(cx: &mut Cx<'_>, dir: DirId, source: &[u8]) -> PropScope {
    select_source(cx, dir, source, false, true).unwrap_or_else(|| PropScope::Source {
        dir,
        path: source.into(),
    })
}

pub(super) fn select_source(
    cx: &mut Cx<'_>,
    dir: DirId,
    source: &[u8],
    scoped: bool,
    create: bool,
) -> Option<PropScope> {
    let [source_path, binary_path] = source_paths(cx, source);
    let relative = !scoped && !path::to_native(source).is_absolute();
    let existing = relative
        .then(|| {
            [&source_path, &binary_path]
                .into_iter()
                .find(|path| source_file_resolves_path(cx, path))
        })
        .flatten();
    let first = existing.unwrap_or(&source_path);
    let alternative =
        (relative && existing.is_none() && source_path != binary_path).then_some(&binary_path);
    let paths = std::iter::once(first).chain(alternative);
    let exact_extension = [115, 187].into_iter().all(|id| {
        super::generated::policy(cx, dir, rcmake_diag::PolicyId::new(id))
            == rcmake_diag::PolicyStatus::New
    });
    let matches = |candidate: &[u8]| {
        paths.clone().any(|prefix| {
            candidate == prefix.as_slice()
                || !exact_extension
                    && candidate
                        .strip_prefix(prefix.as_slice())
                        .and_then(|suffix| suffix.strip_prefix(b"."))
                        .is_some_and(known_source_extension)
        })
    };
    let known = cx
        .project()
        .dir(dir)
        .source_props
        .iter()
        .find_map(|(key, props)| {
            if let Some(resolved) = props.get(RESOLVED_SOURCE_LOCATION) {
                return matches(resolved).then(|| (key.clone(), resolved.clone()));
            }
            if matches(key) {
                return Some((key.clone(), key.clone()));
            }
            props
                .get(SOURCE_BINARY_ALTERNATIVE)
                .filter(|alternate| matches(alternate))
                .map(|alternate| (key.clone(), alternate.clone()))
        });
    let key = match known {
        Some((key, matched)) => {
            if key != matched {
                bind_source(cx.project().dir_mut(dir), &key, &matched);
            }
            matched
        }
        None if !create => return None,
        None => {
            let key = first.clone();
            let directory = cx.project().dir_mut(dir);
            let props = directory.source_props.entry(key.clone()).or_default();
            if let Some(alternate) = alternative {
                // An absolute lookup can bind this record to either directory.
                props.set(SOURCE_BINARY_ALTERNATIVE.into(), alternate.clone());
                record_source_order(directory, source, true);
            } else {
                record_source_order(directory, &key, false);
            }
            key
        }
    };
    if !relative || existing.is_some() {
        let props = cx
            .project()
            .dir_mut(dir)
            .source_props
            .entry(key.clone())
            .or_default();
        props.unset(SOURCE_BINARY_ALTERNATIVE);
        if !relative {
            props.set(ABSOLUTE_SOURCE_PATH.into(), "1".into());
        }
    }
    Some(PropScope::Source { dir, path: key })
}

pub(super) fn source_file_resolves_path(cx: &Cx<'_>, source: &[u8]) -> bool {
    let file = path::to_native(source);
    // A recognized extension has a definite spelling even if its directory is ambiguous.
    !file
        .extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| known_source_extension(ext.as_bytes()))
        && cx.host().metadata(&file, Follow::Yes).is_ok()
}

fn known_source_extension(extension: &[u8]) -> bool {
    SOURCE_EXTENSIONS
        .split_ascii_whitespace()
        .any(|known| extension == known.as_bytes())
}

pub(super) const SOURCE_EXTENSIONS: &str = "c C c++ cc cpp cxx cu mpp m M mm ixx cppm ccm cxxm c++m h hh h++ hm hpp hxx in txx f F for f77 f90 f95 f03 hip ispc";

pub(super) fn source_paths(cx: &Cx<'_>, source: &[u8]) -> [BString; 2] {
    let file = path::to_native(source);
    [cx.current_source_dir(), cx.current_binary_dir()]
        .map(|base| path::from_native_cmake(&normalized_path(&base.join(&file))))
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

pub(super) struct SourceSelection<'a> {
    pub dir: DirId,
    pub source: &'a [u8],
    pub scoped: bool,
}

pub(super) fn source_scopes<'a>(
    cx: &mut Cx<'_>,
    mut names: &[&'a [u8]],
    query: bool,
) -> Result<Vec<SourceSelection<'a>>, CmdError> {
    let is_option = |word: &[u8]| matches!(word, b"DIRECTORY" | b"TARGET_DIRECTORY");
    let section_end = |words: &[&[u8]]| {
        words
            .iter()
            .position(|word| is_option(word))
            .unwrap_or(words.len())
    };
    let (sources, rest) = names.split_at(section_end(names));
    names = rest;
    let scoped = !rest.is_empty();
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
        scopes.extend(sources.iter().map(|&source| SourceSelection {
            dir,
            source,
            scoped,
        }));
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

pub(super) fn writable_target(
    cx: &mut Cx<'_>,
    name: &[u8],
    property: &[u8],
) -> Result<TargetId, CmdError> {
    let dir = cx.current_dir_id();
    if matches!(property, b"ALIASED_TARGET" | b"ALIAS_GLOBAL")
        && matches!(
            cx.project().lookup_target(dir, name),
            Some(TargetBinding::Alias(_))
        )
    {
        return Err(CmdError::status(format!(
            "can not set property {} on an ALIAS target: {}",
            BString::from(property),
            BString::from(name),
        )));
    }
    let id = find_target(cx, name)?;
    if cx.project().target(id).is_some_and(|target| {
        target
            .props
            .get(b"SYMBOLIC")
            .is_some_and(|value| rcmake_base::truth::is_on(value))
    }) {
        return Err(CmdError::status("can not be used on a SYMBOLIC target."));
    }
    Ok(id)
}

pub(super) fn alias_value(cx: &mut Cx<'_>, target: &[u8], name: &[u8]) -> Option<BString> {
    let dir = cx.current_dir_id();
    let model = cx.project();
    let TargetBinding::Alias(id) = model.lookup_target(dir, target)? else {
        return None;
    };
    match name {
        b"ALIASED_TARGET" => Some(model.target(id)?.name.clone()),
        b"ALIAS_GLOBAL" => Some(
            if model.dir(dir).local_targets.contains_key(target) {
                "FALSE"
            } else {
                "TRUE"
            }
            .into(),
        ),
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
