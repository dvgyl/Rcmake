use std::collections::BTreeSet;

use rcmake_base::{BString, list};
use rcmake_diag::{Effect, MessageKind, PolicyId, PolicyStatus};
use rcmake_eval::args::{KeywordParser, Parsed, Slot};
use rcmake_eval::{CmdError, CmdResult, Cx};
use rcmake_expr::ExpandedArg;
use rcmake_host::path::from_native;

use super::lexical::{Lexical, NATIVE_LIST_SEPARATOR, cmake_style, native_list, native_style};
use super::{Result, error, set, unknown};

pub(super) fn run(cx: &mut Cx<'_>, args: &[ExpandedArg]) -> CmdResult {
    let args: Vec<_> = args.iter().map(|arg| arg.value.as_slice()).collect();
    let [command, input, rest @ ..] = args.as_slice() else {
        return Err(error("must be called with at least two arguments."));
    };
    match *command {
        b"SET" => assign(cx, input, rest),
        b"CONVERT" => convert(cx, input, rest),
        b"COMPARE" => compare(cx, input, rest),
        b"GET" => get(cx, input, rest),
        b"IS_PREFIX" => prefix(cx, input, rest),
        b"NATIVE_PATH" => native(cx, input, rest),
        b"HASH"
        | b"IS_ABSOLUTE"
        | b"IS_RELATIVE"
        | b"HAS_ROOT_NAME"
        | b"HAS_ROOT_DIRECTORY"
        | b"HAS_ROOT_PATH"
        | b"HAS_FILENAME"
        | b"HAS_EXTENSION"
        | b"HAS_STEM"
        | b"HAS_RELATIVE_PART"
        | b"HAS_PARENT_PATH" => query(cx, command, input, rest),
        b"APPEND" | b"APPEND_STRING" | b"REMOVE_FILENAME" | b"REPLACE_FILENAME"
        | b"REMOVE_EXTENSION" | b"REPLACE_EXTENSION" | b"NORMAL_PATH" | b"RELATIVE_PATH"
        | b"ABSOLUTE_PATH" => modify(cx, command, input, rest),
        _ => Err(unknown(command)),
    }
}

fn unexpected(command: &[u8]) -> CmdError {
    error([command, b" called with unexpected arguments."].concat())
}

fn arity(command: &[u8], count: &[u8]) -> CmdError {
    error([command, b" must be called with ", count, b" arguments."].concat())
}

fn input_path(cx: &mut Cx<'_>, name: &[u8]) -> Result<BString> {
    cx.var(name)
        .ok_or_else(|| error("undefined variable for input path."))
}

fn check_output(name: &[u8]) -> Result<()> {
    if name.is_empty() {
        return Err(error("Invalid name for output variable."));
    }
    Ok(())
}

fn check_path_name(name: &[u8]) -> Result<()> {
    if name.is_empty() {
        return Err(error("Invalid name for path variable."));
    }
    Ok(())
}

fn query(cx: &mut Cx<'_>, command: &[u8], name: &[u8], args: &[&[u8]]) -> Result {
    let [out] = args else {
        return Err(arity(command, b"two"));
    };
    let input = input_path(cx, name)?;
    check_output(out)?;
    let path = Lexical::new(&input);
    let value = match command {
        b"HASH" => return set(cx, out, super::path_hash::hash(&path.normalize())),
        b"IS_ABSOLUTE" => path.absolute(),
        b"IS_RELATIVE" => !path.absolute(),
        _ => !component(
            path,
            command.strip_prefix(b"HAS_").unwrap_or_default(),
            false,
        )?
        .is_empty(),
    };
    set(cx, out, bool_value(value))
}

fn bool_value(value: bool) -> &'static str {
    if value { "ON" } else { "OFF" }
}

fn assign(cx: &mut Cx<'_>, name: &[u8], rest: &[&[u8]]) -> Result {
    if !(1..=2).contains(&rest.len()) {
        return Err(arity(b"SET", b"two or three"));
    }
    check_path_name(name)?;
    let normalized = KeywordParser::new()
        .keyword("NORMALIZE", Slot::Flag)
        .parse(rest);
    let [input] = normalized.unparsed() else {
        return Err(unexpected(b"SET"));
    };
    let input = cmake_style(input);
    let value = if normalized.flag("NORMALIZE") {
        Lexical::new(&input).normalize()
    } else {
        input
    };
    set(cx, name, value)
}

