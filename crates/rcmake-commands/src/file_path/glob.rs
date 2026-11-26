use std::collections::{BTreeMap, BTreeSet};
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use rcmake_base::{BString, list, truth};
use rcmake_diag::{DiagAction, DiagCategory, PolicyId, PolicyStatus};
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
    let mut state = State {
        output: BTreeSet::new(),
        budget: cx.limits().regex_steps,
        bytes: 0,
    };
    let mut evaluated = false;
    let mut expressions = Vec::<Expression<'_>>::new();
    let mut configure_depends = false;
    macro_rules! flush_globs {
        () => {
            let batch = Batch {
                options: &options,
                expressions: &expressions,
                configure_depends,
                variable: out,
                warning_limit: RETAINED_WARNING_BYTES,
            };
            batch.run(cx, &mut state)?;
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
                // CMake gives this warning before it checks for the next expression.
                if evaluated && !configure_depends {
                    warning(
                        cx,
                        "CONFIGURE_DEPENDS flag was given after a glob expression was already evaluated.",
                    );
                }
                if args.len() == 0 {
                    return Err(error(
                        [
                            command,
                            b" requires a glob expression after CONFIGURE_DEPENDS.",
                        ]
                        .concat(),
                    ));
                }
                configure_depends = true;
            }
            pattern => {
                evaluated = true;
                match Expression::new(cx, pattern) {
                    Ok(expression) => expressions.push(expression),
                    Err(cause) => {
                        // The earlier expressions run first, so their errors come first.
                        flush_globs!();
                        return Err(cause);
                    }
                }
            }
        }
    }
    flush_globs!();
    set(cx, out, list::join(state.output))
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

/// The result list, the regex step budget, and the counted result bytes of one call.
struct State {
    output: BTreeSet<BString>,
    budget: u64,
    bytes: usize,
}

/// The largest total size of cycle warnings that one batch walk keeps.
///
/// A batch walk emits its warnings only after the walk. A larger total stops
/// the batch, and the expressions then run one at a time with direct warnings.
const RETAINED_WARNING_BYTES: usize = 1024 * 1024;

/// The expressions between two option changes of one `file(GLOB)` call.
struct Batch<'a> {
    options: &'a Options,
    expressions: &'a [Expression<'a>],
    configure_depends: bool,
    variable: &'a [u8],
    warning_limit: usize,
}

impl Batch<'_> {
    /// Run the expressions with the result, diagnostics, and errors of a run in expression order.
    ///
    /// One walk does the shared directory reads for all expressions. That walk
    /// has no side effects until it completes. If the walk fails or reaches a
    /// limit, the expressions run again one at a time, so the first error,
    /// the cache checks, and the warnings stay in expression order.
    ///
    /// An audit host reports a denied read when the read occurs. A shared walk
    /// could then report a later expression before an earlier cache mismatch,
    /// so with an audit host the expressions always run one at a time.
    fn run(&self, cx: &mut Cx<'_>, state: &mut State) -> Result<()> {
        if self.expressions.len() > 1
            && !crate::hermetic::audits_reads(cx)
            && self.run_shared(cx, state)?
        {
            return Ok(());
        }
        for expression in self.expressions {
            let glob = Glob::new(
                cx,
                self.options,
                state,
                std::slice::from_ref(expression),
                self.configure_depends,
                Warnings::Direct,
            );
            let (result, mut results, bytes) = glob.walk_all();
            state.bytes = bytes;
            result?;
            let matched = results.pop().map(|glob| glob.matched).unwrap_or_default();
            if self.configure_depends {
                record(cx, self.variable, expression.text, self.options, &matched)?;
            }
            state.output.extend(matched);
        }
        Ok(())
    }

    /// Return `false` without side effects if the shared walk cannot give the exact result.
    fn run_shared(&self, cx: &mut Cx<'_>, state: &mut State) -> Result<bool> {
        let warnings = if cx.diag_state().get(DiagCategory::Author) == DiagAction::Ignore {
            Warnings::Drop
        } else {
            Warnings::Retain {
                left: self.warning_limit,
            }
        };
        let budget = state.budget;
        let glob = Glob::new(
            cx,
            self.options,
            state,
            self.expressions,
            self.configure_depends,
            warnings,
        );
        let (result, results, _) = glob.walk_all();
        // Count the bytes as a run in expression order counts them.
        let mut bytes = state.bytes;
        let mut seen = BTreeSet::new();
        for name in results.iter().flat_map(|glob| &glob.matched) {
            if self.configure_depends || (!state.output.contains(name) && seen.insert(name)) {
                bytes = bytes.saturating_add(name.len() + 1);
            }
        }
        if result.is_err() || bytes > cx.limits().max_value_bytes.saturating_add(1) {
            state.budget = budget;
            return Ok(false);
        }
        state.bytes = bytes;
        for (expression, glob) in self.expressions.iter().zip(results) {
            for message in glob.warnings {
                warning(cx, message);
            }
            if self.configure_depends {
                record(
                    cx,
                    self.variable,
                    expression.text,
                    self.options,
                    &glob.matched,
                )?;
            }
            state.output.extend(glob.matched);
        }
        Ok(true)
    }
}

