use std::io::ErrorKind;

use jiff::{
    Timestamp,
    civil::{Date, DateTime, Time},
    tz::{Offset, TimeZone},
};
use rcmake_archive::{
    ArchiveFormat, CreateOptions, ExtractOptions, create_command_archive, extract_archive,
    list_command_archive,
};
use rcmake_base::BString;
use rcmake_host::{Follow, Stream, path::to_native};

use super::{ToolCtx, error, is_dir, path, read_all, usage, write};

#[derive(Clone, Copy, PartialEq, Eq)]
enum Compression {
    None,
    Gzip,
    Bzip2,
    Xz,
    Lzma,
    Zstd,
    Ppmd,
}

#[derive(Default)]
struct TarOptions<'a> {
    inputs: Vec<BString>,
    compression: Option<Compression>,
    format: Option<&'a [u8]>,
    mtime: Option<&'a [u8]>,
    level: Option<i32>,
    encoding: Option<&'a [u8]>,
    touch: bool,
}

const COMPRESSION_ERROR: &[u8] =
    b"CMake Error: Can only compress a tar file one way; at most one flag of z, j, or J may be used\n";

pub(super) fn tar(args: &[BString], cx: &ToolCtx<'_>) -> i32 {
    let [flags, archive, rest @ ..] = args else {
        return usage(cx, 1);
    };

    let mut options = TarOptions::default();
    if let Err(status) = parse_options(rest, &mut options, cx) {
        return status;
    }

    let mut action = None;
    let mut verbose = false;
    for &flag in flags.as_slice() {
        let compression = match flag {
            b'c' | b'x' | b't' => {
                action = Some(flag);
                None
            }
            b'v' => {
                verbose = true;
                None
            }
            b'-' | b'f' => None,
            b'z' => Some(Compression::Gzip),
            b'j' => Some(Compression::Bzip2),
            b'J' => Some(Compression::Xz),
            _ => {
                error(cx, &[b"tar: Unknown argument: ", &[flag], b"\n"]);
                None
            }
        };
        if let Some(compression) = compression
            && options.compression.replace(compression).is_some()
        {
            return error(cx, &[COMPRESSION_ERROR]);
        }
    }
    if let Err(status) = validate_level(&options, cx) {
        return status;
    }
    match action {
        Some(b'c') => create(archive, options, verbose, cx),
        Some(b't') => extract(archive, options, true, verbose, cx),
        Some(b'x') => extract(archive, options, false, verbose, cx),
        _ => error(cx, &[b"CMake Error: tar: No action specified. Please choose: 't' (list), 'c' (create) or 'x' (extract)\n"]),
    }
}

fn parse_options<'a>(
    args: &'a [BString],
    opts: &mut TarOptions<'a>,
    cx: &ToolCtx<'_>,
) -> Result<(), i32> {
    let mut flags = true;
    for arg in args {
        if flags && arg == b"--" {
            flags = false;
        } else if flags && arg.starts_with(b"--") {
            parse_option(arg, opts, cx)?;
        } else {
            opts.inputs.push(arg.clone());
        }
    }
    Ok(())
}

