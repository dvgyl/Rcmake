use std::collections::BTreeSet;
use std::sync::Arc;

use rcmake_base::{
    BString,
    list::{Empty, ListBudget},
};
use rcmake_diag::{PolicyId, PolicyStatus};
use rcmake_expr::ExpandedArg;
use rcmake_syntax::CommandInvocation;

use super::BlockResult;
use crate::util::{IntError, cat, stoi};
use crate::{CmdError, CmdResult, Flow, Interp, code::Code};

/// Remaining iteration values for an active `foreach()` loop, used for
/// read-only speculative probe lookahead.
#[derive(Debug)]
enum ForeachRemaining {
    Items {
        items: Arc<Vec<BString>>,
        next: usize,
    },
    Range {
        current_value: i64,
        next_value: i64,
        step: i64,
        remaining: u64,
    },
    Zip {
        lists: Arc<Vec<Vec<BString>>>,
        next: usize,
        max: usize,
    },
}

/// Active `foreach()` loop state exposed read-only via [`crate::Cx::foreach_lookahead`].
#[derive(Debug)]
pub(crate) struct ForeachFrame {
    cmds: Arc<[CommandInvocation]>,
    start: usize,
    end: usize,
    scope_depth: usize,
    vars: Vec<BString>,
    remaining: ForeachRemaining,
}

impl ForeachFrame {
    pub(crate) fn contains_command(&self, cmds: &Arc<[CommandInvocation]>, index: usize) -> bool {
        Arc::ptr_eq(&self.cmds, cmds) && (self.start..self.end).contains(&index)
    }
}

/// Read-only view of the innermost active `foreach()` loop for speculative probe lookahead.
#[derive(Clone, Copy, Debug)]
pub struct ForeachLookahead<'a> {
    frame: &'a ForeachFrame,
}

impl<'a> ForeachLookahead<'a> {
    pub(crate) fn new(frame: &'a ForeachFrame) -> Self {
        Self { frame }
    }

