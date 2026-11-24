use rcmake_base::{BString, list, truth};
use rcmake_eval::{CmdError, CmdResult, Cx, Flow, LogLevel, RunMode};
use rcmake_expr::ExpandedArg;
use rcmake_host::path;
use rcmake_model::DirId;
use rcmake_syntax::Argument;

use super::{quoted, status};
use crate::dirs::{find_directory, normalized_path};

const TOP_LEVEL_INCLUDES: &[u8] = b"_RCMAKE_IN_TOP_LEVEL_INCLUDES";
const FIND_PACKAGE_PROVIDER: &[u8] = b"_RCMAKE_FIND_PACKAGE_PROVIDER";
const FETCHCONTENT_PROVIDER: &[u8] = b"__FETCHCONTENT_MAKEAVAILABLE_SERIAL_PROVIDER";

pub(super) fn language(cx: &mut Cx<'_>, _: &[ExpandedArg]) -> CmdResult {
    let raw = cx.raw_args().to_vec();
    let mut args = LanguageArgs {
        raw: &raw,
        expanded: Vec::new().into_iter(),
    };
    let result = language_operation(cx, &mut args);
    if matches!(result, Err(CmdError::Status(_))) {
        cx.set_fatal();
    }
    result
}

struct LanguageArgs<'a> {
    raw: &'a [Argument],
    expanded: std::vec::IntoIter<ExpandedArg>,
}

impl LanguageArgs<'_> {
    fn next(&mut self, cx: &mut Cx<'_>) -> Result<Option<BString>, CmdError> {
        while self.expanded.len() == 0 {
            let Some((arg, rest)) = self.raw.split_first() else {
                return Ok(None);
            };
            self.raw = rest;
            self.expanded = cx.expand_raw_args(core::slice::from_ref(arg))?.into_iter();
        }
        Ok(self.expanded.next().map(|arg| arg.value))
    }

    fn required(&mut self, cx: &mut Cx<'_>, message: &str) -> Result<BString, CmdError> {
        self.next(cx)?.ok_or_else(|| status(message))
    }

    fn finish(&mut self, cx: &mut Cx<'_>) -> Result<Vec<ExpandedArg>, CmdError> {
        let mut rest: Vec<_> = self.expanded.by_ref().collect();
        rest.extend(cx.expand_raw_args(core::mem::take(&mut self.raw))?);
        Ok(rest)
    }
}

fn language_operation(cx: &mut Cx<'_>, args: &mut LanguageArgs<'_>) -> CmdResult {
    let op = args.required(cx, "called with incorrect number of arguments")?;
    match op.as_slice() {
        b"CALL" => {
            let command = call_name(cx, args, false)?;
            let expanded = cx.expand_raw_args(args.raw)?;
            cx.call_command(&command, expanded)
        }
        b"DEFER" => defer(cx, args),
        b"TRACE" => trace(cx, args),
        b"PRINT_TARGETS" | b"PRINT_VARIABLES" | b"PRINT_PROPERTIES" => {
            let args = args.finish(cx)?;
            let args = super::values(&args);
            match op.as_slice() {
                b"PRINT_TARGETS" => super::introspection::targets(cx, &args),
                b"PRINT_VARIABLES" => super::introspection::variables(cx, &args),
                _ => super::introspection::properties(cx, &args),
            }
        }
        b"EVAL" => {
            let args = args.finish(cx)?;
            eval(cx, &args)
        }
        b"GET_EXPERIMENTAL_FEATURE_ENABLED" => {
            let args = args.finish(cx)?;
            super::experimental::enabled(cx, &super::values(&args))
        }
        b"GET_MESSAGE_LOG_LEVEL" => {
            let args = args.finish(cx)?;
            get_message_log_level(cx, &args)
        }
        b"SET_DEPENDENCY_PROVIDER" => {
            let args = args.finish(cx)?;
            set_dependency_provider(cx, &args)
        }
        b"EXIT" => {
            let code = args.required(cx, "EXIT requires one argument")?;
            exit(cx, &code)
        }
        _ => Err(status("called with unknown meta-operation")),
    }
}

