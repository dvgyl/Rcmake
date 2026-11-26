use std::collections::{BTreeMap, BTreeSet};

use rcmake_base::{BString, list, path_lex::Lexical};
use rcmake_eval::{CmdError, CmdResult, Cx, Flow};
use rcmake_expr::ExpandedArg;
use rcmake_host::{Stream, path::from_native};
use rcmake_model::{DirId, ProjectModel, SourceGroupDef, TargetBinding, TargetId};
use rcmake_regex::MatchOptions;

use super::values;

const GROUP: &[u8] = b"SOURCE_GROUP";
const HAS_RULES: &[u8] = b"\0source_group_has_regex";
const RULE: &[u8] = b"\0source_group_regex:";
const REGEX_GROUP: &[u8] = b"\0source_group_regex";
const FILE_GROUP: &[u8] = b"\0source_group_file";
const FILE_SET_GROUP: &[u8] = b"\0source_group_file_set";
/// Maximum number of components in one source group name (`a\b\c` has 3).
/// CMake has no limit. The limit keeps the nested group tree, and each walk
/// and drop of it, bounded for untrusted input.
const MAX_GROUP_DEPTH: usize = 256;

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
        let files = tree(cx, &base, &cwd, root, options.prefix, &options.files)?;
        let names = files
            .iter()
            .map(|(_, group)| group_names(group))
            .collect::<Result<Vec<_>, _>>()?;
        let groups = &mut cx.project().dir_mut(dir).source_groups;
        for ((path, _), names) in files.iter().zip(&names) {
            if let Some(found) = group_mut(groups, names) {
                found.files.insert(path.clone());
            }
        }
        files
    } else {
        let group = group_path(
            options
                .group
                .ok_or_else(|| CmdError::status("missing source group name."))?,
        );
        let names = group_names(&group)?;
        let valid_regex = options
            .regex
            .is_some_and(|pattern| cx.regex(pattern).is_ok());
        if let Some(pattern) = options.regex {
            register_regex(cx, dir, &group, pattern, options.legacy)?;
        }
        let files: Vec<_> = options
            .files
            .iter()
            .map(|path| (full_path(cx, &base, path), group.clone()))
            .collect();
        let creates = options.legacy
            || options.regex.is_some()
            || !files.is_empty()
            || !options.file_sets.is_empty();
        if creates {
            let groups = &mut cx.project().dir_mut(dir).source_groups;
            if let Some(found) = group_mut(groups, &names) {
                if valid_regex {
                    found.regex = options.regex.map(BString::from);
                }
                found
                    .files
                    .extend(files.iter().map(|(path, _)| path.clone()));
            }
        }
        assign_file_sets(cx, dir, &group, options.file_sets);
        files
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

/// Split a group name into its components. An empty name selects the default
/// group `""`. A name with more than [`MAX_GROUP_DEPTH`] components is an error.
fn group_names(group: &[u8]) -> Result<Vec<BString>, CmdError> {
    let names: Vec<BString> = group
        .split(|&byte| matches!(byte, b'/' | b'\\'))
        .filter(|part| !part.is_empty())
        .map(BString::from)
        .collect();
    if names.len() > MAX_GROUP_DEPTH {
        return Err(CmdError::status(format!(
            "group name has {} components.  The limit is {MAX_GROUP_DEPTH}.",
            names.len()
        )));
    }
    Ok(if names.is_empty() {
        vec![BString::default()]
    } else {
        names
    })
}

/// Find or create a group by its name components (`GetOrCreateSourceGroup`).
/// A new top-level group goes after all existing groups; a new child goes after
/// its siblings.
fn group_mut<'a>(
    groups: &'a mut Vec<SourceGroupDef>,
    names: &[BString],
) -> Option<&'a mut SourceGroupDef> {
    if groups.is_empty() {
        *groups = rcmake_resolve::ide::default_source_groups();
    }
    let (first, rest) = names.split_first()?;
    let index = match groups.iter().position(|group| group.name == *first) {
        Some(index) => index,
        None => {
            groups.push(SourceGroupDef {
                name: first.clone(),
                ..SourceGroupDef::default()
            });
            groups.len() - 1
        }
    };
    let mut group = groups.get_mut(index)?;
    for name in rest {
        let index = match group.children.iter().position(|child| child.name == *name) {
            Some(index) => index,
            None => {
                group.children.push(SourceGroupDef {
                    name: name.clone(),
                    ..SourceGroupDef::default()
                });
                group.children.len() - 1
            }
        };
        group = group.children.get_mut(index)?;
    }
    Some(group)
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

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::Arc;

    use rcmake_eval::{Interp, InterpConfig, RunMode};
    use rcmake_host::{PureHost, Stream};
    use rcmake_model::{DirId, SourceGroupDef};

    use super::MAX_GROUP_DEPTH;

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    fn configure(source: &str) -> Result<(Interp, Arc<PureHost>, i32), Box<dyn std::error::Error>> {
        let src = PathBuf::from("/work/src");
        let command = PathBuf::from("/bin/cmake");
        let host = Arc::new(PureHost::new(src.clone(), command.clone()));
        host.seed_file(&src.join("CMakeLists.txt"), source.as_bytes())?;
        let mut interp = Interp::new(
            InterpConfig::new(
                host.clone(),
                RunMode::Configure,
                "/share/cmake-4.5",
                command,
                src.clone(),
            ),
            crate::register_all,
        );
        let outcome = interp.run_configure(&src, &PathBuf::from("/work/build"), &[]);
        Ok((interp, host, outcome.exit_code))
    }

    fn depth(groups: &[SourceGroupDef], name: &[u8]) -> usize {
        let mut depth = 0;
        let mut current = groups.iter().find(|group| group.name == name);
        while let Some(group) = current {
            depth += 1;
            current = group.children.first();
        }
        depth
    }

    #[test]
    fn source_group_at_the_depth_limit_resolves_its_full_name() -> TestResult {
        let name = vec!["g"; MAX_GROUP_DEPTH].join("/");
        let (mut interp, host, exit) = configure(&format!(
            "cmake_minimum_required(VERSION 4.5)\nproject(groups NONE)\n\
             source_group(\"{name}\" FILES deep.c REGULAR_EXPRESSION \"[.]x$\")\n"
        ))?;
        assert_eq!(exit, 0, "{}", host.stream_bytes(Stream::Stderr));
        let mut cx = interp.cx();
        let dir = cx.project().dir(DirId::ROOT);
        assert_eq!(depth(&dir.source_groups, b"g"), MAX_GROUP_DEPTH);
        let expected = vec!["g"; MAX_GROUP_DEPTH].join("\\");
        let mut never = |_: &[u8], _: &[u8]| false;
        assert_eq!(
            rcmake_resolve::ide::find_source_group(dir, b"/work/src/deep.c", &mut never),
            expected.as_str()
        );
        let mut regex = |pattern: &[u8], path: &[u8]| pattern == b"[.]x$" && path.ends_with(b".x");
        assert_eq!(
            rcmake_resolve::ide::find_source_group(dir, b"/work/src/other.x", &mut regex),
            expected.as_str()
        );
        Ok(())
    }

    #[test]
    fn source_group_deeper_than_the_limit_is_an_error() -> TestResult {
        let name = vec!["g"; MAX_GROUP_DEPTH + 1].join("\\\\");
        let (mut interp, host, exit) = configure(&format!(
            "cmake_minimum_required(VERSION 4.5)\nproject(groups NONE)\n\
             source_group(\"{name}\" FILES deep.c)\n"
        ))?;
        assert_ne!(exit, 0);
        let stderr = host.stream_bytes(Stream::Stderr).to_string();
        assert!(
            stderr.contains("group name has 257 components."),
            "{stderr}"
        );
        let mut cx = interp.cx();
        let dir = cx.project().dir(DirId::ROOT);
        assert!(dir.source_groups.iter().all(|group| group.name != "g"));
        assert!(dir.source_props.is_empty());
        Ok(())
    }

    #[test]
    fn source_group_tree_deeper_than_the_limit_changes_nothing() -> TestResult {
        let deep = vec!["d"; MAX_GROUP_DEPTH + 1].join("/");
        let (mut interp, host, exit) = configure(&format!(
            "cmake_minimum_required(VERSION 4.5)\nproject(groups NONE)\n\
             source_group(TREE . FILES top.c {deep}/leaf.c)\n"
        ))?;
        assert_ne!(exit, 0);
        assert!(
            host.stream_bytes(Stream::Stderr)
                .to_string()
                .contains("The limit is 256.")
        );
        let mut cx = interp.cx();
        let dir = cx.project().dir(DirId::ROOT);
        assert!(dir.source_groups.iter().all(|group| group.files.is_empty()));
        Ok(())
    }
}
