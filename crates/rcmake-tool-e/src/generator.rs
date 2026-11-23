use std::path::Path;

use rcmake_base::{BString, truth};
use rcmake_host::{HostResult, Stream, WriteMode, path::to_native};

use super::{ToolCtx, error, path, read_all, write};

pub(super) fn symlink_library(args: &[BString], cx: &ToolCtx<'_>) -> i32 {
    let [file, soname, name] = args else {
        return 1;
    };
    let first = symlink(file, soname, cx);
    let second = symlink(soname, name, cx);
    symlink_status(second.and(first), b"cmake_symlink_library", cx)
}

pub(super) fn symlink_executable(args: &[BString], cx: &ToolCtx<'_>) -> i32 {
    let [file, name] = args else {
        return 1;
    };
    symlink_status(symlink(file, name, cx), b"cmake_symlink_executable", cx)
}

fn symlink(file: &[u8], name: &[u8], cx: &ToolCtx<'_>) -> HostResult<()> {
    if file == name {
        return Ok(());
    }
    let basename = file
        .rsplit(|byte| *byte == b'/' || (cfg!(windows) && *byte == b'\\'))
        .next()
        .unwrap_or_default();
    let link = path(cx, name);
    if !name.is_empty() {
        let _ = cx.host.remove_file(&link);
    }
    let result = cx.host.symlink(&to_native(basename), &link);
    if let Err(err) = &result {
        error(
            cx,
            &[
                b"CMake Error: failed to create symbolic link '",
                name,
                b"': ",
                err.os_message.as_bytes(),
                b"\n",
            ],
        );
    }
    result
}

fn symlink_status(result: HostResult<()>, command: &[u8], cx: &ToolCtx<'_>) -> i32 {
    match result {
        Ok(()) => 0,
        Err(err) => error(
            cx,
            &[
                b"CMake Error: ",
                command,
                b": System Error: ",
                err.os_message.as_bytes(),
                b"\n",
            ],
        ),
    }
}

pub(super) fn progress_start(args: &[BString], cx: &ToolCtx<'_>) -> i32 {
    let [dir, count] = args else {
        return 1;
    };
    let dir = path(cx, dir).join("Progress");
    let count = integer(count).or_else(|| {
        let bytes = cx.host.read(&path(cx, count), read_all()).ok()?;
        let first = bytes
            .split(u8::is_ascii_whitespace)
            .find(|part| !part.is_empty())?;
        integer(first)
    });
    let _ = cx.host.remove_dir_all(&dir);
    if let Some(count) = count.filter(|count| *count != 0) {
        let _ = cx.host.create_dir_all(&dir);
        let _ = cx.host.write(
            &dir.join("count.txt"),
            format!("{count}\n").as_bytes(),
            WriteMode::Truncate,
        );
    }
    0
}

fn integer(bytes: &[u8]) -> Option<i32> {
    std::str::from_utf8(bytes).ok()?.parse().ok()
}

fn report_progress(dir: &Path, numbers: &[u8], cx: &ToolCtx<'_>) {
    let Ok(bytes) = cx.host.read(&dir.join("count.txt"), read_all()) else {
        return;
    };
    let Some(count) =
        integer(bytes.strip_suffix(b"\n").unwrap_or(&bytes)).filter(|count| *count > 0)
    else {
        return;
    };
    for number in numbers.split(|byte| *byte == b',') {
        if !number.is_empty()
            && !matches!(number, b"." | b".." | b"count.txt")
            && !number.iter().any(|byte| b"/\\:".contains(byte))
        {
            let _ = cx
                .host
                .write(&dir.join(to_native(number)), b"empty", WriteMode::CreateNew);
        }
    }
    if let Ok(entries) = cx.host.read_dir(dir) {
        let completed = entries
            .iter()
            .filter(|entry| entry.name != "count.txt")
            .count();
        let percent = completed.saturating_mul(100) / count as usize;
        write(cx, Stream::Stdout, &[format!("[{percent:3}%] ").as_bytes()]);
    }
}

pub(super) fn echo_color(args: &[BString], cx: &ToolCtx<'_>) -> i32 {
    let mut color = None;
    let mut bold = false;
    let mut newline = true;
    let mut enabled = true;
    let mut progress_dir = None;
    let terminal = color_terminal(cx);
    for arg in args {
        if let Some(style) = color_style(arg) {
            color = Some(style);
            bold = false;
            continue;
        }
        match arg.as_slice() {
            b"--bold" => bold = true,
            b"--no-newline" => newline = false,
            b"--newline" => newline = true,
            _ => {
                if let Some(value) = arg.strip_prefix(b"--switch=") {
                    enabled = truth::is_on(value);
                } else if let Some(dir) = arg.strip_prefix(b"--progress-dir=") {
                    progress_dir = (!dir.is_empty()).then(|| path(cx, dir).join("Progress"));
                } else if let Some(numbers) = arg.strip_prefix(b"--progress-num=") {
                    if let Some(dir) = &progress_dir {
                        report_progress(dir, numbers, cx);
                    }
                } else {
                    if enabled && terminal && (bold || color.is_some()) {
                        let reset: &[u8] = if color.unwrap_or(0) == 0 {
                            b"\x1b[0m"
                        } else {
                            b""
                        };
                        let weight: &[u8] = if bold { b"\x1b[1m" } else { b"" };
                        let color = color
                            .filter(|style| *style > 0)
                            .map(|style| format!("\x1b[{style}m"))
                            .unwrap_or_default();
                        write(
                            cx,
                            Stream::Stdout,
                            &[reset, weight, color.as_bytes(), arg, b"\x1b[0m"],
                        );
                    } else {
                        write(cx, Stream::Stdout, &[arg]);
                    }
                    if newline {
                        write(cx, Stream::Stdout, &[b"\n"]);
                    }
                }
            }
        }
    }
    0
}

fn color_style(arg: &[u8]) -> Option<u8> {
    Some(match arg {
        b"--black" => 30,
        b"--red" => 31,
        b"--green" => 32,
        b"--yellow" => 33,
        b"--blue" => 34,
        b"--magenta" => 35,
        b"--cyan" => 36,
        b"--white" => 37,
        b"--normal" => 0,
        _ => return None,
    })
}

fn color_terminal(cx: &ToolCtx<'_>) -> bool {
    let env_on = |key| {
        cx.host
            .env_get(key)
            .is_some_and(|value| !value.is_empty() && value != "0")
    };
    if env_on(b"NO_COLOR".as_slice()) {
        return false;
    }
    if env_on(b"CLICOLOR_FORCE") {
        return true;
    }
    cx.host.is_terminal(Stream::Stdout)
        && !cx
            .host
            .env_get(b"CLICOLOR")
            .is_some_and(|value| value == "0")
        && cx.host.env_get(b"TERM").is_some_and(|term| {
            [
                "ansi", "cygwin", "linux", "rxvt", "screen", "tmux", "vt100", "vt220", "xterm",
            ]
            .iter()
            .any(|prefix| term.starts_with(prefix.as_bytes()))
        })
}
