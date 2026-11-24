use rcmake_base::BString;
use rcmake_host::Host;

use super::{Argument, Options, generator_error, is_generator};

pub(super) fn parse(
    arg: &mut Argument<'_, '_>,
    opts: &mut Options,
    host: &dyn Host,
) -> Result<bool, String> {
    match arg.flag {
        b"-G" => {
            let maybe = arg.value().filter(|v| !v.is_empty());
            if maybe.is_none_or(|g| !is_generator(g)) {
                generator_error(host, maybe);
                return Err(String::new());
            }
            opts.generator = maybe.map(BString::from);
        }
        b"-A" | b"-T" => {
            let (slot, missing) = if arg.flag == b"-A" {
                (&mut opts.architecture, "No platform specified for -A")
            } else {
                (&mut opts.toolset, "No toolset specified for -T")
            };
            *slot = Some(arg.value().ok_or(missing)?.into());
        }
        _ => return Ok(false),
    }
    Ok(true)
}
