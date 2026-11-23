//! Cross-platform utilities for `cmake -E`.

#![forbid(unsafe_code)]

mod archive;
mod binary;
mod depfile;
mod files;
mod generator;
mod process;

#[cfg(test)]
mod tests;

use std::path::{Path, PathBuf};

use rcmake_base::{BString, hash::HashAlgo, limits::Limits};
use rcmake_host::Host;
use rcmake_host::{FileKind, Follow, ReadRange, Stream, path::to_native};
use rcmake_json::Value;

/// Context for a `cmake -E` command.
pub struct ToolCtx<'a> {
    /// Host for file, environment, process, clock, and stream operations.
    pub host: &'a dyn Host,
    /// Current working directory.
    pub cwd: &'a Path,
    /// Executable path shown in the usage message.
    pub exe_display: &'a [u8],
    /// Function that supplies the capabilities document.
    pub capabilities: &'a dyn Fn() -> Value,
}

/// Run a `cmake -E` command and return its exit code.
#[must_use]
pub fn run(args: &[BString], cx: &ToolCtx<'_>) -> i32 {
    let Some((command, args)) = args.split_first() else {
        return usage(cx, 1);
    };
    match command.as_slice() {
        b"true" => 0,
        b"false" => 1,
        b"echo" | b"echo_append" => {
            let mut output = args.join(&b' ');
            if command == b"echo" {
                output.push(b'\n');
            }
            cx.host.write_out(Stream::Stdout, &output);
            0
        }
        b"capabilities" if args.is_empty() => {
            let mut json = rcmake_json::write_compact(&(cx.capabilities)());
            json.push(b'\n');
            cx.host.write_out(Stream::Stdout, &json);
            0
        }
        b"capabilities" => error(cx, &[b"-E capabilities accepts no additional arguments\n"]),
        b"cat" => binary::cat(args, cx),
        b"bin2c" => binary::bin2c(args, cx),
        b"make_directory" => files::make_directory(args, cx),
        b"touch" | b"touch_nocreate" => files::touch(command, args, cx),
        b"copy"
        | b"copy_if_different"
        | b"copy_if_newer"
        | b"copy_directory"
        | b"copy_directory_if_different"
        | b"copy_directory_if_newer" => files::copy(command, args, cx),
        b"remove" | b"remove_directory" | b"rm" => files::remove(command, args, cx),
        b"rename" => files::rename(args, cx),
        b"compare_files" => files::compare(args, cx),
        b"create_symlink" | b"create_hardlink" => files::link(command, args, cx),
        b"md5sum" => checksum(HashAlgo::Md5, args, cx),
        b"sha1sum" => checksum(HashAlgo::Sha1, args, cx),
        b"sha224sum" => checksum(HashAlgo::Sha224, args, cx),
        b"sha256sum" => checksum(HashAlgo::Sha256, args, cx),
        b"sha384sum" => checksum(HashAlgo::Sha384, args, cx),
        b"sha512sum" => checksum(HashAlgo::Sha512, args, cx),
        b"environment" => {
            process::print_environment(cx.host.env_all(), cx);
            0
        }
        b"env" => process::env(args, cx),
        b"chdir" => process::chdir(args, cx),
        b"time" => process::time(args, cx),
        b"sleep" => process::sleep(args, cx),
        b"tar" => archive::tar(args, cx),
        b"cmake_symlink_library" if args.len() == 3 => generator::symlink_library(args, cx),
        b"cmake_symlink_executable" if args.len() == 2 => generator::symlink_executable(args, cx),
        b"cmake_transform_depfile" if args.len() == 8 => depfile::transform(args, cx),
        b"cmake_echo_color" => generator::echo_color(args, cx),
        b"cmake_progress_start" if args.len() == 2 => generator::progress_start(args, cx),
        // CMake 4.5.1 records progress in cmake_echo_color.
        b"cmake_progress_report" if !args.is_empty() => 0,
        _ => unknown(command, cx),
    }
}

fn split_once(input: &[u8], delimiter: u8) -> Option<(&[u8], &[u8])> {
    let mut parts = input.splitn(2, |byte| *byte == delimiter);
    Some((parts.next()?, parts.next()?))
}