/// How a walk handles cycle warnings.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Warnings {
    /// Emit each warning when the walk finds the cycle.
    Direct,
    /// Keep the warnings until the walk completes. Stop the walk if more than
    /// `left` bytes are necessary.
    Retain { left: usize },
    /// Do not make the warnings, because author warnings are disabled.
    Drop,
}

struct Glob<'a, 'cx> {
    cx: &'a mut Cx<'cx>,
    options: &'a Options,
    output: &'a BTreeSet<BString>,
    budget: &'a mut u64,
    bytes: usize,
    expressions: &'a [Expression<'a>],
    results: Vec<GlobResult>,
    warnings: Warnings,
    /// `true` with CONFIGURE_DEPENDS, where each expression counts its own matches.
    configure_depends: bool,
}

struct GlobResult {
    matched: BTreeSet<BString>,
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
        state: &'a mut State,
        expressions: &'a [Expression<'a>],
        configure_depends: bool,
        warnings: Warnings,
    ) -> Self {
        Self {
            cx,
            options,
            output: &state.output,
            budget: &mut state.budget,
            bytes: state.bytes,
            expressions,
            results: expressions
                .iter()
                .map(|_| GlobResult {
                    matched: BTreeSet::new(),
                    warnings: Vec::new(),
                    links: Vec::new(),
                })
                .collect(),
            warnings,
            configure_depends,
        }
    }

    /// Walk every expression and return the walk result, the matches, and the counted bytes.
    fn walk_all(mut self) -> (Result<()>, Vec<GlobResult>, usize) {
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
        (result, self.results, self.bytes)
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
                        if self.link_cycle(cursor.expression, &child, link)? {
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

    /// Return `true` if the link target is already on the followed link stack.
    fn link_cycle(&mut self, index: usize, path: &Path, link: &FollowedLink) -> Result<bool> {
        let (Some(glob), Some(expression)) =
            (self.results.get_mut(index), self.expressions.get(index))
        else {
            return Ok(false);
        };
        let Some(start) = glob
            .links
            .iter()
            .position(|previous| previous.target == link.target)
        else {
            return Ok(false);
        };
        if self.warnings == Warnings::Drop {
            return Ok(true);
        }
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
        match &mut self.warnings {
            Warnings::Direct => warning(self.cx, message),
            Warnings::Retain { left } => {
                *left = left
                    .checked_sub(message.len())
                    .ok_or_else(|| error("GLOB keeps too many cycle warnings"))?;
                glob.warnings.push(message);
            }
            Warnings::Drop => {}
        }
        Ok(true)
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
        let Some(glob) = self.results.get_mut(index) else {
            return Ok(());
        };
        // Without CONFIGURE_DEPENDS, a name counts once for the full call.
        let counted = self.configure_depends || !self.output.contains(&name);
        if glob.matched.insert(name) && counted {
            self.bytes = self.bytes.saturating_add(size + 1);
            if self.bytes > self.cx.limits().max_value_bytes.saturating_add(1) {
                return Err(error("GLOB exceeds the variable size limit"));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use rcmake_eval::{CmdError, Interp, InterpConfig};
    use rcmake_host::{Host, PureHost, Stream};

    use super::*;
    use crate::test_support::TestResult;

    const DRIVE: &str = if cfg!(windows) { "C:" } else { "" };

    /// Make a tree in which each followed link reaches four cycles.
    fn cyclic_tree() -> TestResult<(Arc<PureHost>, Interp)> {
        let cwd = format!("{DRIVE}/work");
        let host = Arc::new(PureHost::new(cwd.clone().into(), "/cmake/bin/cmake".into()));
        host.seed_file(Path::new(&format!("{cwd}/tree/sub/file.txt")), b"data")?;
        for index in 0..4 {
            host.symlink(
                Path::new(".."),
                Path::new(&format!("{cwd}/tree/sub/loop{index}")),
            )?;
        }
        let interp = Interp::new(
            InterpConfig::new(
                host.clone(),
                RunMode::Script,
                "/cmake",
                "/cmake/bin/cmake",
                cwd,
            ),
            super::super::register,
        );
        Ok((host, interp))
    }

    fn texts() -> [String; 2] {
        [
            format!("{DRIVE}/work/tree/*"),
            format!("{DRIVE}/work/tree/*.txt"),
        ]
    }

    fn options() -> Options {
        Options {
            recursive: true,
            directories: false,
            follow: true,
            relative: None,
        }
    }

    fn state(cx: &Cx<'_>) -> State {
        State {
            output: BTreeSet::new(),
            budget: cx.limits().regex_steps,
            bytes: 0,
        }
    }

    fn run_batch(
        interp: &mut Interp,
        warning_limit: usize,
    ) -> core::result::Result<BTreeSet<BString>, CmdError> {
        let mut cx = interp.cx();
        let texts = texts();
        let expressions = texts
            .iter()
            .map(|text| Expression::new(&mut cx, text.as_bytes()))
            .collect::<Result<Vec<_>>>()?;
        let mut state = state(&cx);
        Batch {
            options: &options(),
            expressions: &expressions,
            configure_depends: false,
            variable: b"out",
            warning_limit,
        }
        .run(&mut cx, &mut state)?;
        Ok(state.output)
    }

    #[test]
    fn disabled_author_warnings_keep_no_cycle_warning_text() -> TestResult {
        let (_host, mut interp) = cyclic_tree()?;
        let mut cx = interp.cx();
        let texts = texts();
        let expressions = texts
            .iter()
            .map(|text| Expression::new(&mut cx, text.as_bytes()))
            .collect::<Result<Vec<_>>>()
            .map_err(|error| format!("{error:?}"))?;
        let options = options();
        let mut kept = Vec::new();
        for warnings in [Warnings::Retain { left: usize::MAX }, Warnings::Drop] {
            let mut state = state(&cx);
            let glob = Glob::new(&mut cx, &options, &mut state, &expressions, false, warnings);
            let (result, results, _) = glob.walk_all();
            result.map_err(|error| format!("{error:?}"))?;
            kept.push(
                results
                    .iter()
                    .map(|glob| glob.warnings.len())
                    .collect::<Vec<_>>(),
            );
        }
        let [retained, dropped] = kept.as_slice() else {
            return Err("Expected two walks.".into());
        };
        assert!(retained.iter().all(|&count| count > 0), "{retained:?}");
        assert_eq!(dropped, &[0, 0]);
        Ok(())
    }

    #[test]
    fn a_batch_over_the_warning_limit_runs_each_expression_with_direct_warnings() -> TestResult {
        let (wide, mut unlimited) = cyclic_tree()?;
        let expected =
            run_batch(&mut unlimited, usize::MAX).map_err(|error| format!("{error:?}"))?;
        let (narrow, mut limited) = cyclic_tree()?;
        let output = run_batch(&mut limited, 16).map_err(|error| format!("{error:?}"))?;
        assert_eq!(output, expected);
        assert!(
            output
                .iter()
                .any(|name| name.ends_with(b"/tree/sub/file.txt"))
        );
        let warnings = wide.stream_bytes(Stream::Stderr).to_string();
        assert_eq!(narrow.stream_bytes(Stream::Stderr).to_string(), warnings);
        let [all, txt] = texts();
        let first = warnings
            .find(&format!("globbing for '{all}'"))
            .ok_or("Missing warning for the first expression.")?;
        let last = warnings
            .rfind(&format!("globbing for '{all}'"))
            .ok_or("Missing warning for the first expression.")?;
        let second = warnings
            .find(&format!("globbing for '{txt}'"))
            .ok_or("Missing warning for the second expression.")?;
        assert!(first < second && last < second, "{warnings}");
        Ok(())
    }
}
