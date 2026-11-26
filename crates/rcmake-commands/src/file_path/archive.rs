use rcmake_archive::{
    ArchiveError, ArchiveFormat, CreateOptions, ExtractOptions, SevenZipCompression,
    TarCompression, ZipCompression, create_filtered_archive, extract_archive, list_archive_details,
    parse_archive_mtime,
};
use rcmake_base::BString;
use rcmake_diag::{PolicyId, PolicyStatus};
use rcmake_eval::{
    Cx, Flow,
    args::{KeywordParser, Parsed, Slot},
};
use rcmake_host::{
    Stream,
    path::{from_native, to_native},
};

use super::{Result, error, file::binary_destination};

pub(super) fn create(cx: &mut Cx<'_>, args: &[&[u8]]) -> Result {
    let request = KeywordParser::new()
        .keyword("OUTPUT", Slot::Single)
        .keyword("PATHS", Slot::Multi)
        .keyword("FORMAT", Slot::Single)
        .keyword("COMPRESSION", Slot::Single)
        .keyword("COMPRESSION_LEVEL", Slot::Single)
        .keyword("THREADS", Slot::Single)
        .keyword("ENCODING", Slot::Single)
        .keyword("MTIME", Slot::SingleMaybe)
        .keyword("WORKING_DIRECTORY", Slot::SingleMaybe)
        .keyword("PATTERNS_EXCLUDE", Slot::Multi)
        .keyword("VERBOSE", Slot::Flag)
        .parse(args);
    validate_keywords(cx, &request)?;
    let paths = request.multi("PATHS");
    if paths.is_empty() {
        return Err(error("ARCHIVE_CREATE requires a non-empty list of PATHS"));
    }
    let output = value(&request, "OUTPUT");
    let format = value(&request, "FORMAT");
    let format = if format.is_empty() { b"paxr" } else { format };
    let compression = value(&request, "COMPRESSION");
    let level = compression_level(format, compression, request.single("COMPRESSION_LEVEL"))?;
    let threads = request.single("THREADS").map_or(Ok(1), |input| {
        integer(input)
            .ok_or_else(|| error(format!("number of threads {input} should be at least 0")))
    })?;
    let encoding = encoding(cx, &request);
    let archive_format = format_with_compression(format, compression, threads)?;
    let failure = || error([b"failed to compress: ".as_slice(), output].concat());
    if (format == b"raw" && !value(&request, "ENCODING").is_empty())
        || !supported_encoding(&encoding)
    {
        let diagnostic = if format == b"raw" {
            format!(
                "archive_write_set_format_option(hdrcharset): Undefined option: `raw:hdrcharset={encoding}'"
            )
        } else {
            let name = if format == b"paxr" {
                "pax".into()
            } else {
                String::from_utf8_lossy(format)
            };
            format!("archive_write_set_format_option(hdrcharset): {name}: invalid charset name")
        };
        archive_diagnostic(cx, diagnostic.as_bytes());
        return Err(failure());
    }
    let mtime = value(&request, "MTIME");
    let mtime = if mtime.is_empty() {
        None
    } else if let Some(seconds) = parse_archive_mtime(cx.host(), mtime) {
        Some(seconds)
    } else {
        archive_diagnostic(cx, &[b"unable to parse mtime '", mtime, b"'"].concat());
        return Err(failure());
    };
    if output.is_empty() {
        archive_diagnostic(
            cx,
            b"Cannot open output file \"\": No such file or directory",
        );
        return Err(failure());
    }

    let cwd = cx.host().initial_cwd();
    let working = value(&request, "WORKING_DIRECTORY");
    let base = if working.is_empty() {
        cwd
    } else {
        let path = to_native(working);
        cx.resolve_path(&from_native(&cwd.join(path)))
    };
    let inputs = paths
        .iter()
        .map(|path| cx.resolve_path(&from_native(&base.join(to_native(path)))))
        .collect::<Vec<_>>();
    let options = CreateOptions {
        format: archive_format,
        mtime,
        compression_level: level,
        verbose: false,
    };
    let entries = create_filtered_archive(
        cx.host(),
        &base.join(to_native(output)),
        &base,
        &inputs,
        &options,
        request.multi("PATTERNS_EXCLUDE"),
        threads,
    )
    .map_err(|err| {
        archive_diagnostic(cx, err.message.as_bytes());
        failure()
    })?;
    if request.flag("VERBOSE") {
        print_entries(cx, &entries, b"");
    }
    Ok(Flow::Normal)
}

fn value<'a>(request: &'a Parsed, key: &str) -> &'a [u8] {
    request.single(key).map_or(b"", |value| value.as_slice())
}

