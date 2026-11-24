use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use rcmake_base::BString;
use rcmake_host::{
    CopyOptions, FileKind, Follow, HostError, HostResult, Metadata, ReadRange, WriteMode,
    path::to_native,
};

use super::{ToolCtx, error, is_dir, path, read_all, usage};

const NO_FILE: &str = "No such file or directory";

fn fs_error(kind: ErrorKind, message: &str) -> HostError {
    HostError {
        kind,
        errno: None,
        os_message: message.to_owned(),
        path: None,
    }
}

pub(super) fn make_directory(args: &[BString], cx: &ToolCtx<'_>) -> i32 {
    if args.is_empty() {
        return usage(cx, 1);
    }
    let mut status = 0;
    for dir in args {
        let result = if dir.is_empty() {
            Err(fs_error(ErrorKind::InvalidInput, "Invalid argument"))
        } else {
            let path = path(cx, dir);
            if cx
                .host
                .metadata(&path, Follow::Yes)
                .is_ok_and(|m| m.kind != FileKind::Dir)
            {
                Err(fs_error(ErrorKind::AlreadyExists, "File exists"))
            } else {
                cx.host.create_dir_all(&path)
            }
        };
        if let Err(err) = result {
            status = error(
                cx,
                &[
                    b"Error creating directory \"",
                    dir,
                    b"\": ",
                    err.os_message.as_bytes(),
                    b"\n",
                ],
            );
        }
    }
    status
}

pub(super) fn touch(command: &[u8], args: &[BString], cx: &ToolCtx<'_>) -> i32 {
    if args.is_empty() {
        return usage(cx, 1);
    }
    let create = command == b"touch";
    for file in args {
        if (!file.is_empty() && touch_file(&path(cx, file), create, cx).is_ok())
            || (file.is_empty() && !create)
        {
            continue;
        }
        return error(
            cx,
            &[
                b"cmake -E ",
                command,
                b": failed to update \"",
                file,
                b"\".\n",
            ],
        );
    }
    0
}

fn touch_file(file: &Path, create: bool, cx: &ToolCtx<'_>) -> HostResult<()> {
    match cx.host.metadata(file, Follow::Yes) {
        Ok(_) => {}
        Err(err) if is_missing(err.kind) => {
            if !create {
                return Ok(());
            }
            cx.host.write(file, b"", WriteMode::Append)?;
        }
        Err(err) => return Err(err),
    }
    let now = cx.host.now();
    cx.host.set_times(file, Some(now), Some(now))
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum CopyWhen {
    Always,
    Different,
    Newer,
}

pub(super) fn copy(command: &[u8], args: &[BString], cx: &ToolCtx<'_>) -> i32 {
    let directory = command.starts_with(b"copy_directory");
    let when = if command.ends_with(b"_newer") {
        CopyWhen::Newer
    } else if command.ends_with(b"_different") {
        CopyWhen::Different
    } else {
        CopyWhen::Always
    };

    let mut sources = Vec::new();
    let mut destination = None;
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        if arg == b"-t" {
            let Some(dir) = args.next() else {
                return if directory {
                    error(cx, &[b"CMake Error: Invalid value used with -t\n"])
                } else {
                    usage(cx, 1)
                };
            };
            if dir.starts_with(b"-") {
                return error(cx, &[b"CMake Error: Invalid value used with -t\n"]);
            }
            destination = Some(dir);
        } else {
            sources.push(arg);
        }
    }
    let target_option = destination.is_some();
    if !target_option && sources.len() < 2 {
        return if directory {
            error(
                cx,
                &[
                    b"Error: No directories or target specified (for ",
                    command,
                    b" command).\n",
                ],
            )
        } else {
            usage(cx, 1)
        };
    }
    let Some(destination) = destination.or_else(|| sources.pop()) else {
        return usage(cx, 1);
    };
    if directory && sources.is_empty() {
        return error(
            cx,
            &[
                b"Error: No source directories specified (for ",
                command,
                b" command).\n",
            ],
        );
    }
    let dest_path = path(cx, destination);
    let dest_is_dir = !directory && !destination.is_empty() && is_dir(cx, &dest_path);
    if !directory && (target_option || sources.len() > 1) && !dest_is_dir {
        return error(
            cx,
            &[
                b"Error: Target (for ",
                command,
                b" command) \"",
                destination,
                b"\" is not a directory.\n",
            ],
        );
    }

    let mut status = 0;
    for source in sources {
        let src_path = path(cx, source);
        let result = if source.is_empty() || destination.is_empty() {
            Err(fs_error(ErrorKind::NotFound, NO_FILE))
        } else if directory {
            copy_tree(&src_path, &dest_path, when, cx)
        } else {
            let source = cx.host.metadata(&src_path, Follow::Yes);
            if source.as_ref().is_ok_and(|meta| meta.kind == FileKind::Dir) {
                if dest_is_dir {
                    Ok(())
                } else {
                    create_copy_directory(&dest_path, cx)
                }
            } else {
                let target = if dest_is_dir {
                    src_path
                        .file_name()
                        .map_or_else(|| dest_path.clone(), |name| dest_path.join(name))
                } else {
                    dest_path.clone()
                };
                copy_entry(&src_path, &target, when, cx, source, dest_is_dir)
            }
        };
        if let Err(err) = result {
            let action: &[u8] = match (directory, when) {
                (true, _) => b"Error copying directory from \"",
                (false, CopyWhen::Always) => b"Error copying file \"",
                (false, CopyWhen::Different) => b"Error copying file (if different) from \"",
                (false, CopyWhen::Newer) => b"Error copying file (if newer) from \"",
            };
            status = error(
                cx,
                &[
                    action,
                    source,
                    b"\" to \"",
                    destination,
                    b"\": ",
                    err.os_message.as_bytes(),
                    b"\n",
                ],
            );
        }
    }
    status
}