fn convert(cx: &mut Cx<'_>, input: &[u8], args: &[&[u8]]) -> Result {
    let [op, out, rest @ ..] = args else {
        return Err(arity(b"CONVERT", b"three or four"));
    };
    if rest.len() > 1 {
        return Err(arity(b"CONVERT", b"three or four"));
    }
    let native = match *op {
        b"TO_CMAKE_PATH_LIST" => false,
        b"TO_NATIVE_PATH_LIST" => true,
        _ => {
            return Err(error(
                [b"CONVERT called with an unknown action: ", *op, b"."].concat(),
            ));
        }
    };
    check_output(out)?;
    let normalize = match rest {
        [] => false,
        [b"NORMALIZE"] => true,
        _ => return Err(unexpected(b"CONVERT")),
    };
    let norm = |path: BString| {
        if normalize {
            Lexical::new(&path).normalize()
        } else {
            path
        }
    };
    let value = if native {
        list::split(input, list::Empty::Drop)
            .into_iter()
            .map(|part| native_style(&norm(part)))
            .collect::<Vec<_>>()
            .join(&NATIVE_LIST_SEPARATOR)
            .into()
    } else {
        list::join(
            native_list(input)
                .filter(|part| !part.is_empty())
                .map(|part| norm(cmake_style(part))),
        )
    };
    set(cx, out, value)
}

