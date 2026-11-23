//! `cmake -E cmake_ninja_dyndep`: collate the C++ module scan results of one target
//! into a Ninja dyndep file (ADR-012).

use rcmake_base::BString;
use rcmake_cxxmodules::ModmapFormat;
use rcmake_cxxmodules::dyndep::DyndepRequest;

use crate::{ToolCtx, error, path, read_all};

const PREFIX: &[u8] = b"CMake Error: -E cmake_ninja_dyndep ";

/// Run `cmake -E cmake_ninja_dyndep` with `args` and return its exit code.
pub(crate) fn run(args: &[BString], cx: &ToolCtx<'_>) -> i32 {
    let request = match parse(args) {
        Ok(request) => request,
        Err(message) => return error(cx, &[PREFIX, &message, b"\n"]),
    };
    let text = match cx.host.read(&path(cx, &request.tdi), read_all()) {
        Ok(text) => text,
        Err(_) => {
            return error(
                cx,
                &[
                    PREFIX,
                    b"failed to parse ",
                    &request.tdi,
                    b"File not found: ",
                    &request.tdi,
                    b"\n",
                ],
            );
        }
    };
    let result = rcmake_cxxmodules::tdi::read_tdi(&text);
    match result {
        Ok(_) => error(
            cx,
            &[PREFIX, b"C++ module collation is not supported yet\n"],
        ),
        Err(err) => error(cx, &[PREFIX, err.to_string().as_bytes(), b"\n"]),
    }
}

fn parse(args: &[BString]) -> Result<DyndepRequest, BString> {
    let mut request = DyndepRequest::default();
    for arg in args {
        if let Some(value) = arg.strip_prefix(b"--tdi=") {
            request.tdi = value.into();
        } else if let Some(value) = arg.strip_prefix(b"--lang=") {
            request.lang = value.into();
        } else if let Some(value) = arg.strip_prefix(b"--dd=") {
            request.dd = value.into();
        } else if let Some(value) = arg.strip_prefix(b"--modmapfmt=") {
            let format = ModmapFormat::from_name(value).ok_or_else(|| {
                BString::from([b"does not understand the ", value, b" module map format"].concat())
            })?;
            request.modmap_format = Some(format);
        } else if arg.starts_with(b"--") {
            return Err([b"unknown argument: ", arg.as_slice()].concat().into());
        } else {
            request.ddis.push(arg.clone());
        }
    }
    for (value, flag) in [
        (&request.tdi, b"--tdi=".as_slice()),
        (&request.lang, b"--lang="),
        (&request.dd, b"--dd="),
    ] {
        if value.is_empty() {
            return Err([b"requires value for ", flag].concat().into());
        }
    }
    Ok(request)
}
