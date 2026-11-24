//! Check syntax recovery and byte offsets in editor buffers.

use std::{ops::Range, sync::Arc};

use rcmake_diag::{SourceMap, source::SourceKind};
use rcmake_syntax::{Argument, parse_tolerant};

use super::{Check, mutate, pick, preview, random_bytes, require};
use crate::Rng;

const OPEN_BRACKET: &[u8] = b"set(x [=[open\n";
const RECOVERED: &[u8] = b"message(recovered)\n";
const OVER_LIMIT_REPEATS: usize = 8_192;

pub(super) fn check(rng: &mut Rng) -> Check {
    let mode = rng.below(8);
    let mut input = match mode {
        0 => random_bytes(rng),
        1 => b"\xef\xbb\xbfIF(ON)\r\nset(x [==[\r\nvalue]==] \"a;b\")\r\nendif()\r\n".to_vec(),
        2 => b"IF(ON)\nset(x [=[open\nmessage(recovered)\n".to_vec(),
        // Each open bracket can hide the commands that follow it.
        // The largest case exceeds the parenthesis nesting limit.
        3 => {
            let count = *pick(rng, &[128, 512, 2_048, OVER_LIMIT_REPEATS]);
            let mut input = OPEN_BRACKET.repeat(count);
            input.extend_from_slice(RECOVERED);
            input
        }
        _ => {
            let mut input = b"\xef\xbb\xbf".to_vec();
            for _ in 0..rng.below(64) {
                input.extend_from_slice(pick(
                    rng,
                    &[
                        b"if((ON))\r\n".as_slice(),
                        b"foreach(x IN ITEMS a b)\n",
                        b"endwhile()\n",
                        b"set(x \"open\n",
                        b"set(x [=[open\n",
                        b"#[==[comment\n",
                        b"literal ]=] ]==] \\\n",
                        b"\xff)\n",
                        b"message(ok)\n",
                    ],
                ));
            }
            input
        }
    };
    if mode >= 4 {
        mutate(rng, &mut input, b"\x00\xff\r\n#[=[");
    }
    let input: Arc<[u8]> = input.into();
    let source = SourceMap::default()
        .add("fuzz.cmake".into(), SourceKind::File, Arc::clone(&input))
        .ok_or("Cannot register the editor buffer.")?;
    let parsed = parse_tolerant(source, &input);
    require(parsed.source == source, || {
        "Recovery changed the source identifier.".into()
    })?;
    let complete = parsed.commands.iter().map(|command| &command.span);
    let partial = parsed.partial_commands.iter().map(|command| &command.span);
    require(ordered_within(complete, &(0..input.len())), || {
        format!("Invalid complete command spans. Input {}.", preview(&input))
    })?;
    require(ordered_within(partial, &(0..input.len())), || {
        format!("Invalid partial command spans. Input {}.", preview(&input))
    })?;
    for command in &parsed.commands {
        check_arguments(&command.span, &command.args)?;
    }
    for command in &parsed.partial_commands {
        if let Some(span) = &command.parenthesis_span {
            require(ordered_within([span], &command.span), || {
                format!("Parenthesis span {span:?} is outside {:?}.", command.span)
            })?;
            check_arguments(span, &command.args)?;
        } else {
            require(command.args.is_empty(), || {
                "A command without an opening parenthesis has arguments.".into()
            })?;
        }
    }
    require(
        parsed.unclosed_blocks.is_sorted_by(|a, b| a < b)
            && parsed.unclosed_blocks.iter().all(|&index| {
                parsed.commands.get(index).is_some_and(|command| {
                    matches!(
                        command.lower.as_slice(),
                        b"if" | b"foreach" | b"while" | b"function" | b"macro" | b"block"
                    )
                })
            }),
        || "Recovery returned invalid block indices.".into(),
    )?;
    if mode == 1 {
        require(
            parsed.commands.len() == 3
                && parsed.partial_commands.is_empty()
                && parsed.unclosed_blocks.is_empty()
                && parsed.errors.is_empty(),
            || "Recovery changed a valid listfile.".into(),
        )?;
    }
    if matches!(mode, 2 | 3) {
        let expected_partials = if mode == 2 {
            1
        } else {
            (input.len() - RECOVERED.len()) / OPEN_BRACKET.len()
        };
        if expected_partials == OVER_LIMIT_REPEATS {
            require(
                parsed.commands.is_empty()
                    && parsed.partial_commands.is_empty()
                    && matches!(parsed.errors.as_slice(), [error]
                        if error.message.as_slice()
                            == b"Listfile exceeds the editor parenthesis nesting limit."),
                || "Recovery did not reject input above the parenthesis nesting limit.".into(),
            )?;
            return Ok(false);
        }
        require(
            parsed
                .commands
                .last()
                .is_some_and(|command| command.name.as_slice() == b"message")
                && parsed.partial_commands.len() == expected_partials
                && !parsed.errors.is_empty(),
            || "Recovery missed a partial command or the command after it.".into(),
        )?;
    }
    Ok(parsed.errors.is_empty())
}

fn check_arguments(parent: &Range<usize>, args: &[Argument]) -> Result<(), String> {
    require(
        ordered_within(args.iter().map(|arg| &arg.span), parent),
        || format!("Invalid argument spans in {parent:?}."),
    )
}

fn ordered_within<'a>(
    spans: impl IntoIterator<Item = &'a Range<usize>>,
    parent: &Range<usize>,
) -> bool {
    let mut previous_end = parent.start;
    spans.into_iter().all(|span| {
        let valid = previous_end <= span.start && span.start <= span.end && span.end <= parent.end;
        previous_end = span.end;
        valid
    })
}
