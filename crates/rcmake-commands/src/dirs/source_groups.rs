use std::collections::BTreeSet;

use rcmake_base::{BString, list, path_lex::Lexical};
use rcmake_eval::{CmdError, CmdResult, Cx, Flow};
use rcmake_expr::ExpandedArg;
use rcmake_host::path::from_native;
use rcmake_model::{DirId, ProjectModel, TargetId};
use rcmake_regex::MatchOptions;

use super::values;

const GROUP: &[u8] = b"SOURCE_GROUP";
const HAS_RULES: &[u8] = b"\0source_group_has_regex";
const RULE: &[u8] = b"\0source_group_regex:";
const REGEX_GROUP: &[u8] = b"\0source_group_regex";
const FILE_GROUP: &[u8] = b"\0source_group_file";

pub(super) fn command(cx: &mut Cx<'_>, args: &[ExpandedArg]) -> CmdResult {
    let Some((name, rest)) = args.split_first() else {
        return Err(CmdError::status(
            "called with incorrect number of arguments",
        ));
    };
    let dir = cx.current_dir_id();
    let base = cx.project().dir(dir).source_dir.clone();
    let (files, regex) = if name.value == b"TREE" {
        let cwd = from_native(&cx.host().initial_cwd());
        (tree(&base, &cwd, rest)?, None)
    } else {
        named(&base, &name.value, rest)?
    };

    if let Some((group, pattern)) = regex {
        cx.regex(pattern).map_err(|_| {
            CmdError::status(format!(
                "regular expression \"{}\" is invalid",
                BString::from(pattern)
            ))
        })?;
        let key = BString::from([RULE, &group].concat());
        let props = cx.directory_props();
        props.set(HAS_RULES.into(), "1".into());
        props.unset(&key);
        props.set(key, pattern.into());

        let model = cx.project();
        let directory = model.dir(dir);
        let mut paths = directory
            .source_props
            .keys()
            .chain(&directory.listfiles)
            .cloned()
            .collect::<BTreeSet<_>>();
        for &id in &directory.targets {
            paths.extend(target_paths(model, id));
        }
        apply_regex(cx, dir, paths)?;
    }

    let directory = cx.project().dir_mut(dir);
    for (path, group) in files {
        let props = directory.source_props.entry(path).or_default();
        if props.get(GROUP).is_none()
            || props.get(GROUP) == props.get(FILE_GROUP)
            || props.get(GROUP) == props.get(REGEX_GROUP)
        {
            props.unset(REGEX_GROUP);
            props.set(FILE_GROUP.into(), group.clone());
            props.set(GROUP.into(), group);
        }
    }
    Ok(Flow::Normal)
}

type Assignment = (BString, BString);
type RegexRule<'a> = (BString, &'a [u8]);

fn named<'a>(
    base: &[u8],
    name: &[u8],
    args: &'a [ExpandedArg],
) -> Result<(Vec<Assignment>, Option<RegexRule<'a>>), CmdError> {
    let group = group_path(name);
    let mut args = values(args).peekable();
    let mut files = Vec::new();
    let mut regex = None;
    while let Some(arg) = args.next() {
        match arg {
            b"FILES" => {
                while let Some(&path) = args
                    .peek()
                    .filter(|&&arg| !matches!(arg, b"REGULAR_EXPRESSION" | b"FILES"))
                {
                    args.next();
                    if !path.is_empty() {
                        files.push((absolute(base, path), group.clone()));
                    }
                }
            }
            b"REGULAR_EXPRESSION" => {
                if args
                    .peek()
                    .is_some_and(|&next| !matches!(next, b"FILES" | b"REGULAR_EXPRESSION"))
                {
                    regex = args.next().map(|pattern| (group.clone(), pattern));
                }
            }
            legacy if files.is_empty() && regex.is_none() && args.peek().is_none() => {
                regex = Some((group.clone(), legacy));
            }
            _ => return Err(CmdError::status("called with incorrect arguments")),
        }
    }
    Ok((files, regex))
}

