use rcmake_base::{BString, hash::HashAlgo, list};
use rcmake_diag::{PolicyId, PolicyStatus};
use rcmake_eval::{CmdResult, Cx};
use rcmake_expr::ExpandedArg;
use rcmake_host::path::{from_native_cmake, to_native};

use super::{
    Result, archive, configure, download, elf_rpath, error, generate, glob, install_cmd, lock,
    macho, mutation, reading, set, unknown, upload, warning,
};
use rcmake_base::path_lex::{Lexical, native_list, native_style};

pub(super) fn run(cx: &mut Cx<'_>, args: &[ExpandedArg]) -> CmdResult {
    let args: Vec<_> = args.iter().map(|arg| arg.value.as_slice()).collect();
    let [command, rest @ ..] = args.as_slice() else {
        return Err(error("must be called with at least one argument."));
    };
    match *command {
        b"READ" | b"STRINGS" | b"SIZE" | b"TIMESTAMP" | b"READ_SYMLINK" => {
            reading::run(cx, command, rest)
        }
        b"WRITE" | b"APPEND" | b"TOUCH" | b"TOUCH_NOCREATE" | b"MAKE_DIRECTORY" | b"REMOVE"
        | b"REMOVE_RECURSE" | b"RENAME" | b"COPY_FILE" | b"CREATE_LINK" | b"CHMOD"
        | b"CHMOD_RECURSE" | b"COPY" => mutation::run(cx, command, rest),
        b"GLOB" | b"GLOB_RECURSE" => glob::run(cx, *command == b"GLOB_RECURSE", rest),
        b"READ_ELF" => elf_rpath::read(cx, rest),
        b"READ_MACHO" => macho::run(cx, rest),
        b"INSTALL" => install_cmd::run(cx, rest),
        b"DOWNLOAD" => download::run(cx, rest),
        b"UPLOAD" => upload::run(cx, rest),
        b"RPATH_CHECK" | b"RPATH_CHANGE" | b"RPATH_REMOVE" | b"CHRPATH" => {
            elf_rpath::run(cx, command, rest)
        }
        b"LOCK" => lock::run(cx, rest),
        b"GENERATE" => generate::run(cx, rest),
        b"CONFIGURE" => configure::run(cx, rest),
        b"DIFFERENT" => reading::different(cx, rest),
        b"REAL_PATH" => real_path(cx, rest),
        b"RELATIVE_PATH" => relative_path(cx, rest),
        b"TO_CMAKE_PATH" | b"TO_NATIVE_PATH" => convert(cx, *command == b"TO_NATIVE_PATH", rest),
        b"ARCHIVE_CREATE" => archive::create(cx, rest),
        b"ARCHIVE_EXTRACT" => archive::extract(cx, rest),
        command => match HashAlgo::parse(command) {
            Some(algo) => reading::hash(cx, command, algo, rest),
            None => Err(unknown(command)),
        },
    }
}

fn real_path(cx: &mut Cx<'_>, args: &[&[u8]]) -> Result {
    let [input, out, options @ ..] = args else {
        return Err(error(
            "REAL_PATH must be called with at least two arguments.",
        ));
    };
    let mut options = options.iter().copied();
    let mut base = from_native_cmake(&cx.current_source_dir());
    let mut expand = false;
    while let Some(option) = options.next() {
        match option {
            b"BASE_DIRECTORY" => {
                let value = options.next().ok_or_else(|| {
                    crate::command_error(
                        cx,
                        "Error after keyword \"BASE_DIRECTORY\":\n\n  missing required value\n",
                    )
                })?;
                base = value.into();
            }
            b"EXPAND_TILDE" => expand = true,
            _ => {
                return Err(error(
                    [b"REAL_PATH called with unexpected arguments: ", option].concat(),
                ));
            }
        }
    }
    let input = if expand {
        expand_home(cx, input)
    } else {
        BString::from(*input)
    };
    let path = if Lexical::new(&input).absolute() || base.is_empty() {
        input.clone()
    } else if input.is_empty() {
        base
    } else {
        [base.as_slice(), b"/", input.as_slice()].concat().into()
    };
    let before_links = cx.policy(PolicyId::new(152)) != PolicyStatus::New;
    let candidate = if before_links {
        Lexical::new(&path).normalize()
    } else {
        path.clone()
    };
    let canonicalize = |candidate: &[u8]| {
        let native = if Lexical::new(candidate).absolute() {
            cx.resolve_path(candidate)
        } else {
            cx.host().initial_cwd().join(to_native(candidate))
        };
        cx.host().canonicalize(&native).ok()
    };
    let resolved = canonicalize(&candidate);
    if before_links
        && cx.policy(PolicyId::new(152)) == PolicyStatus::Warn
        && canonicalize(&path) != resolved
    {
        cx.issue_policy_warning(PolicyId::new(152), b"", b"");
    }
    let result = match resolved {
        Some(resolved) => from_native_cmake(&resolved),
        None => {
            warning(
                cx,
                [
                    b"Given path:\n\n  ",
                    input.as_slice(),
                    b"\n\ndoes not refer to an existing path on disk.",
                ]
                .concat(),
            );
            candidate
        }
    };
    set(cx, out, result)
}

