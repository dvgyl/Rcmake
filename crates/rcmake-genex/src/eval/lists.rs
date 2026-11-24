use std::collections::HashSet;

use rcmake_base::{
    BString, list,
    list_ops::{self, SortCompare, SortOptions},
};

use super::{Call, Evaluator, Value, boolean, strict_bool, strings::convert};
use crate::{GenexError, GenexNode};

type Result<T> = std::result::Result<T, GenexError>;

#[derive(Default)]
struct ListArguments<'a> {
    values: Vec<BString>,
    bodies: Vec<(usize, &'a [GenexNode])>,
}

impl<'a> ListArguments<'a> {
    fn body(&self, index: usize) -> Option<&'a [GenexNode]> {
        self.bodies
            .iter()
            .find_map(|&(n, body)| (n == index).then_some(body))
    }

    fn takes_body(&self) -> bool {
        match self.values.as_slice() {
            [op, _, _, mode] if op == "FILTER" => mode == "PREDICATE",
            [op, _, options @ ..] if op == "SORT" => {
                self.bodies.is_empty() && options.last().is_some_and(|opt| opt == "COMPARATOR")
            }
            [op, _, action, options @ ..] if op == "TRANSFORM" => {
                let fixed = transform_arity(action).unwrap_or_default();
                (action == "APPLY" && options.is_empty())
                    || (options.len() == fixed + 1
                        && options.last().is_some_and(|opt| opt == "PREDICATE"))
            }
            _ => false,
        }
    }
}

