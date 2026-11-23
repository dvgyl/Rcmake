//! Shared keyword argument parsing helpers for CMake built-in commands.
//!
//! [`KeywordParser`](crate::args::KeywordParser) follows the rules of CMake's `cmArgumentParser`:
//!
//! - Keywords match exactly (case-sensitive).
//! - A keyword ends the value list of the previous keyword.
//! - A one-value keyword takes the next argument. Later arguments go to the
//!   unparsed list until the next keyword.
//! - A keyword that expects a value but receives none records
//!   `missing required value`.
//! - Keyword errors are sorted by keyword name, as in CMake.
//!
//! Example: parse `block(SCOPE_FOR VARIABLES PROPAGATE a b)`.
//!
//! ```
//! use rcmake_eval::args::{KeywordParser, Slot};
//!
//! let parser = KeywordParser::new()
//!     .keyword("SCOPE_FOR", Slot::MultiNonEmpty)
//!     .keyword("PROPAGATE", Slot::Multi);
//! let args: Vec<&[u8]> = vec![b"SCOPE_FOR", b"VARIABLES", b"PROPAGATE", b"a", b"b"];
//! let parsed = parser.parse(&args);
//! assert!(parsed.seen("SCOPE_FOR"));
//! assert_eq!(parsed.multi("PROPAGATE").len(), 2);
//! assert!(parsed.unparsed().is_empty());
//! ```

use std::collections::BTreeMap;

use rcmake_base::BString;
use rcmake_expr::ExpandedArg;

/// Simple cursor over an [`ExpandedArg`] slice for built-in command parsers.
#[derive(Debug)]
pub struct ArgCursor<'a> {
    args: &'a [ExpandedArg],
    pos: usize,
}

impl<'a> ArgCursor<'a> {
    /// Construct a cursor positioned at the beginning of `args`.
    #[must_use]
    pub const fn new(args: &'a [ExpandedArg]) -> Self {
        Self { args, pos: 0 }
    }

    /// Peek the next argument without advancing.
    #[must_use]
    pub fn peek(&self) -> Option<&'a ExpandedArg> {
        self.args.get(self.pos)
    }

    /// Consume and return the next argument.
    pub fn next_arg(&mut self) -> Option<&'a ExpandedArg> {
        let arg = self.args.get(self.pos)?;
        self.pos += 1;
        Some(arg)
    }

    /// Remaining unconsumed arguments.
    #[must_use]
    pub fn rest(&self) -> &'a [ExpandedArg] {
        self.args.get(self.pos..).unwrap_or(&[])
    }

    /// Consume the next argument if its value equals `keyword`.
    pub fn eat(&mut self, keyword: &[u8]) -> bool {
        if self.peek().is_some_and(|a| a.value.as_slice() == keyword) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    /// Current position (number of consumed arguments).
    #[must_use]
    pub const fn position(&self) -> usize {
        self.pos
    }
}

/// Byte view of one command argument, so that [`KeywordParser`] accepts
/// [`ExpandedArg`], [`BString`], and byte slices.
pub trait ArgBytes {
    /// The argument bytes.
    fn arg_bytes(&self) -> &[u8];
}

impl ArgBytes for ExpandedArg {
    fn arg_bytes(&self) -> &[u8] {
        self.value.as_slice()
    }
}

impl ArgBytes for BString {
    fn arg_bytes(&self) -> &[u8] {
        self.as_slice()
    }
}

impl ArgBytes for &[u8] {
    fn arg_bytes(&self) -> &[u8] {
        self
    }
}

impl ArgBytes for Vec<u8> {
    fn arg_bytes(&self) -> &[u8] {
        self.as_slice()
    }
}

/// How a keyword consumes the arguments that follow it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Slot {
    /// Option without values (`bool`). Following arguments are unparsed.
    Flag,
    /// Exactly one value is required (`std::string`).
    Single,
    /// One non-empty value is required (`NonEmpty<std::string>`).
    SingleNonEmpty,
    /// Zero or one value (`Maybe<std::string>`).
    SingleMaybe,
    /// Zero or more values; repeated keywords append (`MaybeEmpty<std::vector>`).
    Multi,
    /// One or more values; repeated keywords append (`NonEmpty<std::vector>`).
    MultiNonEmpty,
    /// Zero or more values; each occurrence starts a new group
    /// (`MaybeEmpty<std::vector<std::vector>>`).
    Groups,
    /// One or more values; each occurrence starts a new group
    /// (`NonEmpty<std::vector<std::vector>>`).
    GroupsNonEmpty,
}

