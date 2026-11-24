use std::fmt::Write as _;
use std::io::ErrorKind;
use std::path::PathBuf;

use rcmake_base::{BString, Limits};
use rcmake_host::{FileKind, Follow, HostError, HostResult, ReadRange, Stream, WriteMode};

use super::{ToolCtx, error, is_dir, path, read_all, split_once};

const CHUNK: u64 = 64 * 1024;
const STDIO: &[u8] = b"-";

pub(super) fn cat(args: &[BString], cx: &ToolCtx<'_>) -> i32 {
    let stdin = BString::from(STDIO);
    let args = if args.is_empty() {
        std::slice::from_ref(&stdin)
    } else {
        args
    };
    let mut options = true;
    let mut status = 0;
    for arg in args {
        if options && arg == b"--" {
            options = false;
        } else if arg == STDIO {
            if stream_stdin(cx).is_err() {
                status = error(
                    cx,
                    &[b"CMake Error: -: error reading standard input (ignoring)\n"],
                );
            }
        } else if options && arg.starts_with(b"-") {
            status = error(cx, &[b"CMake Error: ", arg, b": option not handled\n"]);
        } else if let Err(reason) = stream_file(arg, cx) {
            status = error(
                cx,
                &[
                    b"CMake Error: ",
                    arg,
                    b": ",
                    reason.as_bytes(),
                    b" (ignoring)\n",
                ],
            );
        }
    }
    status
}

fn stream_stdin(cx: &ToolCtx<'_>) -> HostResult<()> {
    loop {
        let data = cx.host.read_stdin(CHUNK)?;
        cx.host.write_out(Stream::Stdout, &data);
        if (data.len() as u64) < CHUNK {
            return Ok(());
        }
    }
}

fn stream_file(arg: &[u8], cx: &ToolCtx<'_>) -> Result<(), String> {
    let path = path(cx, arg);
    if arg.is_empty() {
        return Err("no such file or directory".to_owned());
    }
    let metadata = cx.host.metadata(&path, Follow::Yes).map_err(file_error)?;
    if metadata.kind == FileKind::Dir {
        return Err("is a directory".to_owned());
    }
    if metadata.kind != FileKind::File || metadata.len == 0 {
        let data = cx.host.read(&path, read_all()).map_err(file_error)?;
        cx.host.write_out(Stream::Stdout, &data);
        return Ok(());
    }
    let mut offset = 0;
    while offset < metadata.len {
        let range = ReadRange::Slice {
            offset,
            len: Some(CHUNK.min(metadata.len - offset)),
        };
        let data = cx.host.read(&path, range).map_err(file_error)?;
        if data.is_empty() {
            break;
        }
        cx.host.write_out(Stream::Stdout, &data);
        offset += data.len() as u64;
    }
    Ok(())
}

fn file_error(err: HostError) -> String {
    match err.kind {
        ErrorKind::NotFound => "no such file or directory".to_owned(),
        ErrorKind::PermissionDenied => "permission denied".to_owned(),
        _ => err.os_message,
    }
}

const BIN2C_USAGE: &[u8] =
    b"bin2c Usage: -E bin2c [<options>...] [--] [<input-file> [<output-file>]]\n";
const TEMPLATE: &[u8] = b"--template-file";
const ARRAY: &[u8] = b"--template-array-placeholder";
const LENGTH: &[u8] = b"--template-length-placeholder";

#[derive(Default)]
struct Bin2cOptions<'a> {
    template: Option<&'a [u8]>,
    array: Option<&'a [u8]>,
    length: Option<&'a [u8]>,
    signed: bool,
    decimal: bool,
    trailing_comma: bool,
}

pub(super) fn bin2c(args: &[BString], cx: &ToolCtx<'_>) -> i32 {
    let (options, paths) = match bin2c_options(args) {
        Ok(parsed) => parsed,
        Err(message) if message.is_empty() => return error(cx, &[BIN2C_USAGE]),
        Err(message) => return error(cx, &[&message, b"\n\n", BIN2C_USAGE]),
    };
    let input = paths.first().map_or(STDIO, |arg| arg.as_slice());
    let output = paths.get(1).map_or(STDIO, |arg| arg.as_slice());

    let template = if let Some(template) = options.template {
        match cx.host.read(&path(cx, template), read_all()) {
            Ok(data) => Some(data),
            Err(_) => {
                return error(
                    cx,
                    &[
                        b"Could not open template file for reading: \"",
                        template,
                        b"\"\n",
                    ],
                );
            }
        }
    } else {
        None
    };

    let raw = if input == STDIO {
        cx.host.read_stdin(Limits::default().max_file_read)
    } else {
        let input_path = path(cx, input);
        if !input.is_empty() && is_dir(cx, &input_path) {
            Ok(Vec::new())
        } else {
            cx.host.read(&input_path, read_all())
        }
    };
    let Ok(raw) = raw else {
        return error(
            cx,
            &[b"Could not open file for reading: \"", input, b"\"\n"],
        );
    };

    let output_path: Option<PathBuf> = (output != STDIO).then(|| path(cx, output));
    if let Some(path) = &output_path {
        if output.is_empty()
            || path.parent().is_some_and(|parent| !is_dir(cx, parent))
            || cx.host.write(path, b"", WriteMode::Truncate).is_err()
        {
            return error(
                cx,
                &[b"Could not open file for writing: \"", output, b"\"\n"],
            );
        }
    }
    let mut write_failed = false;
    let mut emit = |bytes: &[u8]| {
        if let Some(path) = &output_path {
            if !write_failed {
                write_failed = cx.host.write(path, bytes, WriteMode::Append).is_err();
            }
        } else {
            cx.host.write_out(Stream::Stdout, bytes);
        }
    };
    let rendered = match template {
        Some(template) => render_template(&template, &raw, &options, &mut emit),
        None => {
            render_array(&raw, &options, &mut emit);
            Ok(())
        }
    };
    if write_failed {
        return error(cx, &[b"Could not write to file: \"", output, b"\"\n"]);
    }
    rendered.map_or_else(|message| error(cx, &[message]), |()| 0)
}

