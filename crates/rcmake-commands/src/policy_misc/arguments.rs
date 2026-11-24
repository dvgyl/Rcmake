use std::collections::{BTreeMap, BTreeSet};

use rcmake_base::{BString, ByteSlice, list};
use rcmake_diag::{DiagCategory, Effect, MessageKind, PolicyId, PolicyStatus};
use rcmake_eval::{CmdError, CmdResult, Cx, Flow};
use rcmake_expr::ExpandedArg;

use super::{quoted, unsigned, values};

enum Keyword {
    Option(bool),
    One { seen: bool, value: Option<BString> },
    Multi(Vec<BString>),
}

pub(super) fn parse(cx: &mut Cx<'_>, args: &[ExpandedArg]) -> CmdResult {
    let args = values(args);
    let (prefix, lists, input, preserve) = match args.as_slice() {
        [b"PARSE_ARGV", index, prefix, options, one, multi] => {
            let index = unsigned(index)
                .ok_or_else(|| quoted("PARSE_ARGV index '", index, "' is not an unsigned integer"))
                .map_err(|message| fatal(cx, message))?;
            (
                *prefix,
                [*options, *one, *multi],
                caller_args(cx, Some(index))?,
                true,
            )
        }
        [b"PARSE_ARGV", ..] => {
            return Err(fatal(
                cx,
                "PARSE_ARGV must be called with exactly 6 arguments.".into(),
            ));
        }
        [b"PARSE_ARGN", prefix, options, one, multi] => (
            *prefix,
            [*options, *one, *multi],
            caller_args(cx, None)?,
            true,
        ),
        [b"PARSE_ARGN", ..] => {
            return Err(fatal(
                cx,
                "PARSE_ARGN must be called with exactly 5 arguments.".into(),
            ));
        }
        [prefix, options, one, multi, tail @ ..] => {
            let input = tail.iter().flat_map(|arg| split(arg)).collect();
            (*prefix, [*options, *one, *multi], input, false)
        }
        _ => {
            return Err(CmdError::status(
                "must be called with at least 4 arguments.",
            ));
        }
    };

    let mut keywords = BTreeMap::new();
    let kinds: [fn() -> Keyword; 3] = [
        || Keyword::Option(false),
        || Keyword::One {
            seen: false,
            value: None,
        },
        || Keyword::Multi(Vec::new()),
    ];
    for (names, kind) in lists.into_iter().zip(kinds) {
        for name in split(names) {
            match keywords.entry(name) {
                std::collections::btree_map::Entry::Vacant(entry) => {
                    entry.insert(kind());
                }
                std::collections::btree_map::Entry::Occupied(entry) => {
                    cx.issue(
                        MessageKind::Warning,
                        Effect::None,
                        None,
                        quoted("keyword defined more than once: ", entry.key(), ""),
                    );
                }
            }
        }
    }

    let mut pending = None;
    let mut missing = BTreeSet::new();
    let mut unparsed = Vec::new();
    for value in input {
        if let Some(keyword) = keywords.get_mut(&value) {
            record_missing(&mut missing, pending.take());
            match keyword {
                Keyword::Option(present) => *present = true,
                Keyword::One { seen, .. } => {
                    *seen = true;
                    pending = Some((value, false));
                }
                Keyword::Multi(_) => pending = Some((value, false)),
            }
            continue;
        }
        match pending.take() {
            Some((name, _)) => match keywords.get_mut(&name) {
                Some(Keyword::One { value: slot, .. }) => *slot = Some(value),
                Some(Keyword::Multi(items)) => {
                    items.push(value);
                    pending = Some((name, true));
                }
                _ => {}
            },
            None => unparsed.push(value),
        }
    }
    record_missing(&mut missing, pending);

    let policy = cx.policy(PolicyId::new(174));
    for (keyword, parsed) in keywords {
        let var = output_name(prefix, &keyword);
        let value = match parsed {
            Keyword::Option(present) => Some(if present { "TRUE" } else { "FALSE" }.into()),
            Keyword::Multi(list) => output_list(&list, preserve),
            Keyword::One { seen, value } => {
                if value.as_ref().is_some_and(|value| !value.is_empty()) {
                    value
                } else if preserve && seen && policy == PolicyStatus::New {
                    Some(BString::default())
                } else {
                    if preserve && seen && policy == PolicyStatus::Warn {
                        cx.issue(
                            MessageKind::AuthorWarning,
                            Effect::None,
                            Some(DiagCategory::Author),
                            [
                                b"The ".as_slice(),
                                keyword.as_slice(),
                                b" keyword was followed by an empty string or no value at all.  Policy CMP0174 is not set, so cmake_parse_arguments() will unset the ",
                                var.as_slice(),
                                b" variable rather than setting it to an empty string.",
                            ].concat().into(),
                        );
                    }
                    None
                }
            }
        };
        set_result(cx, &var, value);
    }
    set_result(
        cx,
        &output_name(prefix, b"UNPARSED_ARGUMENTS"),
        output_list(&unparsed, preserve),
    );
    set_result(
        cx,
        &output_name(prefix, b"KEYWORDS_MISSING_VALUES"),
        (!missing.is_empty()).then(|| list::join(missing)),
    );
    Ok(Flow::Normal)
}

