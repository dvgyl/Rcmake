use rcmake_base::BString;
use rcmake_eval::{CmdResult, Cx, Flow, LogLevel};
use rcmake_expr::ExpandedArg;

use super::status;

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
        _ => Err(status("called with unknown meta-operation")),
    }
}