impl Evaluator<'_> {
    pub(super) fn list(&mut self, call: Call<'_>) -> Value {
        match call.name {
            b"IN_LIST" => {
                let [value, input] = self.fixed(call)?;
                return Ok(boolean(
                    list::split(&input, list::Empty::Keep).contains(&value),
                ));
            }
            b"JOIN" => {
                let [input, glue] = self.fixed(call)?;
                return self.join(list::split(&input, list::Empty::Drop), &glue);
            }
            b"REMOVE_DUPLICATES" => {
                let [input] = self.fixed(call)?;
                let mut items = list::split(&input, list::Empty::Keep);
                deduplicate(&mut items);
                return self.join(items, b";");
            }
            b"FILTER" => {
                let [input, mode, pattern] = self.fixed(call)?;
                let mut items = list::split(&input, list::Empty::Keep);
                self.filter(call, &mut items, &mode, &pattern, None)?;
                return self.join(items, b";");
            }
            _ => {}
        }

        let mut args = ListArguments::default();
        for nodes in call.expr.params.iter().flatten() {
            if args.takes_body() {
                args.bodies.push((args.values.len(), nodes));
                args.values.push(BString::default());
            } else {
                args.values
                    .extend(self.expanded_parameter(nodes, call.ctx)?);
            }
        }
        let [op, input, rest @ ..] = args.values.as_slice() else {
            return Err(call.error("$<LIST> expression requires at least two parameters."));
        };
        validate_arity(call, op, rest)?;
        let mut items = list::split(input, list::Empty::Keep);
        match (op.as_slice(), rest) {
            (b"LENGTH", []) => return Ok(items.len().to_string().into()),
            (b"FIND", [needle]) => {
                return Ok(items
                    .iter()
                    .position(|item| item == needle)
                    .map_or_else(|| "-1".into(), |index| index.to_string().into()));
            }
            (b"JOIN", [glue]) => return self.join(items, glue),
            (b"GET", [_, ..]) => {
                let mut output = BString::default();
                for (position, argument) in flatten(rest).enumerate() {
                    let index = item_index(call, &argument, items.len(), false)?;
                    let item = items
                        .get(index)
                        .ok_or_else(|| call.error("given empty list"))?;
                    if position != 0 {
                        self.append(&mut output, b";")?;
                    }
                    self.append(&mut output, item)?;
                }
                return Ok(output);
            }
            (b"SUBLIST", [begin, length]) => {
                if items.is_empty() {
                    return Ok(BString::default());
                }
                let begin = nonnegative_index(call, begin, items.len())?;
                let length = count(call, length, items.len() - begin)?;
                let mut selected = items.into_iter().skip(begin).take(length);
                return self.join(&mut selected, b";");
            }
            (b"APPEND", [_, ..]) => items.extend(flatten(rest)),
            (b"PREPEND", [_, ..]) => items.splice(..0, flatten(rest)).for_each(drop),
            (b"INSERT", [index, input @ ..]) if !input.is_empty() => {
                let index = item_index(call, index, items.len(), true)?;
                items.splice(index..index, flatten(input)).for_each(drop);
            }
            (b"POP_BACK", []) => {
                items.pop();
            }
            (b"POP_FRONT", []) => {
                items.drain(..1.min(items.len()));
            }
            (b"REMOVE_ITEM", [_, ..]) => {
                let excluded: HashSet<_> = flatten(rest).collect();
                items.retain(|item| !excluded.contains(item));
            }
            (b"REMOVE_AT", [_, ..]) => {
                let indexes = flatten(rest);
                let positions = indexes
                    .map(|index| {
                        if items.is_empty() && index == "0" {
                            return Err(call.error("index: 0 out of range (0, 0)"));
                        }
                        item_index(call, &index, items.len(), false)
                    })
                    .collect::<Result<HashSet<_>>>()?;
                items = items
                    .into_iter()
                    .enumerate()
                    .filter_map(|(i, item)| (!positions.contains(&i)).then_some(item))
                    .collect();
            }
            (b"REMOVE_DUPLICATES", []) => deduplicate(&mut items),
            (b"FILTER", [mode, pattern])
                if !matches!(pattern.as_slice(), b"REGEX" | b"PREDICATE") =>
            {
                self.filter(call, &mut items, mode, pattern, None)?;
            }
            (b"FILTER", [mode, kind, pattern])
                if matches!(kind.as_slice(), b"REGEX" | b"PREDICATE") =>
            {
                self.filter(call, &mut items, mode, pattern, args.body(4))?;
            }
            (b"TRANSFORM", [action, params @ ..]) => {
                self.transform(call, &mut items, action, params, &args)?;
            }
            (b"REVERSE", []) => items.reverse(),
            (b"SORT", options) => self.sort(call, &mut items, options, &args)?,
            _ => return Err(call.error(format!("Invalid arguments for $<LIST:{op}>."))),
        }
        self.join(items, b";")
    }

    pub(super) fn join(&self, items: impl IntoIterator<Item = BString>, separator: &[u8]) -> Value {
        let mut out = BString::default();
        for (index, item) in items.into_iter().enumerate() {
            if index != 0 {
                self.append(&mut out, separator)?;
            }
            self.append(&mut out, &item)?;
        }
        Ok(out)
    }

    fn filter(
        &mut self,
        call: Call<'_>,
        items: &mut Vec<BString>,
        mode: &[u8],
        pattern: &[u8],
        body: Option<&[GenexNode]>,
    ) -> Result<()> {
        let include = match mode {
            b"INCLUDE" => true,
            b"EXCLUDE" => false,
            _ if call.name == b"LIST" => return Err(call.error(format!(
                "sub-command FILTER does not recognize operator \"{}\".  It must be either INCLUDE or EXCLUDE.",
                BString::from(mode),
            ))),
            _ => return Err(call.error("The filter mode must be INCLUDE or EXCLUDE.")),
        };
        let regex = if body.is_none() {
            Some(if call.name == b"LIST" {
                list_regex(call, pattern, "sub-command FILTER, failed")?
            } else {
                self.regex(call, pattern)?
            })
        } else {
            None
        };
        let mut kept = Vec::new();
        for item in items.drain(..) {
            let matched = if let Some(body) = body {
                let value = self.bound(body, call.ctx, vec![item.clone()])?;
                strict_bool(call, &value)?
            } else if let Some(regex) = &regex {
                self.regex_matches(call, regex, &item)?
            } else {
                false
            };
            if matched == include {
                kept.push(item);
            }
        }
        *items = kept;
        Ok(())
    }

    fn transform(
        &mut self,
        call: Call<'_>,
        items: &mut [BString],
        action: &BString,
        params: &[BString],
        args: &ListArguments<'_>,
    ) -> Result<()> {
        let arity = transform_arity(action).ok_or_else(|| {
            call.error(format!("sub-command TRANSFORM, {action} invalid action."))
        })?;
        if params.len() < arity {
            return Err(call.error(format!(
                "sub-command TRANSFORM, action {action} expects {arity} argument(s)."
            )));
        }
        let (operands, selector) = params.split_at(arity);
        if let (b"REPLACE", [pattern, replacement]) = (action.as_slice(), operands) {
            let _ = list_regex(
                call,
                pattern,
                "sub-command TRANSFORM, action REPLACE: Failed",
            )?;
            let _ = rcmake_regex::Replacement::parse(replacement).map_err(|error| {
                let message = error
                    .message
                    .strip_prefix("string sub-command REGEX, mode REPLACE: ")
                    .unwrap_or(&error.message);
                call.error(
                    [
                        b"sub-command TRANSFORM, action REPLACE: ".as_slice(),
                        message.as_bytes(),
                    ]
                    .concat(),
                )
            })?;
        }
        let selected = self.selected(call, items, selector, args.body(arity + 4))?;
        for index in selected {
            let Some(item) = items.get_mut(index) else {
                continue;
            };
            let transformed = match (action.as_slice(), operands) {
                (b"APPEND", [value]) => {
                    self.charge(item.len().saturating_add(value.len()))?;
                    self.append(item, value)?;
                    continue;
                }
                (b"PREPEND", [value]) => {
                    let mut out = value.clone();
                    self.append(&mut out, item)?;
                    out
                }
                (b"TOLOWER" | b"TOUPPER", _) => convert(action, item),
                (b"STRIP", _) => list_ops::strip_spaces(item).into(),
                (b"REPLACE", [pattern, replacement]) => {
                    self.regex_output(call, item, pattern, Some(replacement), true)?
                }
                (b"APPLY", [_]) => {
                    let body = args
                        .body(3)
                        .ok_or_else(|| call.error("APPLY requires a body."))?;
                    self.bound(body, call.ctx, vec![item.clone()])?
                }
                _ => return Err(call.error("Invalid transformation operands.")),
            };
            self.charge(transformed.len().max(1))?;
            *item = transformed;
        }
        Ok(())
    }

    fn selected(
        &mut self,
        call: Call<'_>,
        items: &[BString],
        selector: &[BString],
        body: Option<&[GenexNode]>,
    ) -> Result<Vec<usize>> {
        match selector {
            [] => Ok((0..items.len()).collect()),
            [kind, indexes @ ..] if kind == "AT" => {
                let indexes = flatten(indexes).collect::<Vec<_>>();
                if indexes.is_empty() {
                    return Err(call.error(
                        "sub-command TRANSFORM, selector AT expects at least one numeric value.",
                    ));
                }
                indexes.iter().map(|index| {
                    if list_ops::parse_index(index).is_none() {
                        return Err(call.error(format!("sub-command TRANSFORM, selector AT: '{index}': unexpected argument.")));
                    }
                    selector_index(call, kind, index, items.len())
                }).collect()
            }
            [kind, operands @ ..] if kind == "FOR" => {
                let [start, stop, tail @ ..] = operands else {
                    return Err(call.error(
                        "sub-command TRANSFORM, selector FOR expects, at least, two arguments.",
                    ));
                };
                if list_ops::parse_index(start).is_none() || list_ops::parse_index(stop).is_none() {
                    return Err(call.error("sub-command TRANSFORM, selector FOR expects, at least, two numeric values."));
                }
                let step = match tail.first() {
                    Some(step) => list_ops::parse_index(step)
                        .and_then(|n| usize::try_from(n).ok())
                        .filter(|&n| n > 0)
                        .ok_or_else(|| call.error("sub-command TRANSFORM, selector FOR expects positive numeric value for <step>."))?,
                    None => 1,
                };
                reject_selector_tail(call, kind, tail.get(1..).unwrap_or_default())?;
                let start = selector_index(call, kind, start, items.len())?;
                let stop = selector_index(call, kind, stop, items.len())?;
                if start > stop {
                    return Err(call.error(format!("sub-command TRANSFORM, selector FOR expects <start> to be no greater than <stop> ({start} > {stop})")));
                }
                Ok((start..=stop).step_by(step).collect())
            }
            [kind, options @ ..] if kind == "REGEX" => {
                let [pattern, tail @ ..] = options else {
                    return Err(call.error("sub-command TRANSFORM, selector REGEX expects 'regular expression' argument."));
                };
                reject_selector_tail(call, kind, tail)?;
                let regex = list_regex(
                    call,
                    pattern,
                    "sub-command TRANSFORM, selector REGEX failed",
                )?;
                let mut selected = Vec::new();
                for (index, item) in items.iter().enumerate() {
                    if self.regex_matches(call, &regex, item)? {
                        selected.push(index);
                    }
                }
                Ok(selected)
            }
            [kind, _] if kind == "PREDICATE" => {
                let body = body.ok_or_else(|| call.error("PREDICATE requires a body."))?;
                let mut selected = Vec::new();
                for (index, item) in items.iter().enumerate() {
                    let value = self.bound(body, call.ctx, vec![item.clone()])?;
                    if strict_bool(call, &value)? {
                        selected.push(index);
                    }
                }
                Ok(selected)
            }
            _ => reject_selector_tail(call, b"", selector).map(|()| Vec::new()),
        }
    }

    fn sort(
        &mut self,
        call: Call<'_>,
        items: &mut [BString],
        options: &[BString],
        args: &ListArguments<'_>,
    ) -> Result<()> {
        let mut parsed = SortOptions::default();
        let mut seen = HashSet::new();
        let mut body = None;
        let mut options = options.iter().enumerate();
        while let Some((i, option)) = options.next() {
            let mut parts = option.splitn(2, |&byte| byte == b':');
            let key = parts.next().unwrap_or_default();
            let value = parts.next().unwrap_or_default();
            if !seen.insert(key) {
                return Err(call.error(format!(
                    "sub-command SORT, {} option has been specified multiple times.",
                    BString::from(key)
                )));
            }
            match (key, value) {
                (b"COMPARE", b"STRING") => parsed.compare = SortCompare::String,
                (b"COMPARE", b"FILE_BASENAME") => parsed.compare = SortCompare::FileBasename,
                (b"COMPARE", b"NATURAL") => parsed.compare = SortCompare::Natural,
                (b"CASE", b"SENSITIVE") => parsed.insensitive = false,
                (b"CASE", b"INSENSITIVE") => parsed.insensitive = true,
                (b"ORDER", b"ASCENDING") => parsed.descending = false,
                (b"ORDER", b"DESCENDING") => parsed.descending = true,
                (b"COMPARATOR", b"") => {
                    options.next();
                    body = Some(
                        args.body(i + 3)
                            .ok_or_else(|| call.error("COMPARATOR requires a body."))?,
                    );
                }
                (b"COMPARE" | b"CASE" | b"ORDER", _) => {
                    return Err(call.error(format!(
                        "sub-command SORT, an invalid {} option has been specified: \"{}\".",
                        BString::from(key),
                        BString::from(value),
                    )));
                }
                _ => {
                    return Err(
                        call.error(format!("sub-command SORT, option \"{option}\" is invalid."))
                    );
                }
            }
        }
        if let Some(body) = body {
            if seen.contains(b"COMPARE".as_slice()) {
                return Err(call.error("COMPARATOR cannot be combined with COMPARE."));
            }
            return self.sort_bound(call, items, parsed, body);
        }
        list_ops::sort_items(items, parsed);
        Ok(())
    }

    fn sort_bound(
        &mut self,
        call: Call<'_>,
        items: &mut [BString],
        options: SortOptions,
        body: &[GenexNode],
    ) -> Result<()> {
        // Merge runs without assuming that the user callback defines a valid order.
        let mut order: Vec<_> = items.iter().collect();
        let mut merged = Vec::with_capacity(order.len());
        let mut width = 1;
        while width < order.len() {
            for run in order.chunks(width * 2) {
                let (left, right) = run.split_at(width.min(run.len()));
                let (mut left, mut right) = (
                    left.iter().copied().peekable(),
                    right.iter().copied().peekable(),
                );
                while let (Some(&left_item), Some(&right_item)) = (left.peek(), right.peek()) {
                    let (first, second) = if options.descending {
                        (left_item, right_item)
                    } else {
                        (right_item, left_item)
                    };
                    let operands = [first, second]
                        .into_iter()
                        .map(|item| {
                            if options.insensitive {
                                item.to_ascii_lowercase().into()
                            } else {
                                item.clone()
                            }
                        })
                        .collect();
                    let value = self.bound(body, call.ctx, operands)?;
                    let next = if strict_bool(call, &value)? {
                        right.next()
                    } else {
                        left.next()
                    };
                    merged.extend(next);
                }
                merged.extend(left);
                merged.extend(right);
            }
            std::mem::swap(&mut order, &mut merged);
            merged.clear();
            width *= 2;
        }
        let sorted: Vec<_> = order.into_iter().cloned().collect();
        items.clone_from_slice(&sorted);
        Ok(())
    }
}

