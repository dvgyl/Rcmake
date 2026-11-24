//! Check typo suggestions and terminal cell layout.

use std::sync::Arc;

use rcmake_diag::{
    Diagnostic, Effect, Location, MessageKind, SourceMap,
    annotate::{Annotations, Confidence, SourceSpan},
    rich::{RichOptions, render_rich},
    source::SourceKind,
    suggest::{Namespace, best_match_in, suggest_candidates_in},
};

use super::{Check, mutate, pick, random_bytes, require};
use crate::Rng;

const MAX_NAME: usize = 128;

pub(super) fn check(rng: &mut Rng) -> Check {
    check_suggestions(rng)?;
    check_rich(rng)?;
    Ok(true)
}

fn check_suggestions(rng: &mut Rng) -> Result<(), String> {
    let mode = rng.below(8);
    let (namespace, input, mut candidates) = match mode {
        0 => (
            Namespace::Command,
            b"add_execuatble".to_vec(),
            [
                b"add_executable".as_slice(),
                b"ADD_EXECUTABLE",
                b"add_library",
            ]
            .map(<[u8]>::to_vec)
            .to_vec(),
        ),
        1 => (
            Namespace::Name,
            b"result0".to_vec(),
            [b"result4".as_slice(), b"result2", b"result3", b"result1"]
                .map(<[u8]>::to_vec)
                .to_vec(),
        ),
        _ => {
            let namespace = *pick(
                rng,
                &[Namespace::Command, Namespace::Keyword, Namespace::Name],
            );
            let length = *pick(
                rng,
                &[0, 1, 2, 3, 4, 7, 8, MAX_NAME - 1, MAX_NAME, MAX_NAME + 1],
            );
            let input = rng.bytes(length);
            let mut candidates = vec![input.clone(), input.to_ascii_uppercase(), random_bytes(rng)];
            for _ in 0..rng.below(16) {
                let mut candidate = input.clone();
                mutate(rng, &mut candidate, b"\xffX");
                candidates.push(candidate);
            }
            (namespace, input, candidates)
        }
    };
    let ranked = suggest_candidates_in(namespace, &input, &candidates);
    let best = best_match_in(namespace, &input, &candidates);
    require(
        ranked.len() <= 3
            && (ranked.is_empty() || (1..=MAX_NAME).contains(&input.len()))
            && ranked.iter().all(|(name, _)| {
                (1..=MAX_NAME).contains(&name.len())
                    && candidates
                        .iter()
                        .any(|candidate| candidate.as_slice() == name.as_slice())
            })
            && best
                .as_ref()
                .is_none_or(|name| ranked.first() == Some(name)),
        || "Suggestion ranking returned an invalid candidate.".into(),
    )?;
    for (index, (name, confidence)) in ranked.iter().enumerate() {
        require(
            (*confidence != Confidence::MachineApplicable || best.as_ref() == ranked.get(index))
                && ranked.iter().take(index).all(|(previous, _)| {
                    if namespace == Namespace::Command {
                        !name.as_slice().eq_ignore_ascii_case(previous.as_slice())
                    } else {
                        name != previous
                    }
                }),
            || "Suggestion ranking returned a duplicate or an unsafe automatic fix.".into(),
        )?;
    }
    candidates.reverse();
    require(
        ranked == suggest_candidates_in(namespace, &input, &candidates)
            && best == best_match_in(namespace, &input, &candidates),
        || "Changing candidate order changed the suggestions.".into(),
    )?;
    match mode {
        0 => require(
            best == Some(("ADD_EXECUTABLE".into(), Confidence::MachineApplicable)),
            || "A transposed command name did not get the expected fix.".into(),
        ),
        1 => require(
            best.is_none()
                && ranked.iter().map(|(name, _)| name.as_slice()).eq([
                    b"result1".as_slice(),
                    b"result2",
                    b"result3",
                ]),
            || "Tied names did not get the expected byte order.".into(),
        ),
        _ => Ok(()),
    }
}

fn check_rich(rng: &mut Rng) -> Result<(), String> {
    let mode = rng.below(8);
    let input = match mode {
        0 => "\t界e\u{301}x\r\n".as_bytes().to_vec(),
        1 => random_bytes(rng),
        _ => {
            let mut input = Vec::new();
            for _ in 0..rng.below(384) {
                input.extend_from_slice(pick(
                    rng,
                    &[
                        b"\t".as_slice(),
                        b"\r\n",
                        b"\xff\x1b\x00",
                        "界".as_bytes(),
                        "e\u{301}".as_bytes(),
                        "\u{301}\u{301}".as_bytes(),
                        "👩\u{200d}💻".as_bytes(),
                        "🇺🇳".as_bytes(),
                        "1\u{fe0f}\u{20e3}".as_bytes(),
                    ],
                ));
            }
            if mode == 2 {
                input.extend_from_slice("界".repeat(512).as_bytes());
                input.extend_from_slice("\u{301}".repeat(512).as_bytes());
            }
            mutate(rng, &mut input, b"\xff\r\n");
            input
        }
    };
    let length = input.len() as u32;
    let mut sources = SourceMap::default();
    let source = sources
        .add("fuzz.cmake".into(), SourceKind::File, Arc::from(input))
        .ok_or("Cannot register the diagnostic source.")?;
    let (start, end) = match mode {
        0 => (6, 7),
        2 => (length.saturating_sub(768), length),
        3 => (u32::MAX, 0),
        _ => (
            rng.below(u64::from(length) + 2) as u32,
            rng.below(u64::from(length) + 2) as u32,
        ),
    };
    let annotations = Annotations::primary(SourceSpan { source, start, end }, "glyph");
    let diagnostic = Diagnostic {
        kind: MessageKind::Error,
        effect: Effect::Fatal,
        category: None,
        policy: None,
        location: Location::None,
        body: random_bytes(rng).into(),
        annotations: None,
    };
    let width = if mode == 0 {
        100
    } else {
        *pick(rng, &[0, 20, 21, 31, 80, 4_096, usize::MAX])
    };
    let options = RichOptions {
        color: false,
        terminal_width: width,
        unicode: mode % 2 != 0,
    };
    let rendered = render_rich(&diagnostic, Some(&annotations), &sources, options);
    let text = std::str::from_utf8(rendered.as_slice())
        .map_err(|error| format!("The renderer returned invalid UTF-8. {error}"))?;
    require(
        text.ends_with('\n')
            && text
                .chars()
                .all(|ch| !ch.is_control() || matches!(ch, '\n' | '\t')),
        || "The renderer returned unexpected terminal control bytes.".into(),
    )?;
    if mode == 0 {
        require(
            text.contains("1 |     界e\u{301}x\n  |       ^ glyph\n"),
            || "A tab, wide character, or combining mark changed the marker column.".into(),
        )?;
    }
    let clamped = RichOptions {
        terminal_width: width.clamp(20, 4_096),
        ..options
    };
    require(
        rendered == render_rich(&diagnostic, Some(&annotations), &sources, clamped),
        || "Clamping the terminal width changed the diagnostic.".into(),
    )
}
