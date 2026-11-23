use rcmake_base::{BString, hash::HashAlgo};
use rcmake_eval::Cx;
use rcmake_host::{FileKind, FileTime, Follow, HostError, ReadRange, path::from_native};

use super::{Result, error, set};

pub(super) fn run(cx: &mut Cx<'_>, command: &[u8], args: &[&[u8]]) -> Result {
    let [input, out, rest @ ..] = args else {
        return Err(error(
            [command, b" must be called with at least two arguments."].concat(),
        ));
    };
    let path = cx.resolve_path(input);
    match command {
        b"READ" => read(cx, &path, out, rest),
        b"STRINGS" => super::strings::run(cx, &path, out, rest),
        b"TIMESTAMP" => timestamp(cx, &path, out, rest),
        b"SIZE" if rest.is_empty() => {
            let size = cx
                .host()
                .metadata(&path, Follow::Yes)
                .ok()
                .filter(|meta| meta.kind != FileKind::Dir)
                .and_then(|meta| {
                    cx.host()
                        .read(
                            &path,
                            ReadRange::Slice {
                                offset: 0,
                                len: Some(0),
                            },
                        )
                        .ok()
                        .map(|_| meta.len)
                });
            match size {
                Some(size) => set(cx, out, size.to_string()),
                None => Err(error(
                    [
                        b"SIZE requested of path that is not readable:\n\n  ".as_slice(),
                        input,
                        b"\n",
                    ]
                    .concat(),
                )),
            }
        }
        b"READ_SYMLINK" => {
            let result = match rest {
                [] => None,
                [b"RESULT", result] => Some(*result),
                _ => return Err(error("READ_SYMLINK called with unexpected arguments.")),
            };
            match cx.host().read_link(&path) {
                Ok(target) => {
                    set(cx, out, from_native(&target))?;
                    super::mutation::status(cx, result, Ok(()))
                }
                Err(_) => super::mutation::status(
                    cx,
                    result,
                    Err([
                        b"READ_SYMLINK requested of path that is not a symlink:\n  ".as_slice(),
                        input,
                    ]
                    .concat()
                    .into()),
                ),
            }
        }
        _ => Err(error("SIZE must be called with exactly two arguments.")),
    }
}

fn read(cx: &mut Cx<'_>, path: &std::path::Path, out: &[u8], args: &[&[u8]]) -> Result {
    let mut offset = 0;
    let mut limit = None;
    let mut hex = false;
    let mut args = args.iter().copied();
    while let Some(option) = args.next() {
        match option {
            b"OFFSET" => {
                offset = args
                    .next()
                    .and_then(decimal_prefix)
                    .map_or(0, |n| u64::try_from(n).unwrap_or(u64::MAX));
            }
            b"LIMIT" => {
                limit = args
                    .next()
                    .and_then(decimal_prefix)
                    .and_then(|n| u64::try_from(n).ok())
            }
            b"HEX" => hex = true,
            _ => {}
        }
    }
    let max = cx
        .limits()
        .max_file_read
        .min((cx.limits().max_value_bytes / if hex { 2 } else { 1 }) as u64);
    let cap = if hex { max } else { cx.limits().max_file_read };
    let len = if hex {
        limit.unwrap_or(u64::MAX).min(cap.saturating_add(1))
    } else {
        cap.saturating_add(1)
    };
    let data = cx
        .host()
        .read(
            path,
            ReadRange::Slice {
                offset,
                len: Some(len),
            },
        )
        .map_err(|e| open_error(path, &e))?;
    if (data.len() as u64 > cap && limit.is_none_or(|limit| limit > cap))
        || (hex && data.len() as u64 > max)
    {
        return Err(error("READ exceeds the file size limit"));
    }

    if hex {
        let mut output = Vec::with_capacity(data.len() * 2);
        const HEX: &[u8; 16] = b"0123456789abcdef";
        for b in data {
            output.extend(HEX.get(usize::from(b >> 4)));
            output.extend(HEX.get(usize::from(b & 15)));
        }
        set(cx, out, output)
    } else {
        let mut output = Vec::new();
        let mut remaining = limit.unwrap_or(u64::MAX);
        for raw in data.split_inclusive(|&byte| byte == b'\n') {
            if remaining == 0 {
                break;
            }
            let has_newline = raw.ends_with(b"\n");
            let line = raw.strip_suffix(b"\n").unwrap_or(raw);
            let line = line.strip_suffix(b"\r").unwrap_or(line);
            let take = line
                .len()
                .min(usize::try_from(remaining).unwrap_or(usize::MAX));
            if output
                .len()
                .saturating_add(take)
                .saturating_add(usize::from(has_newline))
                > cx.limits().max_value_bytes
            {
                return Err(error("READ exceeds the variable size limit"));
            }
            output.extend_from_slice(line.get(..take).unwrap_or_default());
            if has_newline {
                output.push(b'\n');
            }
            remaining = remaining.saturating_sub((line.len() + usize::from(has_newline)) as u64);
        }
        set(cx, out, output)
    }
}