fn copy_tree(src: &Path, dst: &Path, when: CopyWhen, cx: &ToolCtx<'_>) -> HostResult<()> {
    let metadata = cx.host.metadata(src, Follow::Yes)?;
    if metadata.kind != FileKind::Dir {
        return Err(fs_error(ErrorKind::NotADirectory, "Not a directory"));
    }

    let canonical_src = cx.host.canonicalize(src)?;
    let projected_dst = canonical_destination(dst, cx)?;
    if projected_dst == canonical_src {
        return Ok(());
    }
    if projected_dst.starts_with(&canonical_src) {
        return Err(fs_error(ErrorKind::InvalidInput, "Invalid argument"));
    }
    copy_tree_entries(src, dst, when, cx, &mut Vec::new(), metadata)
}

fn canonical_destination(dst: &Path, cx: &ToolCtx<'_>) -> HostResult<PathBuf> {
    let mut absent = Vec::new();
    let mut current = dst;
    loop {
        match cx.host.canonicalize(current) {
            Ok(mut resolved) => {
                for name in absent.into_iter().rev() {
                    resolved.push(name);
                }
                return Ok(resolved);
            }
            Err(err) if err.kind == ErrorKind::NotFound => {
                let (Some(parent), Some(name)) = (current.parent(), current.file_name()) else {
                    return Err(err);
                };
                absent.push(name);
                current = parent;
            }
            Err(err) => return Err(err),
        }
    }
}

fn copy_tree_entries(
    src: &Path,
    dst: &Path,
    when: CopyWhen,
    cx: &ToolCtx<'_>,
    ancestors: &mut Vec<(u64, u64)>,
    metadata: Metadata,
) -> HostResult<()> {
    let identity = (metadata.dev, metadata.ino);
    if ancestors.len() >= 256 || ancestors.contains(&identity) {
        return Err(fs_error(
            ErrorKind::InvalidInput,
            "Too many levels of symbolic links",
        ));
    }
    let target = cx.host.metadata(dst, Follow::Yes);
    if let Ok(target) = &target {
        if target.kind != FileKind::Dir {
            return Err(fs_error(ErrorKind::AlreadyExists, "File exists"));
        }
        if (target.dev, target.ino) == identity {
            return Ok(());
        }
    }
    let mut entries = cx.host.read_dir(src)?;
    entries.sort_unstable_by(|a, b| a.name.cmp(&b.name));
    if target.is_err() {
        cx.host.create_dir_all(dst)?;
    }
    ancestors.push(identity);
    let result = entries.iter().try_for_each(|entry| {
        let name = to_native(&entry.name);
        let from = src.join(&name);
        let to = dst.join(name);
        let meta = cx.host.metadata(&from, Follow::Yes)?;
        if meta.kind == FileKind::Dir {
            copy_tree_entries(&from, &to, when, cx, ancestors, meta)
        } else {
            copy_entry(&from, &to, when, cx, Ok(meta), true)
        }
    });
    ancestors.pop();
    result
}

