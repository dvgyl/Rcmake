use rcmake_host::Host;

use crate::presets::Listing;
use crate::resolve_cli_path;

use super::{Argument, Options, unknown_option};

pub(super) fn parse(
    arg: &mut Argument<'_, '_>,
    opts: &mut Options,
    host: &dyn Host,
) -> Result<bool, String> {
    match arg.flag {
        b"--preset" => {
            opts.preset = Some(arg.required("No preset specified for --preset")?.into());
        }
        b"--presets-file" => {
            opts.presets_file = Some(resolve_cli_path(
                host,
                arg.required("No file specified for --presets-file")?,
            ));
        }
        b"--list-presets" => {
            let mode = arg.value();
            if let Some(value) = mode.filter(|value| value.starts_with(b"-")) {
                return Err(unknown_option(value));
            }
            if mode == Some(b"") {
                crate::emit_cli_error(host, "Invalid value used with --list-presets");
                return Err(String::new());
            }
            opts.list_presets = Some(Listing::parse(mode)?);
        }
        b"--fresh" => opts.fresh = true,
        _ => return Ok(false),
    }
    Ok(true)
}
