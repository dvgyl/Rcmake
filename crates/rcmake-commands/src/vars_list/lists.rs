use std::collections::{BTreeSet, HashSet};
use std::ffi::c_long;
use std::sync::Arc;

use rcmake_base::{BString, list, list_ops};
use rcmake_diag::{PolicyId, PolicyStatus};
use rcmake_eval::{CmdError, CmdResult, Cx, Flow};
use rcmake_expr::ExpandedArg;
use rcmake_host::Stream;
use rcmake_regex::{MatchOptions, Regex};

use super::{bytes, callbacks::Callback, cat, sort::Sort, transform};

pub(super) fn list(cx: &mut Cx<'_>, args: &[ExpandedArg]) -> CmdResult {
    let [command, name, rest @ ..] = args else {
        return Err(CmdError::status(
            "must be called with at least two arguments.",
        ));
    };
    let command = bytes(command);
    let name = bytes(name);
    if command == b"APPEND" {
        cx.append_list_var(name, rest)?;
        return Ok(Flow::Normal);
    }
    if command == b"PREPEND" {
        let raw = cx.var(name);
        if !rest.is_empty() {
            // Check the exact joined size before the value grows. A separator goes between
            // two items, and between the items and a nonempty existing value.
            let existing_len = raw.as_ref().map_or(0, |raw| raw.len());
            let separators = rest.len() - 1 + usize::from(existing_len > 0);
            let joined = rest
                .iter()
                .try_fold(existing_len, |len, item| len.checked_add(bytes(item).len()))
                .and_then(|len| len.checked_add(separators));
            let Some(joined) = joined.filter(|&len| len <= cx.limits().max_value_bytes) else {
                return Err(CmdError::status(
                    "Variable value exceeded maximum allowed byte length.",
                ));
            };
            let mut out = Vec::with_capacity(joined);
            for (i, item) in rest.iter().enumerate() {
                if i != 0 {
                    out.push(b';');
                }
                out.extend_from_slice(bytes(item));
            }
            if let Some(existing) = raw
                && !existing.is_empty()
            {
                out.push(b';');
                out.extend_from_slice(&existing);
            }
            cx.set_var(name, BString::from(out));
        }
        return Ok(Flow::Normal);
    }
    match (command, rest) {
        (b"LENGTH", [out]) => {
            let count = cx.with_var(name, |raw| {
                list::iter(raw.unwrap_or_default(), list::Empty::Keep).count()
            });
            cx.set_var(bytes(out), count.to_string().into());
        }
        (b"LENGTH", _) => {
            return Err(CmdError::status(
                "sub-command LENGTH requires two arguments.",
            ));
        }
        (b"FIND", [needle, out]) => {
            let needle = bytes(needle);
            let found = cx.with_var(name, |raw| {
                list::iter(raw.unwrap_or_default(), list::Empty::Keep)
                    .position(|item| item.as_ref() == needle)
            });
            cx.set_var(
                bytes(out),
                found
                    .map_or_else(|| "-1".to_owned(), |i| i.to_string())
                    .into(),
            );
        }
        (b"FIND", _) => {
            return Err(CmdError::status(
                "sub-command FIND requires three arguments.",
            ));
        }
        (b"JOIN", [glue, out]) => {
            let max_bytes = cx.limits().max_value_bytes;
            let joined = cx.with_var(name, |raw| {
                join_with(
                    list::iter(raw.unwrap_or_default(), list::Empty::Keep),
                    bytes(glue),
                    max_bytes,
                )
            })?;
            cx.set_var(bytes(out), joined);
        }
        (b"JOIN", _) => {
            return Err(CmdError::status(format!(
                "sub-command JOIN requires three arguments ({} found).",
                rest.len() + 1
            )));
        }
        (b"GET", [indices @ .., out]) if !indices.is_empty() => {
            let raw = cx.var(name);
            let value = match raw.as_deref() {
                None => BString::from("NOTFOUND"),
                Some(raw_bytes) => get_elements(cx, raw_bytes, indices)?,
            };
            cx.set_var(bytes(out), value);
        }
        (b"GET", _) => {
            return Err(CmdError::status(
                "sub-command GET requires at least three arguments.",
            ));
        }
        (b"SUBLIST", [begin, length, out]) => {
            let raw = cx.var(name);
            let raw_bytes = raw.as_deref().unwrap_or_default();
            let value = sublist(cx, raw_bytes, bytes(begin), bytes(length))?;
            cx.set_var(bytes(out), value);
        }
        (b"SUBLIST", _) => {
            return Err(CmdError::status(format!(
                "sub-command SUBLIST requires four arguments ({} found).",
                rest.len() + 1
            )));
        }
        _ => return change_list(cx, command, name, rest),
    }
    Ok(Flow::Normal)
}

