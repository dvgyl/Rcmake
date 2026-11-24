//! Create a module definition file from object symbols and existing exports.

use std::collections::BTreeSet;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use rcmake_base::{BString, ByteSlice};
use rcmake_host::{
    CommandSpec, Input, Output, ProcessResult, RunRequest, Stream, WriteMode, path as native,
};

use super::{ToolCtx, error, path, read_all};

pub(super) fn run(args: &[BString], cx: &ToolCtx<'_>) -> i32 {
    let [destination, response, option] = args else {
        return error(
            cx,
            &[b"Usage: cmake -E __create_def <output> <object-list> --nm=<tool>\n"],
        );
    };
    let Some(nm) = option.strip_prefix(b"--nm=").filter(|nm| !nm.is_empty()) else {
        return error(cx, &[b"__create_def requires --nm=<tool>\n"]);
    };
    match write_definition(cx, &path(cx, destination), response, nm) {
        Ok(()) => 0,
        Err(message) => error(cx, &[message.as_bytes(), b"\n"]),
    }
}

fn write_definition(
    cx: &ToolCtx<'_>,
    destination: &Path,
    response: &[u8],
    nm: &[u8],
) -> Result<(), String> {
    let mut data = BTreeSet::new();
    let mut exports = BTreeSet::new();
    let list = cx
        .host
        .read(&path(cx, response), read_all())
        .map_err(|err| err.to_string())?;
    let mut objects = Vec::new();
    for line in list
        .lines()
        .map(<[u8]>::trim_ascii)
        .filter(|line| !line.is_empty())
    {
        if line.to_ascii_lowercase().ends_with(b".def") {
            let file = cx
                .host
                .read(&path(cx, line), read_all())
                .map_err(|err| err.to_string())?;
            exports.extend(
                file.lines()
                    .map(<[u8]>::trim_ascii)
                    .skip_while(|line| *line != b"EXPORTS")
                    .skip(1)
                    .filter(|line| !line.is_empty())
                    .map(BString::from),
            );
        } else {
            objects.push(line);
        }
    }
    if !objects.is_empty() {
        let symbols = read_symbols(cx, destination, nm, &objects)?;
        for line in symbols.lines() {
            let Some((object, entry)) = line.rsplit_once_str(b": ") else {
                return Err(format!(
                    "Couldn't parse nm output line: {}",
                    String::from_utf8_lossy(line)
                ));
            };
            let mut fields = entry
                .split(|byte| byte.is_ascii_whitespace())
                .filter(|part| !part.is_empty());
            let (Some(symbol), Some(kind)) = (fields.next(), fields.next()) else {
                return Err(format!(
                    "Couldn't parse nm output line: {}",
                    String::from_utf8_lossy(line)
                ));
            };
            match kind {
                b"B" | b"D" => {
                    data.insert(BString::from(symbol));
                }
                b"T" => {
                    exports.insert(BString::from(symbol));
                }
                b"R" if symbol.starts_with(b"??_7") => {
                    exports.insert(BString::from(symbol));
                }
                b"A" | b"C" | b"I" | b"N" | b"R" | b"S" | b"U" | b"V" | b"W" | b"a" | b"b"
                | b"c" | b"d" | b"i" | b"n" | b"r" | b"s" | b"t" | b"v" | b"w" => {}
                _ => super::write(
                    cx,
                    Stream::Stderr,
                    &[
                        b"Ignoring symbol '",
                        symbol,
                        b"' with unrecognized type '",
                        kind,
                        b"' in object '",
                        object,
                        b"'.\n",
                    ],
                ),
            }
        }
    }
    let mut output = BString::from("EXPORTS \n");
    for (symbols, suffix) in [(&data, b" \t DATA\n".as_slice()), (&exports, b"\n")] {
        for symbol in symbols {
            output.push(b'\t');
            output.extend_from_slice(symbol);
            output.extend_from_slice(suffix);
        }
    }
    cx.host
        .write(destination, &output, WriteMode::Truncate)
        .map_err(|err| err.to_string())
}

fn read_symbols(
    cx: &ToolCtx<'_>,
    destination: &Path,
    nm: &[u8],
    objects: &[&[u8]],
) -> Result<Vec<u8>, String> {
    static NEXT_FILE: AtomicU64 = AtomicU64::new(0);
    let id = NEXT_FILE.fetch_add(1, Ordering::Relaxed);
    let file = destination.with_extension(format!("nm-{}-{id}.rsp", cx.host.system().pid));
    let mut contents = BString::default();
    for object in objects {
        contents.push(b'"');
        for &byte in *object {
            if matches!(byte, b'\\' | b'"') {
                contents.push(b'\\');
            }
            contents.push(byte);
        }
        contents.extend_from_slice(b"\"\n");
    }
    cx.host
        .write(&file, &contents, WriteMode::CreateNew)
        .map_err(|err| err.to_string())?;
    let mut response = BString::from("@");
    response.extend_from_slice(&native::from_native(&file));
    let args = vec![
        "--no-weak".into(),
        "--defined-only".into(),
        "--format=posix".into(),
        "--print-file-name".into(),
        response,
    ];
    let request = RunRequest {
        pipeline: vec![CommandSpec {
            program: nm.into(),
            args,
        }],
        cwd: cx.cwd.to_path_buf(),
        env: cx.host.env_all(),
        stdin: Input::Null,
        stdout: Output::Capture,
        stderr: Output::Capture,
        timeout: Some(Duration::from_secs(120)),
        tee: false,
    };
    let result = cx.host.run(&request, &mut |_, _| {});
    let _ = cx.host.remove_file(&file);
    let nm = String::from_utf8_lossy(nm);
    let result = result.map_err(|err| format!("{nm} returned an error: {err}"))?;
    match result.results.first() {
        Some(ProcessResult::Exited(0)) => Ok(result.stdout),
        Some(ProcessResult::SpawnFailed(err)) => Err(format!("{nm} returned an error: {err}")),
        result_code => {
            let stderr = String::from_utf8_lossy(&result.stderr);
            let detail = stderr.trim_end();
            if detail.is_empty() {
                Err(format!("{nm} returned an error: {result_code:?}"))
            } else {
                Err(format!("{nm} returned an error: {detail}"))
            }
        }
    }
}
