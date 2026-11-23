//! CMake 4.5.1 semicolon-delimited list splitting and joining.
//!
//! Splitting follows CMake's exact `cmList` byte scan:
//! - Maintains a **signed** square-bracket depth (`[` increments, `]` decrements);
//!   an unescaped `;` separates elements only when `depth == 0`.
//! - Brackets remain in the element bytes unchanged.
//! - A backslash immediately followed by `;` (`\;`) contributes a literal `;`
//!   (tested by checking the immediately following byte, not backslash parity).
//! - Empty-element handling (`Empty::Keep` vs `Empty::Drop`) and empty-whole-input
//!   handling (`EmptyInput::Empty` vs `EmptyInput::SingleEmpty`) are independent so
//!   `list()`, `foreach(IN LISTS)`, unquoted argument expansion, and `if(IN_LIST)`
//!   all match CMake 4.5.1 byte-for-byte.

use bstr::BString;

/// Controls whether empty elements between/around semicolons are kept or dropped.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Empty {
    /// Preserve empty elements (used by `list()`, `foreach(IN LISTS)`, `if(IN_LIST)`).
    Keep,
    /// Drop empty elements (used when splitting unquoted command arguments).
    Drop,
}

/// Controls how a completely empty input byte slice (`b""`) is split when `Empty::Keep` is set.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EmptyInput {
    /// An empty whole input produces zero elements (`list(LENGTH)`, `foreach(IN LISTS)`).
    ZeroElements,
    /// An empty whole input produces one empty element (`if("" IN_LIST empty_var)`).
    OneEmptyElement,
}

/// Split `v` into CMake list elements, appending to `out`.
///
/// For `Empty::Keep`, a completely empty input produces zero elements (matching
/// `list(LENGTH)` and `foreach(IN LISTS)`). Use [`split_with_empty_input`] when
/// an empty whole input must yield one empty element (e.g., `if(IN_LIST)`).
pub fn split_into(v: &[u8], empty: Empty, out: &mut Vec<BString>) {
    split_with_empty_input(v, empty, EmptyInput::ZeroElements, out);
}

/// Split `v` into CMake list elements with explicit empty-whole-input behavior.
pub fn split_with_empty_input(
    v: &[u8],
    empty: Empty,
    empty_input: EmptyInput,
    out: &mut Vec<BString>,
) {
    if v.is_empty() {
        if empty == Empty::Keep && empty_input == EmptyInput::OneEmptyElement {
            out.push(BString::default());
        }
        return;
    }

    let mut current = Vec::new();
    let mut bracket_depth: isize = 0;
    let mut i = 0;

    while let Some(&b) = v.get(i) {
        if b == b'\\' && v.get(i + 1) == Some(&b';') {
            current.push(b';');
            i += 2;
            continue;
        }
        if b == b'[' {
            bracket_depth = bracket_depth.saturating_add(1);
            current.push(b);
            i += 1;
            continue;
        }
        if b == b']' {
            bracket_depth = bracket_depth.saturating_sub(1);
            current.push(b);
            i += 1;
            continue;
        }
        if b == b';' && bracket_depth == 0 {
            if empty == Empty::Keep || !current.is_empty() {
                out.push(BString::from(core::mem::take(&mut current)));
            }
            i += 1;
            continue;
        }
        current.push(b);
        i += 1;
    }

    if empty == Empty::Keep || !current.is_empty() {
        out.push(BString::from(current));
    }
}

/// Convenience wrapper around [`split_into`] returning a newly allocated `Vec<BString>`.
#[must_use]
pub fn split(v: &[u8], empty: Empty) -> Vec<BString> {
    let mut out = Vec::new();
    split_into(v, empty, &mut out);
    out
}

/// Join elements with `;`. Matches CMake by never escaping embedded semicolons.
#[must_use]
pub fn join<I, S>(items: I) -> BString
where
    I: IntoIterator<Item = S>,
    S: AsRef<[u8]>,
{
    let mut out = Vec::new();
    for (idx, item) in items.into_iter().enumerate() {
        if idx > 0 {
            out.push(b';');
        }
        out.extend_from_slice(item.as_ref());
    }
    BString::from(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_keep_and_drop_match_cmake_oracle() {
        let kept = split(br"a;b\;c;;d", Empty::Keep);
        assert_eq!(
            kept,
            vec![
                BString::from("a"),
                BString::from("b;c"),
                BString::from(""),
                BString::from("d"),
            ]
        );

        let dropped = split(br";a;b\;c;;d;", Empty::Drop);
        assert_eq!(
            dropped,
            vec![BString::from("a"), BString::from("b;c"), BString::from("d"),]
        );
    }

    #[test]
    fn signed_bracket_depth_suppresses_splitting() {
        assert_eq!(
            split(b"a[b;c];d", Empty::Keep),
            vec![BString::from("a[b;c]"), BString::from("d")]
        );
        // Stray ']' makes depth -1 until matching '[' returns depth to 0.
        assert_eq!(
            split(b"a]b;c[d;e", Empty::Keep),
            vec![BString::from("a]b;c[d"), BString::from("e")]
        );
    }

    #[test]
    fn empty_whole_input_modes() {
        assert!(split(b"", Empty::Keep).is_empty());
        let mut out = Vec::new();
        split_with_empty_input(b"", Empty::Keep, EmptyInput::OneEmptyElement, &mut out);
        assert_eq!(out, vec![BString::from("")]);
        assert_eq!(
            split(b";a;;", Empty::Keep),
            vec![
                BString::from(""),
                BString::from("a"),
                BString::from(""),
                BString::from(""),
            ]
        );
    }

    #[test]
    fn join_never_escapes() {
        assert_eq!(join(["a", "b;c", ""]), BString::from("a;b;c;"));
    }
}
