use std::io::ErrorKind;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

use rcmake_base::BString;
use rcmake_diag::{Effect, MessageKind, PolicyId, PolicyStatus};
use rcmake_eval::args::{KeywordParser, Slot};
use rcmake_eval::{CmdError, Cx, Flow};
use rcmake_host::{
    CopyOptions, FileKind, Follow, HostError, ReadRange, WriteMode,
    path::{from_native, to_native},
};

use super::{Result, arg, error, set, warning};

pub(super) fn run(cx: &mut Cx<'_>, command: &[u8], args: &[&[u8]]) -> Result {
    match command {
        b"WRITE" | b"APPEND" => write(cx, command == b"APPEND", args),
        b"TOUCH" | b"TOUCH_NOCREATE" => touch(cx, command == b"TOUCH", args),
        b"MAKE_DIRECTORY" => make_directory(cx, args),
        b"REMOVE" | b"REMOVE_RECURSE" => remove(cx, command, args),
        b"RENAME" | b"COPY_FILE" => pair(cx, command, args),
        b"CREATE_LINK" => create_link(cx, args),
        b"COPY" => super::copy::run(cx, args),
        b"CHMOD" | b"CHMOD_RECURSE" => chmod(cx, command == b"CHMOD_RECURSE", args),
        _ => Err(super::unknown(command)),
    }
}

pub(super) fn status(
    cx: &mut Cx<'_>,
    variable: Option<&[u8]>,
    result: core::result::Result<(), BString>,
) -> Result {
    match result {
        Ok(()) => match variable {
            Some(name) => set(cx, name, "0"),
            None => Ok(Flow::Normal),
        },
        Err(message) => match variable {
            Some(name) => set(cx, name, message),
            None => Err(error(message)),
        },
    }
}

fn write(cx: &Cx<'_>, append: bool, args: &[&[u8]]) -> Result {
    let [input, content @ ..] = args else {
        return Err(error(if append {
            "APPEND must be called with at least one additional argument."
        } else {
            "WRITE must be called with at least one additional argument."
        }));
    };
    let path = cx.resolve_path(input);
    let size = content
        .iter()
        .try_fold(0_usize, |size, part| size.checked_add(part.len()));
    if size.is_none_or(|size| size > cx.limits().max_value_bytes) {
        return Err(error("write content exceeds the size limit"));
    }
    let mode = if append {
        WriteMode::Append
    } else {
        WriteMode::Truncate
    };
    let operation = || {
        if let Some(parent) = path.parent() {
            cx.host().create_dir_all(parent)?;
        }
        cx.host().write(&path, &content.concat(), mode)
    };
    operation().map_err(|e| {
        error(format!(
            "failed to open for writing ({}):\n\n  {}\n",
            e.os_message,
            path.display()
        ))
    })?;
    Ok(Flow::Normal)
}

fn touch(cx: &Cx<'_>, create: bool, args: &[&[u8]]) -> Result {
    for &input in args {
        if input.is_empty() {
            continue;
        }
        let path = cx.resolve_path(input);
        let now = cx.host().now();
        let result = match cx.host().set_times(&path, Some(now), Some(now)) {
            Err(e) if e.kind == ErrorKind::NotFound && create => {
                cx.host().write(&path, b"", WriteMode::Append)
            }
            Err(e) if e.kind == ErrorKind::NotFound => Ok(()),
            result => result,
        };
        result.map_err(|_| error(format!("problem touching file: {}", path.display())))?;
    }
    Ok(Flow::Normal)
}

fn make_directory(cx: &mut Cx<'_>, args: &[&[u8]]) -> Result {
    let mut args = args.iter().copied();
    let mut paths = Vec::new();
    let mut variable = None;
    while let Some(input) = args.next() {
        if input == b"RESULT" {
            variable = Some(arg(&mut args, input)?);
        } else {
            paths.push(input);
        }
    }
    let result = paths.into_iter().try_for_each(|input| {
        let path = cx.resolve_path(input);
        cx.host().create_dir_all(&path).map_err(|e| {
            format!(
                "Failed to create directory: {} Error: {}",
                path.display(),
                e.os_message
            )
            .into()
        })
    });
    status(cx, variable, result)
}

fn remove(cx: &mut Cx<'_>, command: &[u8], args: &[&[u8]]) -> Result {
    for &input in args {
        if input.is_empty() {
            warning(
                cx,
                [b"Ignoring empty file name in ", command, b"."].concat(),
            );
            continue;
        }
        let path = cx.resolve_path(input);
        if let Ok(meta) = cx.host().metadata(&path, Follow::No) {
            if meta.kind != FileKind::Dir {
                let _ = cx.host().remove_file(&path);
            } else if command == b"REMOVE_RECURSE" {
                let _ = cx.host().remove_dir_all(&path);
            }
        }
    }
    Ok(Flow::Normal)
}

