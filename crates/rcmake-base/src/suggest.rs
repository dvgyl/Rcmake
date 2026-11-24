//! Edit distance and typo suggestions for byte strings.

use crate::BString;

/// Levenshtein edit distance over bytes (`cmLevenshteinDistance`).
#[must_use]
pub fn levenshtein(a: &[u8], b: &[u8]) -> usize {
    if a == b {
        return 0;
    }
    if a.is_empty() {
        return b.len();
    }
    if b.is_empty() {
        return a.len();
    }
    let mut previous: Vec<usize> = (0..=b.len()).collect();
    let mut current = vec![0usize; b.len() + 1];
    for (i, &ca) in a.iter().enumerate() {
        if let Some(first) = current.first_mut() {
            *first = i + 1;
        }
        for (j, &cb) in b.iter().enumerate() {
            let cost = usize::from(ca != cb);
            let del = previous
                .get(j + 1)
                .copied()
                .unwrap_or(usize::MAX)
                .saturating_add(1);
            let ins = current
                .get(j)
                .copied()
                .unwrap_or(usize::MAX)
                .saturating_add(1);
            let sub = previous
                .get(j)
                .copied()
                .unwrap_or(usize::MAX)
                .saturating_add(cost);
            if let Some(slot) = current.get_mut(j + 1) {
                *slot = del.min(ins).min(sub);
            }
        }
        core::mem::swap(&mut previous, &mut current);
    }
    previous.last().copied().unwrap_or(0)
}

/// Maximum edit distance for a suggestion.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Threshold {
    /// Use the CMake command threshold, which depends on the input length.
    CmakeCommand,
    /// Use this maximum distance for all input lengths.
    Fixed(usize),
}

/// Return the first closest candidate within the specified edit distance.
#[must_use]
pub fn closest<'a>(
    input: &[u8],
    candidates: &'a [BString],
    threshold: Threshold,
) -> Option<&'a BString> {
    let mut best: Option<(&BString, usize)> = None;
    for cand in candidates {
        let d = levenshtein(input, cand);
        if best.is_none_or(|(_, bd)| d < bd) {
            best = Some((cand, d));
        }
    }
    let (cand, dist) = best?;
    let max = match threshold {
        Threshold::CmakeCommand => (input.len() / 5 + 1).clamp(1, 4),
        Threshold::Fixed(max) => max,
    };
    (dist <= max).then_some(cand)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn closest_command_threshold() {
        let names = vec![BString::from("message"), BString::from("set")];
        assert_eq!(
            closest(b"mesage", &names, Threshold::CmakeCommand),
            Some(&names[0])
        );
        assert_eq!(
            closest(b"foo_unknown", &names, Threshold::CmakeCommand),
            None
        );
        assert_eq!(levenshtein(b"kitten", b"sitting"), 3);
    }
}
