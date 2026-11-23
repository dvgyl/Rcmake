use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use rcmake_base::BString;
use rcmake_eval::{Cx, Flow};
use rcmake_host::{
    CopyOptions, FileKind, Follow,
    path::{from_native, to_native},
};
use rcmake_regex::{MatchOptions, Regex};

use super::file::binary_destination;
use super::lexical::Lexical;
use super::mutation::{Modes, permission};
use super::{Result, arg, error};

#[derive(Default)]
struct Options {
    no_source_permissions: bool,
    files_matching: bool,
    follow_chain: bool,
    modes: Modes,
    rules: Vec<Rule>,
}

struct Rule {
    regex: Arc<Regex>,
    exclude: bool,
    mode: Option<u32>,
    filename_only: bool,
}

pub(super) fn run(cx: &mut Cx<'_>, args: &[&[u8]]) -> Result {
    let (inputs, destination, options) = parse(cx, args)?;
    let host = cx.host();
    let mut budget = cx.limits().regex_steps;
    for input in inputs.into_iter().filter(|input| !input.is_empty()) {
        let source = cx.resolve_path(input);
        host.metadata(&source, Follow::No).map_err(|e| {
            error(format!(
                "COPY cannot find \"{}\": {}.",
                source.display(),
                e.os_message
            ))
        })?;
        let leaf = source.file_name().unwrap_or_default();
        let output = if input.ends_with(b"/") || Lexical::new(input).filename() == b"." {
            destination.clone()
        } else {
            destination.join(leaf)
        };
        copy_entry(cx, &source, &output, &options, &mut budget, 0)?;
    }
    Ok(Flow::Normal)
}

fn parse<'a>(cx: &mut Cx<'_>, args: &[&'a [u8]]) -> Result<(Vec<&'a [u8]>, PathBuf, Options)> {
    let mut options = Options::default();
    let mut inputs = Vec::new();
    let mut destination = None;
    let mut args = args.iter().copied().peekable();
    while let Some(option) = args.next() {
        match option {
            b"DESTINATION" => destination = Some(binary_destination(cx, arg(&mut args, option)?)),
            b"NO_SOURCE_PERMISSIONS" => options.no_source_permissions = true,
            b"USE_SOURCE_PERMISSIONS" => options.no_source_permissions = false,
            b"FILE_PERMISSIONS" => options.modes.files = Some(permissions(&mut args)?),
            b"DIRECTORY_PERMISSIONS" => options.modes.directories = Some(permissions(&mut args)?),
            b"FILES_MATCHING" => options.files_matching = true,
            b"FOLLOW_SYMLINK_CHAIN" => options.follow_chain = true,
            b"PATTERN" | b"REGEX" => {
                let input = arg(&mut args, option)?;
                let filename_only = option == b"PATTERN" && !input.contains(&b'/');
                let pattern = if filename_only {
                    rcmake_regex::glob::to_regex(input)
                } else if option == b"PATTERN" {
                    let glob = rcmake_regex::glob::to_regex(input);
                    [b"/", glob.strip_prefix(b"^").unwrap_or(&glob)]
                        .concat()
                        .into()
                } else {
                    BString::from(input)
                };
                options.rules.push(Rule {
                    regex: cx.regex(&pattern).map_err(|e| error(e.to_string()))?,
                    exclude: false,
                    mode: None,
                    filename_only,
                });
            }
            b"EXCLUDE" | b"PERMISSIONS" => {
                let rule = options.rules.last_mut().ok_or_else(|| {
                    error([b"COPY option ", option, b" requires PATTERN or REGEX."].concat())
                })?;
                if option == b"EXCLUDE" {
                    rule.exclude = true;
                } else {
                    rule.mode = Some(permissions(&mut args)?);
                }
            }
            _ if destination.is_none() && options.rules.is_empty() => inputs.push(option),
            _ => {
                return Err(error(
                    [b"COPY given unknown argument \"", option, b"\"."].concat(),
                ));
            }
        }
    }
    let destination = destination.ok_or_else(|| error("COPY given no DESTINATION"))?;
    Ok((inputs, destination, options))
}

fn permissions<'a>(args: &mut std::iter::Peekable<impl Iterator<Item = &'a [u8]>>) -> Result<u32> {
    let mut flags = None;
    while let Some(mode) = args.peek().and_then(|value| permission(value)) {
        args.next();
        *flags.get_or_insert(0) |= mode;
    }
    flags.ok_or_else(|| error("COPY requires at least one valid permission."))
}

pub(super) fn copy_directory(cx: &Cx<'_>, source: &Path, target: &Path) -> Result<()> {
    let mut budget = cx.limits().regex_steps;
    copy_entry(cx, source, target, &Options::default(), &mut budget, 0)
}

