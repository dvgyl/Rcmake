use rcmake_base::{BString, list};
use rcmake_diag::{PolicyId, PolicyStatus};
use rcmake_eval::{CmdResult, Cx};
use rcmake_expr::ExpandedArg;
use rcmake_host::path::from_native;

use super::lexical::{Lexical, NATIVE_LIST_SEPARATOR, cmake_style, native_list, native_style};
use super::{Result, arg, error, set, unknown};

pub(super) fn run(cx: &mut Cx<'_>, args: &[ExpandedArg]) -> CmdResult {
    let args: Vec<_> = args.iter().map(|arg| arg.value.as_slice()).collect();
    let [command, input, rest @ ..] = args.as_slice() else {
        return Err(error("must be called with at least two arguments."));
    };
    match *command {
        b"SET" => assign(cx, input, rest),
        b"CONVERT" => convert(cx, input, rest),
        b"COMPARE" => compare(cx, input, rest),
        b"GET"
        | b"IS_ABSOLUTE"
        | b"IS_RELATIVE"
        | b"IS_PREFIX"
        | b"NATIVE_PATH"
        | b"HASH"
        | b"HAS_ROOT_NAME"
        | b"HAS_ROOT_DIRECTORY"
        | b"HAS_ROOT_PATH"
        | b"HAS_FILENAME"
        | b"HAS_EXTENSION"
        | b"HAS_STEM"
        | b"HAS_RELATIVE_PART"
        | b"HAS_PARENT_PATH"
        | b"APPEND"
        | b"APPEND_STRING"
        | b"REMOVE_FILENAME"
        | b"REPLACE_FILENAME"
        | b"REMOVE_EXTENSION"
        | b"REPLACE_EXTENSION"
        | b"NORMAL_PATH"
        | b"RELATIVE_PATH"
        | b"PROXIMATE_PATH"
        | b"ABSOLUTE_PATH" => {
            let value = cx
                .var(input)
                .ok_or_else(|| error("undefined variable for input path."))?;
            let path = Lexical::new(&value);
            match *command {
                b"GET" => get(cx, path, rest),
                b"IS_PREFIX" => prefix(cx, path, value.is_empty(), rest),
                b"NATIVE_PATH" => match rest {
                    [out] => set(cx, out, native_style(&value)),
                    [b"NORMALIZE", out] => set(cx, out, native_style(&path.normalize())),
                    _ => Err(unexpected(command)),
                },
                b"HASH" => {
                    let [out] = rest else {
                        return Err(unexpected(command));
                    };
                    set(cx, out, super::path_hash::hash(&path.normalize()))
                }
                b"IS_ABSOLUTE" | b"IS_RELATIVE" => {
                    let [out] = rest else {
                        return Err(unexpected(command));
                    };
                    set(
                        cx,
                        out,
                        bool_value(path.absolute() == (*command == b"IS_ABSOLUTE")),
                    )
                }
                command if let Some(part) = command.strip_prefix(b"HAS_") => {
                    let [out] = rest else {
                        return Err(unexpected(command));
                    };
                    set(
                        cx,
                        out,
                        bool_value(!component(path, part, false)?.is_empty()),
                    )
                }
                _ => modify(cx, command, input, &value, rest),
            }
        }
        _ => Err(unknown(command)),
    }
}

fn unexpected(command: &[u8]) -> rcmake_eval::CmdError {
    error([command, b" called with unexpected arguments."].concat())
}

fn bool_value(value: bool) -> &'static str {
    if value { "ON" } else { "OFF" }
}

fn assign(cx: &mut Cx<'_>, name: &[u8], rest: &[&[u8]]) -> Result {
    let value = match rest {
        [input] => cmake_style(input),
        [b"NORMALIZE", input] => Lexical::new(&cmake_style(input)).normalize(),
        [_, _] => return Err(unexpected(b"SET")),
        _ => return Err(error("SET must be called with two or three arguments.")),
    };
    set(cx, name, value)
}

fn convert(cx: &mut Cx<'_>, input: &[u8], args: &[&[u8]]) -> Result {
    let (op, out, normalize) = match args {
        [op, out] => (*op, *out, false),
        [op, out, b"NORMALIZE"] => (*op, *out, true),
        _ => return Err(unexpected(b"CONVERT")),
    };
    let norm = |path: BString| {
        if normalize {
            Lexical::new(&path).normalize()
        } else {
            path
        }
    };
    let value = match op {
        b"TO_CMAKE_PATH_LIST" => list::join(
            native_list(input)
                .filter(|part| !part.is_empty())
                .map(|part| norm(cmake_style(part))),
        ),
        b"TO_NATIVE_PATH_LIST" => list::split(input, list::Empty::Drop)
            .into_iter()
            .map(|part| native_style(&norm(part)))
            .collect::<Vec<_>>()
            .join(&NATIVE_LIST_SEPARATOR)
            .into(),
        _ => return Err(unexpected(b"CONVERT")),
    };
    set(cx, out, value)
}

fn compare(cx: &mut Cx<'_>, input: &[u8], args: &[&[u8]]) -> Result {
    let [op, rhs, out] = args else {
        return Err(unexpected(b"COMPARE"));
    };
    let equal = Lexical::new(input).equal(Lexical::new(rhs));
    let value = match *op {
        b"EQUAL" => equal,
        b"NOT_EQUAL" => !equal,
        _ => return Err(unexpected(b"COMPARE")),
    };
    set(cx, out, bool_value(value))
}