    /// Raw commands inside the loop body (`foreach` .. `endforeach`).
    #[must_use]
    pub fn body(&self) -> &'a [CommandInvocation] {
        self.frame
            .cmds
            .get(self.frame.start..self.frame.end)
            .unwrap_or_default()
    }

    /// Variable scope depth at which this `foreach()` loop executes.
    #[must_use]
    pub fn scope_depth(&self) -> usize {
        self.frame.scope_depth
    }

    /// Loop variable names bound on each iteration.
    #[must_use]
    pub fn vars(&self) -> &'a [BString] {
        &self.frame.vars
    }

    /// Count of remaining iterations strictly after the currently executing one.
    #[must_use]
    pub fn remaining_len(&self) -> usize {
        match &self.frame.remaining {
            ForeachRemaining::Items { items, next } => items.len().saturating_sub(*next),
            ForeachRemaining::Range { remaining, .. } => {
                usize::try_from(*remaining).unwrap_or(usize::MAX)
            }
            ForeachRemaining::Zip { next, max, .. } => max.saturating_sub(*next),
        }
    }

    /// Visit the current iteration (`iter_index == 0`) followed by up to `limit` remaining
    /// iterations without pre-cloning future bindings.
    ///
    /// Stops cleanly before visiting an iteration whose bindings would exceed `max_slots` total
    /// visited variable slots or `max_bytes` total visited value bytes. Returns `false` if the
    /// callback returned `ControlFlow::Break(false)` or if the current iteration itself could not
    /// fit within the budget.
    pub fn visit_iterations(
        &self,
        limit: usize,
        max_slots: usize,
        max_bytes: usize,
        mut visit: impl FnMut(usize, &[Option<&[u8]>]) -> std::ops::ControlFlow<bool>,
    ) -> bool {
        let mut total_slots = 0usize;
        let mut total_bytes = 0usize;
        let mut emit =
            |iter_index: usize, values: &[Option<&[u8]>]| -> std::ops::ControlFlow<bool> {
                let next_slots = total_slots.saturating_add(values.len());
                let iter_bytes = values.iter().fold(0usize, |acc, v| {
                    acc.saturating_add(v.map_or(0, <[u8]>::len))
                });
                let next_bytes = total_bytes.saturating_add(iter_bytes);
                if next_slots > max_slots || next_bytes > max_bytes {
                    return std::ops::ControlFlow::Break(iter_index > 0);
                }
                total_slots = next_slots;
                total_bytes = next_bytes;
                visit(iter_index, values)
            };

        match &self.frame.remaining {
            ForeachRemaining::Items { items, next } => {
                let current = next.saturating_sub(1);
                let cur_slice = [items.get(current).map(AsRef::<[u8]>::as_ref)];
                if let std::ops::ControlFlow::Break(ok) = emit(0, &cur_slice) {
                    return ok;
                }
                for (offset, item) in items
                    .get(*next..)
                    .unwrap_or_default()
                    .iter()
                    .take(limit)
                    .enumerate()
                {
                    let slice = [Some(AsRef::<[u8]>::as_ref(item))];
                    if let std::ops::ControlFlow::Break(ok) = emit(offset + 1, &slice) {
                        return ok;
                    }
                }
                true
            }
            ForeachRemaining::Range {
                current_value,
                next_value,
                step,
                remaining,
            } => {
                let cur_buf = current_value.to_string();
                let cur_slice = [Some(cur_buf.as_bytes())];
                if let std::ops::ControlFlow::Break(ok) = emit(0, &cur_slice) {
                    return ok;
                }
                let count = usize::try_from(*remaining).unwrap_or(usize::MAX).min(limit);
                let mut value = *next_value;
                for offset in 0..count {
                    let buf = value.to_string();
                    let slice = [Some(buf.as_bytes())];
                    if let std::ops::ControlFlow::Break(ok) = emit(offset + 1, &slice) {
                        return ok;
                    }
                    value = value.saturating_add(*step);
                }
                true
            }
            ForeachRemaining::Zip { lists, next, max } => {
                if lists.len() > max_slots {
                    return false;
                }
                let current = next.saturating_sub(1);
                let mut row: Vec<Option<&[u8]>> = Vec::with_capacity(lists.len());
                row.extend(
                    lists
                        .iter()
                        .map(|list| list.get(current).map(AsRef::<[u8]>::as_ref)),
                );
                if let std::ops::ControlFlow::Break(ok) = emit(0, &row) {
                    return ok;
                }
                for (offset, index) in (*next..*max).take(limit).enumerate() {
                    row.clear();
                    row.extend(
                        lists
                            .iter()
                            .map(|list| list.get(index).map(AsRef::<[u8]>::as_ref)),
                    );
                    if let std::ops::ControlFlow::Break(ok) = emit(offset + 1, &row) {
                        return ok;
                    }
                }
                true
            }
        }
    }
}

/// Iteration plan computed by the `foreach()` command.
enum ForeachPlan {
    /// No iterations (`foreach(x)` or empty lists).
    Nothing,
    /// One variable over explicit items.
    Items { var: BString, items: Vec<BString> },
    /// One variable over `RANGE` values (generated lazily).
    Range {
        var: BString,
        start: i64,
        step: i64,
        count: u64,
    },
    /// `IN ZIP_LISTS`.
    Zip {
        vars: Vec<BString>,
        lists: Vec<Vec<BString>>,
    },
}

/// Outcome of one loop-body pass (`cmForEachFunctionBlocker::invoke`).
struct Pass {
    restore: bool,
    stop: bool,
    flow: Option<Flow>,
}

impl Interp {
    pub(crate) fn exec_foreach(&mut self, code: &Code, i: usize) -> BlockResult {
        let cmds = Arc::clone(&code.cmds);
        let Some(cmd) = cmds.get(i) else {
            return Ok(Ok(Flow::Normal));
        };
        let end = code.end_of(i);
        let mut plan = None;
        let mut opener = Vec::new();
        self.run_opener(code, i, cmd, |this| {
            let args = this.expand(&cmd.args)?;
            plan = this.foreach_plan(&args)?;
            opener = args;
            Ok(Flow::Normal)
        })?;
        let Some(plan) = plan else {
            return Err(CmdError::Reported);
        };
        self.check_block_end(code, i, &opener);

        if let Some(top) = self.loop_blocks.last_mut() {
            *top += 1;
        }
        let result = self.run_foreach(code, i + 1, end, plan);
        if let Some(top) = self.loop_blocks.last_mut() {
            *top = top.saturating_sub(1);
        }
        self.trace_unframed(code, end);
        Ok(result)
    }