fn copy_entry(
    cx: &Cx<'_>,
    source: &Path,
    target: &Path,
    options: &Options,
    budget: &mut u64,
    depth: u32,
) -> Result<()> {
    if depth > cx.limits().max_recursion.min(256) {
        return Err(error("COPY exceeds the directory depth limit"));
    }
    let host = cx.host();
    let meta = host
        .metadata(source, Follow::No)
        .map_err(|e| copy_error("read", source, &e))?;
    let directory = meta.kind == FileKind::Dir;
    let name = from_native(source);
    let filename = source.file_name().map(|name| from_native(Path::new(name)));
    let mut matched = false;
    let mut mode = None;
    for rule in &options.rules {
        let input = if rule.filename_only {
            filename.as_ref().unwrap_or(&name)
        } else {
            &name
        };
        if rule
            .regex
            .find_at(input, 0, MatchOptions::default(), budget)
            .map_err(|e| error(e.to_string()))?
            .is_some()
        {
            if rule.exclude {
                return Ok(());
            }
            matched = true;
            if let Some(permissions) = rule.mode {
                *mode.get_or_insert(0) |= permissions;
            }
        }
    }
    if options.files_matching && !directory && !matched {
        return Ok(());
    }
    let mode = mode.or(options.modes.get(directory)).unwrap_or({
        if options.no_source_permissions {
            if directory { 0o755 } else { 0o644 }
        } else {
            meta.mode & 0o7777
        }
    });
    let existing = host.metadata(target, Follow::No).ok();
    if existing
        .as_ref()
        .is_some_and(|m| (m.dev, m.ino) == (meta.dev, meta.ino))
    {
        return Ok(());
    }
    if directory {
        if depth == 0 && is_descendant(cx, source, target) {
            return Err(error(format!(
                "COPY cannot copy directory \"{}\" into itself.",
                source.display()
            )));
        }
        host.create_dir_all(target)
            .map_err(|e| copy_error("make directory", target, &e))?;
        let children = host
            .read_dir(source)
            .map_err(|e| copy_error("read directory", source, &e))?;
        for entry in children {
            let name = to_native(&entry.name);
            copy_entry(
                cx,
                &source.join(&name),
                &target.join(name),
                options,
                budget,
                depth + 1,
            )?;
        }
    } else {
        if let Some(parent) = target.parent() {
            host.create_dir_all(parent)
                .map_err(|e| copy_error("make directory", parent, &e))?;
        }
        if meta.kind == FileKind::Symlink {
            let link = host
                .read_link(source)
                .map_err(|e| copy_error("read link", source, &e))?;
            let (link, next) = if options.follow_chain {
                let leaf = link
                    .file_name()
                    .ok_or_else(|| error("COPY cannot follow a link without a filename."))?;
                let next_source = source.parent().unwrap_or(source).join(&link);
                let next_target = target.with_file_name(leaf);
                (PathBuf::from(leaf), Some((next_source, next_target)))
            } else {
                (link, None)
            };
            if host.read_link(target).ok() != Some(link.clone()) {
                if existing.is_some() {
                    host.remove_file(target)
                        .map_err(|e| copy_error("remove", target, &e))?;
                }
                host.symlink(&link, target)
                    .map_err(|e| copy_error("create link", target, &e))?;
            }
            if let Some((next_source, next_target)) = next {
                copy_entry(cx, &next_source, &next_target, options, budget, depth + 1)?;
            }
            return Ok(());
        }
        if !existing
            .as_ref()
            .is_some_and(|m| m.kind == FileKind::File && m.mtime == meta.mtime)
        {
            if existing
                .as_ref()
                .is_some_and(|m| m.kind == FileKind::Symlink)
            {
                host.remove_file(target)
                    .map_err(|e| copy_error("remove", target, &e))?;
            }
            host.copy_file(
                source,
                target,
                CopyOptions {
                    preserve_permissions: false,
                    preserve_mtime: true,
                },
            )
            .map_err(|e| copy_error("copy file", source, &e))?;
        }
    }
    host.set_permissions(target, mode)
        .map_err(|e| copy_error("set permissions on", target, &e))?;
    Ok(())
}

fn is_descendant(cx: &Cx<'_>, source: &Path, target: &Path) -> bool {
    let host = cx.host();
    let Ok(source) = host.canonicalize(source) else {
        return false;
    };
    let normalized = Lexical::new(&from_native(target)).normalize();
    to_native(&normalized)
        .ancestors()
        .find_map(|path| match host.canonicalize(path) {
            Ok(path) => Some(path.starts_with(&source)),
            Err(e) if e.kind == ErrorKind::NotFound => None,
            Err(_) => Some(false),
        })
        .unwrap_or(false)
}

fn copy_error(operation: &str, path: &Path, err: &rcmake_host::HostError) -> rcmake_eval::CmdError {
    error(format!(
        "COPY cannot {operation} \"{}\": {}.",
        path.display(),
        err.os_message
    ))
}