fn create_copy_directory(dst: &Path, cx: &ToolCtx<'_>) -> HostResult<()> {
    if cx
        .host
        .metadata(dst, Follow::Yes)
        .is_ok_and(|m| m.kind != FileKind::Dir)
    {
        return Err(fs_error(ErrorKind::AlreadyExists, "File exists"));
    }
    cx.host.create_dir_all(dst)
}

fn copy_entry(
    src: &Path,
    dst: &Path,
    when: CopyWhen,
    cx: &ToolCtx<'_>,
    source: HostResult<Metadata>,
    parent_ready: bool,
) -> HostResult<()> {
    if source
        .as_ref()
        .is_ok_and(|source| source.kind != FileKind::File)
    {
        return Err(fs_error(ErrorKind::InvalidInput, "Invalid argument"));
    }
    if source.is_err() && when == CopyWhen::Newer {
        return source.map(|_| ());
    }
    let existing = cx.host.metadata(dst, Follow::No);
    if let Ok(source) = &source {
        let followed;
        let target = match &existing {
            Ok(target) if target.kind == FileKind::Symlink => {
                followed = cx.host.metadata(dst, Follow::Yes);
                followed.as_ref().ok()
            }
            Ok(target) => Some(target),
            Err(_) => None,
        };
        if let Some(target) = target {
            if (source.dev, source.ino) == (target.dev, target.ino) {
                return Ok(());
            }
            if target.kind == FileKind::File {
                match when {
                    CopyWhen::Newer if target.mtime >= source.mtime => return Ok(()),
                    CopyWhen::Different
                        if source.len == target.len
                            && same_bytes(src, dst, cx).unwrap_or(false) =>
                    {
                        return Ok(());
                    }
                    _ => {}
                }
            }
        }
    }

    if dst
        .as_os_str()
        .as_encoded_bytes()
        .last()
        .is_some_and(|b| *b == b'/' || (cfg!(windows) && *b == b'\\'))
    {
        return Err(fs_error(ErrorKind::NotFound, NO_FILE));
    }
    if !parent_ready && let Some(parent) = dst.parent() {
        create_copy_directory(parent, cx)?;
    }
    // CMake replaces the destination entry before it opens the source.
    let existing = if parent_ready {
        existing
    } else {
        cx.host.metadata(dst, Follow::No)
    };
    match existing {
        Ok(meta) if meta.kind == FileKind::Dir => cx.host.remove_dir(dst)?,
        Ok(_) => cx.host.remove_file(dst)?,
        Err(err) if err.kind == ErrorKind::NotFound => {}
        Err(err) => return Err(err),
    }
    source?;
    cx.host.copy_file(
        src,
        dst,
        CopyOptions {
            preserve_permissions: true,
            preserve_mtime: false,
        },
    )
}

fn is_missing(kind: ErrorKind) -> bool {
    matches!(kind, ErrorKind::NotFound | ErrorKind::NotADirectory)
}

fn same_bytes(first: &Path, second: &Path, cx: &ToolCtx<'_>) -> HostResult<bool> {
    const CHUNK: u64 = 64 * 1024;
    let mut offset = 0;
    loop {
        let range = ReadRange::Slice {
            offset,
            len: Some(CHUNK),
        };
        let a = cx.host.read(first, range)?;
        let b = cx.host.read(second, range)?;
        if a != b {
            return Ok(false);
        }
        if (a.len() as u64) < CHUNK {
            return Ok(true);
        }
        offset += CHUNK;
    }
}

pub(super) fn compare(args: &[BString], cx: &ToolCtx<'_>) -> i32 {
    let (a, b, ignore_eol) = match args {
        [a, b] => (a, b, false),
        [flag, a, b] if flag == b"--ignore-eol" => (a, b, true),
        _ => return usage(cx, if args.len() < 2 { 1 } else { 2 }),
    };
    if a.is_empty() || b.is_empty() {
        return 1;
    }
    let (a, b) = (path(cx, a), path(cx, b));
    if ignore_eol {
        match (cx.host.read(&a, read_all()), cx.host.read(&b, read_all())) {
            (Ok(a), Ok(b)) => i32::from(!lines(&a).eq(lines(&b))),
            _ => 1,
        }
    } else {
        i32::from(!same_bytes(&a, &b, cx).unwrap_or(false))
    }
}

