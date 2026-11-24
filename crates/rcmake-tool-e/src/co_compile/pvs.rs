//! Process the PVS-Studio report for one object.

use std::path::Path;

use rcmake_base::BString;
use rcmake_host::{
    FileKind, Follow, Output, Stream,
    path::{from_native, to_native},
};

use super::{MISSING_TOOL, ToolCtx, contains, error, execute, path, write};
use crate::read_all;

pub(super) fn analyze(
    cx: &ToolCtx<'_>,
    mut command: Vec<BString>,
    compile: &[BString],
    source: &[u8],
    object: &[u8],
) -> i32 {
    if command.first().is_none_or(|program| program.is_empty()) {
        return error(cx, &[MISSING_TOOL]);
    }
    let log = BString::from([object, b"-pvs.log"].concat());
    let report = BString::from([object, b"-pvs.err"].concat());
    command.extend([
        "--source-file".into(),
        source.into(),
        "--output-file".into(),
        log.clone(),
        "--cl-params".into(),
    ]);
    command.extend(compile.iter().skip(1).cloned());
    let capture = |command: &[BString]| execute(cx, command, Output::Capture, Output::Capture);
    let (code, out) = match capture(&command) {
        Ok(result) => result,
        Err(code) => return code,
    };
    let log_path = path(cx, &log);
    if code != 0 {
        if code == 7 && cx.host.metadata(&log_path, Follow::Yes).is_err() {
            return 0;
        }
        write(cx, Stream::Stdout, &[&out.stdout]);
        write(cx, Stream::Stderr, &[&out.stderr]);
        return code;
    }

    let name: &[u8] = if cfg!(windows) {
        b"HtmlGenerator.exe"
    } else {
        b"plog-converter"
    };
    let program = command.first().map_or(b"".as_slice(), |arg| arg.as_slice());
    let Some(converter) = find_converter(cx, program, name) else {
        return error(cx, &[b"Could not find ", name, b"\n"]);
    };
    let (code, out) = match capture(&[
        converter,
        "-t".into(),
        "errorfile".into(),
        "-o".into(),
        report.clone(),
        log,
    ]) {
        Ok(result) => result,
        Err(code) => return code,
    };
    if !contains(&out.stdout, b"Total messages 0")
        && let Ok(bytes) = cx.host.read(&path(cx, &report), read_all())
        && let Some((_, diagnostics)) = super::split_once(&bytes, b'\n')
    {
        write(cx, Stream::Stderr, &[diagnostics]);
    }
    let _ = cx.host.remove_file(&log_path);
    if code != 0 {
        write(cx, Stream::Stderr, &[&out.stderr]);
    }
    code
}

fn find_converter(cx: &ToolCtx<'_>, analyzer: &[u8], name: &[u8]) -> Option<BString> {
    let executable = |path: &Path| {
        cx.host.metadata(path, Follow::Yes).is_ok_and(|metadata| {
            metadata.kind == FileKind::File && (cfg!(windows) || metadata.mode & 0o111 != 0)
        })
    };
    let analyzer = to_native(analyzer);
    let name = to_native(name);
    if let Some(parent) = analyzer.parent().filter(|dir| !dir.as_os_str().is_empty()) {
        let sibling = cx.cwd.join(parent).join(&name);
        if executable(&sibling) {
            return Some(from_native(&sibling));
        }
    }
    let search = to_native(&cx.host.env_get(b"PATH")?);
    std::env::split_paths(&search)
        .map(|dir| cx.cwd.join(dir).join(&name))
        .find(|candidate| executable(candidate))
        .map(|candidate| from_native(&candidate))
}
