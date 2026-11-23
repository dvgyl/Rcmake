use std::cmp::Ordering;
use std::collections::BTreeSet;

use rcmake_base::BString;
use rcmake_eval::{CmdError, Cx};
use rcmake_expr::ExpandedArg;

use super::{bytes, callbacks::Callback, cat, status};

#[derive(Default)]
enum Compare<'a> {
    Callback(&'a [u8]),
    #[default]
    String,
    FileBasename,
    Natural,
}

#[derive(Default)]
pub(super) struct Sort<'a> {
    compare: Compare<'a>,
    insensitive: bool,
    descending: bool,
}

impl<'a> Sort<'a> {
    pub(super) fn parse(args: &'a [ExpandedArg]) -> Result<Self, CmdError> {
        let mut sort = Self::default();
        let mut seen = BTreeSet::new();
        let mut args = args.iter().map(bytes);
        while let Some(option) = args.next() {
            if !matches!(option, b"COMPARE" | b"COMPARATOR" | b"CASE" | b"ORDER") {
                return Err(status(cat(&[
                    b"sub-command SORT option \"",
                    option,
                    b"\" is unknown.",
                ])));
            }
            if !seen.insert(option) {
                return Err(status(cat(&[
                    b"sub-command SORT option \"",
                    option,
                    b"\" has been specified multiple times.",
                ])));
            }
            let incompatible = match option {
                b"COMPARE" if seen.contains(b"COMPARATOR".as_slice()) => {
                    Some(b"COMPARATOR".as_slice())
                }
                b"COMPARATOR" if seen.contains(b"COMPARE".as_slice()) => {
                    Some(b"COMPARE".as_slice())
                }
                _ => None,
            };
            if let Some(other) = incompatible {
                return Err(status(cat(&[
                    b"sub-command SORT option \"",
                    option,
                    b"\" is incompatible with \"",
                    other,
                    b"\".",
                ])));
            }
            let value = args.next().ok_or_else(|| {
                status(cat(&[
                    b"sub-command SORT missing argument for option \"",
                    option,
                    b"\".",
                ]))
            })?;
            match (option, value) {
                (b"COMPARATOR", name) => sort.compare = Compare::Callback(name),
                (b"COMPARE", b"STRING") => sort.compare = Compare::String,
                (b"COMPARE", b"FILE_BASENAME") => sort.compare = Compare::FileBasename,
                (b"COMPARE", b"NATURAL") => sort.compare = Compare::Natural,
                (b"CASE", b"SENSITIVE") => sort.insensitive = false,
                (b"CASE", b"INSENSITIVE") => sort.insensitive = true,
                (b"ORDER", b"ASCENDING") => sort.descending = false,
                (b"ORDER", b"DESCENDING") => sort.descending = true,
                _ => {
                    return Err(status(cat(&[
                        b"sub-command SORT value \"",
                        value,
                        b"\" for option \"",
                        option,
                        b"\" is invalid.",
                    ])));
                }
            }
        }
        Ok(sort)
    }

    pub(super) fn apply(&self, cx: &mut Cx<'_>, items: &mut Vec<BString>) -> Result<(), CmdError> {
        if let Compare::Callback(name) = self.compare {
            let callback =
                Callback::new(cx, name, b"sub-command SORT, COMPARATOR: ", "comparator")?;
            *items = self.sort_with_callback(cx, &callback, std::mem::take(items))?;
            return Ok(());
        }
        items.sort_by(|a, b| {
            let a = self.basename(a);
            let b = self.basename(b);
            let order = if matches!(self.compare, Compare::Natural) {
                natural_cmp(a, b, self.insensitive)
            } else if self.insensitive {
                a.iter()
                    .map(u8::to_ascii_lowercase)
                    .cmp(b.iter().map(u8::to_ascii_lowercase))
            } else {
                a.cmp(b)
            };
            if self.descending {
                order.reverse()
            } else {
                order
            }
        });
        Ok(())
    }

    fn sort_with_callback(
        &self,
        cx: &mut Cx<'_>,
        callback: &Callback<'_>,
        mut items: Vec<BString>,
    ) -> Result<Vec<BString>, CmdError> {
        if items.len() < 2 {
            return Ok(items);
        }
        let upper = items.split_off(items.len() / 2);
        let mut left = self
            .sort_with_callback(cx, callback, items)?
            .into_iter()
            .peekable();
        let mut right = self
            .sort_with_callback(cx, callback, upper)?
            .into_iter()
            .peekable();
        let mut merged = Vec::with_capacity(left.len() + right.len());
        while let (Some(a), Some(b)) = (left.peek(), right.peek()) {
            let before = if self.descending {
                self.less(cx, callback, a, b)?
            } else {
                self.less(cx, callback, b, a)?
            };
            merged.extend(if before { right.next() } else { left.next() });
        }
        merged.extend(left);
        merged.extend(right);
        Ok(merged)
    }

    fn less(
        &self,
        cx: &mut Cx<'_>,
        callback: &Callback<'_>,
        a: &[u8],
        b: &[u8],
    ) -> Result<bool, CmdError> {
        let (lower_a, lower_b);
        let (a, b) = if self.insensitive {
            lower_a = a.to_ascii_lowercase();
            lower_b = b.to_ascii_lowercase();
            (lower_a.as_slice(), lower_b.as_slice())
        } else {
            (a, b)
        };
        let less = callback.test(cx, [a, b])?;
        if less && callback.test(cx, [b, a])? {
            return Err(status(
                "sub-command SORT, COMPARATOR: function does not induce a strict weak ordering.  The comparator returned TRUE for both (a, b) and (b, a).",
            ));
        }
        Ok(less)
    }

    fn basename<'item>(&self, item: &'item [u8]) -> &'item [u8] {
        if matches!(self.compare, Compare::FileBasename) {
            item.rsplit(|&byte| byte == b'/' || cfg!(windows) && byte == b'\\')
                .next()
                .unwrap_or_default()
        } else {
            item
        }
    }
}

fn natural_cmp(mut left: &[u8], mut right: &[u8], insensitive: bool) -> Ordering {
    let byte = |b: u8| {
        let b = if insensitive {
            b.to_ascii_lowercase()
        } else {
            b
        };
        b as i8
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
