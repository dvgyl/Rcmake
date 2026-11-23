use std::collections::BTreeSet;
use std::sync::Arc;

use rcmake_base::{BString, list};
use rcmake_eval::{CmdError, CmdResult, Cx, Flow};
use rcmake_expr::ExpandedArg;
use rcmake_host::Stream;
use rcmake_regex::{MatchOptions, Regex};

use super::{bytes, callbacks::Callback, cat, separate::space, sort::Sort, status, transform};

pub(super) fn list(cx: &mut Cx<'_>, args: &[ExpandedArg]) -> CmdResult {
    let [command, name, rest @ ..] = args else {
        return Err(status("must be called with at least two arguments."));
    };
    let command = bytes(command);
    let name = bytes(name);
    let raw = cx.var(name);
    if matches!(command, b"APPEND" | b"PREPEND") {
        if !rest.is_empty() {
            let items = rest.iter().map(bytes);
            let existing = raw
                .as_deref()
                .map(Vec::as_slice)
                .filter(|value| !value.is_empty());
            let value = if command == b"APPEND" {
                list::join(existing.into_iter().chain(items))
            } else {
                list::join(items.chain(existing))
            };
            cx.set_var(name, value);
        }
        return Ok(Flow::Normal);
    }
    let mut items = raw
        .as_ref()
        .map_or_else(Vec::new, |raw| list::split(raw, list::Empty::Keep));
    match (command, rest) {
        (b"LENGTH", [out]) => cx.set_var(bytes(out), items.len().to_string().into()),
        (b"LENGTH", _) => return Err(status("sub-command LENGTH requires two arguments.")),
        (b"GET", [indices @ .., out]) if !indices.is_empty() => {
            let value = if raw.is_none() {
                BString::from("NOTFOUND")
            } else if items.is_empty() {
                return Err(status("GET given empty list"));
            } else {
                let indices = indices
                    .iter()
                    .map(|arg| index(bytes(arg), items.len(), false));
                let values = indices
                    .map(|index| index.map(|index| items.get(index)))
                    .collect::<Result<Vec<_>, _>>()?;
                list::join(values.into_iter().flatten())
            };
            cx.set_var(bytes(out), value);
        }
        (b"GET", _) => return Err(status("sub-command GET requires at least three arguments.")),
        (b"JOIN", [glue, out]) => cx.set_var(
            bytes(out),
            join_with(&items, bytes(glue), cx.limits().max_value_bytes)?,
        ),
        (b"JOIN", _) => {
            return Err(status(format!(
                "sub-command JOIN requires three arguments ({} found).",
                rest.len() + 1
            )));
        }
        (b"SUBLIST", [begin, length, out]) => {
            cx.set_var(bytes(out), sublist(&items, bytes(begin), bytes(length))?)
        }
        (b"SUBLIST", _) => {
            return Err(status(format!(
                "sub-command SUBLIST requires four arguments ({} found).",
                rest.len() + 1
            )));
        }
        (b"FIND", [needle, out]) => {
            let found = items
                .iter()
                .position(|item| item.as_slice() == bytes(needle));
            cx.set_var(
                bytes(out),
                found
                    .map_or_else(|| "-1".to_owned(), |i| i.to_string())
                    .into(),
            );
        }
        (b"FIND", _) => return Err(status("sub-command FIND requires three arguments.")),
        (b"INSERT", [at, values @ ..]) if !values.is_empty() => {
            let at = index(bytes(at), items.len(), true)?;
            items.splice(at..at, values.iter().map(|arg| arg.value.clone()));
            save(cx, name, &items);
        }
        (b"INSERT", _) => {
            return Err(status(
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
            return Err(status(
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
            return Err(status(cat(&[
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
        _ => return Err(status(cat(&[b"does not recognize sub-command ", command]))),
    }
    Ok(Flow::Normal)
}

fn save(cx: &mut Cx<'_>, name: &[u8], items: &[BString]) {
    cx.set_var(name, list::join(items));
}

fn join_with(items: &[BString], glue: &[u8], limit: usize) -> Result<BString, CmdError> {
    let bytes = glue.len().saturating_mul(items.len().saturating_sub(1));
    let size = items
        .iter()
        .fold(bytes, |size, item| size.saturating_add(item.len()));
    if size > limit {
        return Err(status("result exceeds the value size limit"));
    }
    let mut value = Vec::with_capacity(size);
    for (i, item) in items.iter().enumerate() {
        if i > 0 {
            value.extend_from_slice(glue);
        }
        value.extend_from_slice(item);
    }
    Ok(value.into())
}

pub(super) fn number(arg: &[u8]) -> Option<i64> {
    let first = arg.iter().position(|&byte| !space(byte))?;
    std::str::from_utf8(arg.get(first..)?).ok()?.parse().ok()
}

fn list_number(arg: &[u8]) -> Result<i64, CmdError> {
    number(arg)
        .map(|value| i64::from(value as i32))
        .ok_or_else(|| status(cat(&[b"index: ", arg, b" is not a valid index"])))
}

pub(super) fn normalized(index: i64, len: usize) -> i64 {
    if index < 0 {
        index + signed_len(len)
    } else {
        index
    }
}

fn signed_len(len: usize) -> i64 {
    i64::try_from(len).unwrap_or(i64::MAX)
}

fn index(arg: &[u8], len: usize, insert: bool) -> Result<usize, CmdError> {
    let parsed = list_number(arg)?;
    let last = signed_len(len) - i64::from(!insert);
    let relative = normalized(parsed, len);
    if !(0..=last).contains(&relative) {
        return Err(status(format!(
            "index: {parsed} out of range ({}, {last})",
            -signed_len(len)
        )));
    }
    usize::try_from(relative).map_err(|err| status(err.to_string()))
}

fn sublist(items: &[BString], begin: &[u8], length: &[u8]) -> Result<BString, CmdError> {
    if items.is_empty() {
        return Ok(BString::default());
    }
    let begin = list_number(begin)?;
    let length = list_number(length)?;
    let last = signed_len(items.len()) - 1;
    let start = usize::try_from(begin)
        .ok()
        .filter(|&start| start < items.len())
        .ok_or_else(|| status(format!("begin index: {begin} is out of range 0 - {last}")))?;
    if length < -1 {
        return Err(status(format!("length: {length} should be -1 or greater")));
    }
    let count = usize::try_from(length).unwrap_or(usize::MAX);
    Ok(list::join(items.iter().skip(start).take(count)))
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
        return Err(status(cat(&[
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
        status(cat(&[error, b" \"", pattern, b"\"."]))
    })
}

pub(super) fn matches(re: &Regex, input: &[u8], budget: &mut u64) -> Result<bool, CmdError> {
    re.find_at(input, 0, MatchOptions::default(), budget)
        .map(|caps| caps.is_some())
        .map_err(|err| status(err.to_string()))
}

fn filter(
    cx: &mut Cx<'_>,
    name: &[u8],
    args: &[ExpandedArg],
    defined: bool,
    items: Vec<BString>,
) -> Result<(), CmdError> {
    let Some((operator, tail)) = args.split_first() else {
        return Err(status(
            "sub-command FILTER requires an operator to be specified.",
        ));
    };
    let Some((mode, operand)) = tail.split_first() else {
        return Err(status(
            "sub-command FILTER requires a mode to be specified.",
        ));
    };
    let include = match bytes(operator) {
        b"INCLUDE" => true,
        b"EXCLUDE" => false,
        bad => {
            return Err(status(cat(&[
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
            return Err(status(cat(&[
                b"sub-command FILTER does not recognize mode ",
                bad,
            ])));
        }
    };
    let [operand] = operand else {
        return Err(status(cat(&[
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