fn change_list(cx: &mut Cx<'_>, command: &[u8], name: &[u8], rest: &[ExpandedArg]) -> CmdResult {
    let max_args = cx.limits().max_command_arguments;
    let max_bytes = cx.limits().max_command_argument_bytes;
    let mut items = Vec::new();
    let defined = cx.with_var(name, |raw| -> Result<bool, CmdError> {
        let Some(raw) = raw else {
            return Ok(false);
        };
        list::ListBudget::new(max_args, max_bytes)
            .split(raw, list::Empty::Keep, &mut items)
            .map_err(|limit| CmdError::status(limit.to_string()))?;
        Ok(true)
    })?;
    match (command, rest) {
        (b"INSERT", [at, values @ ..]) if !values.is_empty() => {
            let at = checked_index(list_number(cx, bytes(at))?, items.len(), true)?;
            items.splice(at..at, values.iter().map(|arg| arg.value.clone()));
            save(cx, name, &items);
        }
        (b"INSERT", _) => {
            return Err(CmdError::status(
                "sub-command INSERT requires at least three arguments.",
            ));
        }
        (b"POP_BACK" | b"POP_FRONT", _) => pop(cx, command, name, rest, &items),
        (b"REMOVE_ITEM", _) => {
            if defined && !rest.is_empty() {
                if let [single] = rest {
                    let needle = bytes(single);
                    items.retain(|item| item.as_slice() != needle);
                } else if rest.len() <= 8 {
                    items.retain(|item| !rest.iter().any(|r| bytes(r) == item.as_slice()));
                } else {
                    let remove: HashSet<&[u8]> = rest.iter().map(bytes).collect();
                    items.retain(|item| !remove.contains(item.as_slice()));
                }
                save(cx, name, &items);
            }
        }
        (b"REMOVE_AT", []) => {
            return Err(CmdError::status(
                "sub-command REMOVE_AT requires at least two arguments.",
            ));
        }
        (b"REMOVE_AT", _) => remove_at(cx, name, rest, &mut items)?,
        (b"REMOVE_DUPLICATES" | b"REVERSE", []) => {
            if defined {
                if command == b"REVERSE" {
                    items.reverse();
                } else {
                    let mut seen: HashSet<BString> = HashSet::with_capacity(items.len());
                    items.retain(|item| seen.insert(item.clone()));
                }
                save(cx, name, &items);
            }
        }
        (b"REMOVE_DUPLICATES" | b"REVERSE", _) => {
            return Err(CmdError::status(cat(&[
                b"sub-command ",
                command,
                b" only takes one argument.",
            ])));
        }
        (b"FILTER", _) => filter(cx, name, rest, defined, items)?,
        (b"SORT", _) => {
            let sort = Sort::parse(rest)?;
            if defined {
                sort.apply(cx, &mut items)?;
                save(cx, name, &items);
            }
        }
        (b"TRANSFORM", _) => transform::apply(cx, name, rest, defined, items)?,
        _ => {
            return Err(CmdError::status(cat(&[
                b"does not recognize sub-command ",
                command,
            ])));
        }
    }
    Ok(Flow::Normal)
}

fn save(cx: &mut Cx<'_>, name: &[u8], items: &[BString]) {
    cx.set_var(name, list::join(items));
}

fn join_with(items: list::Elements<'_>, glue: &[u8], limit: usize) -> Result<BString, CmdError> {
    let mut value = Vec::new();
    for (i, item) in items.enumerate() {
        let separator = if i == 0 { b"".as_slice() } else { glue };
        if item.len().saturating_add(separator.len()) > limit - value.len() {
            return Err(CmdError::status("result exceeds the value size limit"));
        }
        value.extend_from_slice(separator);
        value.extend_from_slice(&item);
    }
    Ok(value.into())
}