fn parse_option<'a>(arg: &'a [u8], opts: &mut TarOptions<'a>, cx: &ToolCtx<'_>) -> Result<(), i32> {
    let compression = if let Some(format) = arg.strip_prefix(b"--format=") {
        if !matches!(
            format,
            b"7zip" | b"gnutar" | b"pax" | b"paxr" | b"raw" | b"zip"
        ) {
            return Err(error(
                cx,
                &[
                    b"CMake Error: Unknown -E tar --format= argument: ",
                    format,
                    b"\n",
                ],
            ));
        }
        opts.format = Some(format);
        None
    } else if let Some(mtime) = arg.strip_prefix(b"--mtime=") {
        opts.mtime = (!mtime.is_empty()).then_some(mtime);
        None
    } else if let Some(file) = arg.strip_prefix(b"--files-from=") {
        read_names(file, &mut opts.inputs, cx)?;
        None
    } else if let Some(method) = arg.strip_prefix(b"--cmake-tar-compression-method=") {
        Some(match method {
            b"none" | b"store" => Compression::None,
            b"deflate" | b"gzip" => Compression::Gzip,
            b"bzip2" => Compression::Bzip2,
            b"lzma" => Compression::Lzma,
            b"lzma2" | b"xz" => Compression::Xz,
            b"ppmd" => Compression::Ppmd,
            b"zstd" => Compression::Zstd,
            _ => {
                return Err(error(
                    cx,
                    &[
                        b"CMake Error: Unknown --cmake-tar-compression-method value: '",
                        method,
                        b"'\n",
                    ],
                ));
            }
        })
    } else if let Some(level) = arg.strip_prefix(b"--cmake-tar-compression-level=") {
        opts.level = Some(parse_integer(b"compression-level", level, cx)?);
        None
    } else if let Some(threads) = arg.strip_prefix(b"--cmake-tar-threads=") {
        // The available compression methods use one thread.
        parse_integer(b"threads", threads, cx)?;
        None
    } else if let Some(encoding) = arg.strip_prefix(b"--cmake-tar-encoding=") {
        if encoding.is_empty() {
            return Err(error(
                cx,
                &[b"CMake Error: Encoding value is empty - it must be filled if passed\n"],
            ));
        }
        opts.encoding = Some(encoding);
        None
    } else {
        match arg {
            b"--touch" => {
                opts.touch = true;
                None
            }
            b"--zstd" => Some(Compression::Zstd),
            b"--lzma" => Some(Compression::Lzma),
            _ => {
                return Err(error(
                    cx,
                    &[b"CMake Error: Unknown option to -E tar: ", arg, b"\n"],
                ));
            }
        }
    };
    if let Some(compression) = compression
        && opts.compression.replace(compression).is_some()
    {
        return Err(error(cx, &[COMPRESSION_ERROR]));
    }
    Ok(())
}

fn parse_integer(option: &[u8], value: &[u8], cx: &ToolCtx<'_>) -> Result<i32, i32> {
    let parsed = std::str::from_utf8(value)
        .ok()
        .and_then(|text| text.trim_ascii_start().parse::<std::ffi::c_long>().ok());
    let reason = match parsed {
        Some(number) if i32::try_from(number).is_ok() => return Ok(number as i32),
        Some(number) if number < 0 => b"too small".as_slice(),
        Some(_) => b"too large",
        None => b"not a number",
    };
    Err(error(
        cx,
        &[
            b"CMake Error: Invalid --cmake-tar-",
            option,
            b" value: '",
            value,
            b"' - ",
            reason,
            b"\n",
        ],
    ))
}

fn read_names(file: &[u8], names: &mut Vec<BString>, cx: &ToolCtx<'_>) -> Result<(), i32> {
    let list = path(cx, file);
    if !file.is_empty() && is_dir(cx, &list) {
        return Err(error(
            cx,
            &[
                b"CMake Error: -E tar --files-from= file '",
                file,
                b"' is a directory\n",
            ],
        ));
    }
    let data = cx.host.read(&list, read_all()).map_err(|_| {
        error(
            cx,
            &[
                b"CMake Error: -E tar --files-from= file '",
                file,
                b"' not found\n",
            ],
        )
    })?;
    for line in data.split(|b| *b == b'\n') {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        if let Some(name) = line.strip_prefix(b"--add-file=") {
            names.push(BString::from(name));
        } else if line.starts_with(b"-") {
            return Err(error(
                cx,
                &[
                    b"CMake Error: -E tar --files-from='",
                    file,
                    b"' file invalid line:\n",
                    line,
                    b"\n\n",
                ],
            ));
        } else if !line.is_empty() {
            names.push(BString::from(line));
        }
    }
    Ok(())
}