    /// Parse `foreach()` arguments. `Ok(None)` means an error was already reported.
    fn foreach_plan(&mut self, args: &[ExpandedArg]) -> Result<Option<ForeachPlan>, CmdError> {
        let values: Vec<&BString> = args.iter().map(|a| &a.value).collect();
        let Some(&var) = values.first() else {
            return Err(CmdError::Status(
                "called with incorrect number of arguments".into(),
            ));
        };
        if let Some(in_pos) = values.iter().position(|v| v.as_slice() == b"IN") {
            return Ok(self.foreach_in_plan(&values, in_pos));
        }
        if values.get(1).is_some_and(|v| v.as_slice() == b"RANGE") {
            let int = |s: &BString| -> Result<i64, CmdError> {
                stoi(s).map(i64::from).map_err(|e| {
                    let what: &[u8] = match e {
                        IntError::Invalid => b"Invalid integer: '",
                        IntError::OutOfRange => b"Integer out of range: '",
                    };
                    CmdError::Status(cat(&[what, s, b"'"]))
                })
            };
            let fail = |this: &mut Self, e: CmdError| {
                this.fatal = true;
                e
            };
            let parse_at = |this: &mut Self, n: usize| -> Result<i64, CmdError> {
                match values.get(n) {
                    Some(v) => int(v).map_err(|e| fail(this, e)),
                    None => Ok(0),
                }
            };
            let (mut start, mut stop, mut step) = (0i64, 0i64, 0i64);
            match values.len() {
                3 => stop = parse_at(self, 2)?,
                4 => {
                    start = parse_at(self, 2)?;
                    stop = parse_at(self, 3)?;
                }
                5 => {
                    start = parse_at(self, 2)?;
                    stop = parse_at(self, 3)?;
                    step = parse_at(self, 4)?;
                }
                _ => {}
            }
            if step == 0 {
                step = if start > stop { -1 } else { 1 };
            }
            if (start > stop && step > 0) || (start < stop && step < 0) {
                self.fatal = true;
                return Err(CmdError::Status(BString::from(format!(
                    "called with incorrect range specification: start {start}, stop {stop}, step {step}"
                ))));
            }
            let Some(count) = range_count(start, stop, step) else {
                self.fatal = true;
                return Err(CmdError::Status(BString::from(format!(
                    "called with a range that has too many iterations: start {start}, stop {stop}, step {step}"
                ))));
            };
            return Ok(Some(ForeachPlan::Range {
                var: var.clone(),
                start,
                step,
                count,
            }));
        }
        let items: Vec<BString> = values.iter().skip(1).map(|v| (*v).clone()).collect();
        if items.is_empty() {
            return Ok(Some(ForeachPlan::Nothing));
        }
        Ok(Some(ForeachPlan::Items {
            var: var.clone(),
            items,
        }))
    }

