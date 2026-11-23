use std::path::Path;

use rcmake_base::{BString, list};
use rcmake_eval::{CmdResult, Cx, Flow};
use rcmake_expr::ExpandedArg;
use rcmake_host::{FileKind, Follow, path};

use super::{bytes, status};

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
        return Err(status("must be given at least one argument."));
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
                        return Err(status(
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
        status("missing required option: 'UNIX_COMMAND' or 'WINDOWS_COMMAND' or 'NATIVE_COMMAND'")
    })?;
    if separate && !program {
        return Err(status("`SEPARATE_ARGS` option requires `PROGRAM' option"));
    }
    if extra {
        return Err(status("given unexpected argument(s)"));
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
                    list::join(std::iter::once(program).chain(args).map(escape_semicolons))
                } else {
                    list::join([program.as_slice(), args.rest()])
                }
            })
    } else {
        list::join(args.map(escape_semicolons))
    };
    cx.set_var(name, result);
    Ok(Flow::Normal)
}

fn escape_semicolons(arg: BString) -> BString {
    let mut escaped = Vec::with_capacity(arg.len());
    for &byte in arg.iter() {
        if byte == b';' {
            escaped.push(b'\\');
        }
        escaped.push(byte);
    }
    escaped.into()
}

fn find_program(cx: &Cx<'_>, token: &[u8]) -> Option<BString> {
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
            return Some(path::from_native(&candidate));
        }
        #[cfg(windows)]
        for extension in ["com", "exe"] {
            let candidate = candidate.with_extension(extension);
            if available(&candidate) {
                return Some(path::from_native(&candidate));
            }
        }
    }
    None
}

struct Arguments<'a> {
    input: &'a [u8],
    offset: usize,
    windows: bool,
}

impl<'a> Arguments<'a> {
    fn new(input: &'a [u8], windows: bool) -> Self {
        Self {
            input,
            offset: 0,
            windows,
        }
    }

    fn rest(&self) -> &'a [u8] {
        self.input.get(self.offset..).unwrap_or_default()
    }
}

impl Iterator for Arguments<'_> {
    type Item = BString;

    fn next(&mut self) -> Option<Self::Item> {
        let mut input = self.rest().iter().copied().peekable();
        let mut consumed = 0;
        while input.peek().is_some_and(|byte| space(*byte)) {
            input.next();
            consumed += 1;
        }
        input.peek()?;
        let mut quote = None;
        let mut quoted = false;
        let mut token = Vec::new();
        while let Some(byte) = input.next_if(|byte| quote.is_some() || !space(*byte)) {
            consumed += 1;
            match byte {
                b'\\' if !self.windows => {
                    if let Some(escaped) = input.next() {
                        token.push(escaped);
                        consumed += 1;
                    }
                }
                b'\\' => {
                    let mut slashes = 1;
                    while input.next_if_eq(&b'\\').is_some() {
                        consumed += 1;
                        slashes += 1;
                    }
                    if input.next_if_eq(&b'"').is_some() {
                        consumed += 1;
                        token.extend(std::iter::repeat_n(b'\\', slashes / 2));
                        if slashes % 2 == 0 {
                            quote = if quote.is_some() { None } else { Some(b'"') };
                        } else {
                            token.push(b'"');
                        }
                    } else {
                        token.extend(std::iter::repeat_n(b'\\', slashes));
                    }
                }
                b'"' | b'\'' if byte == b'"' || !self.windows => {
                    quoted = true;
                    if quote == Some(byte) {
                        quote = None;
                    } else if quote.is_none() {
                        quote = Some(byte);
                    } else {
                        token.push(byte);
                    }
                }
                _ => token.push(byte),
            }
        }
        self.offset += consumed;
        (quoted || !token.is_empty()).then(|| token.into())
    }
}

use rcmake_base::list_ops::is_space as space;