fn get_elements(cx: &mut Cx<'_>, raw: &[u8], args: &[ExpandedArg]) -> Result<BString, CmdError> {
    let elements = || list::iter(raw, list::Empty::Keep);
    if raw.is_empty() {
        return Err(CmdError::status("GET given empty list"));
    }
    if let [arg] = args {
        let parsed = list_number(cx, bytes(arg))?;
        let selected = match usize::try_from(parsed) {
            Ok(at) => elements().nth(at),
            Err(_) => {
                let at = checked_index(parsed, elements().count(), false)?;
                elements().nth(at)
            }
        };
        return match selected {
            Some(value) => Ok(value.into_owned().into()),
            None => Err(index_range(parsed, elements().count(), false)),
        };
    }

    let indices = args
        .iter()
        .map(|arg| list_number(cx, bytes(arg)))
        .collect::<Result<Vec<_>, _>>()?;
    let mut order = indices
        .iter()
        .copied()
        .enumerate()
        .map(|(slot, at)| (at, slot))
        .collect::<Vec<_>>();
    if indices.iter().any(|at| *at < 0) {
        let len = elements().count();
        for (at, _) in &mut order {
            *at = checked_index(*at, len, false)? as i64;
        }
    }
    order.sort_unstable();
    let mut selected = vec![None; args.len()];
    let mut scan = elements();
    let mut previous = None;
    let mut next = 0;
    for (at, slot) in order {
        let at = usize::try_from(at).map_err(|err| CmdError::status(err.to_string()))?;
        if at >= next {
            let Some(value) = scan.nth(at - next) else {
                let len = elements().count();
                // Report the first invalid index in argument order.
                for index in indices {
                    checked_index(index, len, false)?;
                }
                return Err(index_range(at as i64, len, false));
            };
            previous = Some(slot);
            if let Some(element) = selected.get_mut(slot) {
                *element = Some(value);
            }
            next = at + 1;
        } else if let Some(value) = previous
            .and_then(|previous| selected.get(previous))
            .cloned()
            && let Some(element) = selected.get_mut(slot)
        {
            *element = value;
        }
    }
    Ok(list::join(selected.into_iter().flatten()))
}

fn list_number(cx: &mut Cx<'_>, arg: &[u8]) -> Result<i64, CmdError> {
    let text = arg.split(|byte| *byte == 0).next().unwrap_or_default();
    let start = text.iter().position(|byte| !list_ops::is_space(*byte));
    let number = start
        .and_then(|start| text.get(start..))
        .unwrap_or_default();
    let sign = usize::from(matches!(number.first(), Some(b'+' | b'-')));
    let digits = number
        .iter()
        .skip(sign)
        .take_while(|byte| byte.is_ascii_digit())
        .count();
    let end = sign + digits;
    let parsed = number
        .get(..end)
        .and_then(|prefix| std::str::from_utf8(prefix).ok())
        .and_then(|prefix| prefix.parse::<c_long>().ok());
    if parsed.is_none() || end != number.len() {
        let policy = PolicyId::new(121);
        match cx.policy(policy) {
            PolicyStatus::New => {
                return Err(CmdError::status(cat(&[
                    b"index: ",
                    arg,
                    b" is not a valid index",
                ])));
            }
            PolicyStatus::Warn => {
                let warning = cat(&[b"\nInvalid list index \"", arg, b"\"."]);
                crate::policy_misc::issue_warning(cx, policy, b"", &warning);
            }
            PolicyStatus::Old => {}
        }
    }
    let value = parsed.unwrap_or_else(|| {
        if digits == 0 {
            0
        } else if number.starts_with(b"-") {
            c_long::MIN
        } else {
            c_long::MAX
        }
    });
    #[allow(clippy::unnecessary_cast)] // c_long is i32 on Windows/wasm32 and i64 on 64-bit Unix.
    let narrowed = value as i32;
    Ok(i64::from(narrowed))
}

fn signed_len(len: usize) -> i64 {
    i64::try_from(len).unwrap_or(i64::MAX)
}

fn checked_index(parsed: i64, len: usize, insert: bool) -> Result<usize, CmdError> {
    let last = signed_len(len) - i64::from(!insert);
    let relative = list_ops::normalize_index(parsed, len);
    if !(0..=last).contains(&relative) {
        return Err(index_range(parsed, len, insert));
    }
    usize::try_from(relative).map_err(|err| CmdError::status(err.to_string()))
}

fn index_range(parsed: i64, len: usize, insert: bool) -> CmdError {
    let last = signed_len(len) - i64::from(!insert);
    CmdError::status(format!(
        "index: {parsed} out of range ({}, {last})",
        -signed_len(len)
    ))
}

fn sublist(cx: &mut Cx<'_>, raw: &[u8], begin: &[u8], length: &[u8]) -> Result<BString, CmdError> {
    if raw.is_empty() {
        return Ok(BString::default());
    }
    let begin = list_number(cx, begin)?;
    let length = list_number(cx, length)?;
    let begin_error = || {
        let last = signed_len(list::iter(raw, list::Empty::Keep).count()) - 1;
        CmdError::status(format!("begin index: {begin} is out of range 0 - {last}"))
    };
    let start = usize::try_from(begin).map_err(|_| begin_error())?;
    if length < -1 {
        return Err(CmdError::status(format!(
            "length: {length} should be -1 or greater"
        )));
    }
    let mut items = list::iter(raw, list::Empty::Keep);
    let first = items.nth(start).ok_or_else(begin_error)?;
    let count = usize::try_from(length).unwrap_or(usize::MAX);
    Ok(list::join(std::iter::once(first).chain(items).take(count)))
}

