use std::borrow::Cow;
use std::io::{Cursor, Write};

use indexmap::IndexMap;
use indexmap::map::Entry;
use rcmake_base::{BString, list};
use rcmake_diag::{Effect, MessageKind, PolicyId, PolicyStatus};
use rcmake_eval::{CmdError, CmdResult, Cx, Flow};
use rcmake_expr::ExpandedArg;

use super::{quoted, unsigned};

enum Keyword<'a> {
    Option(bool),
    One {
        seen: bool,
        value: Option<Cow<'a, [u8]>>,
    },
    Multi(Vec<Cow<'a, [u8]>>),
}

impl Keyword<'_> {
    const fn group_order(&self) -> u8 {
        match self {
            Self::Option(_) => 0,
            Self::One { .. } => 1,
            Self::Multi(_) => 2,
        }
    }
}

enum InputMode<'a> {
    Regular(&'a [ExpandedArg]),
    Caller(Vec<BString>),
}

const ARGV_NAMES: [&[u8]; 16] = [
    b"ARGV0", b"ARGV1", b"ARGV2", b"ARGV3", b"ARGV4", b"ARGV5", b"ARGV6", b"ARGV7", b"ARGV8",
    b"ARGV9", b"ARGV10", b"ARGV11", b"ARGV12", b"ARGV13", b"ARGV14", b"ARGV15",
];