fn create(archive: &[u8], opts: TarOptions<'_>, verbose: bool, cx: &ToolCtx<'_>) -> i32 {
    if opts.format == Some(b"raw") {
        return creation_error(
            cx,
            archive,
            &[
                b"CMake Error: archive_write_set_format_option(hdrcharset): Undefined option: `raw:hdrcharset=",
                opts.encoding.unwrap_or(b"UTF-8"),
                b"'\n",
            ],
        );
    }
    let Some(format) = archive_format(&opts, cx) else {
        return 1;
    };
    let mtime = if let Some(mtime) = opts.mtime {
        let Some(seconds) = parse_mtime(mtime, cx) else {
            return creation_error(
                cx,
                archive,
                &[b"CMake Error: unable to parse mtime '", mtime, b"'\n"],
            );
        };
        Some(seconds)
    } else {
        None
    };
    if opts.inputs.is_empty() {
        error(cx, &[b"tar: No files or directories specified\n"]);
    }
    if let Some(encoding) = opts.encoding
        && !(encoding.eq_ignore_ascii_case(b"UTF-8") || (encoding == b"OEM" && !cfg!(windows)))
    {
        return creation_error(
            cx,
            archive,
            &[
                b"CMake Error: Archive pathname encoding is not supported: ",
                encoding,
                b"\n",
            ],
        );
    }
    let inputs: Vec<_> = opts.inputs.iter().map(|name| to_native(name)).collect();
    let options = CreateOptions {
        format,
        mtime,
        compression_level: opts
            .level
            .and_then(|level| u32::try_from(level).ok())
            .filter(|level| *level != 0),
        verbose: false,
    };
    let archive_path = path(cx, archive);
    match create_command_archive(cx.host, &archive_path, cx.cwd, &inputs, &options) {
        Ok(names) => {
            if verbose {
                for name in names {
                    write(
                        cx,
                        Stream::Stdout,
                        &[name.strip_suffix(b"/").unwrap_or(&name), b"\n"],
                    );
                }
            }
            0
        }
        Err(err) => {
            if let Some(name) = opts.inputs.iter().find(|name| {
                cx.host
                    .metadata(&path(cx, name), Follow::No)
                    .is_err_and(|e| e.kind == ErrorKind::NotFound)
            }) {
                return creation_error(
                    cx,
                    archive,
                    &[
                        b"CMake Error: Unable to read from file:\n  ",
                        name,
                        b"\nbecause:\n  Can't lstat ",
                        name,
                        b"\n",
                    ],
                );
            }
            creation_error(
                cx,
                archive,
                &[b"CMake Error: ", err.message.as_bytes(), b"\n"],
            )
        }
    }
}

fn validate_level(opts: &TarOptions<'_>, cx: &ToolCtx<'_>) -> Result<(), i32> {
    let format = opts.format.unwrap_or(b"paxr");
    let zip = format == b"zip";
    if let Some(level) = opts.level {
        if opts.compression.is_none() && !zip && format != b"7zip" {
            return Err(error(cx, &[b"CMake Error: Can not use --cmake-tar-compression-level without compression algorithm selection\n"]));
        }
        let max = if opts.compression == Some(Compression::Zstd) && !zip {
            19
        } else {
            9
        };
        if !(0..=max).contains(&level) {
            return Err(error(
                cx,
                &[format!(
                    "CMake Error: Compression level must be between 0 and {max}. Got {level}\n"
                )
                .as_bytes()],
            ));
        }
    }
    Ok(())
}

fn archive_format(opts: &TarOptions<'_>, cx: &ToolCtx<'_>) -> Option<ArchiveFormat> {
    let format = opts.format.unwrap_or(b"paxr");
    match (format, opts.compression) {
        (b"zip", None | Some(Compression::Gzip)) => Some(ArchiveFormat::Zip),
        (b"zip", Some(Compression::None)) => Some(ArchiveFormat::ZipStored),
        (b"gnutar", None | Some(Compression::None)) => Some(ArchiveFormat::GnuTar),
        (b"gnutar", Some(Compression::Gzip)) => Some(ArchiveFormat::GnuTarGz),
        (b"pax" | b"paxr", None | Some(Compression::None)) => Some(ArchiveFormat::Tar),
        (b"pax" | b"paxr", Some(Compression::Gzip)) => Some(ArchiveFormat::TarGz),
        _ => {
            error(cx, &[b"CMake Error: The archive backend does not support the requested format or compression method\n"]);
            None
        }
    }
}

fn parse_mtime(value: &[u8], cx: &ToolCtx<'_>) -> Option<u64> {
    let text = std::str::from_utf8(value).ok()?;
    if let Some(epoch) = text.strip_prefix('@') {
        return u64::try_from(epoch.parse::<i64>().ok()?).ok();
    }
    let text = text.trim_ascii();
    let upper = text.to_ascii_uppercase();
    let utc = ["UTC", "GMT", "Z"]
        .into_iter()
        .find_map(|suffix| upper.strip_suffix(suffix));
    let (text, zone) = if let Some(text) = utc {
        (text.trim_ascii_end(), TimeZone::UTC)
    } else if let Some((local, offset)) = mtime_offset(text) {
        (local, TimeZone::fixed(offset))
    } else {
        (text, cx.host.time_zone())
    };
    let date_time = if text.contains(' ')
        && let Ok(date_time) = text.parse::<DateTime>()
    {
        date_time
    } else if !text.contains([' ', 'T', 't'])
        && let Ok(date) = text.parse::<Date>()
    {
        date.at(0, 0, 0, 0)
    } else {
        let time = if text.len() <= 4
            && !text.is_empty()
            && text.bytes().all(|byte| byte.is_ascii_digit())
        {
            let number = text.parse::<u32>().ok()?;
            let (hour, minute) = if number < 100 {
                (number, 0)
            } else {
                (number / 100, number % 100)
            };
            if hour > 23 || minute > 59 {
                Time::MIN
            } else {
                Time::new(hour as i8, minute as i8, 0, 0).ok()?
            }
        } else {
            text.parse::<Time>().ok()?
        };
        let now = Timestamp::from_second(cx.host.now().secs)
            .ok()?
            .to_zoned(zone.clone());
        now.date().to_datetime(time)
    };
    u64::try_from(date_time.to_zoned(zone).ok()?.timestamp().as_second()).ok()
}

