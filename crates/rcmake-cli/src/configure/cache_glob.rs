//! Cache-entry name patterns for `-U`.
//!
//! CMake converts the `-U` argument with `cmsys::Glob::PatternToRegex` and
//! matches the whole entry name. `*` and `?` do not match `/`. A bracket
//! expression is a set of bytes, ranges, and an optional `!` or `^` negation.
//! A `]` directly after the opening bracket (or after the negation) is a
//! literal member. A `[` without a closing `]` is a literal byte. All other
//! bytes match themselves, with exact case.

/// One element of a compiled pattern.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Token {
    /// A byte that must match exactly.
    Literal(u8),
    /// `?`: any byte except `/`.
    One,
    /// `*`: zero or more bytes, none of them `/`.
    Run,
    /// `[...]`: a set of inclusive byte ranges.
    Class {
        negated: bool,
        ranges: Vec<(u8, u8)>,
    },
}

/// A compiled `-U` pattern.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CacheGlob {
    /// `None` when the regex conversion gives an invalid range. CMake then
    /// cannot compile the expression, and no entry matches.
    tokens: Option<Vec<Token>>,
}

impl CacheGlob {
    /// Compile a pattern.
    pub(crate) fn new(pattern: &[u8]) -> Self {
        let mut tokens = Vec::with_capacity(pattern.len());
        let mut index = 0;
        while let Some(&byte) = pattern.get(index) {
            index += 1;
            match byte {
                b'*' => {
                    // Adjacent runs match the same names as one run.
                    if tokens.last() != Some(&Token::Run) {
                        tokens.push(Token::Run);
                    }
                }
                b'?' => tokens.push(Token::One),
                b'[' => match class(pattern, index) {
                    Some((Some(token), next)) => {
                        tokens.push(token);
                        index = next;
                    }
                    Some((None, _)) => return Self { tokens: None },
                    None => tokens.push(Token::Literal(b'[')),
                },
                _ => tokens.push(Token::Literal(byte)),
            }
        }
        Self {
            tokens: Some(tokens),
        }
    }

    /// Return `true` when the pattern matches the complete name.
    ///
    /// The matcher is a dynamic program over the name and the tokens. Its
    /// cost is bounded by the product of the two lengths, without
    /// backtracking.
    pub(crate) fn matches(&self, name: &[u8]) -> bool {
        let Some(tokens) = &self.tokens else {
            return false;
        };
        // `row[j]` is true when the first `j` tokens match the name prefix
        // that the loop has consumed.
        let mut row = vec![false; tokens.len() + 1];
        if let Some(first) = row.first_mut() {
            *first = true;
        }
        close_runs(tokens, &mut row);
        for &byte in name {
            let mut next = vec![false; tokens.len() + 1];
            for ((token, states), slot) in tokens
                .iter()
                .zip(row.windows(2))
                .zip(next.iter_mut().skip(1))
            {
                let &[before, after] = states else {
                    continue;
                };
                *slot = match token {
                    // Stay in the run, or enter it from the previous token.
                    Token::Run => byte != b'/' && (after || before),
                    _ => before && token_matches(token, byte),
                };
            }
            close_runs(tokens, &mut next);
            if !next.contains(&true) {
                return false;
            }
            row = next;
        }
        row.last().copied().unwrap_or(false)
    }
}

/// A run can match the empty string, so a state before a run also reaches
/// the state after it.
fn close_runs(tokens: &[Token], row: &mut [bool]) {
    for (index, token) in tokens.iter().enumerate() {
        if *token == Token::Run
            && row.get(index) == Some(&true)
            && let Some(after) = row.get_mut(index + 1)
        {
            *after = true;
        }
    }
}

fn token_matches(token: &Token, byte: u8) -> bool {
    match token {
        Token::Literal(literal) => *literal == byte,
        Token::One | Token::Run => byte != b'/',
        Token::Class { negated, ranges } => {
            ranges
                .iter()
                .any(|&(low, high)| (low..=high).contains(&byte))
                != *negated
        }
    }
}

/// Parse a bracket expression whose content starts at `start`.
///
/// Returns `None` when the bracket does not close, so the `[` is literal.
/// Returns `Some((None, _))` when the expression has an invalid range.
/// Otherwise returns the token and the index after the closing `]`.
fn class(pattern: &[u8], start: usize) -> Option<(Option<Token>, usize)> {
    let mut end = start;
    if matches!(pattern.get(end), Some(b'!' | b'^')) {
        end += 1;
    }
    if pattern.get(end) == Some(&b']') {
        end += 1;
    }
    while pattern.get(end).is_some_and(|byte| *byte != b']') {
        end += 1;
    }
    if pattern.get(end) != Some(&b']') {
        return None;
    }
    let mut body = pattern.get(start..end)?;
    let negated = match body.split_first() {
        Some((b'!' | b'^', rest)) => {
            body = rest;
            true
        }
        _ => false,
    };
    Some((
        members(body).map(|ranges| Token::Class { negated, ranges }),
        end + 1,
    ))
}

