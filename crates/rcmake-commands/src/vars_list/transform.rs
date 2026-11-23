use std::sync::Arc;

use rcmake_base::{BString, list};
use rcmake_diag::{PolicyId, PolicyStatus};
use rcmake_eval::{CmdError, Cx};
use rcmake_expr::ExpandedArg;
use rcmake_regex::{Captures, MatchOptions, Regex, expand_replacement};

use super::lists::{compile, matches, normalized, number};
use super::{bytes, callbacks::Callback, cat, separate::space, status};

const CONTEXT: &[u8] = b"sub-command TRANSFORM, ";
const REPLACE: &[u8] = b"sub-command TRANSFORM, action REPLACE: ";

enum Action<'a> {
    Append(&'a [u8]),
    Prepend(&'a [u8]),
    Lower,
    Upper,
    Strip,
    GenexStrip,
    Replace(&'a [u8], &'a [u8]),
    Apply(&'a [u8]),
}

impl<'a> Action<'a> {
    fn parse(args: &'a [ExpandedArg]) -> Result<(Self, &'a [ExpandedArg]), CmdError> {
        let Some((name, tail)) = args.split_first() else {
            return Err(status(
                "sub-command TRANSFORM requires an action to be specified.",
            ));
        };
        let action = match (bytes(name), tail) {
            (b"APPEND", [value, rest @ ..]) => return Ok((Self::Append(bytes(value)), rest)),
            (b"PREPEND", [value, rest @ ..]) => return Ok((Self::Prepend(bytes(value)), rest)),
            (b"REPLACE", [regex, replacement, rest @ ..]) => {
                return Ok((Self::Replace(bytes(regex), bytes(replacement)), rest));
            }
            (b"APPLY", [name, rest @ ..]) => return Ok((Self::Apply(bytes(name)), rest)),
            (b"APPEND" | b"PREPEND" | b"REPLACE" | b"APPLY", _) => {
                let count = if bytes(name) == b"REPLACE" {
                    b"2"
                } else {
                    b"1"
                };
                return Err(status(cat(&[
                    CONTEXT,
                    b"action ",
                    bytes(name),
                    b" expects ",
                    count,
                    b" argument(s).",
                ])));
            }
            (b"TOLOWER", _) => Self::Lower,
            (b"TOUPPER", _) => Self::Upper,
            (b"STRIP", _) => Self::Strip,
            (b"GENEX_STRIP", _) => Self::GenexStrip,
            (bad, _) => return Err(status(cat(&[CONTEXT, bad, b" invalid action."]))),
        };
        Ok((action, tail))
    }

    fn apply(
        &self,
        cx: &mut Cx<'_>,
        items: &mut [BString],
        selector: Selector<'_>,
    ) -> Result<(), CmdError> {
        match self {
            Self::Append(suffix) => selector.map(cx, items, |_, item| Ok(cat(&[item, suffix]))),
            Self::Prepend(prefix) => selector.map(cx, items, |_, item| Ok(cat(&[prefix, item]))),
            Self::Lower => selector.map(cx, items, |_, item| Ok(item.to_ascii_lowercase().into())),
            Self::Upper => selector.map(cx, items, |_, item| Ok(item.to_ascii_uppercase().into())),
            Self::Strip => selector.map(cx, items, |_, item| Ok(strip(item).into())),
            Self::GenexStrip => selector.map(cx, items, |_, item| Ok(strip_genex(item))),
            Self::Replace(pattern, template) => {
                let replace = Replacement::new(cx, pattern, template)?;
                let mut budget = cx.limits().regex_steps;
                selector.map(cx, items, |cx, item| replace.apply(cx, item, &mut budget))
            }
            Self::Apply(name) => {
                let callback = Callback::new(
                    cx,
                    name,
                    b"sub-command TRANSFORM, action APPLY: ",
                    "transform_apply",
                )?;
                selector.map(cx, items, |cx, item| callback.call(cx, [item]))
            }
        }
    }
}

enum Selector<'a> {
    All,
    At(Vec<i64>),
    For { start: i64, stop: i64, step: usize },
    Regex(Arc<Regex>),
    Predicate(Callback<'a>),
}

impl<'a> Selector<'a> {
    fn name(&self) -> &str {
        match self {
            Self::All => "",
            Self::At(_) => "AT",
            Self::For { .. } => "FOR",
            Self::Regex(_) => "REGEX",
            Self::Predicate(_) => "PREDICATE",
        }
    }

    fn parse(
        cx: &mut Cx<'_>,
        keyword: &[u8],
        args: &'a [ExpandedArg],
    ) -> Result<(Self, &'a [ExpandedArg]), CmdError> {
        match (keyword, args) {
            (b"AT", _) => {
                let indices: Vec<_> = args.iter().map(bytes).map_while(number).collect();
                if indices.is_empty() {
                    return Err(status(
                        "sub-command TRANSFORM, selector AT expects at least one numeric value.",
                    ));
                }
                let (_, rest) = args.split_at(indices.len());
                Ok((Self::At(indices), rest))
            }
            (b"FOR", [start, stop, rest @ ..]) => {
                let (Some(start), Some(stop)) = (number(bytes(start)), number(bytes(stop))) else {
                    return Err(status(
                        "sub-command TRANSFORM, selector FOR expects, at least, two numeric values.",
                    ));
                };
                let (step, rest) = if let Some((head, tail)) = rest.split_first()
                    && let Some(step) = number(bytes(head))
                {
                    let step = usize::try_from(step).ok().filter(|&step| step > 0).ok_or_else(|| {
                        status("sub-command TRANSFORM, selector FOR expects positive numeric value for <step>.")
                    })?;
                    (step, tail)
                } else {
                    (1, rest)
                };
                Ok((Self::For { start, stop, step }, rest))
            }
            (b"FOR", _) => Err(status(
                "sub-command TRANSFORM, selector FOR expects, at least, two arguments.",
            )),
            (b"REGEX", [pattern, rest @ ..]) => {
                let regex = compile(
                    cx,
                    bytes(pattern),
                    b"sub-command TRANSFORM, selector REGEX failed to compile regex",
                )?;
                Ok((Self::Regex(regex), rest))
            }
            (b"PREDICATE", [name, rest @ ..]) => {
                let callback = Callback::new(
                    cx,
                    bytes(name),
                    b"sub-command TRANSFORM, selector PREDICATE: ",
                    "predicate",
                )?;
                Ok((Self::Predicate(callback), rest))
            }
            (b"PREDICATE", _) => Err(status(
                "sub-command TRANSFORM, selector PREDICATE expects 'function name' argument.",
            )),
            _ => Err(status(
                "sub-command TRANSFORM, selector REGEX expects 'regular expression' argument.",
            )),
        }
    }

    fn map(
        self,
        cx: &mut Cx<'_>,
        items: &mut [BString],
        mut operation: impl FnMut(&mut Cx<'_>, &[u8]) -> Result<BString, CmdError>,
    ) -> Result<(), CmdError> {
        let limit = cx.limits().max_value_bytes;
        let mut size = items
            .iter()
            .fold(items.len().saturating_sub(1), |size, value| {
                size.saturating_add(value.len())
            });
        let mut apply = |cx: &mut Cx<'_>, item: &mut BString| {
            let value = operation(cx, item)?;
            size = size.saturating_sub(item.len()).saturating_add(value.len());
            if size > limit {
                return Err(status("result exceeds the value size limit"));
            }
            *item = value;
            Ok(())
        };
        let len = items.len();
        match self {
            Self::All => {
                for item in items {
                    apply(cx, item)?;
                }
            }
            Self::At(indices) => {
                let indices = indices
                    .iter()
                    .map(|&index| selector_index("AT", index, len))
                    .collect::<Result<Vec<_>, _>>()?;
                for index in indices {
                    if let Some(item) = items.get_mut(index) {
                        apply(cx, item)?;
                    }
                }
            }
            Self::For { start, stop, step } => {
                let start = selector_index("FOR", start, len)?;
                let stop = selector_index("FOR", stop, len)?;
                if start > stop {
                    return Err(status(format!(
                        "sub-command TRANSFORM, selector FOR expects <start> to be no greater than <stop> ({start} > {stop})"
                    )));
                }
                for item in items
                    .iter_mut()
                    .skip(start)
                    .take(stop - start + 1)
                    .step_by(step)
                {
                    apply(cx, item)?;
                }
            }
            Self::Regex(re) => {
                let mut budget = cx.limits().regex_steps;
                for item in items {
                    if matches(&re, item, &mut budget)? {
                        apply(cx, item)?;
                    }
                }
            }
            Self::Predicate(callback) => {
                for item in items {
                    if callback.test(cx, [item.as_slice()])? {
                        apply(cx, item)?;
                    }
                }
            }
        }
        Ok(())
    }
}

fn selector_index(selector: &str, raw: i64, len: usize) -> Result<usize, CmdError> {
    let index = normalized(raw, len);
    usize::try_from(index).ok().filter(|&index| index < len).ok_or_else(|| {
        let last = len.wrapping_sub(1);
        status(format!("sub-command TRANSFORM, selector {selector}, index: {index} out of range (-{len}, {last})."))
    })
}

pub(super) fn apply(
    cx: &mut Cx<'_>,
    name: &[u8],
    args: &[ExpandedArg],
    defined: bool,
    mut items: Vec<BString>,
) -> Result<(), CmdError> {
    let (action, mut rest) = Action::parse(args)?;
    let mut out = name;
    let mut selector = Selector::All;
    while let Some((keyword, tail)) = rest.split_first() {
        match (bytes(keyword), tail) {
            (b"OUTPUT_VARIABLE", [var, next @ ..]) => {
                out = bytes(var);
                rest = next;
            }
            (b"OUTPUT_VARIABLE", _) => {
                return Err(status(
                    "sub-command TRANSFORM, OUTPUT_VARIABLE expects variable name argument.",
                ));
            }
            (selector_name @ (b"AT" | b"FOR" | b"REGEX" | b"PREDICATE"), _) => {
                if !matches!(selector, Selector::All) {
                    return Err(status(format!(
                        "sub-command TRANSFORM, selector already specified ({}).",
                        selector.name()
                    )));
                }
                (selector, rest) = Selector::parse(cx, selector_name, tail)?;
            }
            (bad, _) => {
                return Err(status(cat(&[
                    CONTEXT,
                    b"'",
                    bad,
                    b"': unexpected argument(s).",
                ])));
            }
        }
    }
    if defined {
        action.apply(cx, &mut items, selector)?;
    }
    cx.set_var(out, list::join(items));
    Ok(())
}

fn strip(item: &[u8]) -> &[u8] {
    let Some(start) = item.iter().position(|&byte| !space(byte)) else {
        return b"";
    };
    let end = item
        .iter()
        .rposition(|&byte| !space(byte))
        .map_or(0, |index| index + 1);
    item.get(start..end).unwrap_or_default()
}

fn strip_genex(input: &[u8]) -> BString {
    let mut output = Vec::new();
    let mut start = 0;
    let mut depth = 0_usize;
    let mut input_iter = input.iter().copied().enumerate().peekable();
    while let Some((index, byte)) = input_iter.next() {
        match byte {
            b'$' if input_iter.next_if(|&(_, byte)| byte == b'<').is_some() => {
                if depth == 0 {
                    output.extend_from_slice(input.get(start..index).unwrap_or_default());
                    start = index;
                }
                depth += 1;
            }
            b'>' if depth > 0 => {
                depth -= 1;
                if depth == 0 {
                    start = index + 1;
                }
            }
            _ => {}
        }
    }
    output.extend_from_slice(input.get(start..).unwrap_or_default());
    output.into()
}

struct Replacement<'a> {
    regex: Arc<Regex>,
    pattern: &'a [u8],
    template: &'a [u8],
    options: MatchOptions,
}

impl<'a> Replacement<'a> {
    fn new(cx: &mut Cx<'_>, pattern: &'a [u8], template: &'a [u8]) -> Result<Self, CmdError> {
        let regex = compile(
            cx,
            pattern,
            b"sub-command TRANSFORM, action REPLACE: Failed to compile regex",
        )?;
        let replace = Self {
            regex,
            pattern,
            template,
            options: MatchOptions {
                bol_at_start: cx.policy(PolicyId::new(186)) != PolicyStatus::New,
                ..MatchOptions::default()
            },
        };
        // Validate escapes now. Check capture group bounds when a match uses them.
        let empty = Captures {
            groups: [None; rcmake_regex::MAX_CAPTURE_SLOTS],
        };
        expand_replacement(template, b"", &empty, 9).map_err(|err| replace.error(&err.message))?;
        Ok(replace)
    }

    fn apply(&self, cx: &mut Cx<'_>, input: &[u8], budget: &mut u64) -> Result<BString, CmdError> {
        let mut output = Vec::new();
        let mut search = 0;
        let mut copied = 0;
        let mut options = self.options;
        let limit = cx.limits().max_value_bytes;
        while search <= input.len() {
            let capture = self
                .regex
                .find_at(input, search, options, budget)
                .map_err(|err| self.error(&err.to_string()))?;
            let Some(capture) = capture else {
                break;
            };
            let Some((start, end)) = capture.groups.first().copied().flatten() else {
                break;
            };
            let value =
                expand_replacement(self.template, input, &capture, self.regex.group_count())
                    .map_err(|err| self.error(&err.message))?;
            let prefix = input.get(copied..start).unwrap_or_default();
            if output
                .len()
                .saturating_add(prefix.len())
                .saturating_add(value.len())
                > limit
            {
                return Err(self.error("Replacement exceeds the value size limit."));
            }
            output.extend_from_slice(prefix);
            output.extend_from_slice(&value);
            cx.set_match_vars(Some((&capture, input)));
            copied = end;
            search = end;
            options.allow_empty_at_start = start != end;
        }
        let tail = input.get(copied..).unwrap_or_default();
        if output.len().saturating_add(tail.len()) > limit {
            return Err(self.error("Replacement exceeds the value size limit."));
        }
        output.extend_from_slice(tail);
        Ok(output.into())
    }

    fn error(&self, message: &str) -> CmdError {
        let message = message
            .strip_prefix("string sub-command REGEX, mode REPLACE: ")
            .unwrap_or(message);
        if message.contains("contains an out-of-range escape for group") {
            return status(cat(&[
                REPLACE,
                b"replace expression \"",
                self.template,
                b"\" contains an out-of-range escape for regex \"",
                self.pattern,
                b"\".",
            ]));
        }
        status(cat(&[REPLACE, message.as_bytes()]))
    }
}
