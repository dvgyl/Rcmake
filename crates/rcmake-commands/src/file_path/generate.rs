use rcmake_base::BString;
use rcmake_eval::args::{KeywordParser, Slot};
use rcmake_eval::{Cx, Flow};

use super::{Result, error, permission};

pub(super) fn run(cx: &mut Cx<'_>, args: &[&[u8]]) -> Result {
    const SINGLE: &[&str] = &[
        "OUTPUT",
        "INPUT",
        "CONTENT",
        "CONDITION",
        "TARGET",
        "NEWLINE_STYLE",
    ];
    const FLAGS: &[&str] = &["NO_SOURCE_PERMISSIONS", "USE_SOURCE_PERMISSIONS"];
    if args.len() < 4 {
        return Err(error("Incorrect arguments to GENERATE subcommand."));
    }
    let mut parser = KeywordParser::new().keyword("FILE_PERMISSIONS", Slot::MultiNonEmpty);
    for keyword in SINGLE {
        parser = parser.keyword(keyword, Slot::Single);
    }
    for keyword in FLAGS {
        parser = parser.keyword(keyword, Slot::Flag);
    }
    let parsed = parser.parse(args);
    if let Some(message) = parsed.report_error_text() {
        return Err(crate::command_error(cx, message));
    }
    if !parsed.unparsed().is_empty() {
        return Err(error("Unknown argument to GENERATE subcommand."));
    }
    let Some(output) = parsed
        .single("OUTPUT")
        .filter(|_| args.first() == Some(&b"OUTPUT".as_slice()))
    else {
        return Err(error("GENERATE requires OUTPUT as first option."));
    };
    let input_kind = match args.get(2).copied() {
        Some(b"INPUT") => "INPUT",
        Some(b"CONTENT") => "CONTENT",
        _ if !parsed.seen("INPUT") && !parsed.seen("CONTENT") => {
            return Err(error("GENERATE requires INPUT or CONTENT option."));
        }
        _ => return Err(error("Unknown argument to GENERATE subcommand.")),
    };
    let input = parsed
        .single(input_kind)
        .ok_or_else(|| error("GENERATE requires INPUT or CONTENT option."))?;
    let mut record = vec![b"OUTPUT".as_slice(), output, input_kind.as_bytes(), input];
    for keyword in ["CONDITION", "TARGET", "NEWLINE_STYLE"] {
        if let Some(value) = parsed.single(keyword) {
            if keyword != "NEWLINE_STYLE" && value.is_empty() {
                return Err(error(format!(
                    "{keyword} of sub-command GENERATE must not be empty if specified."
                )));
            }
            if keyword == "NEWLINE_STYLE"
                && !matches!(
                    value.as_slice(),
                    b"LF" | b"CRLF" | b"UNIX" | b"DOS" | b"WIN32"
                )
            {
                return Err(error(
                    "GENERATE NEWLINE_STYLE sets an unknown style, only LF, CRLF, UNIX, DOS, and WIN32 are supported",
                ));
            }
            record.extend([keyword.as_bytes(), value]);
        }
    }
    let modes = [
        "NO_SOURCE_PERMISSIONS",
        "USE_SOURCE_PERMISSIONS",
        "FILE_PERMISSIONS",
    ]
    .into_iter()
    .filter(|mode| parsed.seen(mode))
    .collect::<Vec<_>>();
    if let [first, second, ..] = modes.as_slice() {
        return Err(error(format!(
            "given both {first} and {second}. Only one option allowed."
        )));
    }
    if let [mode] = modes.as_slice() {
        if *mode == "USE_SOURCE_PERMISSIONS" && input_kind == "CONTENT" {
            return Err(error("given USE_SOURCE_PERMISSIONS without a file INPUT."));
        }
        record.push(mode.as_bytes());
    }
    let permissions = parsed.multi("FILE_PERMISSIONS");
    let invalid = permissions
        .iter()
        .filter(|token| permission(token).is_none())
        .map(|token| format!("\"{}\"", BString::from(token.as_slice())))
        .collect::<Vec<_>>();
    if !invalid.is_empty() {
        return Err(error(format!(
            "given invalid permission {}.",
            invalid.join(",")
        )));
    }
    record.extend(permissions.iter().map(|value| value.as_slice()));
    let request = crate::install::record_request(cx, "_RCMAKE_FILE_GENERATE", &record);
    let backtrace = cx.backtrace();
    let props = &mut cx.global().props;
    for (index, frame) in backtrace.frames().enumerate() {
        let key = format!("_RCMAKE_FILE_GENERATE_{request}_FRAME_{index}");
        props.set(
            format!("{key}_FILE").into(),
            BString::from(frame.file.as_ref()),
        );
        props.set(format!("{key}_LINE").into(), frame.line.to_string().into());
        if let Some(name) = &frame.name {
            props.set(format!("{key}_NAME").into(), BString::from(name.as_ref()));
        }
        if frame.kind == rcmake_diag::FrameKind::Deferred {
            props.set(format!("{key}_DEFERRED").into(), "1".into());
        }
    }
    Ok(Flow::Normal)
}