fn tree(base: &[u8], cwd: &[u8], args: &[ExpandedArg]) -> Result<Vec<Assignment>, CmdError> {
    let Some((root, rest)) = args.split_first() else {
        return Err(CmdError::status(
            "given invalid argument:\n\n  TREE: missing required value",
        ));
    };
    let root = absolute(cwd, &root.value);
    let root_path = Lexical::new(&root);
    let mut prefix = BString::default();
    let mut files = Vec::new();
    let mut args = values(rest).peekable();
    while let Some(key) = args.next() {
        match key {
            b"PREFIX" => {
                prefix = group_path(
                    args.next()
                        .filter(|&arg| !matches!(arg, b"FILES" | b"PREFIX"))
                        .ok_or_else(|| {
                            CmdError::status(
                                "given invalid argument:\n\n  PREFIX: missing required value",
                            )
                        })?,
                );
            }
            b"FILES" => {
                while let Some(&path) = args
                    .peek()
                    .filter(|&&arg| !matches!(arg, b"PREFIX" | b"FILES"))
                {
                    args.next();
                    if !path.is_empty() {
                        files.push(absolute(base, path));
                    }
                }
            }
            _ => return Err(CmdError::status("called with incorrect arguments")),
        }
    }
    files
        .into_iter()
        .map(|path| {
            let file = Lexical::new(&path);
            let relative = file
                .relative(root_path)
                .filter(|_| root_path.prefix(file))
                .ok_or_else(|| {
                    CmdError::status(format!("\"{root}\" is not a prefix of file \"{path}\""))
                })?;
            let parent = group_path(Lexical::new(&relative).parent());
            let group = if prefix.is_empty() || parent.is_empty() {
                [prefix.as_slice(), parent.as_slice()].concat().into()
            } else {
                [prefix.as_slice(), b"\\", parent.as_slice()]
                    .concat()
                    .into()
            };
            Ok((path, group))
        })
        .collect()
}

fn group_path(path: &[u8]) -> BString {
    path.split(|&byte| matches!(byte, b'/' | b'\\'))
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join(&b'\\')
        .into()
}

fn absolute(base: &[u8], value: &[u8]) -> BString {
    Lexical::new(&Lexical::new(base).append(value)).normalize()
}

fn target_paths(model: &ProjectModel, id: TargetId) -> BTreeSet<BString> {
    let Some(target) = model.target(id) else {
        return BTreeSet::new();
    };
    let base = &model.dir(target.dir).source_dir;
    target
        .sources
        .iter()
        .chain(
            target
                .file_sets
                .iter()
                .filter_map(|&id| model.file_set(id))
                .flat_map(|set| &set.files),
        )
        .flat_map(|entry| list::split(&entry.value, list::Empty::Drop))
        .filter(|source| !source.windows(2).any(|pair| pair == b"$<"))
        .map(|source| absolute(base, &source))
        .collect()
}

pub(crate) fn apply_to_target(cx: &mut Cx<'_>, id: TargetId) -> Result<(), CmdError> {
    let model = cx.project();
    let Some(dir) = model.target(id).map(|target| target.dir) else {
        return Ok(());
    };
    if model.dir(dir).props.get(HAS_RULES).is_none() {
        return Ok(());
    }
    let paths = target_paths(model, id);
    apply_regex(cx, dir, paths)
}

fn apply_regex(cx: &mut Cx<'_>, dir: DirId, paths: BTreeSet<BString>) -> Result<(), CmdError> {
    let props = &cx.project().dir(dir).props;
    let rules: Vec<_> = props
        .names()
        .into_iter()
        .rev()
        .filter_map(|key| {
            Some((
                BString::from(key.strip_prefix(RULE)?),
                props.get(&key)?.clone(),
            ))
        })
        .collect();
    let mut regexes = Vec::new();
    for (group, pattern) in rules {
        let regex = cx
            .regex(&pattern)
            .map_err(|error| CmdError::status(error.to_string()))?;
        regexes.push((group, regex));
    }

    let mut budget = cx.limits().regex_steps;
    let directory = cx.project().dir_mut(dir);
    for path in paths {
        if let Some(props) = directory.source_props.get(&path)
            && props.get(GROUP).is_some()
            && props.get(GROUP) != props.get(REGEX_GROUP)
        {
            continue;
        }
        let mut matched = None;
        for (group, regex) in &regexes {
            if regex
                .find_at(&path, 0, MatchOptions::default(), &mut budget)
                .map_err(|error| CmdError::status(error.to_string()))?
                .is_some()
            {
                matched = Some(group);
                break;
            }
        }
        if let Some(group) = matched {
            let props = directory.source_props.entry(path).or_default();
            props.set(REGEX_GROUP.into(), group.clone());
            props.set(GROUP.into(), group.clone());
        } else if let Some(props) = directory.source_props.get_mut(&path)
            && props.get(REGEX_GROUP).is_some()
        {
            props.unset(REGEX_GROUP);
            props.unset(GROUP);
        }
    }
    Ok(())
}