impl Slot {
    const fn min_values(self) -> usize {
        match self {
            Self::Flag | Self::SingleMaybe | Self::Multi | Self::Groups => 0,
            Self::Single | Self::SingleNonEmpty | Self::MultiNonEmpty | Self::GroupsNonEmpty => 1,
        }
    }

    const fn max_one(self) -> bool {
        matches!(
            self,
            Self::Single | Self::SingleNonEmpty | Self::SingleMaybe
        )
    }
}

/// Declarative keyword parser (`cmArgumentParser`).
#[derive(Clone, Debug, Default)]
pub struct KeywordParser {
    keywords: Vec<(&'static str, Slot)>,
    positional: usize,
}

impl KeywordParser {
    /// Create an empty parser.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Bind `name` with the given [`Slot`] behavior.
    #[must_use]
    pub fn keyword(mut self, name: &'static str, slot: Slot) -> Self {
        self.keywords.push((name, slot));
        self
    }

    /// Bind the first `count` arguments as positional values (until a keyword is seen).
    #[must_use]
    pub const fn positional(mut self, count: usize) -> Self {
        self.positional = count;
        self
    }

    fn find(&self, arg: &[u8]) -> Option<(&'static str, Slot)> {
        self.keywords
            .iter()
            .find(|(name, _)| name.as_bytes() == arg)
            .copied()
    }

    /// Parse `args`.
    #[must_use]
    pub fn parse<A: ArgBytes>(&self, args: &[A]) -> Parsed {
        let mut out = Parsed::default();
        let mut active: Option<(&'static str, Slot)> = None;
        let mut accepting = false;
        let mut seen_values = 0usize;
        let mut done_with_positional = self.positional == 0;

        for arg in args {
            let bytes = arg.arg_bytes();
            if let Some((name, slot)) = self.find(bytes) {
                Self::finish(&mut out, active, seen_values, done_with_positional);
                active = Some((name, slot));
                accepting = slot != Slot::Flag;
                seen_values = 0;
                done_with_positional = true;
                let entry = out.values.entry(name).or_default();
                entry.seen = true;
                if matches!(slot, Slot::Groups | Slot::GroupsNonEmpty) {
                    entry.groups.push(Vec::new());
                }
                continue;
            }
            if !done_with_positional {
                if out.positionals.len() < self.positional {
                    out.positionals.push(BString::from(bytes));
                    if out.positionals.len() == self.positional {
                        done_with_positional = true;
                    }
                    continue;
                }
                done_with_positional = true;
            }
            match active {
                Some((name, slot)) if accepting => {
                    let entry = out.values.entry(name).or_default();
                    match slot {
                        Slot::Flag => {}
                        Slot::Single | Slot::SingleNonEmpty | Slot::SingleMaybe => {
                            if slot == Slot::SingleNonEmpty && bytes.is_empty() {
                                out.errors
                                    .entry(name)
                                    .or_default()
                                    .push("  empty string not allowed\n");
                            }
                            entry.values = vec![BString::from(bytes)];
                        }
                        Slot::Multi | Slot::MultiNonEmpty => {
                            entry.values.push(BString::from(bytes));
                        }
                        Slot::Groups | Slot::GroupsNonEmpty => {
                            if let Some(group) = entry.groups.last_mut() {
                                group.push(BString::from(bytes));
                            }
                        }
                    }
                    seen_values += 1;
                    if slot.max_one() {
                        accepting = false;
                    }
                }
                _ => out.unparsed.push(BString::from(bytes)),
            }
        }
        Self::finish(&mut out, active, seen_values, done_with_positional);
        out
    }

    fn finish(
        out: &mut Parsed,
        active: Option<(&'static str, Slot)>,
        seen_values: usize,
        done_with_positional: bool,
    ) {
        if !done_with_positional {
            return;
        }
        if let Some((name, slot)) = active
            && seen_values < slot.min_values()
        {
            out.errors
                .entry(name)
                .or_default()
                .push("  missing required value\n");
        }
    }
}

/// Values recorded for one keyword.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct KeywordValues {
    seen: bool,
    values: Vec<BString>,
    groups: Vec<Vec<BString>>,
}

/// Result of [`KeywordParser::parse`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Parsed {
    values: BTreeMap<&'static str, KeywordValues>,
    positionals: Vec<BString>,
    unparsed: Vec<BString>,
    errors: BTreeMap<&'static str, Vec<&'static str>>,
}

impl Parsed {
    /// `true` if `keyword` appeared at least once.
    #[must_use]
    pub fn seen(&self, keyword: &str) -> bool {
        self.values.get(keyword).is_some_and(|v| v.seen)
    }