fn pair(cx: &mut Cx<'_>, command: &[u8], args: &[&[u8]]) -> Result {
    let [input, output, rest @ ..] = args else {
        return Err(error(
            [
                command,
                b" must be called with at least two additional arguments",
            ]
            .concat(),
        ));
    };
    let rename_file = command == b"RENAME";
    let parser = KeywordParser::new().keyword("RESULT", Slot::SingleMaybe);
    let args = if rename_file {
        parser.keyword("NO_REPLACE", Slot::Flag)
    } else {
        parser
            .keyword("ONLY_IF_DIFFERENT", Slot::Flag)
            .keyword("INPUT_MAY_BE_RECENT", Slot::Flag)
    }
    .parse(rest);
    if let Some(option) = args.unparsed().first() {
        return Err(error([command, b" unknown argument:\n  ", option].concat()));
    }

    let source = cx.resolve_path(input);
    let target = cx.resolve_path(output);
    let result = if rename_file {
        rename(cx, &source, &target, args.flag("NO_REPLACE"))
    } else {
        copy_file(
            cx,
            &source,
            &target,
            args.flag("ONLY_IF_DIFFERENT"),
            args.flag("INPUT_MAY_BE_RECENT"),
        )
    };
    let variable = args.single("RESULT").filter(|name| !name.is_empty());
    if let Some(name) = variable {
        return status(cx, Some(name), result);
    }
    match result {
        Ok(()) => Ok(Flow::Normal),
        Err(msg) => {
            let source = from_native(&source);
            let target = from_native(&target);
            let detail = match (command, msg.as_slice()) {
                (b"COPY_FILE", b"cannot copy a directory") => {
                    [msg.as_slice(), b"\n  ", source.as_slice()].concat()
                }
                (b"COPY_FILE", b"cannot copy to a directory") => {
                    [msg.as_slice(), b"\n  ", target.as_slice()].concat()
                }
                _ => {
                    let operation = if rename_file {
                        b"rename".as_slice()
                    } else {
                        b"copy"
                    };
                    let reason = if rename_file && msg.as_slice() == b"NO_REPLACE" {
                        b"path not replaced"
                    } else {
                        msg.as_slice()
                    };
                    [
                        b"failed to ",
                        operation,
                        b"\n  ",
                        source.as_slice(),
                        b"\nto\n  ",
                        target.as_slice(),
                        b"\nbecause: ",
                        reason,
                    ]
                    .concat()
                }
            };
            Err(error([command, b" ", &detail].concat()))
        }
    }
}

fn create_link(cx: &mut Cx<'_>, args: &[&[u8]]) -> Result {
    let [input, output, rest @ ..] = args else {
        return Err(error(
            "CREATE_LINK must be called with at least two arguments.",
        ));
    };
    let mut rest = rest.iter().copied();
    let mut variable = None;
    let mut symbolic = false;
    let mut fallback = false;
    while let Some(option) = rest.next() {
        match option {
            b"RESULT" => variable = Some(arg(&mut rest, option)?),
            b"SYMBOLIC" => symbolic = true,
            b"COPY_ON_ERROR" => fallback = true,
            _ => return Err(error([b"unknown argument: \"", option, b"\""].concat())),
        }
    }
    let result = link(cx, input, output, symbolic, fallback);
    status(cx, variable, result)
}

type Operation = core::result::Result<(), BString>;

fn rename(cx: &Cx<'_>, source: &Path, target: &Path, no_replace: bool) -> Operation {
    if no_replace {
        cx.host().metadata(source, Follow::No).map_err(os_message)?;
        match cx.host().metadata(target, Follow::No) {
            Ok(_) => return Err("NO_REPLACE".into()),
            Err(err) if err.kind == ErrorKind::NotFound => {}
            Err(err) => return Err(os_message(err)),
        }
    }
    cx.host().rename(source, target).map_err(os_message)
}

fn copy_file(
    cx: &Cx<'_>,
    source: &Path,
    target: &Path,
    different: bool,
    recent: bool,
) -> Operation {
    let mut attempt = 0;
    loop {
        let result = copy_file_once(cx, source, target, different);
        match result {
            Err((ErrorKind::PermissionDenied, _)) if cfg!(windows) && recent && attempt < 5 => {
                attempt += 1;
                cx.host().sleep(std::time::Duration::from_millis(20));
            }
            _ => return result.map_err(|(_, message)| message),
        }
    }
}

