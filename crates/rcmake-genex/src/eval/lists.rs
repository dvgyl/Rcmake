use std::collections::HashSet;

use rcmake_base::{
    BString, list,
    list_ops::{self, SortCompare, SortOptions},
};

use super::{
    Call, Evaluator, Value, boolean,
    strings::{RegexSearch, convert},
};
use crate::{GenexError, GenexNode};

type Result<T> = std::result::Result<T, GenexError>;

#[derive(Default)]
struct ListArguments<'a> {
    values: Vec<BString>,
    bodies: Vec<(usize, &'a [GenexNode])>,
}

enum TransformSelector {
    All,
    At(Vec<i64>),
    For { start: i64, stop: i64, step: usize },
    Regex(rcmake_regex::Regex),
    PredicateMatches(Vec<usize>),
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
                options.last().is_some_and(|opt| opt == "COMPARATOR")
            }
            [op, _, action, options @ ..] if op == "TRANSFORM" => {
                (action == "APPLY" && options.is_empty())
                    || (transform_arity(action).is_some_and(|arity| options.len() == arity + 1)
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
                // CMP0085: the OLD behavior never finds an empty item. The NEW
                // behavior keeps empty items, so an empty list has one item.
                let policy = rcmake_diag::PolicyId::new(85);
                let status = self.host.directory_policy_status(policy, call.ctx);
                if matches!(
                    status,
                    Some(rcmake_diag::PolicyStatus::Old | rcmake_diag::PolicyStatus::Warn)
                ) {
                    self.flags.context_sensitive = true;
                    if status == Some(rcmake_diag::PolicyStatus::Warn)
                        && value.is_empty()
                        && (input.is_empty()
                            || list::iter(&input, list::Empty::Keep).any(|item| item.is_empty()))
                    {
                        let detail = [
                            b"Search Item:\n  \"\"\nList:\n  \"".as_slice(),
                            &input,
                            b"\"\n",
                        ]
                        .concat();
                        self.host.policy_warning(policy, &detail, call.ctx);
                    }
                    let found = list::iter(&input, list::Empty::Drop)
                        .any(|item| item.as_ref() == value.as_slice());
                    return Ok(boolean(found));
                }
                let found = if input.is_empty() {
                    value.is_empty()
                } else {
                    list::iter(&input, list::Empty::Keep)
                        .any(|item| item.as_ref() == value.as_slice())
                };
                return Ok(boolean(found));
            }
            b"JOIN" => {
                let [input, glue] = self.fixed(call)?;
                return self.join(list::iter(&input, list::Empty::Drop), &glue);
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
                self.expand_parameter_into(nodes, call.ctx, &mut args.values)?;
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
                let indexes = flatten(rest)
                    .map(|index| parse_item_index(call, &index))
                    .collect::<Result<Vec<_>>>()?;
                if items.is_empty()
                    && let Some(index) = indexes.first()
                {
                    return Err(call.error(format!("index: {index} out of range (0, 0)")));
                }
                let positions = indexes
                    .into_iter()
                    .map(|index| checked_item_index(call, index, items.len(), false))
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
                let body = if kind == "PREDICATE" {
                    Some(args.body(4).ok_or_else(|| {
                        call.error(
                            "sub-command FILTER, PREDICATE expects a single <body> argument.",
                        )
                    })?)
                } else {
                    None
                };
                self.filter(call, &mut items, mode, pattern, body)?;
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

    pub(super) fn join<T: AsRef<[u8]>>(
        &self,
        items: impl IntoIterator<Item = T>,
        separator: &[u8],
    ) -> Value {
        let mut out = BString::default();
        for (index, item) in items.into_iter().enumerate() {
            if index != 0 {
                self.append(&mut out, separator)?;
            }
            self.append(&mut out, item.as_ref())?;
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
                "sub-command FILTER does not recognize operator \"{}\". It must be either INCLUDE or EXCLUDE.",
                BString::from(mode),
            ))),
            _ => {
                return Err(call.error(format!(
                    "$<{}:...> second parameter must be either INCLUDE or EXCLUDE",
                    BString::from(call.name)
                )));
            }
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
                self.bound_boolean(call, body, vec![item.clone()], "FILTER, PREDICATE")?
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

    fn bound_boolean(
        &mut self,
        call: Call<'_>,
        body: &[GenexNode],
        values: Vec<BString>,
        operation: &str,
    ) -> Result<bool> {
        let value = self.bound(body, call.ctx, values)?;
        self.charge(value.len().max(1))?;
        match value.as_slice() {
            b"0" => Ok(false),
            b"1" => Ok(true),
            _ => Err(call.error(format!(
                "sub-command {operation} body must evaluate to \"0\" or \"1\", but evaluated to \"{value}\"."
            ))),
        }
    }

    fn transform(
        &mut self,
        call: Call<'_>,
        items: &mut [BString],
        action: &BString,
        params: &[BString],
        args: &ListArguments<'_>,
    ) -> Result<()> {
        let Some(arity) = transform_arity(action) else {
            if items.is_empty() {
                return Ok(());
            }
            return Err(call.error(format!(" sub-command TRANSFORM, {action} invalid action.")));
        };
        let apply_body = if action == "APPLY" {
            Some(args.body(3).ok_or_else(|| {
                call.error("sub-command TRANSFORM, action APPLY expects a <body> argument.")
            })?)
        } else {
            None
        };
        let Some((operands, selector)) = params.split_at_checked(arity) else {
            if items.is_empty() {
                return Ok(());
            }
            return Err(call.error(format!(
                "sub-command TRANSFORM, action {action} expects {arity} argument(s)."
            )));
        };
        let predicate = selector.first().is_some_and(|kind| kind == "PREDICATE");
        if predicate && selector.len() > 2 {
            return Err(call.error(
                "sub-command TRANSFORM, selector PREDICATE expects a single <body> argument and cannot be combined with another selector.",
            ));
        }
        let predicate_body = if predicate {
            Some(args.body(arity + 4).ok_or_else(|| {
                call.error("sub-command TRANSFORM, selector PREDICATE expects a <body> argument.")
            })?)
        } else {
            None
        };
        if items.is_empty() {
            return Ok(());
        }
        let selector = if let Some(body) = predicate_body {
            let mut selected = Vec::new();
            for (index, item) in items.iter().enumerate() {
                if self.bound_boolean(call, body, vec![item.clone()], "TRANSFORM, PREDICATE")? {
                    selected.push(index);
                }
            }
            if selected.is_empty() {
                return Ok(());
            }
            TransformSelector::PredicateMatches(selected)
        } else {
            parse_selector(call, selector)?
        };
        let replace = if let (b"REPLACE", [pattern, replacement]) = (action.as_slice(), operands) {
            let regex = list_regex(
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
            self.flags.context_sensitive = true;
            let bol_at_start = self.host.directory_policy(b"CMP0186", call.ctx) != Some(true);
            Some((regex, pattern, replacement, bol_at_start))
        } else {
            None
        };
        let unique_indexes = apply_body.is_some() && matches!(selector, TransformSelector::At(_));
        let mut selected = self.selected(call, items, selector)?;
        if unique_indexes {
            selected.sort_unstable();
            selected.dedup();
        }
        for index in selected {
            let Some(item) = items.get_mut(index) else {
                continue;
            };
            let transformed = match (action.as_slice(), operands, &replace, apply_body) {
                (b"APPEND", [value], _, _) => {
                    self.charge(item.len().saturating_add(value.len()))?;
                    self.append(item, value)?;
                    continue;
                }
                (b"PREPEND", [value], _, _) => {
                    let mut out = value.clone();
                    self.append(&mut out, item)?;
                    out
                }
                (b"TOLOWER" | b"TOUPPER", _, _, _) => convert(action, item),
                (b"STRIP", _, _, _) => list_ops::strip_spaces(item).into(),
                (_, _, Some((regex, pattern, replacement, bol_at_start)), _) => self
                    .regex_output_compiled(
                        call,
                        regex,
                        item,
                        Some(replacement),
                        RegexSearch::ListReplace {
                            bol_at_start: *bol_at_start,
                            pattern,
                        },
                    )?,
                (_, _, _, Some(body)) => self.bound(body, call.ctx, vec![item.clone()])?,
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
        selector: TransformSelector,
    ) -> Result<Vec<usize>> {
        match selector {
            TransformSelector::All => Ok((0..items.len()).collect()),
            TransformSelector::At(indexes) => indexes
                .into_iter()
                .map(|index| selector_index(call, b"AT", index, items.len()))
                .collect(),
            TransformSelector::For { start, stop, step } => {
                let start = selector_index(call, b"FOR", start, items.len())?;
                let stop = selector_index(call, b"FOR", stop, items.len())?;
                if start > stop {
                    return Err(call.error(format!("sub-command TRANSFORM, selector FOR expects <start> to be no greater than <stop> ({start} > {stop})")));
                }
                Ok((start..=stop).step_by(step).collect())
            }
            TransformSelector::Regex(regex) => {
                let mut selected = Vec::new();
                for (index, item) in items.iter().enumerate() {
                    if self.regex_matches(call, &regex, item)? {
                        selected.push(index);
                    }
                }
                Ok(selected)
            }
            TransformSelector::PredicateMatches(selected) => Ok(selected),
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
        let comparator = options.iter().position(|option| option == "COMPARATOR");
        let body = comparator
            .map(|index| {
                args.body(index + 3).ok_or_else(|| {
                    call.error("sub-command SORT, COMPARATOR expects a <body> argument.")
                })
            })
            .transpose()?;
        for (index, option) in options.iter().enumerate() {
            if comparator.is_some_and(|start| index == start || index == start + 1) {
                continue;
            }
            let mut parts = option.splitn(2, |&byte| byte == b':');
            let key = parts.next().unwrap_or_default();
            let Some(value) = parts
                .next()
                .filter(|_| matches!(key, b"COMPARE" | b"CASE" | b"ORDER"))
            else {
                return Err(
                    call.error(format!("sub-command SORT, option \"{option}\" is invalid."))
                );
            };
            if comparator.is_some() && key == b"COMPARE" {
                return Err(call.error(
                    "sub-command SORT, option \"COMPARE\" is incompatible with \"COMPARATOR\".",
                ));
            }
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
                _ => {
                    return Err(call.error(format!(
                        "sub-command SORT, an invalid {} option has been specified: \"{}\".",
                        BString::from(key),
                        BString::from(value),
                    )));
                }
            }
        }
        if let Some(body) = body {
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
        if items.len() < 2 {
            return Ok(());
        }
        // Merge runs without assuming that the user callback defines a valid order.
        let mut compare = |first: &BString, second: &BString| {
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
            self.bound_boolean(call, body, operands, "SORT, COMPARATOR")
        };
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
                    let precedes = compare(first, second)?;
                    if precedes && compare(second, first)? {
                        return Err(call.error("sub-command SORT, COMPARATOR: function does not induce a strict weak ordering. The comparator returned TRUE for both (a, b) and (b, a)."));
                    }
                    let next = if precedes { right.next() } else { left.next() };
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

fn parse_selector(call: Call<'_>, selector: &[BString]) -> Result<TransformSelector> {
    match selector {
        [] => Ok(TransformSelector::All),
        [kind, indexes @ ..] if kind == "AT" => {
            let indexes = indexes
                .iter()
                .flat_map(|index| list::iter(index, list::Empty::Drop))
                .map(|index| {
                    list_ops::parse_index(&index).ok_or_else(|| {
                        call.error(format!(
                            "sub-command TRANSFORM, selector AT: '{}': unexpected argument.",
                            BString::from(index.into_owned()),
                        ))
                    })
                })
                .collect::<Result<Vec<_>>>()?;
            if indexes.is_empty() {
                return Err(call.error(
                    "sub-command TRANSFORM, selector AT expects at least one numeric value.",
                ));
            }
            Ok(TransformSelector::At(indexes))
        }
        [kind, operands @ ..] if kind == "FOR" => {
            let [start, stop, tail @ ..] = operands else {
                return Err(call.error(
                    "sub-command TRANSFORM, selector FOR expects, at least, two arguments.",
                ));
            };
            let (Some(start), Some(stop)) =
                (list_ops::parse_index(start), list_ops::parse_index(stop))
            else {
                return Err(call.error(
                    "sub-command TRANSFORM, selector FOR expects, at least, two numeric values.",
                ));
            };
            let step = match tail.first() {
                Some(step) => list_ops::parse_index(step)
                    .and_then(|n| usize::try_from(n).ok())
                    .filter(|&n| n > 0)
                    .ok_or_else(|| call.error("sub-command TRANSFORM, selector FOR expects positive numeric value for <step>."))?,
                None => 1,
            };
            reject_selector_tail(call, kind, tail.get(1..).unwrap_or_default())?;
            Ok(TransformSelector::For { start, stop, step })
        }
        [kind, operands @ ..] if kind == "REGEX" => {
            let [pattern, tail @ ..] = operands else {
                return Err(call.error(
                    "sub-command TRANSFORM, selector REGEX expects 'regular expression' argument.",
                ));
            };
            let regex = list_regex(
                call,
                pattern,
                "sub-command TRANSFORM, selector REGEX failed",
            )?;
            reject_selector_tail(call, kind, tail)?;
            Ok(TransformSelector::Regex(regex))
        }
        _ => reject_selector_tail(call, b"", selector).map(|()| TransformSelector::All),
    }
}

fn reject_selector_tail(call: Call<'_>, selector: &[u8], tail: &[BString]) -> Result<()> {
    let Some(first) = tail.first() else {
        return Ok(());
    };
    if !selector.is_empty() && matches!(first.as_slice(), b"REGEX" | b"AT" | b"FOR") {
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

fn selector_index(call: Call<'_>, selector: &[u8], index: i64, len: usize) -> Result<usize> {
    checked_item_index(call, index, len, false).map_err(|error| match error {
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
    checked_item_index(call, parse_item_index(call, value)?, len, append)
}

fn parse_item_index(call: Call<'_>, value: &BString) -> Result<i64> {
    list_ops::parse_index(value)
        .ok_or_else(|| call.error(format!("index: \"{value}\" is not a valid index")))
}

fn checked_item_index(call: Call<'_>, index: i64, len: usize, append: bool) -> Result<usize> {
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
