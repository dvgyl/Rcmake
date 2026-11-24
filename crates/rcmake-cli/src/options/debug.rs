use rcmake_host::{Host, WriteMode};

use crate::resolve_cli_path;

use super::{Argument, Options, out};

pub(super) fn parse<'a>(
    arg: &mut Argument<'a, '_>,
    opts: &mut Options,
    host: &dyn Host,
    profile_format: &mut Option<&'a [u8]>,
) -> Result<bool, String> {
    match arg.flag {
        b"--debugger" | b"--debugger-pipe" | b"--debugger-dap-log" => {
            let name = String::from_utf8_lossy(arg.flag);
            if arg.flag != b"--debugger" {
                let label = if arg.flag == b"--debugger-pipe" {
                    "path"
                } else {
                    "file"
                };
                let _ = arg.required(format!("No {label} specified for {name}"))?;
            }
            return Err(format!("CMake was not built with support for {name}"));
        }
        b"--debug-output" => {
            out(host, "Running with debug output on.\n");
            opts.debug_output = true;
        }
        b"--debug-trycompile" => {
            out(host, "debug trycompile on\n");
            opts.debug_trycompile = true;
        }
        b"--debug-find" => {
            out(
                host,
                "Running with debug output on for the `find` commands.\n",
            );
            opts.debug_find = true;
        }
        b"--debug-find-pkg" | b"--debug-find-var" => {
            let list = arg.required("No names specified for debug find")?;
            let category = if arg.flag == b"--debug-find-pkg" {
                "package(s)"
            } else {
                "variable(s)"
            };
            let names = String::from_utf8_lossy(list).replace(',', " ");
            out(
                host,
                &format!(
                    "Running with debug output on for the 'find' commands for {category} {names}.\n"
                ),
            );
        }
        b"--profiling-format" => {
            let format = profile_value(arg, "No format specified for --profiling-format")?;
            *profile_format = (!format.is_empty()).then_some(format);
        }
        b"--profiling-output" => {
            opts.profile_output = Some(resolve_cli_path(
                host,
                profile_value(arg, "No path specified for --profiling-output")?,
            ));
        }
        _ => return Ok(false),
    }
    Ok(true)
}

fn profile_value<'a>(arg: &mut Argument<'a, '_>, missing: &str) -> Result<&'a [u8], String> {
    match arg.inline {
        Some(b"") => Err(missing.into()),
        _ => arg.value().ok_or_else(|| missing.into()),
    }
}

pub(super) fn validate_profile(
    host: &dyn Host,
    opts: &Options,
    profile_format: Option<&[u8]>,
) -> Result<(), String> {
    if profile_format.is_some() || opts.profile_output.is_some() {
        let error = match (profile_format, &opts.profile_output) {
            (Some(b"google-trace"), Some(path)) => host
                .write(path, b"[\n]\n", WriteMode::Truncate)
                .err()
                .map(|_| {
                    format!(
                        "Could not start profiling: Unable to open: {}",
                        path.display()
                    )
                }),
            (Some(_), None) => {
                Some("--profiling-format specified but no --profiling-output!".into())
            }
            _ => Some("Invalid format specified for --profiling-format".into()),
        };
        if let Some(error) = error {
            crate::emit_cli_error(host, error);
            return Err(String::new());
        }
    }
    Ok(())
}