pub(super) fn parse<'a>(cx: &mut Cx<'_>, args: &'a [ExpandedArg]) -> CmdResult {
    let (prefix, lists, input, preserve) = match args {
        [first, index, prefix, options, one, multi] if first.value.as_slice() == b"PARSE_ARGV" => {
            let index = unsigned(&index.value)
                .ok_or_else(|| {
                    quoted(
                        "PARSE_ARGV index '",
                        &index.value,
                        "' is not an unsigned integer",
                    )
                })
                .map_err(|message| crate::command_error(cx, message))?;
            (
                prefix.value.as_slice(),
                [
                    options.value.as_slice(),
                    one.value.as_slice(),
                    multi.value.as_slice(),
                ],
                InputMode::Caller(caller_args(cx, Some(index))?),
                true,
            )
        }
        [first, ..] if first.value.as_slice() == b"PARSE_ARGV" => {
            return Err(crate::command_error(
                cx,
                "PARSE_ARGV must be called with exactly 6 arguments.",
            ));
        }
        [first, prefix, options, one, multi] if first.value.as_slice() == b"PARSE_ARGN" => (
            prefix.value.as_slice(),
            [
                options.value.as_slice(),
                one.value.as_slice(),
                multi.value.as_slice(),
            ],
            InputMode::Caller(caller_args(cx, None)?),
            true,
        ),
        [first, ..] if first.value.as_slice() == b"PARSE_ARGN" => {
            return Err(crate::command_error(
                cx,
                "PARSE_ARGN must be called with exactly 5 arguments.",
            ));
        }
        [prefix, options, one, multi, tail @ ..] => (
            prefix.value.as_slice(),
            [
                options.value.as_slice(),
                one.value.as_slice(),
                multi.value.as_slice(),
            ],
            InputMode::Regular(tail),
            false,
        ),
        _ => {
            return Err(CmdError::status(
                "must be called with at least 4 arguments.",
            ));
        }
    };

    let mut keywords: IndexMap<Cow<'a, [u8]>, Keyword<'a>> = IndexMap::new();
    let kinds: [fn() -> Keyword<'static>; 3] = [
        || Keyword::Option(false),
        || Keyword::One {
            seen: false,
            value: None,
        },
        || Keyword::Multi(Vec::new()),
    ];
    for (names, kind) in lists.into_iter().zip(kinds) {
        if names.is_empty() {
            continue;
        }
        for name in list::iter(names, list::Empty::Drop) {
            match keywords.entry(name) {
                Entry::Vacant(entry) => {
                    entry.insert(kind());
                }
                Entry::Occupied(entry) => {
                    cx.issue(
                        MessageKind::Warning,
                        Effect::None,
                        None,
                        quoted("keyword defined more than once: ", entry.key().as_ref(), ""),
                    );
                }
            }
        }
    }

    let mut pending: Option<(usize, bool)> = None;
    let mut missing_indices: Vec<usize> = Vec::new();
    let mut unparsed: Vec<Cow<'a, [u8]>> = Vec::new();

    let mut feed = |value: Cow<'a, [u8]>| {
        if let Some((idx, _, keyword)) = keywords.get_full_mut(value.as_ref()) {
            if let Some((prev, false)) = pending.take() {
                missing_indices.push(prev);
            }
            match keyword {
                Keyword::Option(present) => *present = true,
                Keyword::One { seen, .. } => {
                    *seen = true;
                    pending = Some((idx, false));
                }
                Keyword::Multi(_) => pending = Some((idx, false)),
            }
            return;
        }
        match pending.take() {
            Some((idx, _)) => match keywords.get_index_mut(idx) {
                Some((_, Keyword::One { value: slot, .. })) => *slot = Some(value),
                Some((_, Keyword::Multi(items))) => {
                    items.push(value);
                    pending = Some((idx, true));
                }
                _ => {}
            },
            None => unparsed.push(value),
        }
    };

    match input {
        InputMode::Regular(tail) => {
            for arg in tail {
                let slice = arg.value.as_slice();
                if slice.is_empty() {
                    continue;
                }
                if !slice.contains(&b';') {
                    feed(Cow::Borrowed(slice));
                } else {
                    for element in list::iter(slice, list::Empty::Drop) {
                        feed(element);
                    }
                }
            }
        }
        InputMode::Caller(values) => {
            for value in values {
                feed(Cow::Owned(Vec::from(value)));
            }
        }
    }
    if let Some((prev, false)) = pending {
        missing_indices.push(prev);
    }

    let missing_value = if missing_indices.is_empty() {
        None
    } else {
        let mut names: Vec<&[u8]> = missing_indices
            .iter()
            .filter_map(|&idx| keywords.get_index(idx).map(|(k, _)| k.as_ref()))
            .collect();
        names.sort_unstable();
        names.dedup();
        Some(list::join(names))
    };

    let policy = cx.policy(PolicyId::new(174));
    let mut var_buf = Vec::with_capacity(prefix.len().saturating_add(32));
    var_buf.extend_from_slice(prefix);
    var_buf.push(b'_');
    let base_len = var_buf.len();

    let mut outputs: Vec<_> = keywords.into_iter().collect();
    outputs.sort_unstable_by(|(a_name, a_kind), (b_name, b_kind)| {
        a_kind
            .group_order()
            .cmp(&b_kind.group_order())
            .then_with(|| a_name.as_ref().cmp(b_name.as_ref()))
    });

    for (keyword, parsed) in outputs {
        var_buf.truncate(base_len);
        var_buf.extend_from_slice(&keyword);
        let value = match parsed {
            Keyword::Option(present) => Some(if present { "TRUE" } else { "FALSE" }.into()),
            Keyword::Multi(list) => output_list(&list, preserve),
            Keyword::One { seen, value } => {
                if let Some(val) = value.filter(|v| !v.is_empty()) {
                    Some(BString::from(val.into_owned()))
                } else if preserve && seen && policy == PolicyStatus::New {
                    Some(BString::default())
                } else {
                    if preserve && seen && policy == PolicyStatus::Warn {
                        crate::author_warning(
                            cx,
                            [
                                b"The ".as_slice(),
                                keyword.as_ref(),
                                b" keyword was followed by an empty string or no value at all.  Policy CMP0174 is not set, so cmake_parse_arguments() will unset the ",
                                var_buf.as_slice(),
                                b" variable rather than setting it to an empty string.",
                            ]
                            .concat(),
                        );
                    }
                    None
                }
            }
        };
        set_result(cx, &var_buf, value);
    }
    var_buf.truncate(base_len);
    var_buf.extend_from_slice(b"UNPARSED_ARGUMENTS");
    set_result(cx, &var_buf, output_list(&unparsed, preserve));
    var_buf.truncate(base_len);
    var_buf.extend_from_slice(b"KEYWORDS_MISSING_VALUES");
    set_result(cx, &var_buf, missing_value);
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
            crate::command_error(
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
    let cap = usize::try_from(end.saturating_sub(start))
        .unwrap_or(0)
        .min(1024);
    let mut input = Vec::with_capacity(cap);
    let mut size = 0_usize;
    let max_bytes = cx.limits().max_value_bytes;
    let mut name_buf = [0u8; 20];
    for i in start..end {
        let name: &[u8] = if let Some(&static_name) = ARGV_NAMES.get(i as usize) {
            static_name
        } else {
            let mut cursor = Cursor::new(name_buf.as_mut_slice());
            let _ = write!(cursor, "ARGV{i}");
            let len = usize::try_from(cursor.position()).unwrap_or(0);
            name_buf.get(..len).unwrap_or_default()
        };
        let value = cx.var(name).ok_or_else(|| {
            crate::command_error(cx, format!("{mode} called with ARGV{i} not set"))
        })?;
        size = size.saturating_add(value.len()).saturating_add(1);
        if size > max_bytes {
            return Err(CmdError::status(
                "argument data exceeds the value size limit.",
            ));
        }
        input.push(value);
    }
    Ok(input)
}

fn output_list(items: &[Cow<'_, [u8]>], preserve: bool) -> Option<BString> {
    let (first, rest) = items.split_first()?;
    if rest.is_empty() && (!preserve || !first.contains(&b';')) {
        return Some(BString::from(first.as_ref()));
    }
    let total = items
        .iter()
        .map(|item| item.len())
        .sum::<usize>()
        .saturating_add(rest.len());
    let mut out = Vec::with_capacity(total);
    for (i, item) in items.iter().enumerate() {
        if i > 0 {
            out.push(b';');
        }
        let bytes = item.as_ref();
        if preserve && bytes.contains(&b';') {
            for &b in bytes {
                if b == b';' {
                    out.push(b'\\');
                }
                out.push(b);
            }
        } else {
            out.extend_from_slice(bytes);
        }
    }
    Some(BString::from(out))
}

fn set_result(cx: &mut Cx<'_>, name: &[u8], value: Option<BString>) {
    if let Some(value) = value {
        cx.set_var(name, value);
    } else {
        cx.unset_var(name);
    }
}