/// Convert the members of a bracket expression to inclusive byte ranges.
///
/// This follows the regex compiler that CMake uses: a leading `]` or `-` is
/// literal, a `-` before the end is literal, and `a-z` is a range. A range
/// whose start is after its end is a compile error.
fn members(body: &[u8]) -> Option<Vec<(u8, u8)>> {
    let mut ranges = Vec::new();
    let mut index = 0;
    if let Some(&first @ (b']' | b'-')) = body.first() {
        ranges.push((first, first));
        index = 1;
    }
    while let Some(&byte) = body.get(index) {
        if byte != b'-' {
            ranges.push((byte, byte));
            index += 1;
            continue;
        }
        let Some(&high) = body.get(index + 1) else {
            ranges.push((b'-', b'-'));
            break;
        };
        // The range starts at the byte before the dash. The regex compiler
        // has already added that byte as a single member.
        let low = index
            .checked_sub(1)
            .and_then(|before| body.get(before))
            .copied()
            .unwrap_or(b'-');
        if low > high {
            return None;
        }
        if low < high {
            ranges.push((low + 1, high));
        }
        index += 2;
    }
    Some(ranges)
}

#[cfg(test)]
mod tests {
    use super::CacheGlob;

    fn matches(pattern: &str, name: &str) -> bool {
        CacheGlob::new(pattern.as_bytes()).matches(name.as_bytes())
    }

    #[test]
    fn cache_glob_matches_cmake_classes() {
        // Sets and ranges.
        assert!(matches("A[BC]D", "ACD"));
        assert!(!matches("A[BC]D", "AED"));
        assert!(matches("V[0-9]", "V7"));
        assert!(!matches("V[0-9]", "Va"));
        // Both negations.
        assert!(matches("V[!0-9]", "Va"));
        assert!(!matches("V[!0-9]", "V1"));
        assert!(matches("V[^0-9]", "Vb"));
        assert!(!matches("V[^0-9]", "V2"));
        // A leading `]` is a literal member, also after a negation.
        assert!(matches("X[]]", "X]"));
        assert!(matches("X[]a]", "Xa"));
        assert!(matches("X[!]]", "Xq"));
        assert!(!matches("X[!]]", "X]"));
        // An unclosed `[` is literal.
        assert!(matches("A[B", "A[B"));
        assert!(!matches("A[B", "AB"));
        assert!(matches("[^]", "[^]"));
        // Exact case and full anchoring.
        assert!(!matches("foo", "FOO"));
        assert!(!matches("FOO", "FOOBAR"));
        assert!(!matches("FOO", "XFOO"));
        assert!(matches("FOO*", "FOOBAR"));
        // `*` and `?` do not match `/`.
        assert!(!matches("A*", "A/B"));
        assert!(!matches("A?B", "A/B"));
        assert!(matches("A[/]B", "A/B"));
        assert!(matches("A/*", "A/B"));
        // Mixed wildcards.
        assert!(matches("C*_[A-Z]?LE", "CMAKE_TOOLCHAIN_FILE"));
        assert!(matches("*", ""));
        assert!(matches("**", "ANY"));
        assert!(!matches("?", ""));
        assert!(matches("a*b*c", "aXbYbZc"));
        assert!(!matches("a*b*c", "aXbYbZ"));
        // Other regex metacharacters are literal.
        assert!(matches("A.B+", "A.B+"));
        assert!(!matches("A.B", "AxB"));
        assert!(matches("A\\B", "A\\B"));
    }

    #[test]
    fn cache_glob_handles_dash_and_invalid_ranges() {
        assert!(matches("[-a]", "-"));
        assert!(matches("[a-]", "-"));
        assert!(matches("[a-c-e]", "d"));
        assert!(!matches("[a-c-e]", "f"));
        // An invalid range cannot compile, so nothing matches.
        assert!(!matches("[z-a]", "z"));
        assert!(!matches("[z-a]*", "anything"));
        assert!(!matches("[b-a]", "b"));
        // A range from a byte to itself adds nothing new.
        assert!(matches("[ba-a]", "b"));
        assert!(matches("[ba-a]", "a"));
        assert!(!matches("[ba-a]", "c"));
    }

    #[test]
    fn cache_glob_is_linear_in_wildcards() {
        let pattern = "*a".repeat(64);
        let name = "a".repeat(4096);
        assert!(matches(&pattern, &name));
        assert!(!matches(&format!("{pattern}b"), &name));
    }
}
