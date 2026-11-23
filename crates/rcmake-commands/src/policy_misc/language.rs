use rcmake_base::BString;
use rcmake_eval::{CmdResult, Cx, Flow, LogLevel, RunMode};
use rcmake_expr::ExpandedArg;

use super::{quoted, status};

pub(super) fn language(cx: &mut Cx<'_>, args: &[ExpandedArg]) -> CmdResult {
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
                return Err(status("called without CODE argument"));
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
        _ => Err(status("called with unknown meta-operation")),
    }
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