fn validate_arity(call: Call<'_>, operation: &BString, rest: &[BString]) -> Result<()> {
    let (count, exact) = match operation.as_slice() {
        b"LENGTH" | b"POP_BACK" | b"POP_FRONT" | b"REMOVE_DUPLICATES" | b"REVERSE" => (1, true),
        b"FIND" | b"JOIN" => (2, true),
        b"SUBLIST" => (3, true),
        b"FILTER" if rest.get(1).is_some_and(|kind| kind == "PREDICATE") => {
            return if rest.len() == 3 {
                Ok(())
            } else {
                Err(call.error("sub-command FILTER, PREDICATE expects a single <body> argument."))
            };
        }
        b"FILTER" if rest.get(1).is_some_and(|kind| kind == "REGEX") => (4, true),
        b"FILTER" => (3, true),
        b"GET" | b"APPEND" | b"PREPEND" | b"REMOVE_ITEM" | b"REMOVE_AT" | b"TRANSFORM" => {
            (2, false)
        }
        b"INSERT" => (3, false),
        b"SORT" => return Ok(()),
        _ => return Err(call.error(format!("{operation}: invalid option."))),
    };
    let supplied = rest.len() + 1;
    if supplied == count || !exact && supplied > count {
        return Ok(());
    }
    let quantifier = if exact { "exactly" } else { "at least" };
    let requirement = match count {
        1 => "one parameter",
        2 => "two parameters",
        3 => "three parameters",
        _ => "four parameters",
    };
    Err(call.error(format!(
        "$<LIST:{operation}> expression requires {quantifier} {requirement}."
    )))
}