    /// `HandleInMode`. Reports errors itself and returns `None` after an error.
    fn foreach_in_plan(&mut self, values: &[&BString], in_pos: usize) -> Option<ForeachPlan> {
        #[derive(PartialEq)]
        enum Doing {
            None,
            Lists,
            Items,
            Zip,
        }
        let vars: Vec<BString> = values
            .get(..in_pos)
            .unwrap_or_default()
            .iter()
            .map(|v| (*v).clone())
            .collect();
        let mut items: Vec<BString> = Vec::new();
        let mut doing = Doing::None;
        // One budget for the whole plan: copied items, named list elements, and the
        // generated `ZIP_LISTS` variable names.
        let mut budget = ListBudget::new(
            self.config.limits.max_command_arguments,
            self.config.limits.max_command_argument_bytes,
        );
        for &arg in values.get(in_pos + 1..).unwrap_or_default() {
            match arg.as_slice() {
                b"LISTS" | b"ITEMS" => {
                    if doing == Doing::Zip {
                        self.error_here("ZIP_LISTS can not be used with LISTS or ITEMS".into());
                        return None;
                    }
                    if vars.len() != 1 {
                        self.error_here(
                            "ITEMS or LISTS require exactly one iteration variable".into(),
                        );
                        return None;
                    }
                    doing = if arg.as_slice() == b"LISTS" {
                        Doing::Lists
                    } else {
                        Doing::Items
                    };
                }
                b"ZIP_LISTS" => {
                    if doing != Doing::None {
                        self.error_here("ZIP_LISTS can not be used with LISTS or ITEMS".into());
                        return None;
                    }
                    doing = Doing::Zip;
                }
                _ => match doing {
                    Doing::Lists => {
                        let split = self
                            .lookup_var(arg)
                            .map_or(Ok(()), |value| budget.split(value, Empty::Keep, &mut items));
                        if let Err(limit) = split {
                            self.error_here(limit.to_string().into());
                            return None;
                        }
                    }
                    Doing::Items | Doing::Zip => {
                        if let Err(limit) = budget.take(arg.len()) {
                            self.error_here(limit.to_string().into());
                            return None;
                        }
                        items.push(arg.clone());
                    }
                    Doing::None => {
                        self.error_here(cat(&[b"Unknown argument:\n  ", arg, b"\n"]));
                        return None;
                    }
                },
            }
        }
        if doing == Doing::Zip {
            if vars.len() > 1 && vars.len() != items.len() {
                self.error_here(BString::from(format!(
                    "Expected {} list variables, but given {}",
                    vars.len(),
                    items.len()
                )));
                return None;
            }
            if items.is_empty() {
                return Some(ForeachPlan::Nothing);
            }
            if vars.len() <= 1 {
                // Each generated name is `<prefix>_<index>`: check all of them first. Lists
                // that are not defined also get a name.
                let prefix = vars.first().map_or(0, |prefix| prefix.len());
                let names = prefix
                    .checked_add(1 + 20)
                    .and_then(|name| name.checked_mul(items.len()));
                let charged = names
                    .ok_or(rcmake_base::list::ListLimit::Bytes(
                        self.config.limits.max_command_argument_bytes,
                    ))
                    .and_then(|names| budget.charge_bytes(names));
                if let Err(limit) = charged {
                    self.error_here(limit.to_string().into());
                    return None;
                }
            }
            let mut lists = Vec::with_capacity(items.len());
            for list_var in &items {
                let mut elems = Vec::new();
                let split = self
                    .lookup_var(list_var)
                    .map_or(Ok(()), |value| budget.split(value, Empty::Keep, &mut elems));
                if let Err(limit) = split {
                    self.error_here(limit.to_string().into());
                    return None;
                }
                lists.push(elems);
            }
            let names = if vars.len() > 1 {
                vars
            } else {
                let prefix = vars.first().cloned().unwrap_or_default();
                (0..lists.len())
                    .map(|n| cat(&[&prefix, b"_", n.to_string().as_bytes()]))
                    .collect()
            };
            return Some(ForeachPlan::Zip { vars: names, lists });
        }
        match vars.into_iter().next() {
            Some(var) if !items.is_empty() => Some(ForeachPlan::Items { var, items }),
            _ => Some(ForeachPlan::Nothing),
        }
    }

    /// Saved value of a loop variable (`CMP0124`).
    fn loop_var_saved(&self, name: &[u8]) -> Option<BString> {
        if self.policy_status(PolicyId::new(124)) == PolicyStatus::New {
            self.vars.get(name).cloned()
        } else {
            Some(self.lookup_var(name).cloned().unwrap_or_default())
        }
    }

    /// Bytes that [`Self::loop_var_saved`] would copy, without the copy.
    fn loop_var_saved_len(&self, name: &[u8]) -> usize {
        let value = if self.policy_status(PolicyId::new(124)) == PolicyStatus::New {
            self.vars.get(name)
        } else {
            self.lookup_var(name)
        };
        value.map_or(0, |value| value.len())
    }

