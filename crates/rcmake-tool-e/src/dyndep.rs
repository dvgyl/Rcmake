//! `cmake -E cmake_ninja_dyndep` collates one target's module scan results.

use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

use rcmake_base::{BString, limits::Limits, path_lex::Lexical};
use rcmake_cxxmodules::collate::{CollationInput, collate, read_modules_json, write_modules_json};
use rcmake_cxxmodules::dyndep::{DyndepRequest, HEADER};
use rcmake_cxxmodules::p1689::parse_ddi;
use rcmake_cxxmodules::tdi::read_tdi;
use rcmake_cxxmodules::{CxxModulesJson, ModmapFormat, ModulesError, TargetDependInfo};
use rcmake_host::{Stream, WriteMode, path::from_native};

use crate::{ToolCtx, error, path, read_all};

mod fortran;

const COMMAND: &str = "-E cmake_ninja_dyndep";

/// Run `cmake -E cmake_ninja_dyndep` and return its exit code.
pub(crate) fn run(args: &[BString], cx: &ToolCtx<'_>) -> i32 {
    match execute(args, cx) {
        Ok(()) => 0,
        Err(message) => error(cx, &[b"CMake Error: ", &message, b"\n"]),
    }
}

fn execute(args: &[BString], cx: &ToolCtx<'_>) -> Result<(), BString> {
    let args = expand_response(args, cx)?;
    let request = parse(&args).map_err(|err| format!("{COMMAND} {err}"))?;
    if !matches!(request.lang.as_slice(), b"CXX" | b"Fortran") {
        return Err(format!("{COMMAND} does not support language {}", request.lang).into());
    }
    let tdi_bytes = read_input(cx, &request.tdi, &request.tdi)?;
    let tdi = read_tdi(&tdi_bytes)
        .map_err(|err| format!("{COMMAND} failed to parse {}{err}", request.tdi))?;
    for (key, directory) in [
        ("dir-cur-bld", &tdi.dir_cur_bld),
        ("dir-cur-src", &tdi.dir_cur_src),
        ("dir-top-bld", &tdi.dir_top_bld),
        ("dir-top-src", &tdi.dir_top_src),
    ] {
        if !Lexical::new(directory).absolute() {
            return Err(format!("{COMMAND} --tdi= file has no absolute {key}").into());
        }
    }
    if request.lang == b"Fortran" {
        return fortran::execute(&request, cx, &tdi, &tdi_bytes);
    }
    execute_cxx(&request, cx, tdi)
}

fn execute_cxx(
    request: &DyndepRequest,
    cx: &ToolCtx<'_>,
    tdi: TargetDependInfo,
) -> Result<(), BString> {
    let linked = read_tables(cx, &tdi.linked_target_dirs)?;
    let forwarded = read_tables(cx, &tdi.forward_modules_from_target_dirs)?;
    let native = tdi
        .native_target_dir
        .as_ref()
        .map(|dir| read_table(cx, dir))
        .transpose()?;
    let mut ddis = Vec::new();
    for ddi in &request.ddis {
        let contents = read_input(cx, ddi, ddi)?;
        ddis.push(parse_ddi(&contents, ddi).map_err(|err| {
            format!("{err}\nCMake Error: {COMMAND} failed to parse ddi file {ddi}")
        })?);
    }
    let dd = path(cx, &request.dd);
    let input = CollationInput {
        tdi,
        ddis,
        linked,
        forwarded,
        native,
        modmap_format: request.modmap_format,
    };
    let output = match collate(&input) {
        Ok(output) => output,
        Err(err) => {
            if matches!(err, ModulesError::Cycle(_) | ModulesError::PrivateImport(_)) {
                write_output(cx, &dd, HEADER, false)?;
            }
            return Err(err.to_string().into());
        }
    };

    let metadata = write_modules_json(&output.modules).map_err(|err| err.to_string())?;
    for (name, data) in output.modmaps {
        write_output(cx, &path(cx, &name), &data, true)?;
    }
    write_output(cx, &dd.with_file_name("CXXModules.json"), &metadata, true)?;
    if let Some(info) = input.tdi.interface_objects
        && let Some(rsp) = info.rsp_file
    {
        let mut contents = BString::default();
        for object in output.modules.interface_objects.values() {
            let top = input
                .tdi
                .dir_top_bld
                .strip_suffix(b"/")
                .unwrap_or(&input.tdi.dir_top_bld);
            let object = object
                .strip_prefix(top)
                .and_then(|tail| tail.strip_prefix(b"/"))
                .unwrap_or(object);
            append_response_path(&mut contents, object);
        }
        write_output(cx, &path(cx, &rsp), &contents, true)?;
    }
    write_output(cx, &dd, &output.dyndep, false)?;
    let mut diagnostics = output.diagnostics.into_iter();
    if let Some(message) = diagnostics.next() {
        let mut diagnostic = message;
        for message in diagnostics {
            diagnostic.extend_from_slice(b"\nCMake Error: ");
            diagnostic.extend_from_slice(&message);
        }
        return Err(diagnostic);
    }
    Ok(())
}