fn transform_arity(action: &[u8]) -> Option<usize> {
    Some(match action {
        b"APPEND" | b"PREPEND" | b"APPLY" => 1,
        b"REPLACE" => 2,
        b"TOLOWER" | b"TOUPPER" | b"STRIP" => 0,
        _ => return None,
    })
}

fn reject_selector_tail(call: Call<'_>, selector: &[u8], tail: &[BString]) -> Result<()> {
    let Some(first) = tail.first() else {
        return Ok(());
    };
    if !selector.is_empty() && matches!(first.as_slice(), b"REGEX" | b"AT" | b"FOR" | b"PREDICATE")
    {
        return Err(call.error(format!(
            "sub-command TRANSFORM, selector already specified ({}).",
            BString::from(selector)
        )));
    }
    let rest = tail
        .iter()
        .map(|token| token.to_string())
        .collect::<Vec<_>>()
        .join(", ");
    Err(call.error(format!(
        "sub-command TRANSFORM, '{rest}': unexpected argument(s)."
    )))
}

fn selector_index(call: Call<'_>, selector: &[u8], index: &BString, len: usize) -> Result<usize> {
    item_index(call, index, len, false).map_err(|error| match error {
        GenexError::Message { message, .. } => call.error(format!(
            "sub-command TRANSFORM, selector {}, {message}.",
            BString::from(selector),
        )),
        error => error,
    })
}

