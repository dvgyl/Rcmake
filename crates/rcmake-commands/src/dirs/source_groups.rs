use std::collections::{BTreeMap, BTreeSet};

use rcmake_base::{BString, list, path_lex::Lexical};
use rcmake_eval::{CmdError, CmdResult, Cx, Flow};
use rcmake_expr::ExpandedArg;
use rcmake_host::{Stream, path::from_native};
use rcmake_model::{DirId, ProjectModel, TargetBinding, TargetId};
use rcmake_regex::MatchOptions;

use super::values;

const GROUP: &[u8] = b"SOURCE_GROUP";
const HAS_RULES: &[u8] = b"\0source_group_has_regex";
const RULE: &[u8] = b"\0source_group_regex:";
const REGEX_GROUP: &[u8] = b"\0source_group_regex";
const FILE_GROUP: &[u8] = b"\0source_group_file";
const FILE_SET_GROUP: &[u8] = b"\0source_group_file_set";

pub(super) fn command(cx: &mut Cx<'_>, args: &[ExpandedArg]) -> CmdResult {
    if args.is_empty() {
        return Err(CmdError::status(
            "called with incorrect number of arguments",
        ));
    }
    let dir = cx.current_dir_id();
    let base = cx.project().dir(dir).source_dir.clone();
    let options = parse(cx, args)?;
    let files = if let Some(root) = options.root {
        let cwd = from_native(&cx.host().initial_cwd());
        tree(cx, &base, &cwd, root, options.prefix, &options.files)?
    } else {
        let group = group_path(
            options
                .group
                .ok_or_else(|| CmdError::status("missing source group name."))?,
        );
        if let Some(pattern) = options.regex {
            register_regex(cx, dir, &group, pattern, options.legacy)?;
        }
        assign_file_sets(cx, dir, &group, options.file_sets);
        options
            .files
            .into_iter()
            .map(|path| (full_path(cx, &base, path), group.clone()))
            .collect()
    };

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
type FileSets<'a> = BTreeMap<&'a [u8], BTreeSet<&'a [u8]>>;

#[derive(Default)]
struct Options<'a> {
    group: Option<&'a [u8]>,
    root: Option<&'a [u8]>,
    prefix: &'a [u8],
    regex: Option<&'a [u8]>,
    legacy: bool,
    files: Vec<&'a [u8]>,
    file_sets: FileSets<'a>,
}

fn keyword(arg: &[u8]) -> bool {
    old_keyword(arg) || matches!(arg, b"FILE_SETS" | b"TARGET")
}

fn old_keyword(arg: &[u8]) -> bool {
    matches!(arg, b"TREE" | b"PREFIX" | b"FILES" | b"REGULAR_EXPRESSION")
}

fn parse<'a>(cx: &mut Cx<'_>, args: &'a [ExpandedArg]) -> Result<Options<'a>, CmdError> {
    if let [group, pattern] = args
        && !values(args).any(old_keyword)
    {
        return Ok(Options {
            group: Some(&group.value),
            regex: Some(&pattern.value),
            legacy: true,
            ..Options::default()
        });
    }

    let tree = values(args).any(|arg| arg == b"TREE");
    let mut options = Options::default();
    let mut missing = BTreeSet::new();
    let mut targets_without_sets = BTreeSet::new();
    let mut pending_sets = None::<Vec<&[u8]>>;
    let mut unknown = None;

    for section in args.chunk_by(|_, next| !keyword(&next.value)) {
        let Some((key, arguments)) = section.split_first() else {
            continue;
        };
        let key = key.value.as_slice();
        let ignored = if tree {
            matches!(key, b"REGULAR_EXPRESSION" | b"FILE_SETS" | b"TARGET")
        } else {
            key == b"PREFIX"
        };
        if ignored {
            if arguments.is_empty() {
                missing.insert(key);
            }
            for _ in arguments {
                crate::author_warning(
                    cx,
                    format!(
                        "keyword \"{}\" will be ignored in this context.",
                        BString::from(key)
                    ),
                );
            }
            continue;
        }

        let mut arguments = values(arguments);
        match key {
            b"FILES" => options
                .files
                .extend(arguments.filter(|path| !path.is_empty())),
            b"FILE_SETS" => pending_sets.get_or_insert_with(Vec::new).extend(arguments),
            b"TARGET" => {
                let Some(first) = arguments.next() else {
                    missing.insert(key);
                    continue;
                };
                if let Some(sets) = pending_sets.take() {
                    if !sets.is_empty() {
                        options.file_sets.entry(first).or_default().extend(sets);
                    }
                    if let Some(extra) = arguments.next() {
                        targets_without_sets.insert(extra);
                    }
                } else {
                    targets_without_sets.insert(first);
                }
                if let Some(extra) = arguments.next() {
                    unknown.get_or_insert(extra);
                }
            }
            b"TREE" | b"PREFIX" | b"REGULAR_EXPRESSION" => {
                if let Some(value) = arguments.next() {
                    match key {
                        b"TREE" => options.root = Some(value),
                        b"PREFIX" => options.prefix = value,
                        _ => options.regex = Some(value),
                    }
                    if let Some(extra) = arguments.next() {
                        unknown.get_or_insert(extra);
                    }
                } else if key != b"REGULAR_EXPRESSION" {
                    missing.insert(key);
                }
            }
            _ if tree => {
                unknown.get_or_insert(key);
            }
            _ => {
                options.group = Some(key);
                if let Some(extra) = arguments.next() {
                    unknown.get_or_insert(extra);
                }
            }
        }
    }

    if let Some(arg) = unknown {
        return Err(CmdError::status(
            [b"given unknown argument: \"".as_slice(), arg, b"\"."].concat(),
        ));
    }
    if !missing.is_empty() || !targets_without_sets.is_empty() {
        return Err(invalid_arguments(missing, targets_without_sets));
    }
    if !tree && options.group.is_none() {
        return Err(CmdError::status("missing source group name."));
    }
    if let Some(sets) = pending_sets {
        return Err(CmdError::status(
            [
                b"TARGET is required for FILE_SETS ".as_slice(),
                &sets.join(b", ".as_slice()),
                b".",
            ]
            .concat(),
        ));
    }
    Ok(options)
}