fn read_input(cx: &ToolCtx<'_>, label: &[u8], name: &[u8]) -> Result<Vec<u8>, BString> {
    cx.host.read(&path(cx, name), read_all()).map_err(|err| {
        let label = String::from_utf8_lossy(label);
        let name = String::from_utf8_lossy(name);
        if err.kind == std::io::ErrorKind::NotFound {
            format!("{COMMAND} failed to parse {label}File not found: {name}").into()
        } else {
            format!("{COMMAND} failed to read {name}: {err}").into()
        }
    })
}

fn read_tables(cx: &ToolCtx<'_>, directories: &[BString]) -> Result<Vec<CxxModulesJson>, BString> {
    directories.iter().map(|dir| read_table(cx, dir)).collect()
}

fn read_table(cx: &ToolCtx<'_>, directory: &[u8]) -> Result<CxxModulesJson, BString> {
    read_table_file(cx, directory, b"CXXModules.json")
}

fn read_table_file(
    cx: &ToolCtx<'_>,
    directory: &[u8],
    name: &[u8],
) -> Result<CxxModulesJson, BString> {
    let file = Lexical::new(directory).append(name);
    let contents = read_input(cx, directory, &file)?;
    read_modules_json(&contents).map_err(|err| {
        format!(
            "{COMMAND} failed to parse {}{err}",
            String::from_utf8_lossy(directory)
        )
        .into()
    })
}

pub(crate) fn write_output(
    cx: &ToolCtx<'_>,
    file: &Path,
    data: &[u8],
    only_if_changed: bool,
) -> Result<(), BString> {
    if only_if_changed
        && cx
            .host
            .read(file, read_all())
            .is_ok_and(|current| current == data)
    {
        return Ok(());
    }
    static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);
    let pid = cx.host.system().pid;
    let write_error = |err: rcmake_host::HostError| {
        BString::from(format!("Cannot write {}: {err}", from_native(file)))
    };
    for _ in 0..64 {
        let sequence = NEXT_TEMP.fetch_add(1, Ordering::Relaxed);
        let temp = file.with_file_name(format!(".rcmake-dyndep-{pid}-{sequence}.tmp"));
        match cx.host.write(&temp, data, WriteMode::CreateNew) {
            Ok(()) => {
                let result = cx.host.rename(&temp, file);
                if result.is_err() {
                    let _ = cx.host.remove_file(&temp);
                }
                return result.map_err(write_error);
            }
            Err(err) if err.kind == std::io::ErrorKind::AlreadyExists => {}
            Err(err) => return Err(write_error(err)),
        }
    }
    Err(format!("Cannot create a temporary file for {}", from_native(file)).into())
}

fn expand_response(args: &[BString], cx: &ToolCtx<'_>) -> Result<Vec<BString>, BString> {
    let mut expanded = Vec::new();
    let mut bytes = 0_usize;
    let limit = Limits::default().max_value_bytes;
    for arg in args {
        if let Some(name) = arg.strip_prefix(b"@") {
            let contents = match cx.host.read(&path(cx, name), read_all()) {
                Ok(contents) => contents,
                Err(err) => {
                    let diagnostic = format!(
                        "CMake Error: failed to open for reading ({}):\n  {}\n",
                        err.os_message,
                        String::from_utf8_lossy(name)
                    );
                    cx.host.write_out(Stream::Stderr, diagnostic.as_bytes());
                    continue;
                }
            };
            let line = contents
                .split(|&byte| byte == b'\n')
                .next()
                .unwrap_or_default();
            bytes = bytes.saturating_add(line.len());
            if bytes > limit {
                return Err("Response file arguments exceed the expansion limit".into());
            }
            expanded.extend(response_words(line));
        } else {
            expanded.push(arg.clone());
        }
    }
    Ok(expanded)
}

fn response_words(line: &[u8]) -> Vec<BString> {
    if cfg!(windows) {
        return windows_response_words(line);
    }

    let mut words = Vec::new();
    let mut word = BString::default();
    let mut quote = None;
    let mut started = false;
    let mut bytes = line.iter().copied();
    while let Some(byte) = bytes.next() {
        match (quote, byte) {
            (Some(expected), byte) if expected == byte => quote = None,
            (None, b'\'' | b'"') => {
                quote = Some(byte);
                started = true;
            }
            (None | Some(b'"'), b'\\') => {
                if let Some(next) = bytes.next() {
                    word.push(next);
                    started = true;
                }
            }
            (None, byte) if byte.is_ascii_whitespace() => {
                if started {
                    words.push(std::mem::take(&mut word));
                    started = false;
                }
            }
            _ => {
                word.push(byte);
                started = true;
            }
        }
    }
    if started {
        words.push(word);
    }
    words
}

