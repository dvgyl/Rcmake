use std::collections::{BTreeMap, BTreeSet};
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use rcmake_base::{BString, list, truth};
use rcmake_diag::{PolicyId, PolicyStatus};
use rcmake_eval::{Cx, RunMode};
use rcmake_host::{
    FileKind, Follow,
    path::{from_native_cmake, to_native},
};
use rcmake_model::{GlobCall, GlobCheck, GlobKey};
use rcmake_regex::{MatchOptions, Regex};

use super::{Result, error, set, warning};
use rcmake_base::path_lex::Lexical;

pub(super) fn run(cx: &mut Cx<'_>, recursive: bool, args: &[&[u8]]) -> Result {
    let command: &[u8] = if recursive { b"GLOB_RECURSE" } else { b"GLOB" };
    let [out, rest @ ..] = args else {
        return Err(error(
            [
                command,
                b" must be called with at least one additional argument.",
            ]
            .concat(),
        ));
    };
    let mut args = rest.iter().copied();
    let mut options = Options {
        recursive,
        directories: !recursive,
        follow: recursive && cx.policy(PolicyId::new(9)) != PolicyStatus::New,
        relative: None,
    };
    let mut output = BTreeSet::new();
    let mut budget = cx.limits().regex_steps;
    let mut bytes = 0_usize;
    let mut evaluated = false;
    let mut expressions = Vec::<Expression<'_>>::new();
    let mut configure_depends = false;
    macro_rules! flush_globs {
        () => {
            Glob::new(
                cx,
                &options,
                &mut output,
                &mut budget,
                &mut bytes,
                &expressions,
                configure_depends,
            )
            .run(out)?;
            expressions.clear();
        };
    }
    while let Some(input) = args.next() {
        match input {
            b"LIST_DIRECTORIES" => {
                flush_globs!();
                options.directories = match args.next() {
                    Some(value) if truth::is_on(value) => true,
                    Some(value) if truth::is_off(value) => false,
                    _ => return Err(error("LIST_DIRECTORIES missing bool value.")),
                };
            }
            b"RELATIVE" => {
                flush_globs!();
                let base = args.next().ok_or_else(|| {
                    error([command, b" requires a directory after the RELATIVE tag."].concat())
                })?;
                if args.len() == 0 {
                    return Err(error(
                        [command, b" requires a glob expression after the directory."].concat(),
                    ));
                }
                options.relative = (!base.is_empty()).then(|| base.into());
            }
            b"FOLLOW_SYMLINKS" if recursive => {
                flush_globs!();
                if args.len() == 0 {
                    return Err(error(
                        "GLOB_RECURSE requires a glob expression after FOLLOW_SYMLINKS.",
                    ));
                }
                options.follow = true;
            }
            b"CONFIGURE_DEPENDS" if cx.mode() == RunMode::Script => {
                flush_globs!();
                return Err(crate::command_error(
                    cx,
                    "CONFIGURE_DEPENDS is invalid for script and find package modes.",
                ));
            }
            b"CONFIGURE_DEPENDS" => {
                flush_globs!();
                if args.len() == 0 {
                    return Err(error(
                        [
                            command,
                            b" requires a glob expression after CONFIGURE_DEPENDS.",
                        ]
                        .concat(),
                    ));
                }
                if evaluated && !configure_depends {
                    warning(
                        cx,
                        "CONFIGURE_DEPENDS flag was given after a glob expression was already evaluated.",
                    );
                }
                configure_depends = true;
            }
            pattern => {
                evaluated = true;
                expressions.push(Expression::new(cx, pattern)?);
            }
        }
    }
    flush_globs!();
    set(cx, out, list::join(output))
}

fn record(
    cx: &mut Cx<'_>,
    variable: &[u8],
    pattern: &[u8],
    options: &Options,
    files: &BTreeSet<BString>,
) -> Result<()> {
    let key = GlobKey {
        expression: from_native_cmake(&cx.resolve_path(pattern)),
        recursive: options.recursive,
        directories: options.directories,
        follow: options.follow,
        relative: options.relative.clone(),
    };
    let files = list::join(files);
    let call = GlobCall {
        variable: variable.into(),
        backtrace: cx.backtrace(),
    };
    let root = rcmake_host::path::from_native_cmake(&cx.current_source_dir());
    let Some(previous) = cx.project().glob_checks.get_mut(&key) else {
        cx.project().glob_checks.insert(
            key,
            GlobCheck {
                files,
                calls: vec![call],
            },
        );
        return Ok(());
    };
    if previous.files == files {
        previous.calls.push(call);
        return Ok(());
    }
    let locations = previous
        .calls
        .iter()
        .map(|record| {
            if let Some(frame) = record.backtrace.top() {
                let file = frame
                    .file
                    .strip_prefix(root.as_slice())
                    .and_then(|path| path.strip_prefix(b"/"))
                    .unwrap_or(&frame.file);
                format!(
                    "  {} at {}:{} (file)",
                    record.variable,
                    BString::from(file),
                    frame.line
                )
            } else {
                format!("  {}", record.variable)
            }
        })
        .collect::<Vec<_>>()
        .join("\n");
    Err(crate::command_error(
        cx,
        format!(
            "The glob expression\n {}\nwas already present in the glob cache but the directory contents have changed during the configuration run.\nMatching glob expressions:\n{locations}",
            key.expression,
        ),
    ))
}