fn caller_args(cx: &mut Cx<'_>, index: Option<u32>) -> Result<Vec<BString>, CmdError> {
    let mode = if index.is_some() {
        "PARSE_ARGV"
    } else {
        "PARSE_ARGN"
    };
    let count = |cx: &mut Cx<'_>, name: &str| {
        let raw = cx.var_or_empty(name.as_bytes());
        unsigned(&raw).ok_or_else(|| {
            fatal(
                cx,
                quoted(
                    &format!("{mode} called with {name}='"),
                    &raw,
                    "' that is not an unsigned integer",
                ),
            )
        })
    };
    let (end, start) = match index {
        Some(index) => (count(cx, "ARGC")?, index),
        None => {
            let extra = count(cx, "ARGNC")?;
            let total = count(cx, "ARGC")?;
            (total, total.wrapping_sub(extra))
        }
    };
    let mut input = Vec::new();
    let mut size = 0_usize;
    for i in start..end {
        let name = format!("ARGV{i}");
        let value = cx
            .var(name.as_bytes())
            .ok_or_else(|| fatal(cx, format!("{mode} called with {name} not set").into()))?;
        size = size.saturating_add(value.len()).saturating_add(1);
        if size > cx.limits().max_value_bytes {
            return Err(CmdError::status(
                "argument data exceeds the value size limit.",
            ));
        }
        input.push(value);
    }
    Ok(input)
}

fn split(input: &[u8]) -> Vec<BString> {
    list::split(input, list::Empty::Drop)
}

fn record_missing(missing: &mut BTreeSet<BString>, pending: Option<(BString, bool)>) {
    if let Some((name, false)) = pending {
        missing.insert(name);
    }
}

fn output_name(prefix: &[u8], suffix: &[u8]) -> BString {
    [prefix, b"_", suffix].concat().into()
}

fn output_list(items: &[BString], preserve: bool) -> Option<BString> {
    if items.is_empty() {
        return None;
    }
    Some(if preserve {
        list::join(items.iter().map(|item| item.replace(b";", b"\\;")))
    } else {
        list::join(items)
    })
}

fn set_result(cx: &mut Cx<'_>, name: &[u8], value: Option<BString>) {
    if let Some(value) = value {
        cx.set_var(name, value);
    } else {
        cx.unset_var(name);
    }
}

fn fatal(cx: &mut Cx<'_>, body: BString) -> CmdError {
    cx.issue(MessageKind::Error, Effect::Fatal, None, body);
    CmdError::Reported
}
