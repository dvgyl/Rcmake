//! Parse the CPack command line.

use rcmake_base::{BString, CMAKE_VERSION_STR, suggest::levenshtein};
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
                if flag == b"--list-presets" && attached != Some(b"defined") {
                    return Err(CpackError::Argument(
                        "Invalid value specified for --list-presets.\nThe only supported value is \"defined\".".into(),
                    ));
                }
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
    if let Some(option) = candidates.filter(|option| option.len() > 3).find(|option| {
        arg.len().abs_diff(option.len()) <= 1 && levenshtein(arg, option.as_bytes()) <= 1
    }) {
        message.push_str(&format!(". Did you mean: {option}?"));
    }
    error(message)
}

pub(crate) fn print_information(host: &dyn Host, options: &CPackOptions) -> bool {
    if options.version {
        let text = format!(
            "cpack version {CMAKE_VERSION_STR}\nrcmake {}\n",
            env!("CARGO_PKG_VERSION")
        );
        host.write_out(Stream::Stdout, text.as_bytes());
    } else if options.help {
        host.write_out(Stream::Stdout, crate::USAGE);
    } else {
        return false;
    }
    true
}