fn compare(cx: &mut Cx<'_>, input: &[u8], args: &[&[u8]]) -> Result {
    let [op, rhs, out] = args else {
        return Err(arity(b"COMPARE", b"four"));
    };
    let equal = Lexical::new(input).equal(Lexical::new(rhs));
    let value = match *op {
        b"EQUAL" => equal,
        b"NOT_EQUAL" => !equal,
        _ => {
            return Err(error(
                [
                    b"COMPARE called with an unknown comparison operator: ",
                    *op,
                    b".",
                ]
                .concat(),
            ));
        }
    };
    check_output(out)?;
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

fn get(cx: &mut Cx<'_>, name: &[u8], args: &[&[u8]]) -> Result {
    let [part, rest @ ..] = args else {
        return Err(arity(b"GET", b"at least three"));
    };
    if rest.is_empty() {
        return Err(arity(b"GET", b"at least three"));
    }
    let parser = match *part {
        b"EXTENSION" | b"STEM" => KeywordParser::new().keyword("LAST_ONLY", Slot::Flag),
        b"ROOT_NAME" | b"ROOT_DIRECTORY" | b"ROOT_PATH" | b"FILENAME" | b"RELATIVE_PART"
        | b"PARENT_PATH" => KeywordParser::new(),
        _ => {
            return Err(error(
                [b"GET called with an unknown action: ", *part, b"."].concat(),
            ));
        }
    };
    let args = parser.parse(rest);
    let [out] = args.unparsed() else {
        return Err(unexpected(b"GET"));
    };
    check_output(out)?;
    let value = input_path(cx, name)?;
    set(
        cx,
        out,
        component(Lexical::new(&value), part, args.flag("LAST_ONLY"))?,
    )
}

fn native(cx: &mut Cx<'_>, name: &[u8], args: &[&[u8]]) -> Result {
    if !(1..=2).contains(&args.len()) {
        return Err(arity(b"NATIVE_PATH", b"two or three"));
    }
    let args = KeywordParser::new()
        .keyword("NORMALIZE", Slot::Flag)
        .parse(args);
    let [out] = args.unparsed() else {
        return Err(unexpected(b"NATIVE_PATH"));
    };
    check_output(out)?;
    let mut value = input_path(cx, name)?;
    if args.flag("NORMALIZE") {
        value = Lexical::new(&value).normalize();
    }
    set(cx, out, native_style(&value))
}

fn prefix(cx: &mut Cx<'_>, name: &[u8], args: &[&[u8]]) -> Result {
    if !(2..=3).contains(&args.len()) {
        return Err(arity(b"IS_PREFIX", b"three or four"));
    }
    let args = KeywordParser::new()
        .keyword("NORMALIZE", Slot::Flag)
        .parse(args);
    let [target, out] = args.unparsed() else {
        return Err(unexpected(b"IS_PREFIX"));
    };
    let value = input_path(cx, name)?;
    check_output(out)?;
    let path = Lexical::new(&value);
    let target = Lexical::new(target);
    let result = if value.is_empty() {
        let policy = PolicyId::new(223);
        let status = cx.policy(policy);
        if status == PolicyStatus::Warn {
            cx.issue_policy_warning(policy, b"", b"");
        }
        status != PolicyStatus::New
    } else if args.flag("NORMALIZE") {
        Lexical::new(&path.normalize()).prefix(Lexical::new(&target.normalize()))
    } else {
        path.prefix(target)
    };
    set(cx, out, bool_value(result))
}

fn modify(cx: &mut Cx<'_>, command: &[u8], name: &[u8], args: &[&[u8]]) -> Result {
    if command == b"APPEND" {
        check_path_name(name)?;
    }
    let mut parser = KeywordParser::new().keyword("OUTPUT_VARIABLE", Slot::SingleNonEmpty);
    if matches!(command, b"ABSOLUTE_PATH" | b"RELATIVE_PATH") {
        parser = parser.keyword("BASE_DIRECTORY", Slot::Single);
    }
    if command == b"ABSOLUTE_PATH" {
        parser = parser.keyword("NORMALIZE", Slot::Flag);
    }
    if matches!(command, b"REMOVE_EXTENSION" | b"REPLACE_EXTENSION") {
        parser = parser.keyword("LAST_ONLY", Slot::Flag);
    }
    let args = parser.parse(args);
    if args.has_errors() {
        return Err(report_keyword_errors(cx, &args));
    }
    let inputs = args.unparsed();
    match (command, inputs) {
        (b"APPEND" | b"APPEND_STRING", _) | (_, []) => {}
        (b"REPLACE_FILENAME" | b"REPLACE_EXTENSION", [_]) => {}
        _ => return Err(unexpected(command)),
    }
    let input = if command == b"APPEND" {
        cx.var(name).unwrap_or_default()
    } else {
        input_path(cx, name)?
    };
    let path = Lexical::new(&input);
    let replacement = inputs
        .first()
        .map_or(b"".as_slice(), |word| word.as_slice());
    let last = args.flag("LAST_ONLY");
    let value = match command {
        b"APPEND" => inputs
            .iter()
            .fold(input, |path, part| Lexical::new(&path).append(part)),
        b"APPEND_STRING" => {
            let mut output = input;
            for part in inputs {
                output.extend_from_slice(part);
            }
            output
        }
        b"NORMAL_PATH" => path.normalize(),
        b"REMOVE_FILENAME" => path.without_filename().into(),
        b"REPLACE_FILENAME" if !path.filename().is_empty() => {
            Lexical::new(path.without_filename()).append(replacement)
        }
        b"REPLACE_FILENAME" => input,
        b"REMOVE_EXTENSION" | b"REPLACE_EXTENSION" => replace_extension(path, last, replacement),
        b"ABSOLUTE_PATH" | b"RELATIVE_PATH" => {
            let source = from_native(&cx.current_source_dir());
            let base = Lexical::new(args.single("BASE_DIRECTORY").unwrap_or(&source));
            if command == b"ABSOLUTE_PATH" {
                let absolute = base.append(&input);
                if args.flag("NORMALIZE") {
                    Lexical::new(&absolute).normalize()
                } else {
                    absolute
                }
            } else {
                path.relative(base).unwrap_or_default()
            }
        }
        _ => return Err(unexpected(command)),
    };
    let out = args
        .single("OUTPUT_VARIABLE")
        .map_or(name, |word| word.as_slice());
    set(cx, out, value)
}

fn report_keyword_errors(cx: &mut Cx<'_>, args: &Parsed) -> CmdError {
    let mut text = BString::default();
    for (keyword, messages) in args.keyword_errors() {
        text.extend_from_slice(format!("Error after keyword \"{keyword}\":\n").as_bytes());
        for message in messages.iter().copied().collect::<BTreeSet<_>>() {
            text.extend_from_slice(message.as_bytes());
        }
    }
    cx.issue(MessageKind::Error, Effect::SetError, None, text);
    CmdError::Reported
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
