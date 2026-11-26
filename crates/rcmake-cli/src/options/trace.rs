use rcmake_eval::{TraceConfig, TraceFormat};
use rcmake_host::{Host, WriteMode};

use crate::resolve_cli_path;

use super::{Argument, Options, out};

pub(super) fn parse(
    arg: &mut Argument<'_, '_>,
    opts: &mut Options,
    host: &dyn Host,
) -> Result<bool, String> {
    match arg.flag {
        b"--trace" => {
            out(host, "Put cmake in trace mode.\n");
            opts.trace.get_or_insert_with(TraceConfig::default);
        }
        b"--trace-expand" => {
            out(
                host,
                "Put cmake in trace mode, but with variables expanded.\n",
            );
            let trace = opts.trace.get_or_insert_with(TraceConfig::default);
            trace.expand = true;
        }
        b"--trace-format" => {
            out(
                host,
                "Put cmake in trace mode and sets the trace output format.\n",
            );
            let fmt = arg.required("No format specified for --trace-format")?;
            let format = match fmt {
                b"human" => TraceFormat::Human,
                b"json-v1" => TraceFormat::JsonV1,
                _ => {
                    return Err("Invalid format specified for --trace-format. Valid formats are human, json-v1.".into());
                }
            };
            let trace = opts.trace.get_or_insert_with(TraceConfig::default);
            trace.format = format;
        }
        b"--trace-source" => {
            let src = arg.required("No file specified for --trace-source")?;
            out(
                host,
                "Put cmake in trace mode, but output only lines of a specified file. Multiple options are allowed.\n",
            );
            let trace = opts.trace.get_or_insert_with(TraceConfig::default);
            trace.sources.push(src.into());
        }
        b"--trace-redirect" => {
            let path = arg.required("No file specified for --trace-redirect")?;
            let absolute = resolve_cli_path(host, path);
            if let Err(err) = host.write(&absolute, b"", WriteMode::Truncate) {
                crate::emit_cli_error(
                    host,
                    format!(
                        "Error opening trace file {}: {err}",
                        String::from_utf8_lossy(path)
                    ),
                );
                return Err(String::new());
            }
            out(
                host,
                &format!(
                    "Put cmake in trace mode and redirect trace output to a file instead of stderr.\nTrace will be written to {}\n",
                    absolute.display()
                ),
            );
            let trace = opts.trace.get_or_insert_with(TraceConfig::default);
            trace.redirect = Some(absolute);
        }
        _ => return Ok(false),
    }
    Ok(true)
}
