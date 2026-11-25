use std::path::Path;
use std::time::Duration;

use rcmake_base::path_lex::Lexical;
use rcmake_diag::{Effect, MessageKind};
use rcmake_eval::{CmdError, Cx, FileLockScope, Flow};
use rcmake_host::{
    FileKind, Follow, WriteMode,
    path::{from_native, to_native},
};

use super::{Result, error, set};

pub(super) fn run(cx: &mut Cx<'_>, args: &[&[u8]]) -> Result {
    match lock(cx, args) {
        Err(CmdError::Status(message)) => {
            cx.issue(MessageKind::Error, Effect::SetError, None, message);
            Err(CmdError::Reported)
        }
        result => result,
    }
}

fn lock(cx: &mut Cx<'_>, args: &[&[u8]]) -> Result {
    let [path, options @ ..] = args else {
        return Err(error("sub-command LOCK requires at least two arguments."));
    };
    let mut path = cx.resolve_path(path);
    let mut directory = false;
    let mut release = false;
    let mut guard = FileLockScope::Process;
    let mut output = b"".as_slice();
    let mut timeout = None;
    let mut args = options.iter().copied();
    while let Some(option) = args.next() {
        match option {
            b"DIRECTORY" => directory = true,
            b"RELEASE" => release = true,
            b"RESULT_VARIABLE" => {
                output = args
                    .next()
                    .ok_or_else(|| error("expected variable name after RESULT_VARIABLE"))?;
            }
            b"GUARD" => {
                let expected = b"expected FUNCTION, FILE or PROCESS after GUARD";
                guard = match args.next() {
                    Some(b"FUNCTION") => FileLockScope::Function,
                    Some(b"FILE") => FileLockScope::File,
                    Some(b"PROCESS") => FileLockScope::Process,
                    Some(value) => {
                        return Err(error(
                            [expected.as_slice(), b", but got:\n  \"", value, b"\"."].concat(),
                        ));
                    }
                    None => return Err(error(expected.as_slice())),
                };
            }
            b"TIMEOUT" => {
                let value = args
                    .next()
                    .ok_or_else(|| error("expected timeout value after TIMEOUT"))?;
                let secs = std::str::from_utf8(value)
                    .ok()
                    .and_then(|text| {
                        text.trim_start_matches(|ch: char| {
                            ch.is_ascii_whitespace() || ch == '\u{b}'
                        })
                        .parse::<i64>()
                        .ok()
                    })
                    .and_then(|secs| u64::try_from(secs).ok())
                    .ok_or_else(|| {
                        error(
                            [
                                b"TIMEOUT value \"",
                                value,
                                b"\" is not an unsigned integer.",
                            ]
                            .concat(),
                        )
                    })?;
                timeout = Some(Duration::from_secs(secs));
            }
            _ => {
                return Err(error(
                    [
                        b"expected DIRECTORY, RELEASE, GUARD, RESULT_VARIABLE or TIMEOUT"
                            .as_slice(),
                        b"\nbut got: \"",
                        option,
                        b"\".",
                    ]
                    .concat(),
                ));
            }
        }
    }
    if directory {
        path.push("cmake.lock");
    }
    let host = cx.host();
    let path = to_native(&Lexical::new(&from_native(&path)).normalize());
    if let Some(parent) = path.parent() {
        host.create_dir_all(parent)
            .map_err(|_| creation_error("directory", parent))?;
    }
    if host
        .metadata(&path, Follow::Yes)
        .is_ok_and(|meta| meta.kind != FileKind::File)
    {
        return Err(creation_error("file", &path));
    }
    host.write(&path, b"", WriteMode::Append)
        .map_err(|_| creation_error("file", &path))?;
    let path = host.canonicalize(&path).unwrap_or(path);
    let result = if release {
        cx.unlock_file(&path)
    } else {
        cx.lock_file(&path, guard, timeout)
    };
    match result {
        Ok(()) if output.is_empty() => Ok(Flow::Normal),
        Ok(()) => set(cx, output, "0"),
        Err(reason) if !output.is_empty() => set(cx, output, reason),
        Err(reason) => Err(error(format!(
            "error locking file\n  \"{}\"\n{reason}.",
            path.display()
        ))),
    }
}

fn creation_error(kind: &str, path: &Path) -> CmdError {
    error(format!(
        "{kind}\n  \"{}\"\ncreation failed (check permissions).",
        path.display()
    ))
}