fn path(cx: &ToolCtx<'_>, arg: &[u8]) -> PathBuf {
    cx.cwd.join(to_native(arg))
}

fn is_dir(cx: &ToolCtx<'_>, path: &Path) -> bool {
    cx.host
        .metadata(path, Follow::Yes)
        .is_ok_and(|m| m.kind == FileKind::Dir)
}

fn read_all() -> ReadRange {
    ReadRange::All {
        limit: Limits::default().max_file_read,
    }
}

fn write(cx: &ToolCtx<'_>, stream: Stream, parts: &[&[u8]]) {
    cx.host.write_out(stream, &parts.concat());
}

fn error(cx: &ToolCtx<'_>, parts: &[&[u8]]) -> i32 {
    write(cx, Stream::Stderr, parts);
    1
}

fn usage(cx: &ToolCtx<'_>, code: i32) -> i32 {
    error(
        cx,
        &[
            b"CMake Error: cmake version 4.5.1\nUsage: ",
            cx.exe_display,
            b" -E <command> [arguments...]\nAvailable commands:\n",
            include_bytes!("usage.txt"),
            b"\n",
        ],
    );
    code
}

fn unknown(command: &[u8], cx: &ToolCtx<'_>) -> i32 {
    let mut message = [
        b"CMake Error: Unknown command \"".as_slice(),
        command,
        b"\".",
    ]
    .concat();
    if let Some(name) = closest_command(command) {
        message.extend_from_slice(b" Did you mean \"");
        message.extend_from_slice(name);
        message.extend_from_slice(b"\"?");
    }
    error(cx, &[&message, b"\n"]);
    usage(cx, 1)
}

fn closest_command(command: &[u8]) -> Option<&'static [u8]> {
    let mut best = None;
    let mut threshold = (command.len() / 5 + 1).min(4) + 1;
    for line in include_bytes!("usage.txt").split(|byte| *byte == b'\n') {
        let Some(name) = line
            .strip_prefix(b"  ")
            .and_then(|rest| rest.split(|b| *b == b' ').next())
        else {
            continue;
        };
        if name.is_empty() || command.len().abs_diff(name.len()) >= threshold {
            continue;
        }
        let distance = edit_distance(command, name);
        if distance < threshold {
            best = Some(name);
            threshold = distance;
        }
    }
    best
}

fn edit_distance(left: &[u8], right: &[u8]) -> usize {
    let mut row: Vec<usize> = (0..=right.len()).collect();
    for (index, a) in left.iter().enumerate() {
        let mut diagonal = index;
        let mut previous = index + 1;
        for (b, entry) in right.iter().zip(row.iter_mut().skip(1)) {
            let distance = (diagonal + usize::from(a != b)).min(previous.min(*entry) + 1);
            diagonal = *entry;
            *entry = distance;
            previous = distance;
        }
        if let Some(first) = row.first_mut() {
            *first = index + 1;
        }
    }
    row.last().copied().unwrap_or_default()
}

fn checksum(algo: HashAlgo, args: &[BString], cx: &ToolCtx<'_>) -> i32 {
    if args.is_empty() {
        return usage(cx, 1);
    }
    let mut failures: i32 = 0;
    let mut stdin_used = false;
    for file in args {
        let result = if file == b"-" {
            if stdin_used {
                error(cx, &[b"-: No such file or directory\n"]);
                failures = failures.saturating_add(1);
                continue;
            }
            stdin_used = true;
            cx.host.read_stdin(Limits::default().max_file_read)
        } else {
            let file_path = path(cx, file);
            if is_dir(cx, &file_path) && !file.is_empty() {
                error(cx, &[b"Error: ", file, b" is a directory\n"]);
                failures = failures.saturating_add(1);
                continue;
            }
            cx.host.read(&file_path, read_all())
        };
        match result {
            Ok(bytes) => write(
                cx,
                Stream::Stdout,
                &[algo.hex_digest(&bytes).as_bytes(), b"  ", file, b"\n"],
            ),
            Err(err) => {
                let reason = if file.is_empty() {
                    "No such file or directory"
                } else {
                    &err.os_message
                };
                error(cx, &[file, b": ", reason.as_bytes(), b"\n"]);
                failures = failures.saturating_add(1);
            }
        }
    }
    failures
}