fn validate_keywords(cx: &mut Cx<'_>, request: &Parsed) -> Result<()> {
    if let Some(unparsed) = request.unparsed().first() {
        return Err(error(
            [b"Unrecognized argument: \"".as_slice(), unparsed, b"\""].concat(),
        ));
    }
    if let Some(message) = request.report_error_text() {
        return Err(crate::command_error(cx, message));
    }
    Ok(())
}

fn compression_level(format: &[u8], method: &[u8], level: Option<&BString>) -> Result<Option<u32>> {
    let Some(level) = level else {
        return Ok(None);
    };
    if method == b"None" {
        return Err(error(format!(
            "compression level is not supported for compression \"None\".  Provided compression level: {level}"
        )));
    }
    let max = if method == b"Zstd" && format != b"zip" {
        19
    } else {
        9
    };
    let level = integer(level)
        .filter(|&value| value <= max)
        .ok_or_else(|| {
            error(format!(
                "compression level {level} for {} should be in range 0 to {max}",
                String::from_utf8_lossy(method)
            ))
        })?;
    Ok(Some(level))
}

fn integer(value: &[u8]) -> Option<u32> {
    std::str::from_utf8(value).ok()?.parse().ok()
}

fn format_with_compression(format: &[u8], method: &[u8], threads: u32) -> Result<ArchiveFormat> {
    if !matches!(
        format,
        b"7zip" | b"gnutar" | b"pax" | b"paxr" | b"raw" | b"zip"
    ) {
        return Err(error(
            [b"archive format ".as_slice(), format, b" not supported"].concat(),
        ));
    }
    let filter = match method {
        b"" | b"None" | b"PPMd" => None,
        b"GZip" | b"Deflate" => Some(TarCompression::Gzip),
        b"BZip2" => Some(TarCompression::Bzip2),
        b"LZMA" => Some(TarCompression::Lzma),
        b"XZ" | b"LZMA2" => Some(TarCompression::Xz),
        b"Zstd" => Some(TarCompression::Zstd),
        b"Compress" => Some(TarCompression::Compress),
        _ => {
            return Err(error(
                [
                    b"compression type ".as_slice(),
                    method,
                    b" is not supported",
                ]
                .concat(),
            ));
        }
    };
    if (method == b"PPMd" && format != b"7zip")
        || (method == b"Compress" && matches!(format, b"7zip" | b"zip"))
    {
        return Err(error(
            [
                method,
                b" compression is not supported for ",
                format,
                b" format",
            ]
            .concat(),
        ));
    }
    Ok(match format {
        b"7zip" => {
            let compression = match filter {
                Some(TarCompression::Gzip) => SevenZipCompression::Deflate,
                Some(TarCompression::Bzip2) => SevenZipCompression::Bzip2,
                Some(TarCompression::Lzma) => SevenZipCompression::Lzma,
                Some(TarCompression::Xz) => SevenZipCompression::Lzma2,
                Some(TarCompression::Zstd) => SevenZipCompression::Zstd,
                _ if method == b"PPMd" => SevenZipCompression::Ppmd,
                _ if method == b"None" => SevenZipCompression::Store,
                _ => SevenZipCompression::Lzma2,
            };
            ArchiveFormat::SevenZip {
                compression,
                threads,
            }
        }
        b"zip" => match filter {
            Some(TarCompression::Bzip2) => ArchiveFormat::ZipCompressed(ZipCompression::Bzip2),
            Some(TarCompression::Lzma) => ArchiveFormat::ZipCompressed(ZipCompression::Lzma),
            Some(TarCompression::Xz) => ArchiveFormat::ZipCompressed(ZipCompression::Xz),
            Some(TarCompression::Zstd) => ArchiveFormat::ZipCompressed(ZipCompression::Zstd),
            _ if method == b"None" => ArchiveFormat::ZipStored,
            _ => ArchiveFormat::Zip,
        },
        b"raw" => ArchiveFormat::Raw(filter),
        b"gnutar" => filter.map_or(ArchiveFormat::GnuTar, ArchiveFormat::GnuTarCompressed),
        _ => match filter {
            None => ArchiveFormat::Tar,
            Some(TarCompression::Gzip) => ArchiveFormat::TarGz,
            Some(TarCompression::Bzip2) => ArchiveFormat::TarBz2,
            Some(TarCompression::Xz) => ArchiveFormat::TarXz,
            Some(TarCompression::Lzma) => ArchiveFormat::TarLzma,
            Some(TarCompression::Zstd) => ArchiveFormat::TarZstd,
            Some(TarCompression::Compress) => ArchiveFormat::TarZ,
        },
    })
}

