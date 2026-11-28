use rcmake_host::{Host, WriteMode};

use crate::resolve_cli_path;

use super::{Argument, Options, out};

pub(super) fn parse<'a>(
    arg: &mut Argument<'a, '_>,
    opts: &mut Options,
    host: &dyn Host,
    profile_format: &mut Option<&'a [u8]>,
    profile_output_raw: &mut Option<&'a [u8]>,
) -> Result<bool, String> {
    match arg.flag {
        b"--debugger" if arg.inline.is_none() => {
            if !rcmake_dap::TRANSPORT_SUPPORTED {
                return Err("CMake was not built with support for --debugger".into());
            }
            out(host, "Running with debugger on.\n");
            opts.debugger = true;
        }
        b"--debugger-pipe" => {
            let path = arg.required("No path specified for --debugger-pipe")?;
            if !rcmake_dap::TRANSPORT_SUPPORTED {
                return Err("CMake was not built with support for --debugger-pipe".into());
            }
            opts.debugger_pipe = Some(rcmake_host::path::to_native(path));
        }
        b"--debugger-dap-log" => {
            let path = arg.required("No file specified for --debugger-dap-log")?;
            if !rcmake_dap::TRANSPORT_SUPPORTED {
                return Err("CMake was not built with support for --debugger-dap-log".into());
            }
            opts.debugger_dap_log = Some(resolve_cli_path(host, path));
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
            let (target, category) = if arg.flag == b"--debug-find-pkg" {
                (
                    &mut opts.debug_find_packages,
                    "the 'find' commands for package(s)",
                )
            } else {
                (&mut opts.debug_find_variables, "the variable(s)")
            };
            let names: Vec<rcmake_base::BString> = list
                .split(|&byte| byte == b',')
                .filter(|name| !name.is_empty())
                .map(rcmake_base::BString::from)
                .collect();
            let body = names
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join(" ");
            out(
                host,
                &format!("Running with debug output on for {category} {body}.\n"),
            );
            target.extend(names);
        }
        b"--profiling-format" => {
            let format = profile_value(arg, "No format specified for --profiling-format")?;
            *profile_format = (!format.is_empty()).then_some(format);
        }
        b"--profiling-output" => {
            let raw = profile_value(arg, "No path specified for --profiling-output")?;
            *profile_output_raw = Some(raw);
            opts.profile_output = Some(resolve_cli_path(host, raw));
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
    profile_output_raw: Option<&[u8]>,
) -> Result<(), String> {
    if profile_format.is_some() || opts.profile_output.is_some() {
        let error = match (profile_format, &opts.profile_output) {
            (Some(b"google-trace"), Some(path)) => host
                .write(path, b"[\n]\n", WriteMode::Truncate)
                .err()
                .map(|_| {
                    let rendered = profile_output_raw.map_or_else(
                        || rcmake_host::path::from_native_cmake(path),
                        rcmake_base::BString::from,
                    );
                    format!("Could not start profiling: Unable to open: {rendered}")
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