struct Options {
    recursive: bool,
    directories: bool,
    follow: bool,
    relative: Option<BString>,
}

const CASE_INSENSITIVE: bool = cfg!(any(target_os = "macos", windows));

struct Expression<'a> {
    text: &'a [u8],
    root: PathBuf,
    patterns: Vec<Arc<Regex>>,
}

impl<'a> Expression<'a> {
    fn new(cx: &mut Cx<'_>, text: &'a [u8]) -> Result<Self> {
        let mut glob = Self {
            text,
            root: PathBuf::new(),
            patterns: Vec::new(),
        };
        if text.is_empty() {
            return Ok(glob);
        }
        let absolute = from_native_cmake(&cx.resolve_path(text));
        let path = Lexical::new(&absolute);
        let components: Vec<_> = path.components().collect();
        let prefix = components
            .iter()
            .take_while(|component| {
                !component
                    .iter()
                    .any(|b| matches!(b, b'*' | b'?' | b'[' | b'\\'))
            })
            .count();
        if prefix == components.len() && text.ends_with(b"/") {
            return Ok(glob);
        }
        let (literal, expression) =
            components.split_at(prefix.min(components.len().saturating_sub(1)));
        glob.root = to_native(&path.root_path());
        for &name in literal {
            glob.root.push(to_native(name));
        }
        for &component in expression {
            let component = if CASE_INSENSITIVE {
                component.to_ascii_lowercase()
            } else {
                component.to_vec()
            };
            glob.patterns.push(
                cx.regex(&rcmake_regex::glob::to_regex(&component))
                    .map_err(|e| error(e.to_string()))?,
            );
        }
        Ok(glob)
    }
}

struct Glob<'a, 'cx> {
    cx: &'a mut Cx<'cx>,
    options: &'a Options,
    output: &'a mut BTreeSet<BString>,
    budget: &'a mut u64,
    bytes: &'a mut usize,
    expressions: &'a [Expression<'a>],
    results: Vec<GlobResult>,
}

struct GlobResult {
    matched: Option<BTreeSet<BString>>,
    warnings: Vec<BString>,
    links: Vec<FollowedLink>,
}

#[derive(Clone, Copy)]
struct Cursor<'a> {
    expression: usize,
    patterns: &'a [Arc<Regex>],
    followed_link: bool,
}

#[derive(Clone)]
struct FollowedLink {
    parent: PathBuf,
    target: PathBuf,
}

