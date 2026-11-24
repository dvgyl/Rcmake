use std::io::ErrorKind;

use jiff::Timestamp;
use rcmake_archive::{
    ArchiveFormat, CreateOptions, ExtractOptions, SevenZipCompression, TarCompression,
    ZipCompression, create_command_archive, extract_archive, list_command_archive,
    parse_archive_mtime,
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
    Compress,
}

#[derive(Default)]
struct TarOptions<'a> {
    inputs: Vec<BString>,
    compression: Option<Compression>,
    format: Option<&'a [u8]>,
    mtime: Option<&'a [u8]>,
    level: Option<i32>,
    threads: u32,
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
            b'Z' => Some(Compression::Compress),
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
    } else if let Some(level) = arg
        .strip_prefix(b"--cmake-tar-compression-level=")
        .or_else(|| arg.strip_prefix(b"--cmake-compression-level="))
    {
        opts.level = Some(parse_integer(b"compression-level", level, cx)?);
        None
    } else if let Some(threads) = arg.strip_prefix(b"--cmake-tar-threads=") {
        opts.threads = u32::try_from(parse_integer(b"threads", threads, cx)?).unwrap_or_default();
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
        .and_then(|text| text.trim_ascii_start().parse::<i64>().ok());
    let reason = match parsed {
        Some(number) => match i32::try_from(number) {
            Ok(value) => return Ok(value),
            Err(_) if number < 0 => b"too small".as_slice(),
            Err(_) => b"too large",
        },
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
    let format = match archive_format(&opts) {
        Ok(format) => format,
        Err(message) => return creation_error(cx, archive, &[message.as_bytes()]),
    };
    let mtime = if let Some(mtime) = opts.mtime {
        let Some(seconds) = parse_archive_mtime(cx.host, mtime) else {
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
        && !(encoding.eq_ignore_ascii_case(b"UTF-8")
            || (encoding == b"OEM" && (!cfg!(windows) || opts.format == Some(b"7zip"))))
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

fn archive_format(opts: &TarOptions<'_>) -> Result<ArchiveFormat, String> {
    let format = opts.format.unwrap_or(b"paxr");
    let compression = opts.compression.unwrap_or(Compression::None);
    if format == b"zip" {
        return match compression {
            Compression::None if opts.compression.is_some() => Ok(ArchiveFormat::ZipStored),
            Compression::None | Compression::Gzip => Ok(ArchiveFormat::Zip),
            Compression::Bzip2 => Ok(ArchiveFormat::ZipCompressed(ZipCompression::Bzip2)),
            Compression::Lzma => Ok(ArchiveFormat::ZipCompressed(ZipCompression::Lzma)),
            Compression::Xz => Ok(ArchiveFormat::ZipCompressed(ZipCompression::Xz)),
            Compression::Zstd => Ok(ArchiveFormat::ZipCompressed(ZipCompression::Zstd)),
            _ => Err(String::from(
                "CMake Error: The archive backend does not support the requested ZIP compression method\n",
            )),
        };
    }
    if format == b"7zip" {
        let compression = match opts.compression {
            None | Some(Compression::Lzma) => SevenZipCompression::Lzma,
            Some(Compression::None) => SevenZipCompression::Store,
            Some(Compression::Gzip) => SevenZipCompression::Deflate,
            Some(Compression::Bzip2) => SevenZipCompression::Bzip2,
            Some(Compression::Xz) => SevenZipCompression::Lzma2,
            Some(Compression::Zstd) => SevenZipCompression::Zstd,
            Some(Compression::Ppmd) => SevenZipCompression::Ppmd,
            Some(Compression::Compress) => {
                return Err(String::from(
                    "CMake Error: Unix compress is not supported for 7zip\n",
                ));
            }
        };
        return Ok(ArchiveFormat::SevenZip {
            compression,
            threads: opts.threads,
        });
    }

    let (filter, tar) = match compression {
        Compression::None => {
            return Ok(if format == b"gnutar" {
                ArchiveFormat::GnuTar
            } else {
                ArchiveFormat::Tar
            });
        }
        Compression::Gzip => (TarCompression::Gzip, ArchiveFormat::TarGz),
        Compression::Bzip2 => (TarCompression::Bzip2, ArchiveFormat::TarBz2),
        Compression::Xz => (TarCompression::Xz, ArchiveFormat::TarXz),
        Compression::Lzma => (TarCompression::Lzma, ArchiveFormat::TarLzma),
        Compression::Zstd => (TarCompression::Zstd, ArchiveFormat::TarZstd),
        Compression::Compress => (TarCompression::Compress, ArchiveFormat::TarZ),
        Compression::Ppmd => {
            return Err(format!(
                "CMake Error: PPMd is not supported for {}\n",
                String::from_utf8_lossy(format)
            ));
        }
    };
    Ok(if format == b"gnutar" {
        ArchiveFormat::GnuTarCompressed(filter)
    } else {
        tar
    })
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

#[cfg(test)]
mod tests {
    use std::path::Path;

    use rcmake_host::{Host, PureHost};

    use super::*;

    #[test]
    fn tar_selects_compression_from_flags() -> Result<(), Box<dyn std::error::Error>> {
        let cwd = Path::new("/work");
        let host = PureHost::new(cwd.to_owned(), "/bin/cmake".into());
        let cx = ToolCtx {
            host: &host,
            cwd,
            exe_display: b"cmake",
            capabilities: &|| rcmake_json::Value::Null,
        };
        let input = cwd.join("input");
        let archive = cwd.join("archive");
        for (flags, options, magic) in [
            (
                "cjf",
                &["--cmake-tar-compression-level=1"][..],
                b"BZh1".as_slice(),
            ),
            ("cJf", &["--format=gnutar"][..], b"\xfd7zXZ\0"),
            ("cf", &["--lzma"][..], b"\x5d\0\0\x80\0"),
            (
                "cf",
                &["--zstd", "--cmake-compression-level=19"][..],
                b"\x28\xb5\x2f\xfd",
            ),
            ("cZf", &[][..], b"\x1f\x9d\x90"),
        ] {
            host.seed_file(&input, b"contents")?;
            let args = ["tar", flags, "archive"]
                .into_iter()
                .chain(options.iter().copied())
                .chain(["input"])
                .map(BString::from)
                .collect::<Vec<_>>();
            assert_eq!(super::super::run(&args, &cx), 0, "{args:?}");
            assert!(host.read(&archive, read_all())?.starts_with(magic));
            host.remove_file(&input)?;
            for action in ["tf", "xf"] {
                let args = ["tar", action, "archive"].map(BString::from);
                assert_eq!(super::super::run(&args, &cx), 0, "{args:?}");
            }
            assert_eq!(host.read(&input, read_all())?, b"contents");
        }
        Ok(())
    }
}
