//! Typo suggestions for CMake names.

#![forbid(unsafe_code)]

use rcmake_base::{BString, suggest::levenshtein};

use crate::annotate::Confidence;

const MAX_NAME_BYTES: usize = 128;
const MAX_SUGGESTIONS: usize = 3;

/// Return the first result from [suggest_candidates], if one exists.
#[must_use]
pub fn best_match<I, T>(input: &[u8], candidates: I) -> Option<(BString, Confidence)>
where
    I: IntoIterator<Item = T>,
    T: AsRef<[u8]>,
{
    suggest_candidates(input, candidates).into_iter().next()
}

/// Return up to three distinct candidate names and their confidence.
///
/// Compare bytes with ASCII case folding. Rank by Levenshtein distance, then
/// by the original bytes. The distance limit is one third of the input length,
/// rounded down, with a minimum of one. Skip empty names and names over 128 bytes.
/// Mark a unique case-insensitive match or a sole candidate at distance one as
/// machine applicable.
#[must_use]
pub fn suggest_candidates<I, T>(input: &[u8], candidates: I) -> Vec<(BString, Confidence)>
where
    I: IntoIterator<Item = T>,
    T: AsRef<[u8]>,
{
    if input.is_empty() || input.len() > MAX_NAME_BYTES {
        return Vec::new();
    }

    let limit = (input.len() / 3).max(1);
    let folded_input = input.to_ascii_lowercase();
    let mut ranked: Vec<(usize, BString)> = Vec::with_capacity(MAX_SUGGESTIONS);
    for candidate in candidates {
        let name = candidate.as_ref();
        if name.is_empty()
            || name.len() > MAX_NAME_BYTES
            || input.len().abs_diff(name.len()) > limit
        {
            continue;
        }
        let distance = levenshtein(&folded_input, &name.to_ascii_lowercase());
        if distance <= limit
            && let Err(index) =
                ranked.binary_search_by(|(d, n)| (*d, n.as_slice()).cmp(&(distance, name)))
            && index < MAX_SUGGESTIONS
        {
            if ranked.len() == MAX_SUGGESTIONS {
                ranked.pop();
            }
            ranked.insert(index, (distance, BString::from(name)));
        }
    }

    let unique = ranked.len() == 1;
    let exact_matches = ranked
        .iter()
        .take_while(|(distance, _)| *distance == 0)
        .count();
    ranked
        .into_iter()
        .map(|(distance, name)| {
            let confidence = if (distance == 0 && exact_matches == 1) || (distance == 1 && unique) {
                Confidence::MachineApplicable
            } else {
                Confidence::MaybeIncorrect
            };
            (name, confidence)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ranks_case_insensitive_matches_before_edits() {
        assert_eq!(
            suggest_candidates(b"MESSAGE", ["messages", "message", "mesage"]),
            [
                (BString::from("message"), Confidence::MachineApplicable),
                (BString::from("mesage"), Confidence::MaybeIncorrect),
                (BString::from("messages"), Confidence::MaybeIncorrect),
            ],
        );
        assert_eq!(
            best_match(b"MESAGE", ["message", "set"]),
            Some(("message".into(), Confidence::MachineApplicable)),
        );
    }

    #[test]
    fn ties_are_bytewise_distinct_and_limited() {
        let names = ["set", "bet", "GET", "let", "GET", "met"];
        let expected: [(BString, Confidence); 3] = [
            ("GET".into(), Confidence::MaybeIncorrect),
            ("bet".into(), Confidence::MaybeIncorrect),
            ("let".into(), Confidence::MaybeIncorrect),
        ];
        assert_eq!(suggest_candidates(b"net", names), expected);
        assert_eq!(
            suggest_candidates(b"net", names.into_iter().rev()),
            expected
        );
        assert_eq!(
            best_match(b"get", ["Get", "GET", "Get"]),
            Some(("GET".into(), Confidence::MaybeIncorrect)),
        );
    }

    #[test]
    fn large_and_distant_names_do_not_match() {
        let long = vec![b'x'; MAX_NAME_BYTES + 1];
        assert!(best_match(&long, [&long]).is_none());
        assert!(best_match(b"x", [&long]).is_none());
        assert!(best_match(b"xx", ["yy"]).is_none());
        assert!(best_match(b"", [""]).is_none());
        assert_eq!(
            best_match(b"ab\xffdef", [b"AB\xffXEZ".as_slice()]),
            Some((b"AB\xffXEZ".as_slice().into(), Confidence::MaybeIncorrect)),
        );
    }
}
