//! Pure list algorithms shared by `list()` and `$<LIST:...>`: index parsing, item
//! trimming, and `SORT` ordering (`STRING`, `FILE_BASENAME`, `NATURAL`).
//!
//! Callback comparators (`list(SORT ... COMPARATOR <fn>)`) need the interpreter, so
//! they stay in the command crate.

use core::cmp::Ordering;

use bstr::BString;

/// Whitespace for `list(TRANSFORM STRIP)` and index parsing (`isspace` in the C locale).
#[must_use]
pub const fn is_space(byte: u8) -> bool {
    byte.is_ascii_whitespace() || byte == b'\x0b'
}

/// Remove leading and trailing whitespace ([`is_space`]).
#[must_use]
pub fn strip_spaces(item: &[u8]) -> &[u8] {
    let Some(start) = item.iter().position(|&byte| !is_space(byte)) else {
        return b"";
    };
    let end = item
        .iter()
        .rposition(|&byte| !is_space(byte))
        .map_or(0, |index| index + 1);
    item.get(start..end).unwrap_or_default()
}

/// Parse a list index or count: leading whitespace, then a signed decimal integer.
/// Returns `None` for any other text.
#[must_use]
pub fn parse_index(arg: &[u8]) -> Option<i64> {
    let first = arg.iter().position(|&byte| !is_space(byte))?;
    core::str::from_utf8(arg.get(first..)?).ok()?.parse().ok()
}

/// Convert a negative index (counted from the end) into an index from the start. The
/// result can still be out of range.
#[must_use]
pub fn normalize_index(index: i64, len: usize) -> i64 {
    if index < 0 {
        index.saturating_add(i64::try_from(len).unwrap_or(i64::MAX))
    } else {
        index
    }
}

/// `list(SORT COMPARE <mode>)`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SortCompare {
    /// `STRING`: byte-wise comparison (default).
    #[default]
    String,
    /// `FILE_BASENAME`: compare the text after the last `/`.
    FileBasename,
    /// `NATURAL`: digit runs compare by numeric value.
    Natural,
}

/// Options of a pure `list(SORT)`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SortOptions {
    /// Comparison mode.
    pub compare: SortCompare,
    /// `CASE INSENSITIVE`.
    pub insensitive: bool,
    /// `ORDER DESCENDING`.
    pub descending: bool,
}

/// Compare two list items with `options`.
#[must_use]
fn compare_items(a: &[u8], b: &[u8], options: SortOptions) -> Ordering {
    let (a, b) = if options.compare == SortCompare::FileBasename {
        (file_basename(a), file_basename(b))
    } else {
        (a, b)
    };
    let order = if options.compare == SortCompare::Natural {
        natural_cmp(a, b, options.insensitive)
    } else if options.insensitive {
        a.iter()
            .map(u8::to_ascii_lowercase)
            .cmp(b.iter().map(u8::to_ascii_lowercase))
    } else {
        a.cmp(b)
    };
    if options.descending {
        order.reverse()
    } else {
        order
    }
}

/// Stable sort of `items` with `options` (`list(SORT)` without a comparator).
pub fn sort_items(items: &mut [BString], options: SortOptions) {
    items.sort_by(|a, b| compare_items(a, b, options));
}

fn file_basename(item: &[u8]) -> &[u8] {
    item.rsplit(|&byte| byte == b'/' || cfg!(windows) && byte == b'\\')
        .next()
        .unwrap_or_default()
}

/// `cmSystemTools::strverscmp`-style natural comparison: runs of digits compare by
/// numeric value, and leading zeros compare as fractional parts.
#[must_use]
pub fn natural_cmp(mut left: &[u8], mut right: &[u8], insensitive: bool) -> Ordering {
    let byte = |b: u8| {
        let b = if insensitive {
            b.to_ascii_lowercase()
        } else {
            b
        };
        b.cast_signed()
    };
    while let (Some((&a, rest_a)), Some((&b, rest_b))) = (left.split_first(), right.split_first()) {
        let order = if a.is_ascii_digit() && b.is_ascii_digit() {
            let (digits_a, rest_a) = digit_run(left);
            let (digits_b, rest_b) = digit_run(right);
            left = rest_a;
            right = rest_b;
            if a != b && (a == b'0' || b == b'0') {
                a.cmp(&b)
            } else if a != b'0' {
                digits_a
                    .len()
                    .cmp(&digits_b.len())
                    .then_with(|| digits_a.cmp(digits_b))
            } else {
                let common = digits_a
                    .iter()
                    .zip(digits_b)
                    .take_while(|(a, b)| a == b)
                    .count();
                match (digits_a.get(common), digits_b.get(common)) {
                    (Some(a), Some(b)) => a.cmp(b),
                    // A digit sorts before the end of a prefix that has only zeros.
                    (Some(_), None) if digits_b.iter().all(|&b| b == b'0') => Ordering::Less,
                    (None, Some(_)) if digits_a.iter().all(|&a| a == b'0') => Ordering::Greater,
                    (Some(&a), None) => {
                        byte(a).cmp(&byte(right.first().copied().unwrap_or_default()))
                    }
                    (None, Some(&b)) => {
                        byte(left.first().copied().unwrap_or_default()).cmp(&byte(b))
                    }
                    (None, None) => Ordering::Equal,
                }
            }
        } else {
            left = rest_a;
            right = rest_b;
            byte(a).cmp(&byte(b))
        };
        if !order.is_eq() {
            return order;
        }
    }
    byte(left.first().copied().unwrap_or_default())
        .cmp(&byte(right.first().copied().unwrap_or_default()))
}

fn digit_run(input: &[u8]) -> (&[u8], &[u8]) {
    let end = input
        .iter()
        .position(|byte| !byte.is_ascii_digit())
        .unwrap_or(input.len());
    input.split_at(end)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn natural_sort_orders_digit_runs_by_value() {
        let mut items: Vec<BString> = ["a10", "a2", "a1"].map(BString::from).to_vec();
        sort_items(
            &mut items,
            SortOptions {
                compare: SortCompare::Natural,
                ..SortOptions::default()
            },
        );
        assert_eq!(items, ["a1", "a2", "a10"].map(BString::from).to_vec());
    }

    #[test]
    fn basename_descending_case_insensitive() {
        let mut items: Vec<BString> = ["x/b", "y/A", "z/c"].map(BString::from).to_vec();
        sort_items(
            &mut items,
            SortOptions {
                compare: SortCompare::FileBasename,
                insensitive: true,
                descending: true,
            },
        );
        assert_eq!(items, ["z/c", "x/b", "y/A"].map(BString::from).to_vec());
    }

    #[test]
    fn index_helpers() {
        assert_eq!(parse_index(b"  -2"), Some(-2));
        assert_eq!(parse_index(b"x"), None);
        assert_eq!(normalize_index(-1, 3), 2);
        assert_eq!(strip_spaces(b" \x0ba b\t"), b"a b");
    }
}
