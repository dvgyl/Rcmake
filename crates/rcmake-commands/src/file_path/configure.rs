use rcmake_base::BString;
use rcmake_eval::{
    Cx, CxEnv, Flow,
    args::{KeywordParser, Slot},
};
use rcmake_expr::configure::{ConfigureOptions, configure};
use rcmake_host::{ReadRange, WriteMode};

use super::{Result, error, file::binary_destination};

struct Request {
    output: BString,
    content: BString,
    newline: Option<&'static [u8]>,
    options: ConfigureOptions,
}

impl Request {
    fn parse(cx: &mut Cx<'_>, args: &[&[u8]]) -> Result<Self> {
        let parsed = KeywordParser::new()
            .keyword("OUTPUT", Slot::Single)
            .keyword("CONTENT", Slot::Single)
            .keyword("@ONLY", Slot::Flag)
            .keyword("ESCAPE_QUOTES", Slot::Flag)
            .keyword("NEWLINE_STYLE", Slot::SingleMaybe)
            .parse(args);
        if let Some(arg) = parsed.unparsed().first() {
            cx.set_fatal();
            return Err(error(
                [
                    b"CONFIGURE Unrecognized argument: \"".as_slice(),
                    arg,
                    b"\"",
                ]
                .concat(),
            ));
        }
        if let Some(message) = parsed.report_error_text() {
            return Err(crate::command_error(cx, message));
        }
        let Some(output) = parsed.single("OUTPUT") else {
            cx.set_fatal();
            return Err(error("CONFIGURE OUTPUT option is mandatory."));
        };
        let Some(content) = parsed.single("CONTENT") else {
            cx.set_fatal();
            return Err(error("CONFIGURE CONTENT option is mandatory."));
        };
        let newline = match args.iter().position(|arg| *arg == b"NEWLINE_STYLE") {
            None => None,
            Some(index) => Some(match args.get(index + 1).copied() {
                Some(b"LF" | b"UNIX") => b"\n".as_slice(),
                Some(b"CRLF" | b"DOS" | b"WIN32") => b"\r\n",
                Some(_) => {
                    return Err(error(
                        "CONFIGURE NEWLINE_STYLE sets an unknown style, only LF, CRLF, UNIX, DOS, and WIN32 are supported",
                    ));
                }
                None => {
                    return Err(error(
                        "CONFIGURE NEWLINE_STYLE must set a style: LF, CRLF, UNIX, DOS, or WIN32",
                    ));
                }
            }),
        };
        for character in *b"<>" {
            if output.contains(&character) {
                return Err(error(
                    [
                        b"CONFIGURE called with OUTPUT containing a \"".as_slice(),
                        &[character],
                        b"\".  This character is not allowed.",
                    ]
                    .concat(),
                ));
            }
        }
        Ok(Self {
            output: output.clone(),
            content: content.clone(),
            newline,
            options: ConfigureOptions {
                at_only: parsed.flag("@ONLY"),
                escape_quotes: parsed.flag("ESCAPE_QUOTES"),
            },
        })
    }
}

pub(super) fn run(cx: &mut Cx<'_>, args: &[&[u8]]) -> Result {
    let request = Request::parse(cx, args)?;
    if request.output.is_empty() {
        return Ok(Flow::Normal);
    }
    let output = binary_destination(cx, &request.output);
    let limit = cx.limits().max_value_bytes;
    let mut configured = BString::default();
    for line in request.content.split_inclusive(|&byte| byte == b'\n') {
        let mut env = CxEnv {
            cx: &mut *cx,
            line: None,
            warn_missing: false,
        };
        let expanded =
            configure(line, &mut env, request.options).map_err(|failure| error(failure.message))?;
        let expanded = expanded.strip_suffix(b"\n").unwrap_or(&expanded);
        let newline = request.newline.unwrap_or_else(|| {
            if line.ends_with(b"\n") {
                if cfg!(windows) { b"\r\n" } else { b"\n" }
            } else {
                b""
            }
        });
        if configured
            .len()
            .saturating_add(expanded.len())
            .saturating_add(newline.len())
            > limit
        {
            return Err(error("CONFIGURE output exceeds the value size limit."));
        }
        configured.extend_from_slice(expanded);
        configured.extend_from_slice(newline);
    }
    let host = cx.host();
    let range = ReadRange::All {
        limit: configured.len() as u64,
    };
    if !host
        .read(&output, range)
        .is_ok_and(|old| old == configured.as_slice())
    {
        if let Some(parent) = output.parent() {
            host.create_dir_all(parent)
                .map_err(|failure| error(failure.to_string()))?;
        }
        host.write(&output, &configured, WriteMode::Truncate)
            .map_err(|failure| error(failure.to_string()))?;
    }
    Ok(Flow::Normal)
}
