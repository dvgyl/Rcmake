use std::collections::BTreeSet;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use rcmake_base::{BString, list, truth};
use rcmake_diag::{PolicyId, PolicyStatus};
use rcmake_eval::{Cx, RunMode};
use rcmake_host::{
    FileKind, Follow,
    path::{from_native, to_native},
};
use rcmake_regex::{MatchOptions, Regex};

use super::lexical::Lexical;
use super::{Result, error, set, warning};

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
    let mut configure_depends = false;
    while let Some(input) = args.next() {
        match input {
            b"LIST_DIRECTORIES" => {
                options.directories = match args.next() {
                    Some(value) if truth::is_on(value) => true,
                    Some(value) if truth::is_off(value) => false,
                    _ => return Err(error("LIST_DIRECTORIES missing bool value.")),
                };
            }
            b"RELATIVE" => {
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
                if args.len() == 0 {
                    return Err(error(
                        "GLOB_RECURSE requires a glob expression after FOLLOW_SYMLINKS.",
                    ));
                }
                options.follow = true;
            }
            b"CONFIGURE_DEPENDS" if cx.mode() == RunMode::Script => {
                cx.issue(
                    rcmake_diag::MessageKind::Error,
                    rcmake_diag::Effect::Fatal,
                    None,
                    "CONFIGURE_DEPENDS is invalid for script and find package modes.".into(),
                );
                return Err(rcmake_eval::CmdError::Reported);
            }
            b"CONFIGURE_DEPENDS" => {
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
                let mut glob = Glob {
                    cx,
                    options: &options,
                    output: &mut output,
                    budget: &mut budget,
                    bytes: &mut bytes,
                    case_insensitive: cfg!(any(target_os = "macos", windows)),
                    expression: pattern,
                    links: Vec::new(),
                };
                glob.run()?;
            }
        }
    }
    set(cx, out, list::join(output))
}

struct Options {
    recursive: bool,
    directories: bool,
    follow: bool,
    relative: Option<BString>,
}

struct Glob<'a, 'cx> {
    cx: &'a mut Cx<'cx>,
    options: &'a Options,
    output: &'a mut BTreeSet<BString>,
    budget: &'a mut u64,
    bytes: &'a mut usize,
    case_insensitive: bool,
    expression: &'a [u8],
    links: Vec<FollowedLink>,
}

struct FollowedLink {
    parent: PathBuf,
    target: PathBuf,
}

impl Glob<'_, '_> {
    fn run(&mut self) -> Result<()> {
        if self.expression.is_empty() {
            return Ok(());
        }
        let absolute = from_native(&self.cx.resolve_path(self.expression));
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
        if prefix == components.len() && self.expression.ends_with(b"/") {
            return Ok(());
        }
        let (literal, expression) =
            components.split_at(prefix.min(components.len().saturating_sub(1)));
        let mut root = to_native(&path.root_path());
        for &name in literal {
            root.push(to_native(name));
        }
        let mut patterns = Vec::new();
        for &component in expression {
            let component = if self.case_insensitive {
                component.to_ascii_lowercase()
            } else {
                component.to_vec()
            };
            patterns.push(
                self.cx
                    .regex(&rcmake_regex::glob::to_regex(&component))
                    .map_err(|e| error(e.to_string()))?,
            );
        }
        self.walk(&root, &patterns, 0)
    }

    fn walk(&mut self, path: &Path, patterns: &[Arc<Regex>], depth: u32) -> Result<()> {
        let [pattern, rest @ ..] = patterns else {
            return Ok(());
        };
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
        for entry in entries {
            let child = path.join(to_native(&entry.name));
            let symlink = entry.kind == FileKind::Symlink;
            let directory = entry.kind == FileKind::Dir
                || (symlink
                    && (self.options.follow || !self.options.recursive || !rest.is_empty())
                    && self
                        .cx
                        .host()
                        .metadata(&child, Follow::Yes)
                        .is_ok_and(|meta| meta.kind == FileKind::Dir));
            let name = if self.case_insensitive {
                entry.name.to_ascii_lowercase()
            } else {
                entry.name.into()
            };
            let matches = pattern
                .find_at(&name, 0, MatchOptions::default(), self.budget)
                .map_err(|e| error(e.to_string()))?
                .is_some();
            if !rest.is_empty() {
                if directory && matches {
                    self.walk(&child, rest, depth + 1)?;
                }
            } else if self.options.recursive && directory {
                if symlink {
                    self.follow_link(path, &child, patterns, depth + 1)?;
                } else {
                    self.walk_directory(&child, patterns, depth + 1)?;
                }
            } else if matches && (self.options.directories || !directory) {
                self.insert(&child)?;
            }
        }
        Ok(())
    }

    fn walk_directory(&mut self, path: &Path, patterns: &[Arc<Regex>], depth: u32) -> Result<()> {
        if self.options.directories {
            self.insert(path)?;
        }
        self.walk(path, patterns, depth)
    }

    fn follow_link(
        &mut self,
        parent: &Path,
        path: &Path,
        patterns: &[Arc<Regex>],
        depth: u32,
    ) -> Result<()> {
        let host = self.cx.host();
        let Ok(target) = host.canonicalize(path) else {
            return Ok(());
        };
        let parent = host
            .canonicalize(parent)
            .unwrap_or_else(|_| parent.to_owned());
        if let Some(cycle_start) = self.links.iter().position(|link| link.target == target) {
            let mut message = BString::from(
                [
                    b"Cyclic recursion detected while globbing for '",
                    self.expression,
                    b"':",
                ]
                .concat(),
            );
            for link in self.links.iter().skip(cycle_start) {
                message.extend_from_slice(b"\n");
                message.extend_from_slice(&from_native(&link.parent));
            }
            if let Some(name) = path.file_name() {
                message.extend_from_slice(b"\n");
                message.extend_from_slice(&from_native(&parent.join(name)));
            }
            warning(self.cx, message);
            return Ok(());
        }
        self.links.push(FollowedLink { parent, target });
        let result = self.walk_directory(path, patterns, depth);
        self.links.pop();
        result
    }

    fn insert(&mut self, path: &Path) -> Result<()> {
        let full = from_native(path);
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
        if self.output.insert(name) {
            *self.bytes = self.bytes.saturating_add(size + 1);
            if *self.bytes > self.cx.limits().max_value_bytes.saturating_add(1) {
                return Err(error("GLOB exceeds the variable size limit"));
            }
        }
        Ok(())
    }
}