    /// `true` if the [`Slot::Flag`] `keyword` appeared.
    #[must_use]
    pub fn flag(&self, keyword: &str) -> bool {
        self.seen(keyword)
    }

    /// Value of a one-value keyword (`None` if absent or given without a value).
    #[must_use]
    pub fn single(&self, keyword: &str) -> Option<&BString> {
        self.values.get(keyword).and_then(|v| v.values.first())
    }

    /// Values of a multi-value keyword (empty if absent).
    #[must_use]
    pub fn multi(&self, keyword: &str) -> &[BString] {
        self.values
            .get(keyword)
            .map_or(&[][..], |v| v.values.as_slice())
    }

    /// Groups of a [`Slot::Groups`] keyword, one group per occurrence.
    #[must_use]
    pub fn groups(&self, keyword: &str) -> &[Vec<BString>] {
        self.values
            .get(keyword)
            .map_or(&[][..], |v| v.groups.as_slice())
    }

    /// Positional values bound with [`KeywordParser::positional`].
    #[must_use]
    pub fn positionals(&self) -> &[BString] {
        &self.positionals
    }

    /// Arguments that no keyword consumed.
    #[must_use]
    pub fn unparsed(&self) -> &[BString] {
        &self.unparsed
    }

    /// `true` if any keyword error was recorded.
    #[must_use]
    pub fn has_errors(&self) -> bool {
        !self.errors.is_empty()
    }

    /// Keyword errors as `(keyword, messages)` pairs in keyword order.
    pub fn keyword_errors(&self) -> impl Iterator<Item = (&'static str, &[&'static str])> {
        self.errors.iter().map(|(k, v)| (*k, v.as_slice()))
    }

    /// Text of `ParseResult::MaybeReportError` (`Error after keyword "K":\n  ...`), or
    /// `None` when there are no keyword errors.
    #[must_use]
    pub fn report_error_text(&self) -> Option<BString> {
        if self.errors.is_empty() {
            return None;
        }
        let mut out = Vec::new();
        for (keyword, messages) in &self.errors {
            out.extend_from_slice(b"Error after keyword \"");
            out.extend_from_slice(keyword.as_bytes());
            out.extend_from_slice(b"\":\n");
            for msg in messages {
                out.extend_from_slice(msg.as_bytes());
            }
        }
        Some(BString::from(out))
    }