fn lines(bytes: &[u8]) -> impl Iterator<Item = &[u8]> {
    bytes
        .strip_suffix(b"\n")
        .unwrap_or(bytes)
        .split(|b| *b == b'\n')
        .take(if bytes.is_empty() { 0 } else { usize::MAX })
        .map(|line| line.strip_suffix(b"\r").unwrap_or(line))
}

pub(super) fn remove(command: &[u8], args: &[BString], cx: &ToolCtx<'_>) -> i32 {
    if args.is_empty() {
        return usage(cx, 1);
    }
    match command {
        b"remove_directory" => {
            let mut status = 0;
            for dir in args.iter().filter(|arg| !arg.is_empty()) {
                let Ok(path) = removal_path(dir, cx) else {
                    continue;
                };
                if !is_dir(cx, &path) {
                    continue;
                }
                let result = if cx
                    .host
                    .metadata(&path, Follow::No)
                    .is_ok_and(|m| m.kind == FileKind::Symlink)
                {
                    cx.host.remove_file(&path)
                } else {
                    cx.host.remove_dir_all(&path)
                };
                if let Err(err) = result {
                    status = error(
                        cx,
                        &[
                            b"Error removing directory \"",
                            dir,
                            b"\": ",
                            err.os_message.as_bytes(),
                            b"\n",
                        ],
                    );
                }
            }
            status
        }
        b"remove" => {
            let mut force = false;
            let mut failed = false;
            for arg in args {
                if arg == b"-f" {
                    force = true;
                } else if !arg.is_empty()
                    && let Err(err) = cx.host.remove_file(&path(cx, arg))
                {
                    failed |= !force && !is_missing(err.kind);
                }
            }
            i32::from(failed)
        }
        _ => rm(args, cx),
    }
}

fn rm(args: &[BString], cx: &ToolCtx<'_>) -> i32 {
    let mut flags = true;
    let mut force = false;
    let mut recursive = false;
    let mut has_path = false;
    let mut status = 0;
    for arg in args {
        if flags && arg == b"--" {
            flags = false;
        } else if flags && let Some(options) = arg.strip_prefix(b"-") {
            for option in options {
                match option {
                    b'f' => force = true,
                    b'r' | b'R' => recursive = true,
                    _ => return error(cx, &[b"CMake Error: Unknown -E rm argument: ", arg, b"\n"]),
                }
            }
        } else if !arg.is_empty() {
            has_path = true;
            status |= rm_path(arg, force, recursive, cx);
        }
    }
    if !has_path {
        return error(cx, &[b"CMake Error: Missing file/directory to remove\n"]);
    }
    status
}

fn rm_path(arg: &[u8], force: bool, recursive: bool, cx: &ToolCtx<'_>) -> i32 {
    let file = match removal_path(arg, cx) {
        Ok(path) => path,
        Err(err) if is_missing(err.kind) => return rm_missing(arg, force, cx),
        Err(err) => {
            return error(
                cx,
                &[
                    b"Error removing directory \"",
                    arg,
                    b"\": ",
                    err.os_message.as_bytes(),
                    b"\n",
                ],
            );
        }
    };
    let metadata = cx.host.metadata(&file, Follow::No);
    if metadata.as_ref().is_err_and(|err| is_missing(err.kind)) {
        return rm_missing(arg, force, cx);
    }
    let dir = metadata.is_ok_and(|m| m.kind == FileKind::Dir);
    if dir && !recursive {
        return error(
            cx,
            &[
                b"Error removing directory \"",
                arg,
                b"\" without recursive option.\n",
            ],
        );
    }
    let result = if dir {
        cx.host.remove_dir_all(&file)
    } else {
        cx.host.remove_file(&file)
    };
    match result {
        Ok(()) => 0,
        Err(err) if force && err.kind == ErrorKind::NotFound => 0,
        Err(err) => error(
            cx,
            &[
                if dir {
                    b"Error removing directory \""
                } else {
                    b"Error removing file \""
                },
                arg,
                b"\": ",
                err.os_message.as_bytes(),
                b"\n",
            ],
        ),
    }
}

