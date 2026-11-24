use rcmake_base::ByteSlice;
use rcmake_diag::{PolicyId, PolicyStatus};
use rcmake_eval::{CmdResult, Cx};
use rcmake_host::Stream;
use rcmake_regex::{Captures, MATCH_VAR_SLOTS, MatchOptions, Replacement, Scratch};

use super::{append, concat, set, status};

const MATCH_VARS: [&[u8]; MATCH_VAR_SLOTS] = [
    b"CMAKE_MATCH_0",
    b"CMAKE_MATCH_1",
    b"CMAKE_MATCH_2",
    b"CMAKE_MATCH_3",
    b"CMAKE_MATCH_4",
    b"CMAKE_MATCH_5",
    b"CMAKE_MATCH_6",
    b"CMAKE_MATCH_7",
    b"CMAKE_MATCH_8",
    b"CMAKE_MATCH_9",
];

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
    // Validate escapes even if the input has no matches. Check group numbers on use.
    let parsed = replacement
        .map(Replacement::parse)
        .transpose()
        .map_err(replacement_error)?;
    let regex = regex?;
    let input = concat(cx, inputs)?;
    let mut options = MatchOptions {
        bol_at_start: cx.policy(PolicyId::new(186)) != PolicyStatus::New,
        allow_empty_at_start: true,
    };
    let mut budget = cx.limits().regex_steps;
    let mut output = Vec::new();
    let mut end = 0;
    let mut scratch = Scratch::default();
    let mut expanded = Vec::new();
    let mut matched = false;
    let mut last = None;
    let mut seen = 0;
    let defer_matches = mode != b"MATCH"
        && !MATCH_VARS
            .iter()
            .copied()
            .chain([b"CMAKE_MATCH_COUNT".as_slice()])
            .any(|name| !cx.variable_watches(name).is_empty());
    cx.set_match_vars(None);
    let result = (|| {
        while let Some(caps) = regex
            .find_at_with_scratch(&input, end, options, &mut budget, &mut scratch)
            .map_err(|error| status(error.to_string()))?
        {
            let Some(&Some((start, next))) = caps.groups.first() else {
                break;
            };
            let caps = if defer_matches {
                seen |= nonempty_slots(&caps);
                last.insert(caps)
            } else {
                cx.set_match_vars(Some((&caps, &input)));
                &caps
            };
            if let (Some(parsed), Some(template)) = (&parsed, replacement) {
                append(cx, &mut output, input.get(end..start).unwrap_or_default())?;
                parsed.expand_into(&input, caps, regex.group_count(), &mut expanded)
                    .map_err(|error| {
                        if error.message.contains("contains an out-of-range escape") {
                            status(format!(r#"sub-command REGEX, mode REPLACE: replace expression "{}" contains an out-of-range escape for regex "{pattern_name}"."#, template.to_str_lossy()))
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
        Ok(())
    })();
    if let Some(caps) = &last {
        cx.set_match_vars(Some((caps, &input)));
        // A group captured earlier can become empty in the final match.
        let cleared = seen & !nonempty_slots(caps);
        for (slot, name) in MATCH_VARS.iter().enumerate() {
            if cleared & (1 << slot) != 0 {
                cx.set_var(name, Default::default());
            }
        }
    }
    result?;
    if replacement.is_some() {
        append(cx, &mut output, input.get(end..).unwrap_or_default())?;
    }
    set(cx, out, output)
}

fn nonempty_slots(caps: &Captures) -> u16 {
    caps.groups
        .iter()
        .take(MATCH_VAR_SLOTS)
        .enumerate()
        .fold(0, |slots, (index, span)| {
            if span.is_some_and(|(start, end)| start != end) {
                slots | (1 << index)
            } else {
                slots
            }
        })
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