fn windows_response_words(line: &[u8]) -> Vec<BString> {
    let mut words = Vec::new();
    let mut quoted = false;
    let mut started = false;
    let mut word = BString::default();
    let mut bytes = line.iter().copied().peekable();
    while let Some(byte) = bytes.next() {
        match byte {
            b'\\' => {
                let mut count = 1;
                while bytes.next_if_eq(&b'\\').is_some() {
                    count += 1;
                }
                if bytes.next_if_eq(&b'"').is_some() {
                    word.extend(std::iter::repeat_n(b'\\', count / 2));
                    if count % 2 == 0 {
                        quoted = !quoted;
                    } else {
                        word.push(b'"');
                    }
                } else {
                    word.extend(std::iter::repeat_n(b'\\', count));
                }
                started = true;
            }
            b'"' => {
                quoted = !quoted;
                started = true;
            }
            b' ' | b'\t' | b'\r' if !quoted => {
                if started {
                    words.push(std::mem::take(&mut word));
                    started = false;
                }
            }
            _ => {
                word.push(byte);
                started = true;
            }
        }
    }
    if started {
        words.push(word);
    }
    words
}

fn append_response_path(out: &mut BString, path: &[u8]) {
    if path
        .iter()
        .any(|byte| byte.is_ascii_whitespace() || b"\"'\\$".contains(byte))
    {
        out.push(b'"');
        for &byte in path {
            if b"\"\\".contains(&byte) {
                out.push(b'\\');
            }
            out.push(byte);
        }
        out.push(b'"');
    } else {
        out.extend_from_slice(path);
    }
    out.push(b'\n');
}

fn parse(args: &[BString]) -> Result<DyndepRequest, BString> {
    let mut request = DyndepRequest::default();
    for arg in args {
        if let Some(value) = arg.strip_prefix(b"--tdi=") {
            request.tdi = value.into();
        } else if let Some(value) = arg.strip_prefix(b"--lang=") {
            request.lang = value.into();
        } else if let Some(value) = arg.strip_prefix(b"--dd=") {
            request.dd = value.into();
        } else if let Some(value) = arg.strip_prefix(b"--modmapfmt=") {
            let format = ModmapFormat::from_name(value).ok_or_else(|| {
                BString::from([b"does not understand the ", value, b" module map format"].concat())
            })?;
            request.modmap_format = Some(format);
        } else if !arg.starts_with(b"--") && arg.ends_with(b".ddi") {
            request.ddis.push(arg.clone());
        } else {
            return Err([b"unknown argument: ", arg.as_slice()].concat().into());
        }
    }
    for (value, flag) in [
        (&request.tdi, b"--tdi=".as_slice()),
        (&request.lang, b"--lang="),
        (&request.dd, b"--dd="),
    ] {
        if value.is_empty() {
            return Err([b"requires value for ", flag].concat().into());
        }
    }
    Ok(request)
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::time::Duration;

    use rcmake_host::{Follow, Host, PureHost};

    use super::*;

    #[test]
    fn writes_dyndep_from_response_and_preserves_module_map_time()
    -> Result<(), rcmake_host::HostError> {
        let cwd = PathBuf::from(if cfg!(windows) { "C:/out" } else { "/out" });
        let host = PureHost::new(cwd.clone(), cwd.join("cmake"));
        let root = from_native(&cwd);
        let tdi = format!(
            r#"{{
            "dir-cur-bld":"{root}","dir-cur-src":"{root}",
            "dir-top-bld":"{root}","dir-top-src":"{root}","module-dir":"bmi",
            "cxx-modules":{{"a a.o":{{"type":"CXX_MODULES","visibility":"PUBLIC"}}}}
        }}"#
        )
        .replace('\\', "/");
        host.seed_file(&cwd.join("tdi.json"), tdi.as_bytes())?;
        host.seed_file(
            &cwd.join("a a.ddi"),
            br#"{"version":1,"rules":[{"primary-output":"a a.o","provides":[{"logical-name":"A"}]}]}"#,
        )?;
        host.seed_file(&cwd.join("scan.rsp"), b"\"a a.ddi\"\n")?;
        let cx = ToolCtx {
            host: &host,
            cwd: &cwd,
            exe_display: b"cmake",
            capabilities: &|| rcmake_json::Value::Null,
        };
        let args = [
            "--tdi=tdi.json",
            "--lang=CXX",
            "--dd=out.dd",
            "--modmapfmt=clang",
            "@scan.rsp",
        ]
        .map(BString::from);
        assert_eq!(run(&args, &cx), 0, "{}", host.stream_bytes(Stream::Stderr));
        assert_eq!(
            host.read(&cwd.join("out.dd"), read_all())?,
            b"ninja_dyndep_version = 1.0\nbuild a$ a.o | bmi/A.pcm: dyndep\n  restat = 1\n\n"
        );
        let map = cwd.join("a a.o.modmap");
        assert_eq!(
            host.read(&map, read_all())?,
            b"-x c++-module\n-fmodule-output=\"bmi/A.pcm\"\n"
        );
        let before = host.metadata(&map, Follow::Yes)?.mtime;
        host.sleep(Duration::from_secs(2));
        assert_eq!(run(&args, &cx), 0);
        assert_eq!(host.metadata(&map, Follow::Yes)?.mtime, before);
        Ok(())
    }
}
