use rcmake_eval::{Cx, Flow};

use super::{Result, arg, error, permission};

pub(super) fn run(cx: &mut Cx<'_>, args: &[&[u8]]) -> Result {
    let [b"OUTPUT", _, rest @ ..] = args else {
        return Err(error("GENERATE requires OUTPUT as first option."));
    };
    let [input @ (b"INPUT" | b"CONTENT"), _, options @ ..] = rest else {
        return Err(error("Unknown argument to GENERATE subcommand."));
    };
    let mut args_iter = options.iter().copied().peekable();
    let mut permissions = None;
    while let Some(option) = args_iter.next() {
        match option {
            b"CONDITION" | b"TARGET" => {
                arg(&mut args_iter, option)?;
            }
            b"NEWLINE_STYLE" => {
                if !matches!(
                    arg(&mut args_iter, option)?,
                    b"LF" | b"CRLF" | b"UNIX" | b"DOS" | b"WIN32"
                ) {
                    return Err(error(
                        "GENERATE NEWLINE_STYLE sets an unknown style, only LF, CRLF, UNIX, DOS, and WIN32 are supported",
                    ));
                }
            }
            b"NO_SOURCE_PERMISSIONS" | b"USE_SOURCE_PERMISSIONS" | b"FILE_PERMISSIONS" => {
                if option == b"USE_SOURCE_PERMISSIONS" && *input == b"CONTENT" {
                    return Err(error("given USE_SOURCE_PERMISSIONS without a file INPUT."));
                }
                if permissions
                    .replace(option)
                    .is_some_and(|mode| mode != option)
                {
                    return Err(error("GENERATE given conflicting permission options."));
                }
                if option == b"FILE_PERMISSIONS" {
                    let mut count = 0;
                    while args_iter
                        .next_if(|token| permission(token).is_some())
                        .is_some()
                    {
                        count += 1;
                    }
                    if count == 0 {
                        return Err(error(
                            "GENERATE FILE_PERMISSIONS requires a valid permission.",
                        ));
                    }
                }
            }
            _ => return Err(error("Unknown argument to GENERATE subcommand.")),
        }
    }
    crate::install::record_request(cx, "_RCMAKE_FILE_GENERATE", args);
    Ok(Flow::Normal)
}