    fn restore_loop_var(&mut self, name: &[u8], saved: Option<BString>) {
        match saved {
            Some(v) => self.define(name, v),
            None => self.undefine(name),
        }
    }

    /// One pass over a loop body.
    fn loop_pass(&mut self, code: &Code, start: usize, end: usize) -> Pass {
        match self.run_body(code, start, end) {
            Ok(Flow::Normal | Flow::Continue) => Pass {
                restore: true,
                stop: false,
                flow: None,
            },
            Ok(Flow::Break) => Pass {
                restore: true,
                stop: true,
                flow: None,
            },
            Ok(ret @ Flow::Return { .. }) => Pass {
                restore: true,
                stop: true,
                flow: Some(ret),
            },
            Err(_) => Pass {
                restore: !self.fatal,
                stop: self.fatal,
                flow: None,
            },
        }
    }

    fn run_foreach(
        &mut self,
        code: &Code,
        start: usize,
        end: usize,
        plan: ForeachPlan,
    ) -> CmdResult {
        let mut restore = false;
        let mut flow = None;
        match plan {
            ForeachPlan::Nothing => return Ok(Flow::Normal),
            ForeachPlan::Items { var, items } => {
                let saved = self.loop_var_saved(&var);
                let items = Arc::new(items);
                let pushed = start < end && items.len() > 1;
                if pushed {
                    self.foreach_frames.push(ForeachFrame {
                        cmds: Arc::clone(&code.cmds),
                        start,
                        end,
                        scope_depth: self.vars.depth(),
                        vars: vec![var.clone()],
                        remaining: ForeachRemaining::Items {
                            items: Arc::clone(&items),
                            next: 1,
                        },
                    });
                }
                let mut budget_stopped = false;
                for (index, item) in items.iter().enumerate() {
                    if self.over_budget() || (start == end && !self.take_step()) {
                        budget_stopped = true;
                        break;
                    }
                    if pushed
                        && let Some(frame) = self.foreach_frames.last_mut()
                        && let ForeachRemaining::Items { next, .. } = &mut frame.remaining
                    {
                        *next = index.saturating_add(1);
                    }
                    self.define(&var, item.clone());
                    let pass = self.loop_pass(code, start, end);
                    restore = pass.restore;
                    flow = pass.flow;
                    if pass.stop {
                        break;
                    }
                }
                if pushed {
                    self.foreach_frames.pop();
                }
                if budget_stopped {
                    return Err(CmdError::Reported);
                }
                if restore {
                    self.restore_loop_var(&var, saved);
                }
            }
            ForeachPlan::Range {
                var,
                start: first,
                step,
                count,
            } => {
                let saved = self.loop_var_saved(&var);
                let pushed = start < end && count > 1;
                if pushed {
                    self.foreach_frames.push(ForeachFrame {
                        cmds: Arc::clone(&code.cmds),
                        start,
                        end,
                        scope_depth: self.vars.depth(),
                        vars: vec![var.clone()],
                        remaining: ForeachRemaining::Range {
                            current_value: first,
                            next_value: first.saturating_add(step),
                            step,
                            remaining: count.saturating_sub(1),
                        },
                    });
                }
                let mut value = first;
                let mut budget_stopped = false;
                for i in 0..count {
                    if self.over_budget() || (start == end && !self.take_step()) {
                        budget_stopped = true;
                        break;
                    }
                    let current = value;
                    self.define(&var, current.to_string().into());
                    value = value.saturating_add(step);
                    if pushed
                        && let Some(frame) = self.foreach_frames.last_mut()
                        && let ForeachRemaining::Range {
                            current_value,
                            next_value,
                            remaining,
                            ..
                        } = &mut frame.remaining
                    {
                        *current_value = current;
                        *next_value = value;
                        *remaining = count.saturating_sub(i.saturating_add(1));
                    }
                    let pass = self.loop_pass(code, start, end);
                    restore = pass.restore;
                    flow = pass.flow;
                    if pass.stop {
                        break;
                    }
                }
                if pushed {
                    self.foreach_frames.pop();
                }
                if budget_stopped {
                    return Err(CmdError::Reported);
                }
                if restore {
                    self.restore_loop_var(&var, saved);
                }
            }
            ForeachPlan::Zip { vars, lists } => {
                let max = lists.iter().map(Vec::len).max().unwrap_or(0);
                if max == 0 {
                    // No iteration changes a loop variable, so nothing is saved.
                    return Ok(Flow::Normal);
                }
                // Save each distinct loop variable once, in first-seen order. Reserve the
                // saved bytes before the copies.
                let mut seen = BTreeSet::new();
                let distinct: Vec<&BString> = vars
                    .iter()
                    .filter(|var| seen.insert(var.as_slice()))
                    .collect();
                let saved_bytes = distinct
                    .iter()
                    .map(|var| self.loop_var_saved_len(var))
                    .fold(0, usize::saturating_add);
                if !self.reserve_temp(saved_bytes) {
                    return Err(CmdError::Reported);
                }
                let saved: Vec<(&BString, Option<BString>)> = distinct
                    .into_iter()
                    .map(|var| (var, self.loop_var_saved(var)))
                    .collect();
                let lists = Arc::new(lists);
                let pushed = start < end && max > 1;
                if pushed {
                    self.foreach_frames.push(ForeachFrame {
                        cmds: Arc::clone(&code.cmds),
                        start,
                        end,
                        scope_depth: self.vars.depth(),
                        vars: vars.clone(),
                        remaining: ForeachRemaining::Zip {
                            lists: Arc::clone(&lists),
                            next: 1,
                            max,
                        },
                    });
                }
                let mut stopped = false;
                for n in 0..max {
                    if self.over_budget() || (start == end && !self.take_step()) {
                        stopped = true;
                        break;
                    }
                    if pushed
                        && let Some(frame) = self.foreach_frames.last_mut()
                        && let ForeachRemaining::Zip { next, .. } = &mut frame.remaining
                    {
                        *next = n.saturating_add(1);
                    }
                    for (var, list) in vars.iter().zip(lists.iter()) {
                        match list.get(n) {
                            Some(v) => self.define(var, v.clone()),
                            None => self.undefine(var),
                        }
                    }
                    let pass = self.loop_pass(code, start, end);
                    restore = pass.restore;
                    flow = pass.flow;
                    if pass.stop {
                        break;
                    }
                }
                if pushed {
                    self.foreach_frames.pop();
                }
                if stopped || !restore {
                    drop(saved);
                    self.release_temp(saved_bytes);
                    if stopped {
                        return Err(CmdError::Reported);
                    }
                } else {
                    // Release each saved value as it moves back into the variable store,
                    // so watch callbacks of the restore see the copies that still exist.
                    let mut released = 0_usize;
                    for (var, old) in saved {
                        let charge = old.as_ref().map_or(0, |value| value.len());
                        self.release_temp(charge);
                        released = released.saturating_add(charge);
                        self.restore_loop_var(var, old);
                    }
                    self.release_temp(saved_bytes.saturating_sub(released));
                }
            }
        }
        if self.fatal {
            return Err(CmdError::Reported);
        }
        Ok(flow.unwrap_or(Flow::Normal))
    }
}

/// Number of iterations of `foreach(RANGE)` from `start` to `stop` by a nonzero `step`.
///
/// Return `None` if the count does not fit in `u64` (the full `i64` span with step 1).
fn range_count(start: i64, stop: i64, step: i64) -> Option<u64> {
    start
        .abs_diff(stop)
        .checked_div(step.unsigned_abs())?
        .checked_add(1)
}

#[cfg(test)]
mod tests {
    use super::range_count;

    #[test]
    fn range_count_reports_overflow_instead_of_wrapping() {
        assert_eq!(range_count(i64::MIN, i64::MAX, 1), None);
        assert_eq!(range_count(i64::MAX, i64::MIN, -1), None);
        assert_eq!(range_count(i64::MIN, i64::MAX, 2), Some(1 << 63));
        assert_eq!(range_count(0, 10, 3), Some(4));
        assert_eq!(range_count(5, 5, 1), Some(1));
    }
}