fn mtime_offset(text: &str) -> Option<(&str, Offset)> {
    let (local, offset) = text.split_at_checked(text.len().checked_sub(5)?)?;
    let (sign, digits) = offset.split_at_checked(1)?;
    if !local.contains(':')
        || !matches!(sign, "+" | "-")
        || !digits.bytes().all(|byte| byte.is_ascii_digit())
    {
        return None;
    }
    let (hours, minutes) = digits.split_at_checked(2)?;
    let hours = hours.parse::<i32>().ok()?;
    let minutes = minutes.parse::<i32>().ok()?;
    if minutes > 59 {
        return None;
    }
    let seconds = hours * 3_600 + minutes * 60;
    let seconds = if sign == "-" { -seconds } else { seconds };
    Some((local.trim_ascii_end(), Offset::from_seconds(seconds).ok()?))
}

fn creation_error(cx: &ToolCtx<'_>, archive: &[u8], message: &[&[u8]]) -> i32 {
    error(cx, message);
    error(
        cx,
        &[b"CMake Error: Problem creating tar:\n  ", archive, b"\n"],
    )
}

fn extract(
    archive: &[u8],
    opts: TarOptions<'_>,
    list: bool,
    verbose: bool,
    cx: &ToolCtx<'_>,
) -> i32 {
    let archive_path = path(cx, archive);
    let result = if list {
        let zone = cx.host.time_zone();
        let now = cx.host.now().secs;
        let time = |value: rcmake_host::FileTime| {
            let Ok(stamp) = Timestamp::from_second(value.secs) else {
                return String::new();
            };
            let recent = value.secs.abs_diff(now) <= 365 * 86_400 / 2;
            let format = match (cfg!(windows), recent) {
                (true, true) => "%d %b %H:%M",
                (true, false) => "%d %b  %Y",
                (false, true) => "%e %b %H:%M",
                (false, false) => "%e %b  %Y",
            };
            stamp.to_zoned(zone.clone()).strftime(format).to_string()
        };
        list_command_archive(
            cx.host,
            &archive_path,
            opts.inputs,
            verbose.then_some(&time),
        )
    } else {
        let options = ExtractOptions {
            destination: cx.cwd.to_path_buf(),
            patterns: opts.inputs,
            touch: opts.touch,
            ..Default::default()
        };
        extract_archive(cx.host, &archive_path, &options)
    };
    match result {
        Ok(names) => {
            if list || verbose {
                let prefix = if list { b"".as_slice() } else { b"x " };
                for name in names {
                    write(cx, Stream::Stdout, &[prefix, &name, b"\n"]);
                }
            }
            0
        }
        Err(err) => {
            if archive.is_empty()
                || cx
                    .host
                    .metadata(&archive_path, Follow::Yes)
                    .is_err_and(|e| e.kind == ErrorKind::NotFound)
            {
                error(
                    cx,
                    &[
                        b"CMake Error: Problem with archive_read_open_filename(): Failed to open '",
                        archive,
                        b"'\n",
                    ],
                );
            } else if let Some((reason, name)) = err.unsafe_path() {
                error(
                    cx,
                    &[
                        b"CMake Error: Problem with archive_write_header(): ",
                        reason.as_bytes(),
                        b"\nCMake Error: Current file:\n  ",
                        name,
                        b"\n",
                    ],
                );
            } else {
                let prefix = if err.message.ends_with(": Not found in archive") {
                    b"CMake Error: tar: ".as_slice()
                } else {
                    b"CMake Error: "
                };
                error(cx, &[prefix, err.message.as_bytes(), b"\n"]);
            }
            let message = if list {
                b"CMake Error: Problem listing tar: ".as_slice()
            } else {
                b"CMake Error: Problem extracting tar:\n  "
            };
            error(cx, &[message, archive, b"\n"])
        }
    }
}