fn relative_path(cx: &mut Cx<'_>, args: &[&[u8]]) -> Result {
    let [out, base, input] = args else {
        return Err(error(
            "RELATIVE_PATH must be called with exactly three arguments.",
        ));
    };
    for (kind, path) in [
        (b"directory".as_slice(), *base),
        (b"file".as_slice(), *input),
    ] {
        if !Lexical::new(path).absolute() {
            return Err(error(
                [
                    b"RELATIVE_PATH must be passed a full path to the ",
                    kind,
                    b": ",
                    path,
                ]
                .concat(),
            ));
        }
    }
    set(cx, out, relative_absolute(base, input))
}

pub(super) fn relative_absolute(base: &[u8], input: &[u8]) -> BString {
    let target = Lexical::new(input).normalize();
    let base = Lexical::new(base).normalize();
    let target = target
        .strip_suffix(b"/")
        .filter(|s| !s.is_empty())
        .unwrap_or(&target);
    let value = Lexical::new(target).relative(Lexical::new(&base));
    match value.as_deref() {
        Some(value) if value == b"." => BString::default(),
        Some(value) if value.split(|&byte| byte == b'/').all(|part| part == b"..") => {
            [value.as_slice(), b"/"].concat().into()
        }
        Some(value) => value.as_slice().into(),
        None => input.into(),
    }
}

fn expand_home(cx: &Cx<'_>, input: &[u8]) -> BString {
    if matches!(input, b"~") || input.starts_with(b"~/") {
        let home = if cfg!(windows) {
            cx.host()
                .env_get(b"USERPROFILE")
                .or_else(|| cx.host().env_get(b"HOME"))
        } else {
            cx.host().env_get(b"HOME")
        };
        if let Some(home) = home.filter(|home| !home.is_empty()) {
            return [home.as_slice(), input.get(1..).unwrap_or_default()]
                .concat()
                .into();
        }
    }
    input.into()
}

fn convert(cx: &mut Cx<'_>, native: bool, args: &[&[u8]]) -> Result {
    let [input, out] = args else {
        return Err(error(
            "path conversion must be called with exactly two arguments.",
        ));
    };
    let output = list::join(native_list(input).map(|part| {
        let path = legacy_separators(part, !native);
        if native {
            let style = native_style(&path);
            if cfg!(windows) {
                if style.contains(&b' ') {
                    return [b"\"".as_slice(), style.as_slice(), b"\""].concat().into();
                }
                return style;
            }
            let mut output = BString::default();
            for &b in style.as_slice() {
                if b == b' ' {
                    output.push(b'\\');
                }
                output.push(b);
            }
            output
        } else {
            expand_home(cx, &path)
        }
    }));
    set(cx, out, output)
}

pub(super) fn legacy_separators(path: &[u8], trim_trailing: bool) -> BString {
    let mut output = BString::default();
    for &byte in path {
        if byte == b'/' || (byte == b'\\' && (trim_trailing || cfg!(windows))) {
            if !output.ends_with(b"/") || output.as_slice() == b"/" {
                output.push(b'/');
            }
        } else {
            output.push(byte);
        }
    }
    if trim_trailing
        && output.len() > 1
        && output.ends_with(b"/")
        && !(cfg!(windows) && output.ends_with(b":/"))
    {
        output.pop();
    }
    output
}

pub(super) fn binary_destination(cx: &Cx<'_>, input: &[u8]) -> std::path::PathBuf {
    let input = to_native(input);
    cx.resolve_path(&from_native_cmake(&cx.current_binary_dir().join(input)))
}
