//! Create a module definition file from object symbols and existing exports.

mod coff;

#[cfg(test)]
mod tests;

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use rcmake_base::{BString, ByteSlice};
use rcmake_host::{
    CommandSpec, Input, Output, ProcessResult, RunRequest, Stream, WriteMode, path as native,
};

use super::{ToolCtx, error, path, read_all};

pub(super) fn run(args: &[BString], cx: &ToolCtx<'_>) -> i32 {
    let (destination, response, option) = match args {
        [destination, response] => (destination, response, None),
        [destination, response, option] => (destination, response, Some(option)),
        _ => {
            return error(
                cx,
                &[b"Usage: cmake -E __create_def <output> <object-list> [--nm=<tool>]\n"],
            );
        }
    };
    let nm = if let Some(option) = option {
        let Some(nm) = option.strip_prefix(b"--nm=").filter(|nm| !nm.is_empty()) else {
            return error(cx, &[b"Use --nm=<tool> to specify a symbol tool.\n"]);
        };
        Some(nm)
    } else {
        None
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
    nm: Option<&[u8]>,
) -> Result<(), String> {
    let mut data = BTreeSet::new();
    let mut exports = BTreeSet::new();
    let mut explicit = BTreeMap::new();
    let list = cx
        .host
        .read(&path(cx, response), read_all())
        .map_err(|err| err.to_string())?;
    let mut objects = Vec::new();
    let mut machines = BTreeMap::new();
    let windows = cx.host.system().sysname.eq_ignore_ascii_case(b"Windows");
    for line in list
        .lines()
        .map(<[u8]>::trim_ascii)
        .filter(|line| !line.is_empty())
    {
        if line
            .last_chunk::<4>()
            .is_some_and(|suffix| suffix.eq_ignore_ascii_case(b".def"))
        {
            let file = cx
                .host
                .read(&path(cx, line), read_all())
                .map_err(|err| err.to_string())?;
            for entry in definition_entries(&file) {
                let name = entry
                    .split(|byte| byte.is_ascii_whitespace() || *byte == b'=')
                    .next()
                    .unwrap_or_default();
                explicit.insert(BString::from(name), BString::from(entry));
            }
        } else {
            let object = path(cx, line);
            let file = cx
                .host
                .read(&object, read_all())
                .map_err(|error| format!("{}: {error}", object.display()))?;
            if !coff::exports(&file, &mut data, &mut exports)
                .map_err(|message| format!("{}: {message}", object.display()))?
            {
                if !can_use_nm(&file, windows) {
                    return Err(format!(
                        "{} is not a supported COFF object or LLVM bitcode.",
                        object.display()
                    ));
                }
                if nm.is_none() {
                    return Err(format!(
                        "{} is not a supported COFF object. Use --nm for LLVM bitcode.",
                        object.display()
                    ));
                }
                objects.push(line);
                machines.insert(object, coff::machine(&file));
            }
        }
    }
    if let Some(nm) = nm.filter(|_| !objects.is_empty()) {
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
            let machine = machines.get(&path(cx, object)).copied().unwrap_or_default();
            let symbol = coff::normalize(symbol, machine);
            if coff::excluded(symbol, machine) {
                continue;
            }
            match kind {
                b"B" | b"D" => {
                    data.insert(BString::from(symbol));
                }
                b"T" => {
                    exports.insert(BString::from(symbol));
                }
                b"R" if symbol.starts_with(b"??_7") => {
                    if machine == coff::ARM64EC {
                        data.insert(BString::from(symbol));
                    } else {
                        exports.insert(BString::from(symbol));
                    }
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
    data.retain(|name| !explicit.contains_key(name));
    exports.retain(|name| !data.contains(name) && !explicit.contains_key(name));
    exports.extend(explicit.into_values());
    let mut output = BString::from("EXPORTS \n");
    for (symbols, suffix) in [(&data, b" \t DATA\n".as_slice()), (&exports, b"\n")] {
        for symbol in symbols {
            output.push(b'\t');
            output.extend_from_slice(symbol);
            output.extend_from_slice(suffix);
        }
    }
    if !cx
        .host
        .read(destination, read_all())
        .is_ok_and(|current| current == output.as_slice())
    {
        cx.host
            .write(destination, &output, WriteMode::Truncate)
            .map_err(|err| err.to_string())?;
    }
    Ok(())
}

fn can_use_nm(file: &[u8], windows: bool) -> bool {
    !windows || file.starts_with(b"BC\xc0\xde") || file.starts_with(b"\xde\xc0\x17\x0b")
}

fn definition_entries(contents: &[u8]) -> impl Iterator<Item = &[u8]> {
    let contents = contents.strip_prefix(b"\xef\xbb\xbf").unwrap_or(contents);
    contents.lines().filter_map(|line| {
        let line = line.split(|byte| *byte == b';').next()?.trim_ascii();
        let first = line.split(|byte| byte.is_ascii_whitespace()).next()?;
        if line.is_empty()
            || [
                b"EXPORTS".as_slice(),
                b"LIBRARY",
                b"NAME",
                b"DESCRIPTION",
                b"STACKSIZE",
                b"HEAPSIZE",
                b"VERSION",
                b"SECTIONS",
                b"IMPORTS",
            ]
            .iter()
            .any(|header| first.eq_ignore_ascii_case(header))
        {
            return None;
        }
        Some(line)
    })
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