fn trace(cx: &mut Cx<'_>, args: &mut LanguageArgs<'_>) -> CmdResult {
    let mode = args.required(cx, "TRACE missing a boolean value")?;
    if truth::is_on(&mode) {
        let expand = if let Some(arg) = args.next(cx)? {
            if arg != b"EXPAND" {
                return Err(status(quoted(
                    "TRACE ON given an invalid argument ",
                    &arg,
                    "",
                )));
            }
            if args.next(cx)?.is_some() {
                return Err(status("TRACE ON given too many arguments"));
            }
            true
        } else {
            false
        };
        cx.push_trace(expand);
    } else {
        if args.next(cx)?.is_some() {
            return Err(status("TRACE OFF given too many arguments"));
        }
        if !cx.pop_trace() {
            return Err(status("TRACE OFF request without a corresponding TRACE ON"));
        }
    }
    Ok(Flow::Normal)
}

fn call_name(
    cx: &mut Cx<'_>,
    args: &mut LanguageArgs<'_>,
    deferred: bool,
) -> Result<BString, CmdError> {
    let command = args.required(cx, "CALL missing command name")?;
    if args.expanded.len() != 0 {
        return Err(status("CALL command's arguments must be literal"));
    }
    let lower = command.to_ascii_lowercase();
    if matches!(
        lower.as_slice(),
        b"if"
            | b"elseif"
            | b"else"
            | b"endif"
            | b"foreach"
            | b"endforeach"
            | b"while"
            | b"endwhile"
            | b"function"
            | b"endfunction"
            | b"macro"
            | b"endmacro"
            | b"block"
            | b"endblock"
    ) || (deferred && lower == b"return")
    {
        return Err(status(quoted("invalid command specified: ", &command, "")));
    }
    Ok(command)
}

fn eval(cx: &mut Cx<'_>, args: &[ExpandedArg]) -> CmdResult {
    let Some((code, args)) = args.split_first() else {
        return Err(status("called with incorrect number of arguments"));
    };
    if code.value != b"CODE" {
        let message = if args.iter().any(|arg| arg.value == b"CODE") {
            "called with unsupported arguments between EVAL and CODE arguments"
        } else {
            "called without CODE argument"
        };
        return Err(status(message));
    }
    let mut code = BString::default();
    for (i, arg) in args.iter().enumerate() {
        if code
            .len()
            .saturating_add(arg.value.len())
            .saturating_add(usize::from(i > 0))
            > cx.limits().max_value_bytes
        {
            return Err(status("EVAL CODE exceeds the value size limit."));
        }
        if i > 0 {
            code.push(b' ');
        }
        code.extend_from_slice(&arg.value);
    }
    cx.eval_code(&code)
}

fn get_message_log_level(cx: &mut Cx<'_>, args: &[ExpandedArg]) -> CmdResult {
    let [var] = args else {
        return Err(status(
            "sub-command GET_MESSAGE_LOG_LEVEL expects exactly one argument",
        ));
    };
    let level = match cx.log_level() {
        LogLevel::Error => "ERROR",
        LogLevel::Warning => "WARNING",
        LogLevel::Notice => "NOTICE",
        LogLevel::Status => "STATUS",
        LogLevel::Verbose => "VERBOSE",
        LogLevel::Debug => "DEBUG",
        LogLevel::Trace => "TRACE",
    };
    cx.set_var(&var.value, level.into());
    Ok(Flow::Normal)
}

