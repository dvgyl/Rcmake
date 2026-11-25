use std::sync::Arc;

use rcmake_base::{
    BString,
    list::{Empty, split_into},
};
use rcmake_diag::{PolicyId, PolicyStatus};
use rcmake_expr::ExpandedArg;

use super::BlockResult;
use crate::util::{IntError, cat, stoi};
use crate::{CmdError, CmdResult, Flow, Interp, code::Code};

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
        self.run_opener(code, cmd, |this| {
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
            let span = start.abs_diff(stop);
            let count = span / step.unsigned_abs() + 1;
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
                        let value = self.lookup_var(arg).cloned().unwrap_or_default();
                        split_into(&value, Empty::Keep, &mut items);
                    }
                    Doing::Items | Doing::Zip => items.push(arg.clone()),
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
            let mut lists = Vec::with_capacity(items.len());
            for list_var in &items {
                let value = self.lookup_var(list_var).cloned().unwrap_or_default();
                let mut elems = Vec::new();
                split_into(&value, Empty::Keep, &mut elems);
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
                for item in items {
                    if self.over_budget() || (start == end && !self.take_step()) {
                        return Err(CmdError::Reported);
                    }
                    self.define(&var, item);
                    let pass = self.loop_pass(code, start, end);
                    restore = pass.restore;
                    flow = pass.flow;
                    if pass.stop {
                        break;
                    }
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
                let mut value = first;
                for _ in 0..count {
                    if self.over_budget() || (start == end && !self.take_step()) {
                        return Err(CmdError::Reported);
                    }
                    self.define(&var, value.to_string().into());
                    value = value.saturating_add(step);
                    let pass = self.loop_pass(code, start, end);
                    restore = pass.restore;
                    flow = pass.flow;
                    if pass.stop {
                        break;
                    }
                }
                if restore {
                    self.restore_loop_var(&var, saved);
                }
            }
            ForeachPlan::Zip { vars, lists } => {
                let saved: Vec<Option<BString>> =
                    vars.iter().map(|v| self.loop_var_saved(v)).collect();
                let max = lists.iter().map(Vec::len).max().unwrap_or(0);
                for n in 0..max {
                    if self.over_budget() || (start == end && !self.take_step()) {
                        return Err(CmdError::Reported);
                    }
                    for (var, list) in vars.iter().zip(&lists) {
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
                if restore {
                    for (var, old) in vars.iter().zip(saved) {
                        self.restore_loop_var(var, old);
                    }
                }
            }
        }
        if self.fatal {
            return Err(CmdError::Reported);
        }
        Ok(flow.unwrap_or(Flow::Normal))
    }
}
