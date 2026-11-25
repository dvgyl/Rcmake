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

use std::borrow::Cow;

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
    let mut elements = iter(v, empty);
    if empty == Empty::Keep && empty_input == EmptyInput::OneEmptyElement {
        elements.remaining = Some(v);
    }
    out.extend(elements.map(|element| BString::from(element.into_owned())));
}

/// Iterate over list elements. Borrow each element unless it contains an escaped
/// semicolon. A completely empty input produces zero elements.
#[must_use]
pub fn iter(v: &[u8], empty: Empty) -> Elements<'_> {
    Elements {
        remaining: (!v.is_empty()).then_some(v),
        empty,
    }
}

/// Iterate over list elements without removing escape backslashes.
pub fn iter_raw(v: &[u8], empty: Empty) -> impl Iterator<Item = &[u8]> {
    let mut elements = iter(v, empty);
    std::iter::from_fn(move || elements.next_raw().map(|(bytes, _)| bytes))
}

/// An iterator over the elements of a CMake list.
pub struct Elements<'a> {
    remaining: Option<&'a [u8]>,
    empty: Empty,
}

impl<'a> Elements<'a> {
    fn next_raw(&mut self) -> Option<(&'a [u8], Option<usize>)> {
        while let Some(bytes) = self.remaining.take() {
            let mut depth = 0_isize;
            let mut escape = None;
            let mut i = 0;
            while let Some(&byte) = bytes.get(i) {
                match byte {
                    b'\\' if bytes.get(i + 1) == Some(&b';') => {
                        escape.get_or_insert(i);
                        i += 1;
                    }
                    b'[' => depth = depth.saturating_add(1),
                    b']' => depth = depth.saturating_sub(1),
                    b';' if depth == 0 => {
                        self.remaining = bytes.get(i + 1..);
                        break;
                    }
                    _ => {}
                }
                i += 1;
            }
            if i != 0 || self.empty == Empty::Keep {
                return Some((bytes.get(..i)?, escape));
            }
        }
        None
    }
}

impl<'a> Iterator for Elements<'a> {
    type Item = Cow<'a, [u8]>;

    fn next(&mut self) -> Option<Self::Item> {
        let (bytes, escape) = self.next_raw()?;
        let Some(start) = escape else {
            return Some(Cow::Borrowed(bytes));
        };
        let mut value = Vec::with_capacity(bytes.len() - 1);
        value.extend_from_slice(bytes.get(..start)?);
        let mut i = start;
        while let Some(&byte) = bytes.get(i) {
            if byte == b'\\' && bytes.get(i + 1) == Some(&b';') {
                value.push(b';');
                i += 2;
            } else {
                value.push(byte);
                i += 1;
            }
        }
        Some(Cow::Owned(value))
    }

    fn nth(&mut self, n: usize) -> Option<Self::Item> {
        for _ in 0..n {
            self.next_raw()?;
        }
        self.next()
    }

    fn count(mut self) -> usize {
        std::iter::from_fn(|| self.next_raw()).count()
    }
}

/// Split a CMake list into owned elements.
#[must_use]
pub fn split(v: &[u8], empty: Empty) -> Vec<BString> {
    let mut out = Vec::new();
    split_into(v, empty, &mut out);
    out
}

/// Escape semicolons in an item for use in a CMake list.
#[must_use]
pub fn escape_item(item: &[u8]) -> BString {
    let mut out = Vec::with_capacity(item.len());
    for &byte in item {
        if byte == b';' {
            out.push(b'\\');
        }
        out.push(byte);
    }
    out.into()
}

/// Append an item as-is. Insert a semicolon if the destination has bytes.
pub fn push(out: &mut Vec<u8>, item: &[u8]) {
    if !out.is_empty() {
        out.push(b';');
    }
    out.extend_from_slice(item);
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
    fn iterator_borrows_plain_elements_and_unescapes_only_when_needed() {
        let raw = br";plain;[a;b];a\;b\;c;]";
        let mut kept = iter(raw, Empty::Keep);
        assert_eq!(kept.next(), Some(Cow::Borrowed(b"".as_slice())));
        assert!(matches!(kept.next(), Some(Cow::Borrowed(b"plain"))));
        assert!(matches!(kept.next(), Some(Cow::Borrowed(b"[a;b]"))));
        assert!(matches!(kept.next(), Some(Cow::Owned(bytes)) if bytes == b"a;b;c"));
        assert!(matches!(kept.next(), Some(Cow::Borrowed(b"]"))));
        assert_eq!(kept.next(), None);
        assert_eq!(iter(raw, Empty::Drop).count(), 4);
        assert_eq!(
            iter(raw, Empty::Keep).nth(3).as_deref(),
            Some(b"a;b;c".as_slice())
        );
    }

    #[test]
    fn appends_and_iterates_over_raw_items() {
        let mut out = Vec::new();
        let item = escape_item(br"a\;b;");
        assert_eq!(item, br"a\\;b\;".as_slice());
        push(&mut out, &item);
        push(&mut out, b"[c;d]");
        push(&mut out, b"");
        assert!(iter_raw(&[], Empty::Keep).next().is_none());
        assert_eq!(
            iter_raw(&out, Empty::Keep).collect::<Vec<_>>(),
            [item.as_slice(), b"[c;d]", b""]
        );
        assert_eq!(iter_raw(&out, Empty::Drop).count(), 2);
        assert_eq!(split(&out, Empty::Keep)[0], br"a\;b;".as_slice());
    }

    #[test]
    fn join_never_escapes() {
        assert_eq!(join(["a", "b;c", ""]), BString::from("a;b;c;"));
    }
}
