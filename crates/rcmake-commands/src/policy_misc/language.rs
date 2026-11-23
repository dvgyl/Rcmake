use rcmake_base::BString;
use rcmake_eval::{CmdResult, Cx, Flow, LogLevel, RunMode};
use rcmake_expr::ExpandedArg;

use super::{quoted, status};

pub(super) fn language(cx: &mut Cx<'_>, _: &[ExpandedArg]) -> CmdResult {
    let raw = cx.raw_args().to_vec();
    if raw.first().is_some_and(|a| a.text.as_slice() == b"DEFER") {
        return defer(cx, &raw);
    }
    let args = cx.expand_raw_args(&raw)?;
    let Some((op, args)) = args.split_first() else {
        return Err(status("called with incorrect number of arguments"));
    };
    match op.value.as_slice() {
        b"CALL" => {
            let Some((command, args)) = args.split_first() else {
                return Err(status("CALL missing command name"));
            };
            cx.call_command(&command.value, args.to_vec())
        }
        b"EVAL" => {
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
        b"GET_MESSAGE_LOG_LEVEL" => {
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
        b"EXIT" => exit(cx, args),
        b"DEFER" => defer(cx, &raw),
        _ => Err(status("called with unknown meta-operation")),
    }
}

fn defer(cx: &mut Cx<'_>, raw: &[rcmake_syntax::Argument]) -> CmdResult {
    let mut exp = std::collections::VecDeque::<BString>::new();
    let mut next_raw = 0usize;
    let next_arg = |cx: &mut Cx<'_>,
                    exp: &mut std::collections::VecDeque<BString>,
                    next_raw: &mut usize|
     -> Result<Option<BString>, rcmake_eval::CmdError> {
        while exp.is_empty() {
            let Some(item) = raw.get(*next_raw) else {
                return Ok(None);
            };
            *next_raw += 1;
            let one = cx.expand_raw_args(core::slice::from_ref(item))?;
            exp.extend(one.into_iter().map(|a| a.value));
        }
        Ok(exp.pop_front())
    };

    // Consume "DEFER".
    if next_arg(cx, &mut exp, &mut next_raw)?.is_none() {
        return Err(status("DEFER requires at least one argument"));
    }

    let mut id: Option<BString> = None;
    let mut id_var: Option<BString> = None;
    let mut directory_seen = false;

    while let Some(op) = next_arg(cx, &mut exp, &mut next_raw)? {
        match op.as_slice() {
            b"ID" => {
                if id.is_some() {
                    return Err(status("DEFER ID may not be specified more than once."));
                }
                if id_var.is_some() {
                    return Err(status("DEFER ID and ID_VAR may not both be specified."));
                }
                let Some(val) = next_arg(cx, &mut exp, &mut next_raw)? else {
                    return Err(status("DEFER ID missing value"));
                };
                if val.is_empty() {
                    return Err(status("DEFER ID may not be empty"));
                }
                if val.first().is_some_and(|b| *b >= b'A' && *b <= b'Z') {
                    return Err(status(quoted(
                        "DEFER ID may not start in upper-case 'A-Z':\n  ",
                        &val,
                        "",
                    )));
                }
                id = Some(val);
            }
            b"ID_VAR" => {
                if id_var.is_some() {
                    return Err(status("DEFER ID_VAR may not be specified more than once."));
                }
                if id.is_some() {
                    return Err(status("DEFER ID and ID_VAR may not both be specified."));
                }
                let Some(var) = next_arg(cx, &mut exp, &mut next_raw)? else {
                    return Err(status("DEFER ID_VAR missing variable name"));
                };
                if var.is_empty() {
                    return Err(status("DEFER ID_VAR missing variable name"));
                }
                id_var = Some(var);
            }
            b"DIRECTORY" => {
                if directory_seen {
                    return Err(status(
                        "DEFER DIRECTORY may not be specified more than once.",
                    ));
                }
                directory_seen = true;
                let Some(dir_arg) = next_arg(cx, &mut exp, &mut next_raw)? else {
                    return Err(status("DEFER DIRECTORY missing value"));
                };
                let cur_src = rcmake_host::path::from_native(&cx.current_source_dir());
                let cur_bin = rcmake_host::path::from_native(&cx.current_binary_dir());
                if dir_arg.as_slice() != cur_src.as_slice()
                    && dir_arg.as_slice() != cur_bin.as_slice()
                    && dir_arg.as_slice() != b"."
                {
                    return Err(status(quoted(
                        "DEFER DIRECTORY:\n  ",
                        &dir_arg,
                        "\nis not known.  It may not have been processed yet.",
                    )));
                }
            }
            b"CALL" => {
                let Some(command) = next_arg(cx, &mut exp, &mut next_raw)? else {
                    return Err(status("DEFER CALL missing command name"));
                };
                if command.is_empty() {
                    return Err(status("DEFER CALL missing command name"));
                }
                if matches!(
                    command.to_ascii_lowercase().as_slice(),
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
                ) {
                    return Err(status(quoted(
                        "DEFER CALL command \"",
                        &command,
                        "\" is not allowed.",
                    )));
                }
                let call_id = id.unwrap_or_else(|| cx.defer_new_id());
                if let Some(var) = id_var {
                    cx.set_var(&var, call_id.clone());
                }
                let mut rest = Vec::new();
                let (line, span) = raw
                    .get(next_raw.saturating_sub(1))
                    .map(|a| (a.line, a.span.clone()))
                    .unwrap_or((0, 0..0));
                for extra in exp {
                    rest.push(rcmake_syntax::Argument {
                        kind: rcmake_syntax::ArgKind::Quoted,
                        text: extra,
                        line,
                        span: span.clone(),
                    });
                }
                if let Some(tail) = raw.get(next_raw..) {
                    rest.extend_from_slice(tail);
                }
                if !cx.defer_call(call_id, &command, &rest) {
                    return Err(status(
                        "DEFER CALL may not be scheduled in directory at this time.",
                    ));
                }
                return Ok(Flow::Normal);
            }
            b"GET_CALL_IDS" => {
                if id.is_some() || id_var.is_some() {
                    return Err(status("DEFER GET_CALL_IDS does not accept ID or ID_VAR"));
                }
                let Some(var) = next_arg(cx, &mut exp, &mut next_raw)? else {
                    return Err(status("DEFER GET_CALL_IDS missing output variable"));
                };
                if next_arg(cx, &mut exp, &mut next_raw)?.is_some() {
                    return Err(status("DEFER GET_CALL_IDS given too many arguments"));
                }
                let Some(ids) = cx.defer_get_call_ids() else {
                    return Err(status(
                        "DEFER GET_CALL_IDS may not be called in script mode.",
                    ));
                };
                cx.set_var(&var, rcmake_base::list::join(ids.iter()));
                return Ok(Flow::Normal);
            }
            b"GET_CALL" => {
                if id.is_some() || id_var.is_some() {
                    return Err(status("DEFER GET_CALL does not accept ID or ID_VAR"));
                }
                let Some(call_id) = next_arg(cx, &mut exp, &mut next_raw)? else {
                    return Err(status("DEFER GET_CALL missing id"));
                };
                let Some(var) = next_arg(cx, &mut exp, &mut next_raw)? else {
                    return Err(status("DEFER GET_CALL missing output variable"));
                };
                if next_arg(cx, &mut exp, &mut next_raw)?.is_some() {
                    return Err(status("DEFER GET_CALL given too many arguments"));
                }
                let Some(call) = cx.defer_get_call(&call_id) else {
                    return Err(status("DEFER GET_CALL may not be called in script mode."));
                };
                cx.set_var(&var, rcmake_base::list::join(call.iter()));
                return Ok(Flow::Normal);
            }
            b"CANCEL_CALL" => {
                if id.is_some() || id_var.is_some() {
                    return Err(status("DEFER CANCEL_CALL does not accept ID or ID_VAR"));
                }
                if !cx.defer_active() {
                    return Err(status(
                        "DEFER CANCEL_CALL may not be called in script mode.",
                    ));
                }
                while let Some(call_id) = next_arg(cx, &mut exp, &mut next_raw)? {
                    cx.defer_cancel_call(&call_id);
                }
                return Ok(Flow::Normal);
            }
            other => {
                return Err(status(quoted("DEFER operation \"", other, "\" unknown.")));
            }
        }
    }
    Err(status("DEFER requires at least one argument"))
}

fn exit(cx: &mut Cx<'_>, args: &[ExpandedArg]) -> CmdResult {
    if !args.is_empty() && cx.mode() != RunMode::Script {
        return Err(status("EXIT can be used only in SCRIPT mode"));
    }
    let [arg] = args else {
        return Err(status("EXIT requires one argument"));
    };
    let code = core::str::from_utf8(&arg.value)
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
                &arg.value,
                "\"",
            ))
        })?;
    // CMake passes the low 32 bits of the parsed integer to the exit function.
    cx.request_exit(code as i32);
    Ok(Flow::Normal)
}
