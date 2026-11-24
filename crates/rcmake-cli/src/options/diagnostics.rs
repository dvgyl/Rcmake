use rcmake_diag::DiagState;
use rcmake_eval::LogLevel;
use rcmake_host::Host;

use crate::{apply_warning_flag, resolve_cli_path};

use super::{Argument, Options, out};

pub(super) fn parse(
    arg: &mut Argument<'_, '_>,
    opts: &mut Options,
    host: &dyn Host,
) -> Result<bool, String> {
    match arg.flag {
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
        b"--warn-uninitialized" => {
            out(host, "Warn about uninitialized values.\n");
            opts.warning_flags.push("-Wuninitialized".into());
        }
        b"--no-warn-unused-cli" => {
            out(
                host,
                "Not searching for unused variables given on the command line.\n",
            );
            opts.warning_flags.push("-Wno-unused-cli".into());
        }
        b"--check-system-vars" => {
            opts.check_system_vars = true;
            out(host, "Find problems with variable usage in system files.\n");
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
