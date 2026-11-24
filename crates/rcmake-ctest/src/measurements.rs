//! Read measurements from test output for the dashboard report.

use std::fmt::Write as _;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

use rcmake_base::ByteSlice;
use rcmake_host::{FileKind, Follow, Host, ProcessResult, ReadRange, path};

use crate::junit::escape;

const MAX_ATTACHMENT: u64 = 16 * 1024 * 1024;

pub(crate) fn write(
    host: &dyn Host,
    scratch: &Path,
    cwd: &Path,
    xml: &mut String,
    line: &[u8],
) -> bool {
    let line = line.trim_ascii();
    let Some((tag, rest)) = line
        .strip_prefix(b"<")
        .and_then(|tag| tag.split_once_str(b" "))
    else {
        return false;
    };
    let file = match tag {
        b"DartMeasurement" | b"CTestMeasurement" => false,
        b"DartMeasurementFile" | b"CTestMeasurementFile" => true,
        _ => return false,
    };
    let Some((attrs, body)) = rest.split_once_str(b">") else {
        return false;
    };
    let end = [b"</".as_slice(), tag, b">"].concat();
    let Some(body) = body.strip_suffix(end.as_slice()) else {
        return false;
    };
    let Some((name, kind)) = attribute(attrs, b"name").zip(attribute(attrs, b"type")) else {
        return false;
    };
    let name = escape(name, true);
    let kind = escape(kind, true);
    if !file {
        let _ = writeln!(
            xml,
            "\t\t\t\t<NamedMeasurement type=\"{kind}\" name=\"{name}\">\n\t\t\t\t\t<Value>{}</Value>\n\t\t\t\t</NamedMeasurement>",
            escape(body, false)
        );
        return true;
    }
    let attachment = cwd.join(path::to_native(body.trim_ascii()));
    if !host
        .metadata(&attachment, Follow::Yes)
        .is_ok_and(|meta| meta.kind == FileKind::File && meta.len <= MAX_ATTACHMENT)
    {
        return false;
    }
    let archived = kind == "file";
    let contents = if archived {
        archive(host, scratch, &attachment)
    } else {
        host.read(
            &attachment,
            ReadRange::All {
                limit: MAX_ATTACHMENT,
            },
        )
        .ok()
    };
    let Some(contents) = contents else {
        return false;
    };
    let _ = write!(xml, "\t\t\t\t<NamedMeasurement name=\"{name}\"");
    if archived {
        let filename = attachment
            .file_name()
            .map(Path::new)
            .map(path::from_native)
            .unwrap_or_default();
        let filename = escape(&filename, true);
        let _ = write!(
            xml,
            " encoding=\"base64\" compression=\"tar/gzip\" filename=\"{filename}\" type=\"{kind}\""
        );
    } else {
        let _ = write!(xml, " type=\"{kind}\" encoding=\"base64\"");
    }
    let _ = writeln!(
        xml,
        ">\n\t\t\t\t\t<Value>{}</Value>\n\t\t\t\t</NamedMeasurement>",
        base64(&contents)
    );
    true
}

fn attribute<'a>(mut attrs: &'a [u8], name: &[u8]) -> Option<&'a [u8]> {
    while !attrs.is_empty() {
        let (key, tail) = attrs.trim_ascii_start().split_once_str(b"=")?;
        let value = tail.trim_ascii_start();
        let (&quote, rest) = value.split_first()?;
        if !matches!(quote, b'"' | b'\'') {
            return None;
        }
        let end = rest.iter().position(|&byte| byte == quote)?;
        let (value, tail) = rest.split_at(end);
        if key.trim_ascii_end() == name {
            return Some(value);
        }
        attrs = tail.get(1..)?;
    }
    None
}

fn archive(host: &dyn Host, scratch: &Path, file: &Path) -> Option<Vec<u8>> {
    static SEQUENCE: AtomicU64 = AtomicU64::new(0);
    let id = SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let temporary = scratch.join(format!(
        ".ctest-measurement-{}-{id}.tar.gz",
        host.system().pid
    ));
    let name = path::from_native(Path::new(file.file_name()?));
    let cwd = file.parent()?.to_path_buf();
    host.create_dir_all(scratch).ok()?;
    host.write(&temporary, b"", rcmake_host::WriteMode::CreateNew)
        .ok()?;
    let args = vec![
        "-E".into(),
        "tar".into(),
        "czf".into(),
        path::from_native(&temporary),
        "--".into(),
        name,
    ];
    let packed = crate::script::run_process(
        host,
        cwd,
        path::from_native(&crate::script::cmake_executable(host)),
        args,
        host.env_all(),
        false,
    )
    .ok()
    .is_some_and(|output| output.results.first() == Some(&ProcessResult::Exited(0)));
    let bytes = packed
        .then(|| {
            host.read(
                &temporary,
                ReadRange::All {
                    limit: MAX_ATTACHMENT,
                },
            )
            .ok()
        })
        .flatten();
    let _ = host.remove_file(&temporary);
    bytes
}

fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut output = String::with_capacity(bytes.len().div_ceil(3) * 4);
    let character = |bits: u8| {
        ALPHABET
            .get(usize::from(bits))
            .copied()
            .map_or('=', char::from)
    };
    for chunk in bytes.chunks(3) {
        let Some((&first, rest)) = chunk.split_first() else {
            break;
        };
        let second = rest.first().copied();
        let third = rest.get(1).copied();
        output.push(character(first >> 2));
        output.push(character(
            (first & 3) << 4 | second.unwrap_or_default() >> 4,
        ));
        output.push(second.map_or('=', |byte| {
            character((byte & 15) << 2 | third.unwrap_or_default() >> 6)
        }));
        output.push(third.map_or('=', |byte| character(byte & 63)));
    }
    output
}