fn list_regex(call: Call<'_>, pattern: &[u8], prefix: &str) -> Result<rcmake_regex::Regex> {
    rcmake_regex::Regex::new(pattern).map_err(|_| {
        call.error(format!(
            "{prefix} to compile regex \"{}\".",
            BString::from(pattern)
        ))
    })
}

fn flatten(values: &[BString]) -> impl Iterator<Item = BString> {
    values.iter().flat_map(|value| {
        let mut items = Vec::new();
        list::split_with_empty_input(
            value,
            list::Empty::Keep,
            list::EmptyInput::OneEmptyElement,
            &mut items,
        );
        items
    })
}

fn deduplicate(items: &mut Vec<BString>) {
    let mut seen = HashSet::new();
    items.retain(|item| seen.insert(item.clone()));
}

fn item_index(call: Call<'_>, value: &BString, len: usize, append: bool) -> Result<usize> {
    let index = list_ops::parse_index(value)
        .ok_or_else(|| call.error(format!("index: \"{value}\" is not a valid index")))?;
    if len == 0 && !append {
        return Err(call.error("given empty list"));
    }
    let end = if append { len } else { len.saturating_sub(1) };
    usize::try_from(list_ops::normalize_index(index, len))
        .ok()
        .filter(|&i| i <= end)
        .ok_or_else(|| call.error(format!("index: {index} out of range (-{}, {end})", len)))
}

fn nonnegative_index(call: Call<'_>, value: &BString, len: usize) -> Result<usize> {
    list_ops::parse_index(value)
        .and_then(|n| usize::try_from(n).ok())
        .filter(|&n| n < len)
        .ok_or_else(|| {
            call.error(format!(
                "begin index: {value} is out of range 0 - {}",
                len.saturating_sub(1)
            ))
        })
}

fn count(call: Call<'_>, value: &BString, max: usize) -> Result<usize> {
    match list_ops::parse_index(value) {
        Some(-1) => Ok(max),
        Some(n) if n >= 0 => Ok(usize::try_from(n).unwrap_or(usize::MAX).min(max)),
        _ => Err(call.error(format!("length: {value} should be -1 or greater"))),
    }
}
