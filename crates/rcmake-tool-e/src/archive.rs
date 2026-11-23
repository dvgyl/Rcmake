use std::io::ErrorKind;

use jiff::{
    Timestamp,
    civil::{Date, DateTime, Time},
};
use rcmake_archive::{
    ArchiveFormat, CreateOptions, ExtractOptions, create_archive, extract_archive,
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
    archive: Option<&'a [u8]>,
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
    let [flags, rest @ ..] = args else {
        return usage(cx, 1);
    };
    if rest.is_empty() {
        return usage(cx, 1);
    }

    let mut options = TarOptions::default();
    let mut action = None;
    let mut verbose = false;
    for &flag in flags.strip_prefix(b"-").unwrap_or(flags) {
        let compression = match flag {
            b'c' | b'x' | b't' => {
                action = Some(flag);
                None
            }
            b'v' => {
                verbose = true;
                None
            }
            b'f' => None,
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
    if let Err(status) = parse_options(rest, &mut options, cx) {
        return status;
    }
    let Some(archive) = options.archive else {
        return usage(cx, 1);
    };
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
        } else if opts.archive.is_none() {
            opts.archive = Some(arg);
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
        let parsed = std::str::from_utf8(level)
            .ok()
            .and_then(|s| s.parse::<i32>().ok());
        opts.level = Some(parsed.ok_or_else(|| {
            error(
                cx,
                &[
                    b"CMake Error: Invalid --cmake-tar-compression-level value: '",
                    level,
                    b"' - not a number\n",
                ],
            )
        })?);
        None
    } else if let Some(encoding) = arg.strip_prefix(b"--cmake-tar-encoding=") {
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
        verbose,
    };
    let archive_path = path(cx, archive);
    match create_archive(cx.host, &archive_path, cx.cwd, &inputs, &options) {
        Ok(_) => 0,
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

fn archive_format(opts: &TarOptions<'_>, cx: &ToolCtx<'_>) -> Option<ArchiveFormat> {
    let format = opts.format.unwrap_or(b"paxr");
    let zip = format == b"zip";
    if let Some(level) = opts.level {
        if opts.compression.is_none() && !zip && format != b"7zip" {
            error(cx, &[b"CMake Error: Can not use --cmake-tar-compression-level without compression algorithm selection\n"]);
            return None;
        }
        let max = if opts.compression == Some(Compression::Zstd) && !zip {
            19
        } else {
            9
        };
        if !(0..=max).contains(&level) {
            error(
                cx,
                &[format!(
                    "CMake Error: Compression level must be between 0 and {max}. Got {level}\n"
                )
                .as_bytes()],
            );
            return None;
        }
    }
    let compression = opts.compression.unwrap_or(Compression::None);
    match (format, compression) {
        (b"zip", _) if opts.compression.is_none() || compression == Compression::Gzip => {
            Some(ArchiveFormat::Zip)
        }
        (b"gnutar" | b"pax" | b"paxr", Compression::None) => Some(ArchiveFormat::Tar),
        (b"gnutar" | b"pax" | b"paxr", Compression::Gzip) => Some(ArchiveFormat::TarGz),
        _ => {
            error(cx, &[b"CMake Error: The archive backend does not support the requested format or compression method\n"]);
            None
        }
    }
}

fn parse_mtime(value: &[u8], cx: &ToolCtx<'_>) -> Option<u64> {
    let text = std::str::from_utf8(value).ok()?.trim_ascii();
    if let Some(epoch) = text.strip_prefix('@') {
        return epoch.parse().ok();
    }
    let zone = cx.host.time_zone();
    let date_time = if let Ok(date) = text.parse::<Date>() {
        date.at(0, 0, 0, 0)
    } else if text.contains(' ')
        && let Ok(date_time) = text.parse::<DateTime>()
    {
        date_time
    } else {
        let time = if text.len() <= 4
            && let Ok(number) = text.parse::<i16>()
        {
            let (hour, minute) = if text.len() <= 2 {
                (number, 0)
            } else {
                (number / 100, number % 100)
            };
            Time::new(i8::try_from(hour).ok()?, i8::try_from(minute).ok()?, 0, 0)
                .unwrap_or(Time::MIN)
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
    let options = ExtractOptions {
        destination: cx.cwd.to_path_buf(),
        patterns: opts.inputs,
        list_only: list,
        touch: opts.touch,
        verbose: false,
    };
    match extract_archive(cx.host, &archive_path, &options) {
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
