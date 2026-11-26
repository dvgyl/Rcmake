use rcmake_diag::DiagState;
use rcmake_eval::LogLevel;
use rcmake_host::{Host, Stream};

use crate::{apply_warning_flag, resolve_cli_path};

use super::{Argument, Options, out};

pub(super) fn parse(
    arg: &mut Argument<'_, '_>,
    opts: &mut Options,
    host: &dyn Host,
) -> Result<bool, String> {
    match arg.flag {
        b"--link-no-warning-as-error" if arg.inline.is_none() => {
            out(
                host,
                "Ignoring LINK_WARNING_AS_ERROR target property and CMAKE_LINK_WARNING_AS_ERROR variable.\n",
            );
            opts.ignore_link_warning_as_error = true;
        }
        b"--compile-no-warning-as-error" if arg.inline.is_none() => {
            out(
                host,
                "Ignoring COMPILE_WARNING_AS_ERROR target property and CMAKE_COMPILE_WARNING_AS_ERROR variable.\n",
            );
            opts.ignore_compile_warning_as_error = true;
        }
        b"--log-context" => opts.log_context = true,
        b"--log-level" | b"--loglevel" => {
            let invalid = format!(
                "Invalid level specified for {}",
                String::from_utf8_lossy(arg.flag)
            );
            opts.log_level = Some(LogLevel::parse(arg.required(invalid.as_str())?).ok_or(invalid)?);
        }
        b"--sarif-output" => {
            opts.sarif = Some(resolve_cli_path(
                host,
                arg.required("No file specified for --sarif-output")?,
            ));
        }
        b"--warn-uninitialized" if arg.inline.is_none() => {
            warn_deprecated(host, "--warn-uninitialized", "-Wuninitialized");
            opts.warning_flags.push("-Wuninitialized".into());
        }
        // CMake removed this option, but it still accepts it and does nothing.
        b"--warn-unused-vars" if arg.inline.is_none() => {}
        b"--no-warn-unused-cli" if arg.inline.is_none() => {
            warn_deprecated(host, "--no-warn-unused-cli", "-Wno-unused-cli");
            opts.warning_flags.push("-Wno-unused-cli".into());
        }
        b"--check-system-vars" if arg.inline.is_none() => {
            opts.check_system_vars = true;
            out(
                host,
                "Also check system files when warning about unused and uninitialized variables.\n",
            );
        }
        b"-W" => {
            let raw = arg.value().ok_or("-W must be followed with [no-]<name>.")?;
            let warning = [b"-W", raw].concat();
            if !apply_warning_flag(&warning, &mut DiagState::default()) {
                let name = raw.strip_prefix(b"no-").unwrap_or(raw);
                let name = name.strip_prefix(b"error=").unwrap_or(name);
                return Err(if name.is_empty() {
                    "No warning name provided.".into()
                } else {
                    format!(
                        "The warning category \"{}\" is not known.",
                        String::from_utf8_lossy(name)
                    )
                });
            }
            opts.warning_flags.push(warning.into());
        }
        _ => return Ok(false),
    }
    Ok(true)
}

/// Write the CMake notice for a deprecated option to stderr.
fn warn_deprecated(host: &dyn Host, old: &str, new: &str) {
    host.write_out(
        Stream::Stderr,
        format!("The {old} option is deprecated.  Use {new} instead.\n").as_bytes(),
    );
}