impl<'a, 'cx> Glob<'a, 'cx> {
    fn new(
        cx: &'a mut Cx<'cx>,
        options: &'a Options,
        output: &'a mut BTreeSet<BString>,
        budget: &'a mut u64,
        bytes: &'a mut usize,
        expressions: &'a [Expression<'a>],
        configure_depends: bool,
    ) -> Self {
        Self {
            cx,
            options,
            output,
            budget,
            bytes,
            expressions,
            results: expressions
                .iter()
                .map(|_| GlobResult {
                    matched: configure_depends.then(BTreeSet::new),
                    warnings: Vec::new(),
                    links: Vec::new(),
                })
                .collect(),
        }
    }

    fn run(mut self, variable: &[u8]) -> Result<()> {
        let mut roots = BTreeMap::<&Path, Vec<Cursor<'_>>>::new();
        for (index, expression) in self.expressions.iter().enumerate() {
            if !expression.patterns.is_empty() {
                roots.entry(&expression.root).or_default().push(Cursor {
                    expression: index,
                    patterns: &expression.patterns,
                    followed_link: false,
                });
            }
        }
        let mut roots: Vec<_> = roots.into_iter().collect();
        roots.sort_unstable_by_key(|(_, cursors)| cursors.first().map(|cursor| cursor.expression));
        let result = roots
            .into_iter()
            .try_for_each(|(root, cursors)| self.walk(root, &cursors, 0));
        for (expression, glob) in self.expressions.iter().zip(self.results) {
            for message in glob.warnings {
                warning(self.cx, message);
            }
            if result.is_ok()
                && let Some(matched) = glob.matched
            {
                record(self.cx, variable, expression.text, self.options, &matched)?;
                self.output.extend(matched);
            }
        }
        result
    }

    fn walk(&mut self, path: &Path, cursors: &[Cursor<'_>], depth: u32) -> Result<()> {
        if cursors.is_empty() {
            return Ok(());
        }
        if depth > self.cx.limits().max_recursion.min(256) {
            return Err(error("GLOB exceeds the directory depth limit"));
        }
        let host = self.cx.host();
        let entries = match host.read_dir(path) {
            Ok(entries) => entries,
            Err(e)
                if matches!(
                    e.kind,
                    ErrorKind::NotFound | ErrorKind::NotADirectory | ErrorKind::PermissionDenied
                ) =>
            {
                return Ok(());
            }
            Err(e) => {
                return Err(error(format!(
                    "GLOB cannot read directory \"{}\": {}",
                    path.display(),
                    e.os_message
                )));
            }
        };
        let follow = self.options.follow || !self.options.recursive;
        let inspect_links = follow || cursors.iter().any(|cursor| cursor.patterns.len() > 1);
        let mut children = Vec::new();
        for mut entry in entries {
            let child = path.join(to_native(&entry.name));
            let symlink = entry.kind == FileKind::Symlink;
            let link_directory = symlink
                && inspect_links
                && self
                    .cx
                    .host()
                    .metadata(&child, Follow::Yes)
                    .is_ok_and(|meta| meta.kind == FileKind::Dir);
            if CASE_INSENSITIVE {
                entry.name.make_ascii_lowercase();
            }
            let mut link = None;
            for &cursor in cursors {
                let [pattern, rest @ ..] = cursor.patterns else {
                    continue;
                };
                let more = !rest.is_empty();
                let directory = entry.kind == FileKind::Dir || link_directory && (follow || more);
                let matches = pattern
                    .find_at(&entry.name, 0, MatchOptions::default(), self.budget)
                    .map_err(|e| error(e.to_string()))?
                    .is_some();
                if more {
                    if directory && matches {
                        children.push(Cursor {
                            patterns: rest,
                            followed_link: false,
                            ..cursor
                        });
                    }
                } else if self.options.recursive && directory {
                    if symlink {
                        let link = link.get_or_insert_with(|| self.link(path, &child));
                        let Some(link) = link else {
                            continue;
                        };
                        if self.link_cycle(cursor.expression, &child, link) {
                            continue;
                        }
                    }
                    if self.options.directories {
                        self.insert(&child, cursor.expression)?;
                    }
                    children.push(Cursor {
                        followed_link: symlink,
                        ..cursor
                    });
                } else if matches && (self.options.directories || !directory) {
                    self.insert(&child, cursor.expression)?;
                }
            }
            for cursor in children.iter().filter(|cursor| cursor.followed_link) {
                if let Some(Some(link)) = &link
                    && let Some(result) = self.results.get_mut(cursor.expression)
                {
                    result.links.push(link.clone());
                }
            }
            if !children.is_empty() {
                self.walk(&child, &children, depth + 1)?;
                for cursor in children.drain(..).filter(|cursor| cursor.followed_link) {
                    if let Some(result) = self.results.get_mut(cursor.expression) {
                        result.links.pop();
                    }
                }
            }
        }
        Ok(())
    }

    fn link(&self, parent: &Path, path: &Path) -> Option<FollowedLink> {
        let host = self.cx.host();
        let target = host.canonicalize(path).ok()?;
        let parent = host
            .canonicalize(parent)
            .unwrap_or_else(|_| parent.to_owned());
        Some(FollowedLink { parent, target })
    }

    fn link_cycle(&mut self, index: usize, path: &Path, link: &FollowedLink) -> bool {
        let (Some(glob), Some(expression)) =
            (self.results.get_mut(index), self.expressions.get(index))
        else {
            return false;
        };
        let Some(start) = glob
            .links
            .iter()
            .position(|previous| previous.target == link.target)
        else {
            return false;
        };
        let mut message = BString::from(
            [
                b"Cyclic recursion detected while globbing for '",
                expression.text,
                b"':",
            ]
            .concat(),
        );
        for previous in glob.links.iter().skip(start) {
            message.extend_from_slice(b"\n");
            message.extend_from_slice(&from_native_cmake(&previous.parent));
        }
        if let Some(name) = path.file_name() {
            message.extend_from_slice(b"\n");
            message.extend_from_slice(&from_native_cmake(&link.parent.join(name)));
        }
        glob.warnings.push(message);
        true
    }

    fn insert(&mut self, path: &Path, index: usize) -> Result<()> {
        let full = from_native_cmake(path);
        let name = if let Some(base) = &self.options.relative {
            if Lexical::new(base).absolute() {
                super::file::relative_absolute(base, &full)
            } else {
                BString::default()
            }
        } else {
            full
        };
        let size = name.len();
        let output = self
            .results
            .get_mut(index)
            .and_then(|glob| glob.matched.as_mut())
            .unwrap_or(self.output);
        if output.insert(name) {
            *self.bytes = self.bytes.saturating_add(size + 1);
            if *self.bytes > self.cx.limits().max_value_bytes.saturating_add(1) {
                return Err(error("GLOB exceeds the variable size limit"));
            }
        }
        Ok(())
    }
}
