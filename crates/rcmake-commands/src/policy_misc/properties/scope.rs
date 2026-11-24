use rcmake_base::{BString, list};
use rcmake_eval::{CmdError, Cx, RunMode};
use rcmake_host::path;
use rcmake_model::{DirId, PropScope, TargetBinding, TargetId};

use crate::dirs::{find_directory, normalized_path, scoped_test_directory};

use super::quoted;

const DIRECTORY_ERROR: &str = "DIRECTORY scope provided but requested directory was not found.  This could be because the directory argument was invalid or, it is valid but has not been processed yet.";
pub(super) const RESOLVED_SOURCE_LOCATION: &[u8] = b"\0rcmake-source-location";

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
    let paths = std::iter::once(&source_path).chain(relative.then_some(&binary_path));
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
                        .is_some_and(|ext| {
                            SOURCE_EXTENSIONS
                                .split_ascii_whitespace()
                                .any(|known| ext == known.as_bytes())
                        })
        })
    };
    let model = cx.project();
    let directory = model.dir(dir);
    let known = directory
        .source_props
        .keys()
        .find(|key| matches(key))
        .cloned();
    let key = known.or_else(|| {
        model
            .targets()
            .filter(|(_, target)| target.dir == dir)
            .flat_map(|(_, target)| &target.sources)
            .flat_map(|entry| list::iter(&entry.value, list::Empty::Drop))
            .filter(|entry| !entry.starts_with(b"$<"))
            .flat_map(|entry| {
                let entry = path::to_native(&entry);
                [&directory.source_dir, &directory.binary_dir].map(|base| {
                    path::from_native_cmake(&normalized_path(&path::to_native(base).join(&entry)))
                })
            })
            .find(|key| matches(key))
    });
    if !create {
        return key.map(|path| PropScope::Source { dir, path });
    }
    let key = key.unwrap_or(source_path);
    cx.project()
        .dir_mut(dir)
        .source_props
        .entry(key.clone())
        .or_default();
    Some(PropScope::Source { dir, path: key })
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
