//! Typo suggestions for CMake names.

#![forbid(unsafe_code)]

use rcmake_base::BString;

use crate::annotate::Confidence;

const MAX_NAME_BYTES: usize = 128;
const MAX_SUGGESTIONS: usize = 3;

/// Naming rules for the list of candidates.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Namespace {
    /// Built-in and user-defined command names. Compare ASCII letters without case.
    Command,
    /// Case-sensitive keywords. Use only after the parser identifies a keyword error.
    Keyword,
    /// Other case-sensitive names, such as variables or targets.
    Name,
}

impl Namespace {
    fn distance_limit(self, length: usize) -> usize {
        match (self, length) {
            (_, 0..=2) | (Self::Keyword | Self::Name, 3) => 0,
            (Self::Command, 8..) => 2,
            _ => 1,
        }
    }

    fn eq(self, left: u8, right: u8) -> bool {
        match self {
            Self::Command => left.eq_ignore_ascii_case(&right),
            Self::Keyword | Self::Name => left == right,
        }
    }

    fn same_name(self, left: &[u8], right: &[u8]) -> bool {
        match self {
            Self::Command => left.eq_ignore_ascii_case(right),
            Self::Keyword | Self::Name => left == right,
        }
    }
}

/// Return a command suggestion only if it ranks ahead of all other candidates.
#[must_use]
pub fn best_match<I, T>(input: &[u8], candidates: I) -> Option<(BString, Confidence)>
where
    I: IntoIterator<Item = T>,
    T: AsRef<[u8]>,
{
    best_match_in(Namespace::Command, input, candidates)
}

/// Return the best candidate in `namespace` if its distance is uniquely lowest.
///
/// Select [`Namespace::Keyword`] only after a signature or target rule requires
/// a keyword. A valid library name can look like a misspelled keyword.
#[must_use]
pub fn best_match_in<I, T>(
    namespace: Namespace,
    input: &[u8],
    candidates: I,
) -> Option<(BString, Confidence)>
where
    I: IntoIterator<Item = T>,
    T: AsRef<[u8]>,
{
    let mut ranked = rank(namespace, input, candidates).into_iter();
    let (distance, name) = ranked.next()?;
    if ranked.next().is_some_and(|(next, _)| next == distance) {
        return None;
    }
    Some((name, confidence(distance, true)))
}

/// Return up to three command suggestions in order of increasing distance.
#[must_use]
pub fn suggest_candidates<I, T>(input: &[u8], candidates: I) -> Vec<(BString, Confidence)>
where
    I: IntoIterator<Item = T>,
    T: AsRef<[u8]>,
{
    suggest_candidates_in(Namespace::Command, input, candidates)
}

/// Return up to three distinct candidates ranked by distance, then by byte spelling.
///
/// Count an adjacent transposition as one edit. Names shorter than three bytes
/// require an exact match. Keywords and other names require at least four bytes
/// for a typo and permit one edit. Commands permit two edits from eight bytes.
/// Ignore empty names and names over 128 bytes. Only a candidate with a clear
/// lead and at most one edit is machine applicable.
#[must_use]
pub fn suggest_candidates_in<I, T>(
    namespace: Namespace,
    input: &[u8],
    candidates: I,
) -> Vec<(BString, Confidence)>
where
    I: IntoIterator<Item = T>,
    T: AsRef<[u8]>,
{
    let ranked = rank(namespace, input, candidates);
    let clear_lead = ranked
        .first()
        .is_some_and(|(best, _)| ranked.get(1).is_none_or(|(next, _)| best < next));
    ranked
        .into_iter()
        .enumerate()
        .map(|(index, (distance, name))| (name, confidence(distance, index == 0 && clear_lead)))
        .collect()
}

fn confidence(distance: usize, clear_lead: bool) -> Confidence {
    if clear_lead && distance <= 1 {
        Confidence::MachineApplicable
    } else {
        Confidence::MaybeIncorrect
    }
}

fn rank<I, T>(namespace: Namespace, input: &[u8], candidates: I) -> Vec<(usize, BString)>
where
    I: IntoIterator<Item = T>,
    T: AsRef<[u8]>,
{
    if input.is_empty() || input.len() > MAX_NAME_BYTES {
        return Vec::new();
    }

    let limit = namespace.distance_limit(input.len());
    let mut ranked: Vec<(usize, BString)> = Vec::with_capacity(MAX_SUGGESTIONS);
    for candidate in candidates {
        let name = candidate.as_ref();
        if name.is_empty()
            || name.len() > MAX_NAME_BYTES
            || input.len().abs_diff(name.len()) > limit
        {
            continue;
        }
        let distance = osa_distance(namespace, input, name);
        if distance > limit {
            continue;
        }
        if let Some(index) = ranked
            .iter()
            .position(|(_, ranked_name)| namespace.same_name(ranked_name.as_slice(), name))
        {
            let Some((_, previous)) = ranked.get(index) else {
                continue;
            };
            if previous.as_slice() <= name {
                continue;
            }
            ranked.remove(index);
        }
        let index = ranked.partition_point(|(d, n)| (*d, n.as_slice()) < (distance, name));
        if index < MAX_SUGGESTIONS {
            if ranked.len() == MAX_SUGGESTIONS {
                ranked.pop();
            }
            ranked.insert(index, (distance, BString::from(name)));
        }
    }
    ranked
}