fn set_dependency_provider(cx: &mut Cx<'_>, args: &[ExpandedArg]) -> CmdResult {
    if cx.mode() != RunMode::Configure
        || !cx
            .global()
            .props
            .get(TOP_LEVEL_INCLUDES)
            .is_some_and(|value| value == b"ON")
    {
        return Err(status(
            "Dependency providers can only be set as part of the first call to \
             project(). More specifically, cmake_language(SET_DEPENDENCY_PROVIDER) \
             can only be called while the first project() command processes files \
             listed in CMAKE_PROJECT_TOP_LEVEL_INCLUDES.",
        ));
    }

    const COMMAND: &[u8] = b"SET_DEPENDENCY_PROVIDER";
    const METHODS: &[u8] = b"SUPPORTED_METHODS";
    let is_keyword = |arg: &[u8]| matches!(arg, COMMAND | METHODS);
    let mut input = args.iter().map(|arg| arg.value.as_slice()).peekable();
    let mut command = input.next_if(|arg| !is_keyword(arg)).unwrap_or_default();
    let mut methods = Vec::new();
    while let Some(arg) = input.next() {
        match arg {
            COMMAND => {
                if let Some(name) = input.next_if(|arg| !is_keyword(arg)) {
                    command = name;
                }
            }
            METHODS => {
                while let Some(method) = input.next_if(|arg| !is_keyword(arg)) {
                    methods.push(method);
                }
            }
            _ => return Err(status(quoted("Unrecognized keyword: \"", arg, "\""))),
        }
    }

    if command.is_empty() {
        if !methods.is_empty() {
            return Err(status(
                "Must specify a non-empty command name when provider methods are given",
            ));
        }
    } else {
        if !cx.command_exists(command) {
            return Err(status(quoted(
                "Command \"",
                command,
                "\" is not a defined command",
            )));
        }
        if methods.is_empty() {
            return Err(status("Must specify at least one provider method"));
        }
    }

    let mut find_package = false;
    let mut fetchcontent = false;
    for method in methods {
        match method {
            b"FIND_PACKAGE" => find_package = true,
            b"FETCHCONTENT_MAKEAVAILABLE_SERIAL" => fetchcontent = true,
            _ => {
                return Err(status(quoted(
                    "Unknown dependency provider method \"",
                    method,
                    "\"",
                )));
            }
        }
    }
    let props = &mut cx.global().props;
    if find_package {
        props.set(FIND_PACKAGE_PROVIDER.into(), command.into());
    } else {
        props.unset(FIND_PACKAGE_PROVIDER);
    }
    props.set(
        FETCHCONTENT_PROVIDER.into(),
        if fetchcontent {
            command.into()
        } else {
            BString::default()
        },
    );
    Ok(Flow::Normal)
}

fn defer(cx: &mut Cx<'_>, args: &mut LanguageArgs<'_>) -> CmdResult {
    let mut directory = None;
    let mut id = None;
    let mut id_var = None;
    let mut op = args.required(cx, "DEFER requires at least one argument")?;
    loop {
        match op.as_slice() {
            b"DIRECTORY" => {
                if directory.is_some() {
                    return Err(status("DEFER given multiple DIRECTORY arguments"));
                }
                let dir = args.required(cx, "DEFER DIRECTORY missing value")?;
                if dir.is_empty() {
                    return Err(status("DEFER DIRECTORY may not be empty"));
                }
                directory = Some(find_directory(cx, &dir).ok_or_else(|| {
                    let full_path = path::from_native(&normalized_path(&cx.resolve_path(&dir)));
                    status(quoted(
                        "DEFER DIRECTORY:\n  ",
                        &full_path,
                        "\nis not known.  It may not have been processed yet.",
                    ))
                })?);
            }
            b"ID" => {
                if id.is_some() {
                    return Err(status("DEFER given multiple ID arguments"));
                }
                let value = args.required(cx, "DEFER ID missing value")?;
                if value.is_empty() {
                    return Err(status("DEFER ID may not be empty"));
                }
                if starts_uppercase(&value) {
                    return Err(status("DEFER ID may not start in A-Z."));
                }
                id = Some(value);
            }
            b"ID_VAR" => {
                if id_var.is_some() {
                    return Err(status("DEFER given multiple ID_VAR arguments"));
                }
                let var = args.required(cx, "DEFER ID_VAR missing variable name")?;
                if var.is_empty() {
                    return Err(status("DEFER ID_VAR may not be empty"));
                }
                id_var = Some(var);
            }
            b"CALL" => return defer_call(cx, args, directory, id, id_var),
            b"GET_CALL_IDS" | b"GET_CALL" | b"CANCEL_CALL" => {
                if id.is_some() || id_var.is_some() {
                    return Err(status(quoted(
                        "DEFER ",
                        &op,
                        " does not accept ID or ID_VAR.",
                    )));
                }
                return defer_queue_operation(cx, &op, args, directory);
            }
            _ => return Err(status(quoted("DEFER unknown option:\n  ", &op, ""))),
        }
        op = args.required(cx, "DEFER must be followed by a CALL argument")?;
    }
}

