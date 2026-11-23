//! Parse the CPack command line.

use rcmake_base::{BString, CMAKE_VERSION_STR};
use rcmake_host::{Host, Stream};

use crate::{CPackOptions, CpackError, error};

const VALUE_OPTIONS: &[&str] = &[
    "-G",
    "-C",
    "-B",
    "-P",
    "-R",
    "--vendor",
    "-D",
    "--config",
    "--preset",
    "--presets-file",
];

/// Parse the CPack arguments without the executable name.
///
/// # Errors
///
/// Returns an error for an unknown option or a missing or invalid value.
pub fn parse(args: &[BString]) -> Result<CPackOptions, CpackError> {
    let mut result = CPackOptions::default();
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        match arg.as_slice() {
            b"-V" | b"--verbose" => result.verbose = true,
            b"--debug" => result.debug = true,
            b"--trace" => result.trace = true,
            b"--trace-expand" => result.trace_expand = true,
            b"--list-presets" => result.list_presets = true,
            b"-h" | b"-H" | b"--help" | b"-help" | b"-usage" | b"/?" => result.help = true,
            b"--version" | b"-version" | b"/V" | b"/version" => result.version = true,
            bytes => {
                let (flag, attached) = if let Some(value) =
                    bytes.strip_prefix(b"-D").filter(|value| !value.is_empty())
                {
                    (
                        b"-D".as_slice(),
                        Some(value.strip_prefix(b"=").unwrap_or(value)),
                    )
                } else if let Some((name, value)) = split_option(bytes) {
                    (name, Some(value))
                } else {
                    (bytes, None)
                };
                if !VALUE_OPTIONS.iter().any(|option| option.as_bytes() == flag) {
                    if bytes.starts_with(b"-") {
                        return Err(unknown(arg));
                    }
                    continue;
                }
                let value = attached
                    .or_else(|| {
                        args.next()
                            .map(AsRef::<[u8]>::as_ref)
                            .filter(|value| !value.starts_with(b"-"))
                    })
                    .filter(|value| !value.is_empty())
                    .ok_or_else(|| missing(flag))?;
                let slot = match flag {
                    b"-G" => &mut result.generators,
                    b"-C" => &mut result.build_config,
                    b"-B" => &mut result.package_directory,
                    b"-P" => &mut result.package_name,
                    b"-R" => &mut result.package_version,
                    b"--vendor" => &mut result.vendor,
                    b"--config" => &mut result.config_file,
                    b"--preset" => &mut result.preset,
                    b"--presets-file" => &mut result.presets_file,
                    _ => {
                        let Some((name, value)) = split_option(value) else {
                            return Err(error("Please specify CPack definitions as: KEY=VALUE"));
                        };
                        result.defines.push((name.into(), value.into()));
                        continue;
                    }
                };
                *slot = Some(value.into());
            }
        }
    }
    Ok(result)
}

fn split_option(option: &[u8]) -> Option<(&[u8], &[u8])> {
    let mut parts = option.splitn(2, |&byte| byte == b'=');
    Some((parts.next()?, parts.next()?))
}

fn missing(flag: &[u8]) -> CpackError {
    CpackError::Argument(match flag {
        b"--preset" => "No preset specified for --preset".into(),
        b"--presets-file" => "No file specified for --presets-file".into(),
        _ => format!("Invalid value used with {}", BString::from(flag)).into(),
    })
}

fn unknown(arg: &BString) -> CpackError {
    let mut message = format!("Unknown argument: {arg}");
    let candidates = VALUE_OPTIONS.iter().copied().chain([
        "--verbose",
        "--debug",
        "--trace",
        "--trace-expand",
        "--list-presets",
        "--help",
        "--version",
    ]);
    if let Some(option) = candidates
        .filter(|option| option.len() > 3)
        .find(|option| one_edit(arg, option.as_bytes()))
    {
        message.push_str(&format!(". Did you mean: {option}?"));
    }
    error(message)
}

fn one_edit(left: &[u8], right: &[u8]) -> bool {
    let common = left.iter().zip(right).take_while(|(a, b)| a == b).count();
    let (left, right) = (
        left.get(common..).unwrap_or_default(),
        right.get(common..).unwrap_or_default(),
    );
    match left.len().cmp(&right.len()) {
        std::cmp::Ordering::Less => right.get(1..) == Some(left),
        std::cmp::Ordering::Equal => left.get(1..) == right.get(1..),
        std::cmp::Ordering::Greater => left.get(1..) == Some(right),
    }
}

pub(crate) fn print_information(host: &dyn Host, options: &CPackOptions) -> bool {
    if options.version {
        let text = format!(
            "cpack version {CMAKE_VERSION_STR}\n\nCMake suite maintained and supported by Kitware (kitware.com/cmake).\n"
        );
        host.write_out(Stream::Stdout, text.as_bytes());
    } else if options.help {
        host.write_out(Stream::Stdout, b"Usage\n\n  cpack [options]\n\nOptions\n  -G <generators>     = Override CPACK_GENERATOR\n  -C <configs>        = Select build configurations\n  -D <var>=<value>    = Set a CPack variable\n  --config <file>     = Select the CPack config file\n  -P <name>           = Set the package name\n  -R <version>        = Set the package version\n  -B <dir>            = Set the package directory\n  --vendor <name>     = Set the package vendor\n  --preset <name>     = Use a package preset\n  --presets-file <file> = Select a presets file\n  --list-presets      = List available package presets\n  -V, --verbose       = Enable verbose output\n  --debug             = Enable debug output\n  --trace             = Trace CMake scripts\n  --trace-expand      = Trace CMake scripts with variables expanded\n  --version           = Print the version\n  --help              = Print this help\n\nGenerators\n  TGZ TXZ TZST TBZ2 TZ TAR ZIP ZIP_DEFLATE ZIP_STORE Bundle DragNDrop\n");
    } else {
        return false;
    }
    true
}