fn component(path: Lexical<'_>, part: &[u8], last: bool) -> Result<BString> {
    Ok(match part {
        b"ROOT_NAME" => path.root_name(),
        b"ROOT_DIRECTORY" => path.root_directory(),
        b"ROOT_PATH" => return Ok(path.root_path()),
        b"FILENAME" => path.filename(),
        b"EXTENSION" => path.stem_extension(last).1,
        b"STEM" => path.stem_extension(last).0,
        b"RELATIVE_PART" => path.relative_part(),
        b"PARENT_PATH" => path.parent(),
        _ => {
            return Err(error(
                [b"GET called with an unknown action: ", part, b"."].concat(),
            ));
        }
    }
    .into())
}

fn get(cx: &mut Cx<'_>, path: Lexical<'_>, args: &[&[u8]]) -> Result {
    let (part, out, last) = match args {
        [part, out] => (*part, *out, false),
        [part @ (b"EXTENSION" | b"STEM"), b"LAST_ONLY", out] => (*part, *out, true),
        _ => return Err(unexpected(b"GET")),
    };
    set(cx, out, component(path, part, last)?)
}

fn prefix(cx: &mut Cx<'_>, path: Lexical<'_>, empty: bool, args: &[&[u8]]) -> Result {
    let (target, out, normalize) = match args {
        [target, out] => (*target, *out, false),
        [target, b"NORMALIZE", out] => (*target, *out, true),
        _ => return Err(unexpected(b"IS_PREFIX")),
    };
    let target = Lexical::new(target);
    let result = if empty {
        let policy = PolicyId::new(223);
        let status = cx.policy(policy);
        if status == PolicyStatus::Warn {
            cx.issue_policy_warning(policy, b"", b"");
        }
        status != PolicyStatus::New
    } else if normalize {
        Lexical::new(&path.normalize()).prefix(Lexical::new(&target.normalize()))
    } else {
        path.prefix(target)
    };
    set(cx, out, bool_value(result))
}

fn modify(cx: &mut Cx<'_>, command: &[u8], name: &[u8], input: &[u8], args: &[&[u8]]) -> Result {
    let path = Lexical::new(input);
    let mut args = args.iter().copied();
    let mut out = name;
    let mut base = None;
    let mut normalize = false;
    let mut last = false;
    let mut inputs = Vec::new();
    while let Some(option) = args.next() {
        match option {
            b"OUTPUT_VARIABLE" => out = arg(&mut args, option)?,
            b"BASE_DIRECTORY"
                if matches!(
                    command,
                    b"ABSOLUTE_PATH" | b"RELATIVE_PATH" | b"PROXIMATE_PATH"
                ) =>
            {
                base = Some(arg(&mut args, option)?);
            }
            b"NORMALIZE" if command == b"ABSOLUTE_PATH" => normalize = true,
            b"LAST_ONLY" if matches!(command, b"REMOVE_EXTENSION" | b"REPLACE_EXTENSION") => {
                last = true
            }
            _ => inputs.push(option),
        }
    }
    let value = match (command, inputs.as_slice()) {
        (b"APPEND", _) => inputs.into_iter().fold(BString::from(input), |path, part| {
            Lexical::new(&path).append(part)
        }),
        (b"APPEND_STRING", _) => std::iter::once(input)
            .chain(inputs)
            .flatten()
            .copied()
            .collect::<Vec<_>>()
            .into(),
        (b"NORMAL_PATH", []) => path.normalize(),
        (b"REMOVE_FILENAME", []) => path.without_filename().into(),
        (b"REPLACE_FILENAME", [replacement]) if !path.filename().is_empty() => {
            Lexical::new(path.without_filename()).append(replacement)
        }
        (b"REPLACE_FILENAME", [_]) => input.into(),
        (b"REMOVE_EXTENSION", []) => replace_extension(path, last, b""),
        (b"REPLACE_EXTENSION", [replacement]) => replace_extension(path, last, replacement),
        (b"ABSOLUTE_PATH" | b"RELATIVE_PATH" | b"PROXIMATE_PATH", []) => {
            let source = from_native(&cx.current_source_dir());
            let base = Lexical::new(base.unwrap_or(&source));
            if command == b"ABSOLUTE_PATH" {
                let absolute = base.append(input);
                if normalize {
                    Lexical::new(&absolute).normalize()
                } else {
                    absolute
                }
            } else {
                path.relative(base).unwrap_or_else(|| {
                    if command == b"PROXIMATE_PATH" {
                        input.into()
                    } else {
                        BString::default()
                    }
                })
            }
        }
        _ => return Err(unexpected(command)),
    };
    set(cx, out, value)
}

fn replace_extension(path: Lexical<'_>, last: bool, replacement: &[u8]) -> BString {
    let stem = path.stem_extension(last).0;
    let dot: &[u8] = if replacement.is_empty() || replacement.starts_with(b".") {
        b""
    } else {
        b"."
    };
    [path.without_filename(), stem, dot, replacement]
        .concat()
        .into()
}
