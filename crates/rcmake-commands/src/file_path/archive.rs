use rcmake_archive::{
    ArchiveFormat, CreateOptions, ExtractOptions, create_archive, extract_archive,
};
use rcmake_base::BString;
use rcmake_eval::{Cx, Flow};
use rcmake_host::{Stream, path::from_native};

use super::{Result, arg, error};

pub(super) fn create(cx: &mut Cx<'_>, args: &[&[u8]]) -> Result {
    let mut output = None;
    let mut input = Vec::new();
    let mut format = b"paxr".as_slice();
    let mut compression = b"None".as_slice();
    let mut compression_level = None;
    let mut mtime = None;
    let mut verbose = false;
    let mut working_dir = None;
    let mut args = args.iter().copied().peekable();
    while let Some(option) = args.next() {
        match option {
            b"OUTPUT" => output = Some(arg(&mut args, option)?),
            b"PATHS" => {
                while args.peek().is_some_and(|arg| !create_keyword(arg)) {
                    input.extend(args.next());
                }
            }
            b"FORMAT" => format = arg(&mut args, option)?,
            b"COMPRESSION" => compression = arg(&mut args, option)?,
            b"COMPRESSION_LEVEL" => {
                let value = arg(&mut args, option)?;
                compression_level = Some(
                    std::str::from_utf8(value)
                        .ok()
                        .and_then(|s| s.parse::<u32>().ok())
                        .filter(|&n| n <= 9)
                        .ok_or_else(|| {
                            error("ARCHIVE_CREATE COMPRESSION_LEVEL must be between 0 and 9.")
                        })?,
                );
            }
            b"MTIME" => mtime = Some(parse_mtime(arg(&mut args, option)?)?),
            b"VERBOSE" => verbose = true,
            b"WORKING_DIRECTORY" => working_dir = Some(arg(&mut args, option)?),
            _ => return Err(error([b"archive option is unavailable: ", option].concat())),
        }
    }
    let output = output
        .filter(|path| !path.is_empty())
        .ok_or_else(|| error("option OUTPUT is required."))?;
    if input.is_empty() {
        return Err(error("option PATHS is required."));
    }
    let format = match (format, compression) {
        (b"gnutar" | b"pax" | b"paxr", b"None") => ArchiveFormat::Tar,
        (b"gnutar" | b"pax" | b"paxr", b"GZip") => ArchiveFormat::TarGz,
        (b"zip", b"None" | b"GZip" | b"BZip2" | b"XZ" | b"Zstd") => ArchiveFormat::Zip,
        (format, compression) => {
            return Err(error(
                [
                    b"archive format or compression is unavailable: ",
                    format,
                    b" / ",
                    compression,
                ]
                .concat(),
            ));
        }
    };
    let base = working_dir.map_or_else(|| cx.host().initial_cwd(), |dir| cx.resolve_path(dir));
    let inputs = input
        .into_iter()
        .map(|path| cx.resolve_path(&from_native(&base.join(rcmake_host::path::to_native(path)))))
        .collect::<Vec<_>>();
    let options = CreateOptions {
        format,
        mtime,
        compression_level,
        verbose: false,
    };
    let entries = create_archive(
        cx.host(),
        &cx.resolve_path(output),
        &base,
        &inputs,
        &options,
    )
    .map_err(|e| error([b"failed to compress: ", output, b"\n", e.message.as_bytes()].concat()))?;
    if verbose {
        print_entries(cx, &entries, b"");
    }
    Ok(Flow::Normal)
}

fn create_keyword(option: &[u8]) -> bool {
    matches!(
        option,
        b"OUTPUT"
            | b"PATHS"
            | b"FORMAT"
            | b"COMPRESSION"
            | b"COMPRESSION_LEVEL"
            | b"MTIME"
            | b"VERBOSE"
            | b"WORKING_DIRECTORY"
            | b"PATTERNS_EXCLUDE"
            | b"THREADS"
            | b"ENCODING"
    )
}

fn parse_mtime(input: &[u8]) -> Result<u64> {
    let text = std::str::from_utf8(input).map_err(|_| error("invalid archive MTIME"))?;
    if let Ok(seconds) = text.parse() {
        return Ok(seconds);
    }
    let time = if let Ok(timestamp) = text.parse::<jiff::Timestamp>() {
        timestamp
    } else {
        let datetime = text
            .parse::<jiff::civil::DateTime>()
            .or_else(|_| {
                text.parse::<jiff::civil::Date>()
                    .map(|date| date.at(0, 0, 0, 0))
            })
            .map_err(|_| error("invalid archive MTIME"))?;
        jiff::tz::TimeZone::UTC
            .to_timestamp(datetime)
            .map_err(|e| error(e.to_string()))?
    };
    u64::try_from(time.as_second()).map_err(|_| error("archive MTIME precedes the Unix epoch"))
}

pub(super) fn extract(cx: &mut Cx<'_>, args: &[&[u8]]) -> Result {
    let mut args = args.iter().copied().peekable();
    let mut input = None;
    let mut options = ExtractOptions {
        destination: cx.current_binary_dir(),
        ..ExtractOptions::default()
    };
    while let Some(option) = args.next() {
        match option {
            b"INPUT" => input = Some(arg(&mut args, option)?),
            b"DESTINATION" => {
                options.destination = super::file::binary_destination(cx, arg(&mut args, option)?)
            }
            b"PATTERNS" => {
                while args.peek().is_some_and(|arg| !extract_keyword(arg)) {
                    options.patterns.extend(args.next().map(BString::from));
                }
            }
            b"LIST_ONLY" => options.list_only = true,
            b"VERBOSE" => options.verbose = true,
            b"TOUCH" => options.touch = true,
            _ => return Err(error([b"archive option is unavailable: ", option].concat())),
        }
    }
    let input = input
        .filter(|path| !path.is_empty())
        .ok_or_else(|| error("option INPUT is required."))?;
    let verbose = options.verbose;
    options.verbose = false;
    let entries = extract_archive(cx.host(), &cx.resolve_path(input), &options).map_err(|e| {
        error(
            [
                b"failed to extract:\n\n  ",
                input,
                b"\n\n",
                e.message.as_bytes(),
            ]
            .concat(),
        )
    })?;
    if options.list_only || verbose {
        print_entries(cx, &entries, if options.list_only { b"" } else { b"x " });
    }
    Ok(Flow::Normal)
}

fn extract_keyword(option: &[u8]) -> bool {
    matches!(
        option,
        b"INPUT"
            | b"DESTINATION"
            | b"PATTERNS"
            | b"LIST_ONLY"
            | b"VERBOSE"
            | b"TOUCH"
            | b"PATTERNS_EXCLUDE"
            | b"ENCODING"
    )
}

fn print_entries(cx: &Cx<'_>, entries: &[BString], prefix: &[u8]) {
    for entry in entries {
        cx.host()
            .write_out(Stream::Stdout, &[prefix, entry.as_slice(), b"\n"].concat());
    }
}