fn pop(cx: &mut Cx<'_>, command: &[u8], name: &[u8], outputs: &[ExpandedArg], items: &[BString]) {
    if items.is_empty() {
        for out in outputs {
            cx.unset_var(bytes(out));
        }
        return;
    }
    let count = outputs.len().max(1).min(items.len());
    let remaining = if command == b"POP_BACK" {
        let (remaining, taken) = items.split_at(items.len() - count);
        set_popped(cx, outputs, taken.iter().rev());
        remaining
    } else {
        let (taken, remaining) = items.split_at(count);
        set_popped(cx, outputs, taken.iter());
        remaining
    };
    save(cx, name, remaining);
}

fn set_popped<'a>(
    cx: &mut Cx<'_>,
    outputs: &[ExpandedArg],
    mut items: impl Iterator<Item = &'a BString>,
) {
    for out in outputs {
        if let Some(item) = items.next() {
            cx.set_var(bytes(out), item.clone());
        } else {
            cx.unset_var(bytes(out));
        }
    }
}

fn remove_at(
    cx: &mut Cx<'_>,
    name: &[u8],
    args: &[ExpandedArg],
    items: &mut Vec<BString>,
) -> Result<(), CmdError> {
    if items.is_empty() {
        let indices: Vec<_> = std::iter::once(name)
            .chain(args.iter().map(bytes))
            .collect();
        return Err(CmdError::status(cat(&[
            b"index: ",
            &indices.join(b", ".as_slice()),
            b" out of range (0, 0)",
        ])));
    }
    if let [single] = args {
        let index = checked_index(list_number(cx, bytes(single))?, items.len(), false)?;
        items.remove(index);
        save(cx, name, items);
        return Ok(());
    }
    let indices = args
        .iter()
        .map(|arg| list_number(cx, bytes(arg)))
        .collect::<Result<Vec<_>, _>>()?;
    let removed = indices
        .into_iter()
        .map(|index| checked_index(index, items.len(), false))
        .collect::<Result<BTreeSet<_>, _>>()?;
    let mut i = 0;
    items.retain(|_| {
        let keep = !removed.contains(&i);
        i += 1;
        keep
    });
    save(cx, name, items);
    Ok(())
}

pub(super) fn compile(
    cx: &mut Cx<'_>,
    pattern: &[u8],
    error: &[u8],
) -> Result<Arc<Regex>, CmdError> {
    cx.regex(pattern).map_err(|regex_error| {
        cx.host().write_out(
            Stream::Stdout,
            &cat(&[
                regex_error.message.as_bytes(),
                b"\nRegularExpression::compile(): Error in compile.\n",
            ]),
        );
        CmdError::status(cat(&[error, b" \"", pattern, b"\"."]))
    })
}

pub(super) fn matches(re: &Regex, input: &[u8], budget: &mut u64) -> Result<bool, CmdError> {
    re.find_at(input, 0, MatchOptions::default(), budget)
        .map(|caps| caps.is_some())
        .map_err(|err| CmdError::status(err.to_string()))
}

fn filter(
    cx: &mut Cx<'_>,
    name: &[u8],
    args: &[ExpandedArg],
    defined: bool,
    items: Vec<BString>,
) -> Result<(), CmdError> {
    let Some((operator, tail)) = args.split_first() else {
        return Err(CmdError::status(
            "sub-command FILTER requires an operator to be specified.",
        ));
    };
    let Some((mode, operand)) = tail.split_first() else {
        return Err(CmdError::status(
            "sub-command FILTER requires a mode to be specified.",
        ));
    };
    let include = match bytes(operator) {
        b"INCLUDE" => true,
        b"EXCLUDE" => false,
        bad => {
            return Err(CmdError::status(cat(&[
                b"sub-command FILTER does not recognize operator ",
                bad,
            ])));
        }
    };
    if !defined {
        return Ok(());
    }
    let mode = bytes(mode);
    let regex = match mode {
        b"REGEX" => true,
        b"PREDICATE" => false,
        bad => {
            return Err(CmdError::status(cat(&[
                b"sub-command FILTER does not recognize mode ",
                bad,
            ])));
        }
    };
    let [operand] = operand else {
        return Err(CmdError::status(cat(&[
            b"sub-command FILTER, mode ",
            mode,
            b" requires five arguments.",
        ])));
    };
    let operand = bytes(operand);
    let mut filtered = Vec::new();
    if regex {
        let re = compile(
            cx,
            operand,
            b"sub-command FILTER, mode REGEX failed to compile regex",
        )?;
        let mut budget = cx.limits().regex_steps;
        for item in items {
            if matches(&re, &item, &mut budget)? == include {
                filtered.push(item);
            }
        }
    } else {
        let predicate = Callback::new(
            cx,
            operand,
            b"sub-command FILTER, mode PREDICATE: ",
            "predicate",
        )?;
        for item in items {
            if predicate.test(cx, [item.as_slice()])? == include {
                filtered.push(item);
            }
        }
    }
    save(cx, name, &filtered);
    Ok(())
}