fn osa_distance(namespace: Namespace, input: &[u8], name: &[u8]) -> usize {
    let mut older = [0; MAX_NAME_BYTES + 1];
    let mut previous = std::array::from_fn::<_, { MAX_NAME_BYTES + 1 }, _>(|column| column);
    let mut current = [0; MAX_NAME_BYTES + 1];
    for (row, &left) in input.iter().enumerate() {
        let mut deletion = row + 1;
        if let Some(first) = current.first_mut() {
            *first = deletion;
        }
        let steps = name
            .iter()
            .zip(previous.windows(2))
            .zip(current.iter_mut().skip(1));
        for (column, ((&right, pair), cell)) in steps.enumerate() {
            let [diagonal, above] = pair else {
                continue;
            };
            let substitution = diagonal + usize::from(!namespace.eq(left, right));
            let transposition = row
                .checked_sub(1)
                .and_then(|index| input.get(index))
                .zip(column.checked_sub(1).and_then(|index| name.get(index)))
                .filter(|&(&prior_left, &prior_right)| {
                    namespace.eq(left, prior_right) && namespace.eq(prior_left, right)
                })
                .and_then(|_| older.get(column.saturating_sub(1)))
                .map_or(usize::MAX, |&cost| cost + 1);
            *cell = (deletion + 1)
                .min(above + 1)
                .min(substitution)
                .min(transposition);
            deletion = *cell;
        }
        core::mem::swap(&mut older, &mut previous);
        core::mem::swap(&mut previous, &mut current);
    }
    previous.get(name.len()).copied().unwrap_or(usize::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_names_fold_case_and_rank_transpositions_as_one_edit() {
        assert_eq!(
            best_match(b"MeSSAge", ["message", "messages"]),
            Some(("message".into(), Confidence::MachineApplicable)),
        );
        assert_eq!(
            best_match(b"AdD_Execuatble", ["add_executable", "add_library"]),
            Some(("add_executable".into(), Confidence::MachineApplicable)),
        );
        assert_eq!(osa_distance(Namespace::Command, b"CA", b"ABC"), 3);
    }

    #[test]
    fn ties_return_candidates_without_an_automatic_choice() {
        let names = ["my_tool", "my_pool", "my_cool", "my_foo", "MY_COOL"];
        let expected: [(BString, Confidence); 3] = [
            ("MY_COOL".into(), Confidence::MaybeIncorrect),
            ("my_foo".into(), Confidence::MaybeIncorrect),
            ("my_pool".into(), Confidence::MaybeIncorrect),
        ];
        assert_eq!(suggest_candidates(b"my_fool", names), expected);
        assert_eq!(
            suggest_candidates(b"my_fool", names.into_iter().rev()),
            expected
        );
        assert!(best_match(b"my_fool", names).is_none());
    }

    #[test]
    fn required_keywords_use_case_sensitive_matching() {
        let keywords = ["INTERFACE", "PUBLIC", "PRIVATE"];
        assert_eq!(
            best_match_in(Namespace::Keyword, b"INTERFAEC", keywords),
            Some(("INTERFACE".into(), Confidence::MachineApplicable)),
        );
        assert!(best_match_in(Namespace::Keyword, b"interface", keywords).is_none());
        assert!(best_match_in(Namespace::Keyword, b"INTERFXXE", keywords).is_none());
        assert_eq!(
            best_match_in(Namespace::Name, b"PUBILC", ["public", "PUBILC"]),
            Some(("PUBILC".into(), Confidence::MachineApplicable)),
        );
    }

    #[test]
    fn tiny_unrelated_and_unbounded_names_do_not_match() {
        let long = vec![b'x'; MAX_NAME_BYTES + 1];
        assert!(best_match(&long, [&long]).is_none());
        assert!(best_match(b"x", [&long]).is_none());
        assert!(best_match(b"st", ["set"]).is_none());
        assert!(best_match(b"xx", ["yy"]).is_none());
        assert!(best_match(b"", [""]).is_none());
        assert!(best_match(b"CMAKE_GET_PROPERTY", ["get_property"]).is_none());
        assert_eq!(
            best_match(b"add_exectble", ["add_executable"]),
            Some(("add_executable".into(), Confidence::MaybeIncorrect)),
        );
    }
}
