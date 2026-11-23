use std::collections::BTreeSet;
use std::io::ErrorKind;
use std::path::Path;
use std::sync::Arc;

use rcmake_base::{BString, list, truth};
use rcmake_diag::{PolicyId, PolicyStatus};
use rcmake_eval::{Cx, RunMode};
use rcmake_host::{
    FileKind, Follow, Metadata,
    path::{from_native, to_native},
};
use rcmake_regex::{MatchOptions, Regex};

use super::lexical::Lexical;
use super::{Result, arg, error, set, warning};

pub(super) fn run(cx: &mut Cx<'_>, recursive: bool, args: &[&[u8]]) -> Result {
    let [out, rest @ ..] = args else {
        return Err(error("GLOB requires a variable name."));
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
    while let Some(input) = args.next() {
        match input {
            b"LIST_DIRECTORIES" => {
                options.directories = match args.next() {
                    Some(value) if truth::is_on(value) => true,
                    Some(value) if truth::is_off(value) => false,
                    _ => return Err(error("LIST_DIRECTORIES missing bool value.")),
                };
            }
            b"RELATIVE" => options.relative = Some(arg(&mut args, input)?.into()),
            b"FOLLOW_SYMLINKS" if recursive => options.follow = true,
            b"CONFIGURE_DEPENDS" if cx.mode() == RunMode::Script => {
                cx.issue(
                    rcmake_diag::MessageKind::Error,
                    rcmake_diag::Effect::Fatal,
                    None,
                    "CONFIGURE_DEPENDS is invalid for script and find package modes.".into(),
                );
                return Err(rcmake_eval::CmdError::Reported);
            }
            b"CONFIGURE_DEPENDS" => {}
            pattern => {
                let mut glob = Glob {
                    cx,
                    options: &options,
                    output: &mut output,
                    budget: &mut budget,
                    bytes: &mut bytes,
                    case_insensitive: cfg!(any(target_os = "macos", windows)),
                };
                glob.expression(pattern)?;
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
}

impl Glob<'_, '_> {
    fn expression(&mut self, pattern: &[u8]) -> Result<()> {
        if pattern.is_empty() || pattern.ends_with(b"/") {
            return Ok(());
        }
        let absolute = from_native(&self.cx.resolve_path(pattern));
        let path = Lexical::new(&absolute);
        let components: Vec<_> = path.components().collect();
        let prefix = components
            .iter()
            .take_while(|component| {
                !component
                    .iter()
                    .any(|b| matches!(b, b'*' | b'?' | b'[' | b'\\'))
            })
            .count()
            .min(components.len().saturating_sub(1));
        let (literal, expression) = components.split_at(prefix);
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
        self.walk(&root, &patterns, &mut Vec::new(), 0)
    }

    fn walk(
        &mut self,
        path: &Path,
        patterns: &[Arc<Regex>],
        ancestors: &mut Vec<(u64, u64)>,
        depth: u32,
    ) -> Result<()> {
        let [pattern, rest @ ..] = patterns else {
            return Ok(());
        };
        if depth > self.cx.limits().max_recursion.min(256) {
            return Err(error("GLOB exceeds the directory depth limit"));
        }
        let host = self.cx.host();
        let Ok(Metadata { dev, ino, .. }) = host.metadata(path, Follow::Yes) else {
            return Ok(());
        };
        if ancestors.contains(&(dev, ino)) {
            warning(
                self.cx,
                format!(
                    "Cyclic recursion detected while globbing:\n  {}",
                    path.display()
                ),
            );
            return Ok(());
        }
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
        ancestors.push((dev, ino));
        for entry in entries {
            let child = path.join(to_native(&entry.name));
            let directory = entry.kind == FileKind::Dir
                || (entry.kind == FileKind::Symlink
                    && (self.options.follow || !self.options.recursive)
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
            if rest.is_empty() {
                if (matches || (self.options.recursive && directory))
                    && (self.options.directories || !directory)
                {
                    self.insert(&child)?;
                }
                if self.options.recursive && directory {
                    self.walk(&child, patterns, ancestors, depth + 1)?;
                }
            } else if directory && matches {
                self.walk(&child, rest, ancestors, depth + 1)?;
            }
        }
        ancestors.pop();
        Ok(())
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
