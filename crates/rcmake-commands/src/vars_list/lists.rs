use std::collections::BTreeSet;
use std::sync::Arc;

use rcmake_base::{BString, list, list_ops};
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
    let raw = cx.var(name);
    if matches!(command, b"APPEND" | b"PREPEND") {
        if !rest.is_empty() {
            let items = rest.iter().map(bytes);
            let mut existing = raw.unwrap_or_default();
            let has_value = !existing.is_empty();
            let value = if command == b"APPEND" {
                for (i, item) in items.enumerate() {
                    if has_value || i != 0 {
                        existing.push(b';');
                    }
                    existing.extend_from_slice(item);
                }
                existing
            } else {
                list::join(items.chain(has_value.then_some(existing.as_slice())))
            };
            cx.set_var(name, value);
        }
        return Ok(Flow::Normal);
    }
    let raw_bytes = raw.as_ref().map_or(b"".as_slice(), |raw| raw.as_slice());
    let elements = || list::iter(raw_bytes, list::Empty::Keep);
    match (command, rest) {
        (b"LENGTH", [out]) => cx.set_var(bytes(out), elements().count().to_string().into()),
        (b"LENGTH", _) => {
            return Err(CmdError::status(
                "sub-command LENGTH requires two arguments.",
            ));
        }
        (b"GET", [indices @ .., out]) if !indices.is_empty() => {
            let value = if raw.is_none() {
                BString::from("NOTFOUND")
            } else {
                get_elements(raw_bytes, indices)?
            };
            cx.set_var(bytes(out), value);
        }
        (b"GET", _) => {
            return Err(CmdError::status(
                "sub-command GET requires at least three arguments.",
            ));
        }
        (b"JOIN", [glue, out]) => cx.set_var(
            bytes(out),
            join_with(elements(), bytes(glue), cx.limits().max_value_bytes)?,
        ),
        (b"JOIN", _) => {
            return Err(CmdError::status(format!(
                "sub-command JOIN requires three arguments ({} found).",
                rest.len() + 1
            )));
        }
        (b"SUBLIST", [begin, length, out]) => {
            cx.set_var(bytes(out), sublist(raw_bytes, bytes(begin), bytes(length))?)
        }
        (b"SUBLIST", _) => {
            return Err(CmdError::status(format!(
                "sub-command SUBLIST requires four arguments ({} found).",
                rest.len() + 1
            )));
        }
        (b"FIND", [needle, out]) => {
            let found = elements().position(|item| item.as_ref() == bytes(needle));
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
        _ => return change_list(cx, command, name, rest, raw.as_ref()),
    }
    Ok(Flow::Normal)
}

fn change_list(
    cx: &mut Cx<'_>,
    command: &[u8],
    name: &[u8],
    rest: &[ExpandedArg],
    raw: Option<&BString>,
) -> CmdResult {
    let mut items = raw.map_or_else(Vec::new, |raw| list::split(raw, list::Empty::Keep));
    match (command, rest) {
        (b"INSERT", [at, values @ ..]) if !values.is_empty() => {
            let at = index(bytes(at), items.len(), true)?;
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
            if raw.is_some() && !rest.is_empty() {
                let remove: BTreeSet<_> = rest.iter().map(bytes).collect();
                items.retain(|item| !remove.contains(item.as_slice()));
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
            if raw.is_some() {
                if command == b"REVERSE" {
                    items.reverse();
                } else {
                    let mut seen = BTreeSet::new();
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
        (b"FILTER", _) => filter(cx, name, rest, raw.is_some(), items)?,
        (b"SORT", _) => {
            let sort = Sort::parse(rest)?;
            if raw.is_some() {
                sort.apply(cx, &mut items)?;
                save(cx, name, &items);
            }
        }
        (b"TRANSFORM", _) => transform::apply(cx, name, rest, raw.is_some(), items)?,
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

fn get_elements(raw: &[u8], args: &[ExpandedArg]) -> Result<BString, CmdError> {
    let elements = || list::iter(raw, list::Empty::Keep);
    if raw.is_empty() {
        return Err(CmdError::status("GET given empty list"));
    }
    if let [arg] = args {
        let parsed = list_number(bytes(arg))?;
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

    let indices = args.iter().map(|arg| list_number(bytes(arg)));
    let mut order = indices
        .clone()
        .enumerate()
        .map(|(slot, at)| at.map(|at| (at, slot)))
        .collect::<Result<Vec<_>, _>>();
    if !order
        .as_ref()
        .is_ok_and(|order| order.iter().all(|(at, _)| *at >= 0))
    {
        let len = elements().count();
        order = indices
            .enumerate()
            .map(|(slot, at)| Ok((checked_index(at?, len, false)? as i64, slot)))
            .collect();
    }
    let mut order = order?;
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
                for arg in args {
                    index(bytes(arg), len, false)?;
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

fn list_number(arg: &[u8]) -> Result<i64, CmdError> {
    list_ops::parse_index(arg)
        .map(|value| i64::from(value as i32))
        .ok_or_else(|| CmdError::status(cat(&[b"index: ", arg, b" is not a valid index"])))
}

fn signed_len(len: usize) -> i64 {
    i64::try_from(len).unwrap_or(i64::MAX)
}

fn index(arg: &[u8], len: usize, insert: bool) -> Result<usize, CmdError> {
    checked_index(list_number(arg)?, len, insert)
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

fn sublist(raw: &[u8], begin: &[u8], length: &[u8]) -> Result<BString, CmdError> {
    if raw.is_empty() {
        return Ok(BString::default());
    }
    let begin = list_number(begin)?;
    let length = list_number(length)?;
    let mut items = list::iter(raw, list::Empty::Keep);
    let Some(first) = usize::try_from(begin)
        .ok()
        .and_then(|start| items.nth(start))
    else {
        let last = signed_len(list::iter(raw, list::Empty::Keep).count()) - 1;
        return Err(CmdError::status(format!(
            "begin index: {begin} is out of range 0 - {last}"
        )));
    };
    if length < -1 {
        return Err(CmdError::status(format!(
            "length: {length} should be -1 or greater"
        )));
    }
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
    let removed = args
        .iter()
        .map(|arg| index(bytes(arg), items.len(), false))
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