fn copy_file_once(
    cx: &Cx<'_>,
    source: &Path,
    target: &Path,
    different: bool,
) -> core::result::Result<(), (ErrorKind, BString)> {
    let host = cx.host();
    let from = host.metadata(source, Follow::Yes);
    if from.as_ref().is_ok_and(|meta| meta.kind == FileKind::Dir) {
        return Err((ErrorKind::IsADirectory, "cannot copy a directory".into()));
    }
    let to = host.metadata(target, Follow::Yes);
    if to.as_ref().is_ok_and(|meta| meta.kind == FileKind::Dir) {
        return Err((ErrorKind::IsADirectory, "cannot copy to a directory".into()));
    }
    let from = from.map_err(|e| copy_error(&e, true))?;
    if let Ok(to) = to
        && ((from.dev, from.ino) == (to.dev, to.ino)
            || (different && from.len == to.len && files_equal(cx, source, target, from.len)))
    {
        return Ok(());
    }
    copy_replace(
        cx,
        source,
        target,
        CopyOptions {
            preserve_permissions: true,
            preserve_mtime: false,
        },
    )
    .map_err(|e| copy_error(&e, e.path.as_deref() == Some(source)))
}

pub(super) fn copy_replace(
    cx: &Cx<'_>,
    source: &Path,
    target: &Path,
    options: CopyOptions,
) -> rcmake_host::HostResult<()> {
    static NEXT_COPY: AtomicU64 = AtomicU64::new(0);
    let host = cx.host();
    let parent = target.parent().unwrap_or(target);
    let mut attempts = 0;
    let temporary = loop {
        let id = NEXT_COPY.fetch_add(1, Ordering::Relaxed);
        let path = parent.join(format!(".rcmake-copy-{}-{id:016x}", host.system().pid));
        match host.write(&path, b"", WriteMode::CreateNew) {
            Ok(()) => break path,
            Err(e) if e.kind == ErrorKind::AlreadyExists && attempts < 64 => attempts += 1,
            Err(e) => return Err(e),
        }
    };
    let result = host
        .copy_file(source, &temporary, options)
        .and_then(|()| host.rename(&temporary, target));
    if result.is_err() {
        let _ = host.remove_file(&temporary);
    }
    result
}

fn files_equal(cx: &Cx<'_>, a: &Path, b: &Path, size: u64) -> bool {
    let chunk = 64 * 1024;
    let mut offset = 0;
    while offset < size {
        let range = ReadRange::Slice {
            offset,
            len: Some(chunk),
        };
        match (cx.host().read(a, range), cx.host().read(b, range)) {
            (Ok(a), Ok(b)) if !a.is_empty() && a == b => offset += a.len() as u64,
            _ => return false,
        }
    }
    true
}

fn copy_error(err: &HostError, input: bool) -> (ErrorKind, BString) {
    let end = if input { "input" } else { "output" };
    (err.kind, format!("{} ({end})", err.os_message).into())
}

fn link(cx: &Cx<'_>, input: &[u8], output: &[u8], symbolic: bool, fallback: bool) -> Operation {
    let source = cx.resolve_path(input);
    let target = cx.resolve_path(output);
    let host = cx.host();
    if source == target || input == output {
        return Err("CREATE_LINK cannot use same file and newfile".into());
    }
    if !symbolic && host.metadata(&source, Follow::Yes).is_err() {
        return Err([b"Cannot hard link '", input, b"' as it does not exist."]
            .concat()
            .into());
    }
    if let Ok(existing) = host.metadata(&target, Follow::No)
        && existing.kind != FileKind::Dir
    {
        if !symbolic
            && host
                .metadata(&source, Follow::No)
                .is_ok_and(|meta| (meta.dev, meta.ino) == (existing.dev, existing.ino))
        {
            return Ok(());
        }
        host.remove_file(&target).map_err(os_message)?;
    }
    let linked = if symbolic {
        host.symlink(&to_native(input), &target)
    } else {
        host.hard_link(&source, &target)
    };
    match linked {
        Ok(()) => Ok(()),
        Err(_) if fallback => {
            let source = if symbolic && !to_native(input).is_absolute() {
                target
                    .parent()
                    .map_or(source, |parent| parent.join(to_native(input)))
            } else {
                source
            };
            let meta = host.metadata(&source, Follow::Yes).map_err(os_message)?;
            if meta.kind == FileKind::Dir {
                let policy = cx.policy(PolicyId::new(205));
                if policy == PolicyStatus::New {
                    super::copy::copy_directory(cx, &source, &target).map_err(|e| match e {
                        rcmake_eval::CmdError::Status(message) => message,
                        rcmake_eval::CmdError::Reported => "failed to copy directory".into(),
                    })
                } else {
                    host.create_dir_all(&target).map_err(os_message)
                }
            } else {
                copy_file(cx, &source, &target, false, false)
            }
        }
        Err(e) => Err([
            b"failed to create link '",
            output,
            b"': ",
            e.os_message.as_bytes(),
        ]
        .concat()
        .into()),
    }
}

