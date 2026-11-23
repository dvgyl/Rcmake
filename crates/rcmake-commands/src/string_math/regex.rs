use rcmake_base::ByteSlice;
use rcmake_diag::{PolicyId, PolicyStatus};
use rcmake_eval::{CmdResult, Cx};
use rcmake_host::Stream;
use rcmake_regex::{Captures, MAX_CAPTURE_SLOTS, MatchOptions, expand_replacement};

use super::{append, concat, set, status};

pub(super) fn command(cx: &mut Cx<'_>, args: &[&[u8]]) -> CmdResult {
    let Some((&mode, args)) = args.split_first() else {
        return Err(status("sub-command REGEX requires a mode to be specified."));
    };
    let mode_name = mode.to_str_lossy();
    let arity = |total| {
        status(format!(
            "sub-command REGEX, mode {mode_name} needs at least {total} arguments total to command."
        ))
    };
    let (pattern, replacement, out, inputs) = match (mode, args) {
        (b"QUOTE", [out, inputs @ ..]) if !inputs.is_empty() => return quote(cx, out, inputs),
        (b"MATCH" | b"MATCHALL", [pattern, out, inputs @ ..]) if !inputs.is_empty() => {
            (*pattern, None, *out, inputs)
        }
        (b"REPLACE", [pattern, replacement, out, inputs @ ..]) if !inputs.is_empty() => {
            (*pattern, Some(*replacement), *out, inputs)
        }
        (b"QUOTE", _) => return Err(arity(4)),
        (b"MATCH" | b"MATCHALL", _) => return Err(arity(5)),
        (b"REPLACE", _) => return Err(arity(6)),
        _ => {
            return Err(status(format!(
                "sub-command REGEX does not recognize mode {mode_name}"
            )));
        }
    };
    let pattern_name = pattern.to_str_lossy();

    let regex = cx.regex(pattern).map_err(|error| {
        cx.host().write_out(
            Stream::Stdout,
            format!(
                "{}\nRegularExpression::compile(): Error in compile.\n",
                error.message
            )
            .as_bytes(),
        );
        status(format!(
            "sub-command REGEX, mode {mode_name}: Failed to compile regex \"{pattern_name}\"."
        ))
    });
    if let Some(template) = replacement {
        // Validate escapes even if the input has no matches. Check group numbers on use.
        let empty = Captures {
            groups: [None; MAX_CAPTURE_SLOTS],
        };
        expand_replacement(template, b"", &empty, 9).map_err(replacement_error)?;
    }
    let regex = regex?;
    let input = concat(cx, inputs)?;
    let mut options = MatchOptions {
        bol_at_start: cx.policy(PolicyId::new(186)) != PolicyStatus::New,
        allow_empty_at_start: true,
    };
    let mut budget = cx.limits().regex_steps;
    let mut output = Vec::new();
    let mut end = 0;
    let mut matched = false;
    cx.set_match_vars(None);
    while let Some(caps) = regex
        .find_at(&input, end, options, &mut budget)
        .map_err(|error| status(error.to_string()))?
    {
        let Some(&Some((start, next))) = caps.groups.first() else {
            break;
        };
        cx.set_match_vars(Some((&caps, &input)));
        if let Some(template) = replacement {
            append(cx, &mut output, input.get(end..start).unwrap_or_default())?;
            let expanded = expand_replacement(template, &input, &caps, regex.group_count())
                .map_err(|error| {
                    if error.message.contains("contains an out-of-range escape") {
                        status(format!("sub-command REGEX, mode REPLACE: replace expression \"{}\" contains an out-of-range escape for regex \"{pattern_name}\".", template.to_str_lossy()))
                    } else {
                        replacement_error(error)
                    }
                })?;
            append(cx, &mut output, &expanded)?;
        } else {
            if matched {
                append(cx, &mut output, b";")?;
            }
            append(cx, &mut output, input.get(start..next).unwrap_or_default())?;
            if mode == b"MATCH" {
                break;
            }
        }
        matched = true;
        end = next;
        options.allow_empty_at_start = start != next;
    }
    if replacement.is_some() {
        append(cx, &mut output, input.get(end..).unwrap_or_default())?;
    }
    set(cx, out, output)
}

fn replacement_error(error: rcmake_regex::RegexError) -> rcmake_eval::CmdError {
    status(
        error
            .message
            .strip_prefix("string ")
            .unwrap_or(&error.message),
    )
}

fn quote(cx: &mut Cx<'_>, out: &[u8], inputs: &[&[u8]]) -> CmdResult {
    let mut result = Vec::new();
    for &input in inputs {
        for &byte in input {
            if b".+*?^$()[]|\\".contains(&byte) {
                append(cx, &mut result, b"\\")?;
            }
            append(cx, &mut result, &[byte])?;
        }
    }
    set(cx, out, result)
}
