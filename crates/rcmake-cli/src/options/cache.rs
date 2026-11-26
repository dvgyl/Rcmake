use rcmake_eval::CacheDefine;
use rcmake_host::{Host, Stream};
use rcmake_model::CacheType;

use crate::parse_define;

use super::{Argument, CacheEdit, Options, cli_usage_error_only};

pub(super) fn parse(
    arg: &mut Argument<'_, '_>,
    opts: &mut Options,
    host: &dyn Host,
) -> Result<bool, String> {
    match arg.flag {
        b"--toolchain" | b"--install-prefix" => {
            let (name, message) = if arg.flag == b"--toolchain" {
                ("CMAKE_TOOLCHAIN_FILE", "No file specified for --toolchain")
            } else {
                (
                    "CMAKE_INSTALL_PREFIX",
                    "No install directory specified for --install-prefix",
                )
            };
            opts.cache_edits.push(CacheEdit::Define(CacheDefine {
                name: name.into(),
                ty: Some(if arg.flag == b"--toolchain" {
                    CacheType::Filepath
                } else {
                    CacheType::Path
                }),
                value: arg.required(message)?.into(),
            }));
        }
        b"-C" => {
            let path = if let Some(tail) = arg.inline {
                tail.strip_prefix(b"=").unwrap_or(tail).trim_ascii_start()
            } else {
                arg.value().ok_or("-C must be followed by a file name.")?
            };
            if path.is_empty() {
                return Err("No file name specified for -C".into());
            }
            opts.cache_edits
                .push(CacheEdit::Preload(rcmake_host::path::to_native(path)));
        }
        b"-D" => {
            let raw = arg.value().ok_or("-D must be followed with VAR=VALUE.")?;
            let Some(def) = parse_define(raw) else {
                host.write_out(Stream::Stderr, format!("CMake Error: Parse error in command line argument: {}\n Should be: VAR:type=value\n\n", String::from_utf8_lossy(raw)).as_bytes());
                return Err(cli_usage_error_only(host, opts.persona));
            };
            // `PATH` and `FILEPATH` values are normalized when the edit runs,
            // after the preset environment is active.
            opts.cache_edits.push(CacheEdit::Define(def));
        }
        b"-U" => {
            opts.cache_edits.push(CacheEdit::Remove(
                arg.value().ok_or("-U must be followed with VAR.")?.into(),
            ));
        }
        _ => return Ok(false),
    }
    Ok(true)
}