fn os_message(err: HostError) -> BString {
    err.os_message.into()
}

#[derive(Clone, Copy, Default)]
pub(super) struct Modes {
    pub(super) default: Option<u32>,
    pub(super) files: Option<u32>,
    pub(super) directories: Option<u32>,
}

impl Modes {
    pub(super) fn get(self, directory: bool) -> Option<u32> {
        if directory {
            self.directories
        } else {
            self.files
        }
        .or(self.default)
    }
}

pub(super) fn permission(value: &[u8]) -> Option<u32> {
    Some(match value {
        b"SETUID" => 0o4000,
        b"SETGID" => 0o2000,
        b"OWNER_READ" => 0o400,
        b"OWNER_WRITE" => 0o200,
        b"OWNER_EXECUTE" => 0o100,
        b"GROUP_READ" => 0o040,
        b"GROUP_WRITE" => 0o020,
        b"GROUP_EXECUTE" => 0o010,
        b"WORLD_READ" => 0o004,
        b"WORLD_WRITE" => 0o002,
        b"WORLD_EXECUTE" => 0o001,
        _ => return None,
    })
}

fn chmod(cx: &mut Cx<'_>, recurse: bool, args: &[&[u8]]) -> Result {
    let mut modes = Modes::default();
    let mut active: Option<(&[u8], bool)> = None;
    let mut missing = None;
    let mut paths = Vec::new();
    for &option in args {
        match option {
            b"PERMISSIONS" | b"FILE_PERMISSIONS" | b"DIRECTORY_PERMISSIONS" => {
                missing = missing.or(active
                    .filter(|(_, supplied)| !supplied)
                    .map(|(name, _)| name));
                active = Some((option, false));
            }
            option => {
                let destination = match active {
                    None => {
                        paths.push(option);
                        continue;
                    }
                    Some((b"PERMISSIONS", _)) => &mut modes.default,
                    Some((b"FILE_PERMISSIONS", _)) => &mut modes.files,
                    _ => &mut modes.directories,
                };
                let flag = permission(option).ok_or_else(|| {
                    error([option, b" is an invalid permission specifier"].concat())
                })?;
                if let Some((_, supplied)) = &mut active {
                    *supplied = true;
                }
                *destination.get_or_insert(0) |= flag;
            }
        }
    }
    if let Some(option) = missing.or(active
        .filter(|(_, supplied)| !supplied)
        .map(|(name, _)| name))
    {
        cx.issue(
            MessageKind::Error,
            Effect::Fatal,
            None,
            [
                b"Error after keyword \"",
                option,
                b"\":\n\n  missing required value\n",
            ]
            .concat()
            .into(),
        );
        return Err(CmdError::Reported);
    }
    if active.is_none() {
        return Err(error("No permissions given"));
    }
    for input in paths {
        let start = cx.resolve_path(input);
        let mut pending = vec![(start, 0_u32, None)];
        while let Some((path, depth, post_order_mode)) = pending.pop() {
            let host = cx.host();
            if let Some(mode) = post_order_mode {
                host.set_permissions(&path, mode)
                    .map_err(|e| error(e.os_message))?;
                continue;
            }
            if depth > cx.limits().max_recursion.min(256) {
                return Err(error("CHMOD_RECURSE exceeds the directory depth limit"));
            }
            let metadata = host
                .metadata(&path, Follow::No)
                .map_err(|_| error([b"does not exist:\n\n  ", input, b"\n"].concat()))?;
            let follow = if metadata.kind == FileKind::Symlink {
                let Ok(meta) = host.metadata(&path, Follow::Yes) else {
                    continue;
                };
                meta
            } else {
                metadata.clone()
            };
            let directory = follow.kind == FileKind::Dir;
            let mode = modes.get(directory);
            if let Some(mode) = mode {
                pending.push((path.clone(), depth, Some(mode)));
            }
            if recurse && metadata.kind == FileKind::Dir {
                let children = host.read_dir(&path).map_err(|e| error(e.os_message))?;
                pending.extend(
                    children
                        .into_iter()
                        .rev()
                        .map(|entry| (path.join(to_native(&entry.name)), depth + 1, None)),
                );
            }
        }
    }
    Ok(Flow::Normal)
}
