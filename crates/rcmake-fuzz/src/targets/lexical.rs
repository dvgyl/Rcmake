use rcmake_base::{
    Limits,
    list::{self, Empty},
    path_lex::Lexical,
};
use rcmake_regex::{MatchOptions, Regex};

use super::{Check, mutate, pick, preview, random_bytes, require};
use crate::Rng;

pub(super) fn regex(rng: &mut Rng, limits: &Limits) -> Check {
    let mode = rng.below(8);
    let mut pattern = b"^".to_vec();
    let mut hay = Vec::new();
    for _ in 0..rng.below(16) {
        let (atom, sample) = *pick(
            rng,
            &[
                ("a", "a"),
                ("[a-c]", "b"),
                ("(x|y)", "y"),
                ("[^z]", "x"),
                (".", "_"),
                ("ab*", "abbb"),
                ("c?", ""),
            ],
        );
        pattern.extend_from_slice(atom.as_bytes());
        hay.extend_from_slice(sample.as_bytes());
    }
    pattern.push(b'$');
    match mode {
        0 => {
            pattern = random_bytes(rng);
            hay = random_bytes(rng);
        }
        1 => mutate(rng, &mut pattern, b"\\[)-\xff"),
        2 => pattern = format!("{}x{}", "(".repeat(32), ")".repeat(32)).into_bytes(),
        3 => {
            pattern = b"^(a|aa)+b$".to_vec();
            hay = vec![b'a'; 16 + rng.below(16) as usize];
        }
        _ => {}
    }
    let compiled = match Regex::new(&pattern) {
        Ok(regex) => {
            require(mode != 2, || {
                "Regex accepted more than 31 capture groups.".into()
            })?;
            regex
        }
        Err(error) => {
            require(mode < 4, || {
                format!(
                    "Cannot compile generated regex. {error} Pattern {}.",
                    preview(&pattern)
                )
            })?;
            return Ok(false);
        }
    };
    let options = MatchOptions {
        bol_at_start: rng.below(2) == 0,
        allow_empty_at_start: true,
    };
    let budget = if mode == 3 && rng.below(256) != 0 {
        1 + rng.below(128)
    } else {
        limits.regex_steps
    };
    let matched = search(&compiled, &hay, options, budget)?;
    require(mode < 4 || matched == Some(true), || {
        format!(
            "Regex missed its generated match. Pattern {}. Text {}.",
            preview(&pattern),
            preview(&hay)
        )
    })?;
    Ok(matched.is_some())
}

fn search(
    regex: &Regex,
    hay: &[u8],
    options: MatchOptions,
    mut budget: u64,
) -> Result<Option<bool>, String> {
    let initial = budget;
    let result = regex.find_at(hay, 0, options, &mut budget);
    require(budget <= initial, || {
        "Regex search increased the step budget.".into()
    })?;
    match result {
        Ok(Some(captures)) => {
            require(
                captures
                    .groups
                    .into_iter()
                    .flatten()
                    .all(|(start, end)| start <= end && end <= hay.len()),
                || {
                    format!(
                        "Regex returned an invalid capture. Text {}. {captures:?}",
                        preview(hay)
                    )
                },
            )?;
            Ok(Some(true))
        }
        Ok(None) => Ok(Some(false)),
        Err(_) => {
            require(budget == 0, || {
                "Regex reported its step limit before it used all steps.".into()
            })?;
            Ok(None)
        }
    }
}

pub(super) fn path_list(rng: &mut Rng, limits: &Limits) -> Check {
    let mut path = (*pick(rng, &["", "/", "///", "./", "../", "C:/", "//server/"]))
        .as_bytes()
        .to_vec();
    for _ in 0..rng.below(16) {
        let component = *pick(
            rng,
            &[
                b".".as_slice(),
                b"..",
                b"file.ext",
                b"file;list",
                b"[]",
                b"\xff",
                b"",
            ],
        );
        path.extend_from_slice(component);
        path.extend_from_slice(pick(rng, &[b"/".as_slice(), b"//"]));
    }
    if rng.below(4) == 0 {
        let bytes = random_bytes(rng);
        mutate(rng, &mut path, &bytes);
    }
    let normalized = Lexical::new(&path).normalize();
    let again = Lexical::new(&normalized).normalize();
    require(normalized == again, || {
        format!(
            "Path normalization changed on the second pass. Input {}. First {}. Second {}.",
            preview(&path),
            preview(&normalized),
            preview(&again),
        )
    })?;
    require(normalized.len() <= limits.max_value_bytes, || {
        "Normalized path exceeds the size limit.".into()
    })?;

    let mut literal = Vec::new();
    for _ in 0..rng.below(16) {
        literal.extend_from_slice(pick(
            rng,
            &[
                b";".as_slice(),
                br"\;",
                b"[;",
                b"];",
                &path,
                b"\0",
                b"\xff",
                b"name",
            ],
        ));
    }
    let kept = list::split(&literal, Empty::Keep);
    let dropped = list::split(&literal, Empty::Drop);
    require(
        kept.into_iter().filter(|item| !item.is_empty()).eq(dropped),
        || {
            format!(
                "List split changed a non-empty element. Input {}.",
                preview(&literal)
            )
        },
    )?;
    let elements: Vec<_> = (0..rng.below(8))
        .map(|_| *pick(rng, &[b"a".as_slice(), b"b", b"[c;d]", b"\xff", b"xy"]))
        .collect();
    require(
        list::split(&list::join(&elements), Empty::Keep)
            .iter()
            .map(|s| s.as_slice())
            .eq(elements),
        || "List join changed an element that can be split.".into(),
    )?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exhausts_the_fuzz_step_budget() -> Result<(), String> {
        let regex = crate::targets::setup(Regex::new(b"^(a|aa)+b$"))?;
        let mut budget = Limits::fuzz().regex_steps;
        let result = regex.find_at(&[b'a'; 32], 0, MatchOptions::default(), &mut budget);
        assert!(result.is_err());
        assert_eq!(budget, 0);
        Ok(())
    }
}