fn starts_uppercase(value: &[u8]) -> bool {
    value.first().is_some_and(u8::is_ascii_uppercase)
}

fn defer_call(
    cx: &mut Cx<'_>,
    args: &mut LanguageArgs<'_>,
    directory: Option<DirId>,
    id: Option<BString>,
    id_var: Option<BString>,
) -> CmdResult {
    let command = call_name(cx, args, true)?;

    let id = id.unwrap_or_else(|| cx.defer_new_id());
    if let Some(var) = id_var {
        cx.set_var(&var, id.clone());
    }
    if !cx.defer_call(
        directory.unwrap_or_else(|| cx.current_dir_id()),
        id,
        &command,
        args.raw,
    ) {
        return Err(defer_directory_error(
            cx,
            directory,
            "DEFER CALL may not be scheduled in directory:\n  ",
        ));
    }
    Ok(Flow::Normal)
}

fn defer_queue_operation(
    cx: &mut Cx<'_>,
    op: &[u8],
    args: &mut LanguageArgs<'_>,
    directory: Option<DirId>,
) -> CmdResult {
    let args = args.finish(cx)?;
    let values: Vec<_> = args.iter().map(|arg| arg.value.as_slice()).collect();
    let dir = directory.unwrap_or_else(|| cx.current_dir_id());
    if op == b"CANCEL_CALL" {
        for id in values {
            if starts_uppercase(id) {
                return Err(status(quoted(
                    "DEFER CANCEL_CALL unknown argument:\n  ",
                    id,
                    "",
                )));
            }
            if !cx.defer_cancel_call(dir, id) {
                return Err(defer_directory_error(
                    cx,
                    directory,
                    "DEFER CANCEL_CALL may not update directory:\n  ",
                ));
            }
        }
        return Ok(Flow::Normal);
    }

    let (var, result, message) = if op == b"GET_CALL_IDS" {
        let [var] = values.as_slice() else {
            return Err(status(if values.is_empty() {
                "DEFER GET_CALL_IDS missing output variable"
            } else {
                "DEFER GET_CALL_IDS given too many arguments"
            }));
        };
        (
            *var,
            cx.defer_get_call_ids(dir),
            "DEFER GET_CALL_IDS may not access directory:\n  ",
        )
    } else {
        let (id, var) = match values.as_slice() {
            [] => return Err(status("DEFER GET_CALL missing id")),
            [_] => return Err(status("DEFER GET_CALL missing output variable")),
            [id, var] => (*id, *var),
            _ => return Err(status("DEFER GET_CALL given too many arguments")),
        };
        if id.is_empty() {
            return Err(status("DEFER GET_CALL id may not be empty"));
        }
        if starts_uppercase(id) {
            return Err(status(quoted(
                "DEFER GET_CALL unknown argument:\n ",
                id,
                "",
            )));
        }
        (
            var,
            cx.defer_get_call(dir, id),
            "DEFER GET_CALL may not access directory:\n  ",
        )
    };
    let Some(result) = result else {
        return Err(defer_directory_error(cx, directory, message));
    };
    cx.set_var(var, list::join(result.iter()));
    Ok(Flow::Normal)
}

fn defer_directory_error(cx: &mut Cx<'_>, directory: Option<DirId>, message: &str) -> CmdError {
    let binary = match directory {
        Some(id) => cx.project().dir(id).binary_dir.clone(),
        None => path::from_native(&cx.current_binary_dir()),
    };
    status(quoted(message, &binary, "\nat this time."))
}

fn exit(cx: &mut Cx<'_>, arg: &[u8]) -> CmdResult {
    if cx.mode() != RunMode::Script {
        return Err(status("EXIT can be used only in SCRIPT mode"));
    }
    let code = core::str::from_utf8(arg)
        .ok()
        .and_then(|value| {
            value
                .trim_start_matches(|ch: char| ch.is_ascii_whitespace() || ch == '\u{b}')
                .parse::<i64>()
                .ok()
        })
        .ok_or_else(|| {
            status(quoted(
                "EXIT requires one integral argument, got \"",
                arg,
                "\"",
            ))
        })?;
    // CMake passes the low 32 bits of the parsed integer to the exit function.
    cx.request_exit(code as i32);
    Ok(Flow::Normal)
}
