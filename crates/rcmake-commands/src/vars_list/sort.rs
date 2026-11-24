use std::collections::BTreeSet;

use rcmake_base::BString;
use rcmake_base::list_ops::{SortCompare, SortOptions, sort_items};
use rcmake_eval::{CmdError, Cx};
use rcmake_expr::ExpandedArg;

use super::{bytes, callbacks::Callback, cat};

#[derive(Default)]
pub(super) struct Sort<'a> {
    comparator: Option<&'a [u8]>,
    options: SortOptions,
}

impl<'a> Sort<'a> {
    pub(super) fn parse(args: &'a [ExpandedArg]) -> Result<Self, CmdError> {
        let mut sort = Self::default();
        let mut seen = BTreeSet::new();
        let mut args = args.iter().map(bytes);
        while let Some(option) = args.next() {
            if !matches!(option, b"COMPARE" | b"COMPARATOR" | b"CASE" | b"ORDER") {
                return Err(CmdError::status(cat(&[
                    b"sub-command SORT option \"",
                    option,
                    b"\" is unknown.",
                ])));
            }
            if !seen.insert(option) {
                return Err(CmdError::status(cat(&[
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
                return Err(CmdError::status(cat(&[
                    b"sub-command SORT option \"",
                    option,
                    b"\" is incompatible with \"",
                    other,
                    b"\".",
                ])));
            }
            let value = args.next().ok_or_else(|| {
                CmdError::status(cat(&[
                    b"sub-command SORT missing argument for option \"",
                    option,
                    b"\".",
                ]))
            })?;
            match (option, value) {
                (b"COMPARATOR", name) => sort.comparator = Some(name),
                (b"COMPARE", b"STRING") => sort.options.compare = SortCompare::String,
                (b"COMPARE", b"FILE_BASENAME") => {
                    sort.options.compare = SortCompare::FileBasename;
                }
                (b"COMPARE", b"NATURAL") => sort.options.compare = SortCompare::Natural,
                (b"CASE", b"SENSITIVE") => sort.options.insensitive = false,
                (b"CASE", b"INSENSITIVE") => sort.options.insensitive = true,
                (b"ORDER", b"ASCENDING") => sort.options.descending = false,
                (b"ORDER", b"DESCENDING") => sort.options.descending = true,
                _ => {
                    return Err(CmdError::status(cat(&[
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
        if let Some(name) = self.comparator {
            let callback =
                Callback::new(cx, name, b"sub-command SORT, COMPARATOR: ", "comparator")?;
            *items = self.sort_with_callback(cx, &callback, std::mem::take(items))?;
            return Ok(());
        }
        sort_items(items, self.options);
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
            let before = if self.options.descending {
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
        let (a, b) = if self.options.insensitive {
            lower_a = a.to_ascii_lowercase();
            lower_b = b.to_ascii_lowercase();
            (lower_a.as_slice(), lower_b.as_slice())
        } else {
            (a, b)
        };
        let less = callback.test(cx, [a, b])?;
        if less && callback.test(cx, [b, a])? {
            return Err(CmdError::status(
                "sub-command SORT, COMPARATOR: function does not induce a strict weak ordering.  The comparator returned TRUE for both (a, b) and (b, a).",
            ));
        }
        Ok(less)
    }
}