fn decimal_prefix(bytes: &[u8]) -> Option<i64> {
    let bytes = bytes.trim_ascii_start();
    let sign = usize::from(matches!(bytes.first(), Some(b'+' | b'-')));
    let digits = bytes
        .iter()
        .skip(sign)
        .take_while(|b| b.is_ascii_digit())
        .count();
    std::str::from_utf8(bytes.get(..sign + digits)?)
        .ok()?
        .parse()
        .ok()
}

pub(super) fn hash(cx: &mut Cx<'_>, command: &[u8], algo: HashAlgo, args: &[&[u8]]) -> Result {
    let [input, out] = args else {
        return Err(error(
            [command, b" must be called with exactly two arguments."].concat(),
        ));
    };
    let path = cx.resolve_path(input);
    let data = cx
        .host()
        .read(
            &path,
            ReadRange::All {
                limit: cx.limits().max_file_read,
            },
        )
        .map_err(|e| {
            error(
                [
                    command,
                    b" failed to read file \"",
                    input,
                    b"\": ",
                    e.os_message.as_bytes(),
                ]
                .concat(),
            )
        })?;
    set(cx, out, algo.hex_digest(&data))
}

fn open_error(path: &std::path::Path, err: &HostError) -> rcmake_eval::CmdError {
    error(
        [
            b"failed to open for reading (",
            err.os_message.as_bytes(),
            b"):\n\n  ",
            &from_native(path),
            b"\n",
        ]
        .concat(),
    )
}

fn timestamp(cx: &mut Cx<'_>, path: &std::path::Path, out: &[u8], args: &[&[u8]]) -> Result {
    let (format, utc) = match args {
        [] => (b"".as_slice(), false),
        [b"UTC"] => (b"".as_slice(), true),
        [format] => (*format, false),
        [format, b"UTC"] => (*format, true),
        _ => return Err(error("TIMESTAMP called with unexpected arguments.")),
    };
    let value = cx
        .host()
        .metadata(path, Follow::Yes)
        .ok()
        .and_then(|meta| format_time(cx, meta.mtime, format, utc));
    set(cx, out, value.unwrap_or_default())
}

fn format_time(cx: &Cx<'_>, time: FileTime, format: &[u8], utc: bool) -> Option<BString> {
    let nanos = i32::try_from(time.nanos).ok()?;
    let instant = jiff::Timestamp::new(time.secs, nanos).ok()?;
    let zone = if utc {
        jiff::tz::TimeZone::UTC
    } else {
        cx.host().time_zone()
    };
    let date = instant.to_zoned(zone);
    let format: &[u8] = if format.is_empty() {
        if utc {
            b"%Y-%m-%dT%H:%M:%SZ"
        } else {
            b"%Y-%m-%dT%H:%M:%S"
        }
    } else {
        format
    };
    let mut output = BString::default();
    let mut bytes = format.iter().copied();
    while let Some(byte) = bytes.next() {
        if byte != b'%' {
            output.push(byte);
            continue;
        }
        let Some(specifier) = bytes.next() else {
            output.push(b'%');
            break;
        };
        match specifier {
            b'%' => output.push(b'%'),
            b'f' => output.extend_from_slice(format!("{:06}", time.nanos / 1000).as_bytes()),
            b's' => output.extend_from_slice(time.secs.to_string().as_bytes()),
            b'Z' if utc => output.extend_from_slice(b"GMT"),
            b'a' | b'A' | b'b' | b'B' | b'd' | b'H' | b'I' | b'j' | b'm' | b'M' | b'p' | b'S'
            | b'U' | b'V' | b'w' | b'W' | b'y' | b'Y' | b'z' | b'Z' => {
                output.extend_from_slice(
                    jiff::fmt::strtime::format([b'%', specifier], &date)
                        .ok()?
                        .as_bytes(),
                );
            }
            _ => output.extend_from_slice(&[b'%', specifier]),
        }
    }
    Some(output)
}