fn invalid_arguments(missing: BTreeSet<&[u8]>, targets: BTreeSet<&[u8]>) -> CmdError {
    let mut errors = BTreeMap::<_, Vec<BString>>::new();
    for key in missing {
        errors
            .entry(key)
            .or_default()
            .push("missing required value".into());
    }
    for target in targets {
        errors.entry(b"TARGET".as_slice()).or_default().push(
            [
                b"FILE_SETS is required for TARGET ".as_slice(),
                target,
                b".",
            ]
            .concat()
            .into(),
        );
    }
    let mut body = BString::from(if errors.len() == 1 {
        "given invalid argument:"
    } else {
        "given invalid arguments:"
    });
    for (key, messages) in errors {
        for message in messages {
            body.extend_from_slice(&[b"\n  ".as_slice(), key, b": ", &message].concat());
        }
    }
    CmdError::status(body)
}

fn assign_file_sets(cx: &mut Cx<'_>, dir: DirId, group: &BString, targets: FileSets<'_>) {
    for (name, sets) in targets {
        let model = cx.project();
        let target = model.lookup_target(dir, name).and_then(|binding| {
            let TargetBinding::Target(id) = binding else {
                return None;
            };
            model
                .target(id)
                .filter(|target| target.dir == dir && !target.is_imported())
                .map(|_| id)
        });
        let name = BString::from(name);
        let Some(target) = target else {
            crate::author_warning(
                cx,
                format!("TARGET \"{name}\" is not defined in this directory.  It will be ignored."),
            );
            continue;
        };
        for set in sets {
            let model = cx.project();
            if let Some(id) = crate::targets::find_file_set(model, target, set)
                && let Some(found) = model.file_set_mut(id)
            {
                found.props.set(FILE_SET_GROUP.into(), group.clone());
            } else {
                let set = BString::from(set);
                crate::author_warning(
                    cx,
                    format!(
                        "FILE_SET \"{set}\" is not known for TARGET \"{name}\".  It will ignored."
                    ),
                );
            }
        }
    }
}

fn register_regex(
    cx: &mut Cx<'_>,
    dir: DirId,
    group: &BString,
    pattern: &[u8],
    legacy: bool,
) -> Result<(), CmdError> {
    if let Err(error) = cx.regex(pattern) {
        cx.host().write_out(
            Stream::Stdout,
            format!(
                "{}\nRegularExpression::compile(): Error in compile.\n",
                error.message
            )
            .as_bytes(),
        );
        if legacy {
            return Ok(());
        }
        return Err(CmdError::status(format!(
            "regular expression \"{}\" is invalid",
            BString::from(pattern)
        )));
    }
    let props = cx.directory_props();
    props.set(HAS_RULES.into(), "1".into());
    props.set([RULE, group.as_slice()].concat().into(), pattern.into());

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
    apply_regex(cx, dir, paths)
}

fn tree(
    cx: &Cx<'_>,
    base: &[u8],
    cwd: &[u8],
    root: &[u8],
    prefix: &[u8],
    files: &[&[u8]],
) -> Result<Vec<Assignment>, CmdError> {
    let root = full_path(cx, cwd, root);
    let root_path = Lexical::new(&root);
    let prefix = group_path(prefix);
    files
        .iter()
        .map(|path| {
            let path = full_path(cx, base, path);
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

fn full_path(cx: &Cx<'_>, base: &[u8], value: &[u8]) -> BString {
    let value: BString = value
        .iter()
        .map(|&byte| if byte == b'\\' { b'/' } else { byte })
        .collect::<Vec<_>>()
        .into();
    let home = if matches!(value.as_slice(), b"~" | [b'~', b'/', ..]) {
        cx.host().env_get(b"HOME").filter(|home| !home.is_empty())
    } else {
        None
    };
    let path = match home {
        Some(home) => Lexical::absolute_from(&home, value.strip_prefix(b"~/").unwrap_or_default()),
        None => Lexical::absolute_from(base, &value),
    };
    let lexical = Lexical::new(&path);
    if lexical.relative_part().is_empty() {
        path
    } else {
        path.strip_suffix(b"/").unwrap_or(&path).into()
    }
}

fn group_path(path: &[u8]) -> BString {
    path.split(|&byte| matches!(byte, b'/' | b'\\'))
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join(&b'\\')
        .into()
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
        .map(|source| Lexical::absolute_from(base, &source))
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