fn rm_missing(arg: &[u8], force: bool, cx: &ToolCtx<'_>) -> i32 {
    if force {
        return 0;
    }
    error(
        cx,
        &[
            b"CMake Error: File to remove does not exist and force is not set: ",
            arg,
            b"\n",
        ],
    )
}

fn removal_path(arg: &[u8], cx: &ToolCtx<'_>) -> HostResult<PathBuf> {
    let file = path(cx, arg);
    let Some(end) = arg
        .iter()
        .rposition(|&b| b != b'/' && (!cfg!(windows) || b != b'\\'))
    else {
        return Ok(file);
    };
    let Some(trimmed) = arg.get(..=end).filter(|bytes| bytes.len() < arg.len()) else {
        return Ok(file);
    };
    let entry = path(cx, trimmed);
    match cx.host.metadata(&entry, Follow::No)?.kind {
        FileKind::Dir => Ok(file),
        FileKind::Symlink if is_dir(cx, &entry) => cx.host.canonicalize(&entry),
        _ => Err(fs_error(ErrorKind::NotADirectory, "Not a directory")),
    }
}

pub(super) fn rename(args: &[BString], cx: &ToolCtx<'_>) -> i32 {
    let [old, new] = args else {
        return usage(cx, 1);
    };
    let result = if old.is_empty() || new.is_empty() {
        Err(fs_error(ErrorKind::NotFound, NO_FILE))
    } else {
        cx.host.rename(&path(cx, old), &path(cx, new))
    };
    match result {
        Ok(()) => 0,
        Err(err) => error(
            cx,
            &[
                b"Error renaming from \"",
                old,
                b"\" to \"",
                new,
                b"\": ",
                err.os_message.as_bytes(),
                b"\n",
            ],
        ),
    }
}

pub(super) fn link(command: &[u8], args: &[BString], cx: &ToolCtx<'_>) -> i32 {
    let [old, new] = args else {
        return usage(cx, 1);
    };
    let symbolic = command == b"create_symlink";
    let old_path = if symbolic {
        to_native(old)
    } else {
        path(cx, old)
    };
    let new_path = path(cx, new);
    let hard_source = if symbolic {
        None
    } else if let Ok(metadata) = cx.host.metadata(&old_path, Follow::Yes)
        && !old.is_empty()
    {
        Some(metadata)
    } else {
        return error(
            cx,
            &[
                b"failed to create hard link because source path '",
                old,
                b"' does not exist\n",
            ],
        );
    };
    let kind = if symbolic {
        b"symbolic link".as_slice()
    } else {
        b"hard link"
    };
    if !new.is_empty()
        && let Ok(existing) = cx.host.metadata(&new_path, Follow::No)
    {
        if hard_source.is_some_and(|m| (m.dev, m.ino) == (existing.dev, existing.ino))
            || (symbolic
                && cx
                    .host
                    .read_link(&new_path)
                    .is_ok_and(|target| target == old_path))
        {
            return 0;
        }
        let removed = if existing.kind == FileKind::Dir {
            Err(fs_error(
                ErrorKind::PermissionDenied,
                "Operation not permitted",
            ))
        } else {
            cx.host.remove_file(&new_path)
        };
        if let Err(err) = removed {
            return error(
                cx,
                &[
                    b"failed to create ",
                    kind,
                    b" '",
                    new,
                    b"' because existing path cannot be removed: ",
                    err.os_message.as_bytes(),
                    b"\n",
                ],
            );
        }
    }
    let result = if new.is_empty() {
        Err(fs_error(ErrorKind::NotFound, NO_FILE))
    } else if symbolic {
        cx.host.symlink(&old_path, &new_path)
    } else {
        cx.host
            .canonicalize(&old_path)
            .and_then(|source| cx.host.hard_link(&source, &new_path))
    };
    match result {
        Ok(()) => 0,
        Err(err) => error(
            cx,
            &[
                b"CMake Error: failed to create ",
                if symbolic {
                    b"symbolic link".as_slice()
                } else {
                    b"link"
                },
                b" '",
                new,
                b"': ",
                err.os_message.as_bytes(),
                b"\n",
            ],
        ),
    }
}