fn encoding(cx: &Cx<'_>, request: &Parsed) -> BString {
    let encoding = value(request, "ENCODING");
    if encoding.is_empty() {
        let policy = cx.policy(PolicyId::new(213));
        BString::from(if cfg!(windows) && policy != PolicyStatus::New {
            "OEM"
        } else {
            "UTF-8"
        })
    } else {
        BString::from(encoding)
    }
}

fn supported_encoding(encoding: &[u8]) -> bool {
    encoding.eq_ignore_ascii_case(b"UTF-8") || encoding.eq_ignore_ascii_case(b"OEM")
}

pub(super) fn extract(cx: &mut Cx<'_>, args: &[&[u8]]) -> Result {
    let request = KeywordParser::new()
        .keyword("INPUT", Slot::Single)
        .keyword("DESTINATION", Slot::Single)
        .keyword("PATTERNS", Slot::Multi)
        .keyword("PATTERNS_EXCLUDE", Slot::Multi)
        .keyword("ENCODING", Slot::Single)
        .keyword("LIST_ONLY", Slot::Flag)
        .keyword("VERBOSE", Slot::Flag)
        .keyword("TOUCH", Slot::Flag)
        .parse(args);
    validate_keywords(cx, &request)?;
    let input = value(&request, "INPUT");
    if input.is_empty() {
        return Ok(Flow::Normal);
    }
    let destination = value(&request, "DESTINATION");
    let options = ExtractOptions {
        destination: if destination.is_empty() {
            cx.current_binary_dir()
        } else {
            binary_destination(cx, destination)
        },
        patterns: request.multi("PATTERNS").to_vec(),
        excludes: request.multi("PATTERNS_EXCLUDE").to_vec(),
        list_only: request.flag("LIST_ONLY"),
        verbose: false,
        touch: request.flag("TOUCH"),
    };
    let archive_path = cx.resolve_path(input);
    let encoding = encoding(cx, &request);
    let operation_error = || {
        if options.list_only {
            error([b"failed to list: ".as_slice(), input].concat())
        } else {
            error(
                [
                    b"failed to extract:\n  ".as_slice(),
                    &from_native(&archive_path),
                ]
                .concat(),
            )
        }
    };
    if !supported_encoding(&encoding) {
        archive_diagnostic(
            cx,
            &[b"Cannot set archive encoding: ".as_slice(), &encoding].concat(),
        );
        return Err(operation_error());
    }
    let verbose = request.flag("VERBOSE");
    let result = if options.list_only && verbose {
        let zone = cx.host().time_zone();
        let now = cx.host().now().secs;
        let format_time = |time: rcmake_host::FileTime| {
            let Ok(stamp) = jiff::Timestamp::from_second(time.secs) else {
                return String::new();
            };
            let recent = time.secs.abs_diff(now) <= 365 * 86_400 / 2;
            let format = match (cfg!(windows), recent) {
                (true, true) => "%d %b %H:%M",
                (true, false) => "%d %b  %Y",
                (false, true) => "%e %b %H:%M",
                (false, false) => "%e %b  %Y",
            };
            stamp.to_zoned(zone.clone()).strftime(format).to_string()
        };
        list_archive_details(cx.host(), &archive_path, &options, &format_time)
    } else {
        extract_archive(cx.host(), &archive_path, &options)
    };
    let entries = result.map_err(|err| {
        extract_diagnostic(cx, &err);
        operation_error()
    })?;
    if options.list_only || verbose {
        print_entries(cx, &entries, if options.list_only { b"" } else { b"x " });
    }
    Ok(Flow::Normal)
}

fn extract_diagnostic(cx: &Cx<'_>, error: &ArchiveError) {
    if let Some((reason, name)) = error.unsafe_path() {
        archive_diagnostic(
            cx,
            &[b"Problem with archive_write_header(): ", reason.as_bytes()].concat(),
        );
        archive_diagnostic(cx, &[b"Current file:\n  ", name].concat());
    } else if error.message.ends_with(": Not found in archive") {
        archive_diagnostic(cx, &[b"tar: ", error.message.as_bytes()].concat());
    } else {
        archive_diagnostic(cx, error.message.as_bytes());
    }
}

fn archive_diagnostic(cx: &Cx<'_>, message: &[u8]) {
    cx.host()
        .write_out(Stream::Stderr, &[b"CMake Error: ", message, b"\n"].concat());
}

fn print_entries(cx: &Cx<'_>, entries: &[BString], prefix: &[u8]) {
    for entry in entries {
        cx.host()
            .write_out(Stream::Stdout, &[prefix, entry.as_slice(), b"\n"].concat());
    }
}