    /// `ParseResult::Check`: fail on the first unparsed argument or on keyword errors.
    ///
    /// The error text is suitable for [`crate::CmdError::Status`].
    pub fn check(&self, context: &[u8]) -> Result<(), BString> {
        if let Some(first) = self.unparsed.first() {
            let mut msg = context.to_vec();
            msg.extend_from_slice(b" given unknown argument: \"");
            msg.extend_from_slice(first);
            msg.extend_from_slice(b"\".");
            return Err(BString::from(msg));
        }
        if self.errors.is_empty() {
            return Ok(());
        }
        let mut msg = context.to_vec();
        if !context.is_empty() {
            msg.push(b' ');
        }
        msg.extend_from_slice(b"given invalid ");
        msg.extend_from_slice(if self.errors.len() > 1 {
            b"arguments:".as_slice()
        } else {
            b"argument:".as_slice()
        });
        for (keyword, messages) in &self.errors {
            for m in messages {
                msg.extend_from_slice(b"\n  ");
                msg.extend_from_slice(keyword.as_bytes());
                msg.extend_from_slice(b": ");
                msg.extend_from_slice(m.trim().as_bytes());
            }
        }
        Err(BString::from(msg))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bytes(list: &[&'static str]) -> Vec<&'static [u8]> {
        list.iter().map(|s| s.as_bytes()).collect()
    }

    fn parser() -> KeywordParser {
        KeywordParser::new()
            .keyword("FLAG", Slot::Flag)
            .keyword("ONE", Slot::Single)
            .keyword("NONEMPTY", Slot::SingleNonEmpty)
            .keyword("MAYBE", Slot::SingleMaybe)
            .keyword("LIST", Slot::Multi)
            .keyword("REQ", Slot::MultiNonEmpty)
            .keyword("GROUP", Slot::Groups)
    }

    #[test]
    fn single_value_stops_after_one_argument() {
        let p = parser().parse(&bytes(&["ONE", "a", "b", "FLAG", "c"]));
        assert_eq!(p.single("ONE").map(|v| v.as_slice()), Some(&b"a"[..]));
        assert!(p.flag("FLAG"));
        assert_eq!(p.unparsed(), &[BString::from("b"), BString::from("c")]);
        assert!(!p.has_errors());
    }

    #[test]
    fn repeated_multi_appends_and_groups_split() {
        let p = parser().parse(&bytes(&["LIST", "a", "GROUP", "x", "LIST", "b", "GROUP"]));
        assert_eq!(p.multi("LIST"), &[BString::from("a"), BString::from("b")]);
        assert_eq!(p.groups("GROUP").len(), 2);
        assert_eq!(p.groups("GROUP")[0], vec![BString::from("x")]);
        assert!(p.groups("GROUP")[1].is_empty());
    }

    #[test]
    fn missing_values_are_sorted_by_keyword() {
        let p = parser().parse(&bytes(&["REQ", "ONE", "NONEMPTY", ""]));
        assert_eq!(
            p.report_error_text(),
            Some(BString::from(
                "Error after keyword \"NONEMPTY\":\n  empty string not allowed\nError after keyword \"ONE\":\n  missing required value\nError after keyword \"REQ\":\n  missing required value\n"
            ))
        );
        assert_eq!(
            p.check(b"cmd").unwrap_err(),
            BString::from(
                "cmd given invalid arguments:\n  NONEMPTY: empty string not allowed\n  ONE: missing required value\n  REQ: missing required value"
            )
        );
    }

    #[test]
    fn maybe_and_positionals() {
        let p = KeywordParser::new()
            .positional(2)
            .keyword("MAYBE", Slot::SingleMaybe)
            .parse(&bytes(&["p1", "p2", "extra", "MAYBE"]));
        assert_eq!(p.positionals(), &[BString::from("p1"), BString::from("p2")]);
        assert_eq!(p.unparsed(), &[BString::from("extra")]);
        assert!(p.seen("MAYBE"));
        assert_eq!(p.single("MAYBE"), None);
        assert!(!p.has_errors());
        assert_eq!(
            p.check(b"cmd").unwrap_err(),
            BString::from("cmd given unknown argument: \"extra\".")
        );
    }

    #[test]
    fn keywords_are_case_sensitive() {
        let p = parser().parse(&bytes(&["flag", "One"]));
        assert!(!p.flag("FLAG"));
        assert_eq!(p.unparsed().len(), 2);
    }

    #[test]
    fn cursor_eats_keywords() {
        let args = vec![
            ExpandedArg {
                value: "A".into(),
                kind: rcmake_syntax::ArgKind::Unquoted,
                line: 1,
            },
            ExpandedArg {
                value: "b".into(),
                kind: rcmake_syntax::ArgKind::Quoted,
                line: 1,
            },
        ];
        let mut c = ArgCursor::new(&args);
        assert!(!c.eat(b"a"));
        assert!(c.eat(b"A"));
        assert_eq!(c.position(), 1);
        assert_eq!(c.rest().len(), 1);
        assert_eq!(c.next_arg().map(|a| a.value.as_slice()), Some(&b"b"[..]));
        assert!(c.peek().is_none());
    }
}
