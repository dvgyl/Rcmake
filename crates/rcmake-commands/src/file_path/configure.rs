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
    let windows = cx.host().system().sysname.starts_with(b"Windows");
    let mut configured = BString::default();
    for raw_line in request.content.split_inclusive(|&byte| byte == b'\n') {
        let mut env = CxEnv {
            cx: &mut *cx,
            line: None,
            warn_missing: false,
        };
        let expanded = configure(raw_line, &mut env, request.options)
            .map_err(|failure| error(failure.message))?;
        // `rcmake_expr::configure` always terminates each expanded line with one `\n`.
        let expanded = expanded.strip_suffix(b"\n").unwrap_or(&expanded);
        let formatted = format_configured_line(
            expanded,
            raw_line.ends_with(b"\n"),
            request.newline,
            windows,
        );
        if configured.len().saturating_add(formatted.len()) > limit {
            return Err(error("CONFIGURE output exceeds the value size limit."));
        }
        configured.extend_from_slice(&formatted);
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

fn format_configured_line(
    expanded: &[u8],
    had_newline: bool,
    newline: Option<&[u8]>,
    windows: bool,
) -> Vec<u8> {
    if let Some(newline) = newline {
        // Explicit `NEWLINE_STYLE` opens the output in binary mode: variable-expanded
        // line endings are written verbatim and only the template's line ending uses `newline`.
        let mut out = Vec::with_capacity(expanded.len() + newline.len());
        out.extend_from_slice(expanded);
        out.extend_from_slice(newline);
        return out;
    }
    let trailing: &[u8] = if had_newline {
        if windows { b"\r\n" } else { b"\n" }
    } else {
        b""
    };
    let mut out = Vec::with_capacity(expanded.len() + trailing.len());
    if windows {
        // Default text-mode output (`std::ofstream` without `ios::binary`) translates `\n` to `\r\n`.
        for &byte in expanded {
            if byte == b'\n' {
                out.push(b'\r');
            }
            out.push(byte);
        }
    } else {
        out.extend_from_slice(expanded);
    }
    out.extend_from_slice(trailing);
    out
}

#[cfg(test)]
mod tests {
    use super::format_configured_line;

    #[test]
    fn windows_default_newline_converts_embedded_lf_without_trailing_eol() {
        assert_eq!(
            format_configured_line(b"a\nb", false, None, true),
            b"a\r\nb"
        );
        assert_eq!(format_configured_line(b"a", true, None, true), b"a\r\n");
        // Explicit NEWLINE_STYLE writes variable-expanded line endings verbatim.
        assert_eq!(
            format_configured_line(b"a\r\nb", false, Some(b"\n"), true),
            b"a\r\nb\n"
        );
        assert_eq!(
            format_configured_line(b"a\nb", false, Some(b"\r\n"), true),
            b"a\nb\r\n"
        );
        assert_eq!(format_configured_line(b"a\nb", false, None, false), b"a\nb");
        assert_eq!(
            format_configured_line(b"a", false, Some(b"\r\n"), true),
            b"a\r\n"
        );
        assert_eq!(
            format_configured_line(b"d", false, Some(b"\n"), true),
            b"d\n"
        );
        // Trailing newline inside an expanded variable is preserved when the template line also had `\n`.
        assert_eq!(format_configured_line(b"a\n", true, None, false), b"a\n\n");
    }
}
