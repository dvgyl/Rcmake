use rcmake_base::BString;
use rcmake_eval::{Cx, Flow};
use rcmake_expr::{
    ExpandEnv,
    configure::{ConfigureOptions, configure},
};
use rcmake_host::{ReadRange, WriteMode};

use super::{Result, error, file::binary_destination};

struct Request<'a> {
    output: &'a [u8],
    content: &'a [u8],
    newline: Option<&'static [u8]>,
    options: ConfigureOptions,
}

impl<'a> Request<'a> {
    fn parse(cx: &mut Cx<'_>, args: &[&'a [u8]]) -> Result<Self> {
        let mut output = None;
        let mut content = None;
        let mut newline = None;
        let mut options = ConfigureOptions::default();
        let mut args = args.iter().copied().peekable();
        while let Some(arg) = args.next() {
            match arg {
                b"OUTPUT" | b"CONTENT" => {
                    let value = args.next_if(|&value| {
                        !matches!(
                            value,
                            b"OUTPUT" | b"CONTENT" | b"@ONLY" | b"ESCAPE_QUOTES" | b"NEWLINE_STYLE"
                        )
                    });
                    let Some(value) = value else {
                        return Err(crate::command_error(
                            cx,
                            [
                                b"Error after keyword \"".as_slice(),
                                arg,
                                b"\":\n\n    missing required value",
                            ]
                            .concat(),
                        ));
                    };
                    if arg == b"OUTPUT" {
                        output = Some(value);
                    } else {
                        content = Some(value);
                    }
                }
                b"@ONLY" => options.at_only = true,
                b"ESCAPE_QUOTES" => options.escape_quotes = true,
                b"NEWLINE_STYLE" => {
                    let style = args.next();
                    if newline.is_none() {
                        newline = Some(match style {
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
                        });
                    }
                }
                _ => {
                    return Err(error(
                        [
                            b"CONFIGURE Unrecognized argument: \"".as_slice(),
                            arg,
                            b"\"",
                        ]
                        .concat(),
                    ));
                }
            }
        }
        let output = output.ok_or_else(|| error("CONFIGURE OUTPUT option is mandatory."))?;
        let content = content.ok_or_else(|| error("CONFIGURE CONTENT option is mandatory."))?;
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
            output,
            content,
            newline,
            options,
        })
    }
}

pub(super) fn run(cx: &mut Cx<'_>, args: &[&[u8]]) -> Result {
    let request = Request::parse(cx, args)?;
    if request.output.is_empty() {
        return Ok(Flow::Normal);
    }
    let output = binary_destination(cx, request.output);
    let limit = cx.limits().max_value_bytes;
    let mut configured = BString::default();
    for line in request.content.split_inclusive(|&byte| byte == b'\n') {
        let expanded = configure(line, &mut TemplateEnv(cx), request.options)
            .map_err(|failure| error(failure.message))?;
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

struct TemplateEnv<'a, 'b>(&'a mut Cx<'b>);

impl ExpandEnv for TemplateEnv<'_, '_> {
    fn var(&mut self, name: &[u8]) -> Option<BString> {
        self.0.var(name)
    }

    fn cache_var(&mut self, name: &[u8]) -> Option<BString> {
        self.0.cache().get(name).map(|entry| entry.value.clone())
    }

    fn env_var(&mut self, name: &[u8]) -> Option<BString> {
        self.0.host().env_get(name)
    }

    fn current_line(&self) -> u32 {
        self.0.current_line()
    }
}