fn bin2c_options(args: &[BString]) -> Result<(Bin2cOptions<'_>, &[BString]), Vec<u8>> {
    let mut options = Bin2cOptions::default();
    let mut cursor = args;
    while let Some((arg, rest)) = cursor.split_first() {
        if arg == STDIO || !arg.starts_with(b"-") {
            break;
        }
        cursor = rest;
        match arg.as_slice() {
            b"--" => {
                break;
            }
            b"--signed" => options.signed = true,
            b"--decimal" => options.decimal = true,
            b"--trailing-comma" => options.trailing_comma = true,
            flag if [TEMPLATE, ARRAY, LENGTH].iter().any(|name| name == &flag) => {
                let (value, rest) = cursor
                    .split_first()
                    .filter(|(value, _)| !value.starts_with(b"-"))
                    .ok_or_else(|| {
                        [b"CMake Error: Invalid value used with ".as_slice(), flag].concat()
                    })?;
                set_template_option(&mut options, flag, value)?;
                cursor = rest;
            }
            flag => {
                let (name, value) = split_once(flag, b'=').ok_or_else(Vec::new)?;
                set_template_option(&mut options, name, value)?;
            }
        }
    }
    if cursor.len() > 2
        || cursor
            .iter()
            .any(|arg| arg != STDIO && arg.starts_with(b"-"))
    {
        return Err(Vec::new());
    }
    if options.template.is_none() {
        for (name, value) in [(ARRAY, options.array), (LENGTH, options.length)] {
            if value.is_some() {
                return Err(
                    [b"Cannot use ".as_slice(), name, b" without --template-file"].concat(),
                );
            }
        }
    }
    Ok((options, cursor))
}

fn set_template_option<'a>(
    options: &mut Bin2cOptions<'a>,
    name: &[u8],
    value: &'a [u8],
) -> Result<(), Vec<u8>> {
    match name {
        TEMPLATE => options.template = Some(value),
        ARRAY | LENGTH => {
            let (field, label) = if name == ARRAY {
                (&mut options.array, b"array".as_slice())
            } else {
                (&mut options.length, b"length".as_slice())
            };
            if value
                .iter()
                .any(|byte| !byte.is_ascii_alphanumeric() && !b"_.+-/".contains(byte))
            {
                return Err([
                    b"Invalid ".as_slice(),
                    label,
                    b" placeholder name: \"",
                    value,
                    b"\"",
                ]
                .concat());
            }
            *field = Some(value);
        }
        _ => return Err(Vec::new()),
    }
    Ok(())
}

fn render_array(data: &[u8], options: &Bin2cOptions<'_>, emit: &mut dyn FnMut(&[u8])) {
    let mut chunks = data.chunks(32).peekable();
    let mut line = String::with_capacity(200);
    while let Some(chunk) = chunks.next() {
        line.clear();
        line.push(' ');
        let mut bytes = chunk.iter().peekable();
        while let Some(&byte) = bytes.next() {
            let value = if options.signed {
                i16::from(i8::from_ne_bytes([byte]))
            } else {
                i16::from(byte)
            };
            if options.decimal {
                let _ = write!(line, "{value:>4}");
            } else {
                let sign = if value < 0 { '-' } else { ' ' };
                let _ = write!(line, "{sign}0x{:02X}", value.unsigned_abs());
            }
            if options.trailing_comma || bytes.peek().is_some() || chunks.peek().is_some() {
                line.push(',');
            }
        }
        line.push('\n');
        emit(line.as_bytes());
    }
}

fn render_template(
    template: &[u8],
    data: &[u8],
    options: &Bin2cOptions<'_>,
    emit: &mut dyn FnMut(&[u8]),
) -> Result<(), &'static [u8]> {
    let array = [b"@", options.array.unwrap_or(b"array"), b"@"].concat();
    let length = [b"@", options.length.unwrap_or(b"length"), b"@"].concat();
    let template = template
        .iter()
        .copied()
        .filter(|byte| *byte != b'\r')
        .collect::<Vec<_>>();
    let mut remaining = template.as_slice();
    let mut printed = false;
    while let Some(at) = remaining.iter().position(|byte| *byte == b'@') {
        let (literal, rest) = remaining.split_at(at);
        emit(literal);
        if let Some(after) = rest.strip_prefix(array.as_slice()) {
            if printed {
                return Err(b"Cannot print array twice\n");
            }
            if !data.is_empty() {
                emit(b"\n");
                render_array(data, options, emit);
            }
            printed = true;
            remaining = after;
        } else if let Some(after) = rest.strip_prefix(length.as_slice()) {
            if !printed {
                return Err(b"Cannot print length before array\n");
            }
            emit(data.len().to_string().as_bytes());
            remaining = after;
        } else {
            emit(b"@");
            remaining = remaining.get(at + 1..).unwrap_or_default();
        }
    }
    emit(remaining);
    Ok(())
}
