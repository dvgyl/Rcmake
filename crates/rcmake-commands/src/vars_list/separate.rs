use std::path::Path;

use rcmake_base::cmdline::Arguments;
use rcmake_base::{BString, list};
use rcmake_eval::{CmdError, CmdResult, Cx, Flow};
use rcmake_expr::ExpandedArg;
use rcmake_host::{FileKind, Follow, path};

use super::bytes;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Unix,
    Windows,
    Native,
}

impl Mode {
    fn parse(arg: &[u8]) -> Option<Self> {
        match arg {
            b"UNIX_COMMAND" => Some(Self::Unix),
            b"WINDOWS_COMMAND" => Some(Self::Windows),
            b"NATIVE_COMMAND" => Some(Self::Native),
            _ => None,
        }
    }

    fn windows(self) -> bool {
        self == Self::Windows || self == Self::Native && cfg!(windows)
    }
}

pub(super) fn separate_arguments(cx: &mut Cx<'_>, args: &[ExpandedArg]) -> CmdResult {
    let [name, tail @ ..] = args else {
        return Err(CmdError::status("must be given at least one argument."));
    };
    let name = bytes(name);
    if tail.is_empty() {
        if let Some(mut value) = cx.var(name) {
            for byte in value.iter_mut().filter(|byte| **byte == b' ') {
                *byte = b';';
            }
            cx.set_var(name, value);
        }
        return Ok(Flow::Normal);
    }
    let mut mode = None;
    let mut program = false;
    let mut separate = false;
    let mut input = None;
    let mut extra = false;
    for arg in tail.iter().map(bytes) {
        match arg {
            b"PROGRAM" => program = true,
            b"SEPARATE_ARGS" => separate = true,
            _ => {
                if let Some(next) = Mode::parse(arg) {
                    if mode.is_some_and(|old| old != next) {
                        return Err(CmdError::status(
                            "'UNIX_COMMAND', 'WINDOWS_COMMAND' and 'NATIVE_COMMAND' are mutually exclusive",
                        ));
                    }
                    mode = Some(next);
                } else {
                    extra |= input.replace(arg).is_some();
                }
            }
        }
    }
    let mode = mode.ok_or_else(|| {
        CmdError::status(
            "missing required option: 'UNIX_COMMAND' or 'WINDOWS_COMMAND' or 'NATIVE_COMMAND'",
        )
    })?;
    if separate && !program {
        return Err(CmdError::status(
            "`SEPARATE_ARGS` option requires `PROGRAM' option",
        ));
    }
    if extra {
        return Err(CmdError::status("given unexpected argument(s)"));
    }
    let input = input.unwrap_or_default();
    let windows = if program && !separate {
        cfg!(windows)
    } else {
        mode.windows()
    };
    let mut args = Arguments::new(input, windows);
    let result = if program {
        args.next()
            .and_then(|token| find_program(cx, &token))
            .map_or_else(BString::default, |program| {
                if separate {
                    list::join(
                        std::iter::once(program)
                            .chain(args)
                            .map(|arg| list::escape_item(&arg)),
                    )
                } else {
                    list::join([program.as_slice(), args.rest()])
                }
            })
    } else {
        list::join(args.map(|arg| list::escape_item(&arg)))
    };
    cx.set_var(name, result);
    Ok(Flow::Normal)
}

pub(crate) fn find_program(cx: &Cx<'_>, token: &[u8]) -> Option<BString> {
    if token.is_empty() {
        return None;
    }
    let file = path::to_native(token);
    let cwd = cx.host().initial_cwd();
    if cx.host().metadata(&cwd.join(&file), Follow::Yes).is_ok() {
        return Some(token.into());
    }
    if file.is_absolute() {
        return None;
    }
    let path_list = cx.host().env_get(b"PATH").unwrap_or_default();
    let delimiter = if cfg!(windows) { b';' } else { b':' };
    let dirs = path_list.split(|&byte| byte == delimiter);
    #[cfg(windows)]
    let dirs = std::iter::once(b"".as_slice()).chain(dirs);
    let available = |candidate: &Path| {
        cx.host()
            .metadata(candidate, Follow::Yes)
            .is_ok_and(|meta| {
                meta.kind == FileKind::File && (cfg!(windows) || meta.mode & 0o111 != 0)
            })
    };
    for dir in dirs {
        let candidate = cwd.join(path::to_native(dir)).join(&file);
        if available(&candidate) {
            return Some(path::from_native_cmake(&candidate));
        }
        #[cfg(windows)]
        for extension in ["com", "exe"] {
            let candidate = candidate.with_extension(extension);
            if available(&candidate) {
                return Some(path::from_native_cmake(&candidate));
            }
        }
    }
    None
}
