//! Block commands: `if`, `foreach`, `while`, `function`, `macro`, and `block`.

use std::sync::Arc;

use rcmake_base::BString;
use rcmake_base::list::{Empty, split_into};
use rcmake_diag::{Backtrace, DiagCategory, Effect, MessageKind, PolicyId, PolicyStatus};
use rcmake_expr::ExpandedArg;
use rcmake_syntax::{Argument, CommandInvocation};

use crate::args::{KeywordParser, Slot};
use crate::code::Code;
use crate::util::{IntError, escape_for_cmake, stoi};
use crate::{CmdError, CmdResult, Flow, Interp, UserCommand, UserCommandKind};
use crate::{interp::BodyMode, util::cat};

/// Result of a block command. The outer `Err` means that the opener command failed and
/// did not open a block, so the caller runs the body as ordinary commands. The inner
/// result is the result of the block.
pub(crate) type BlockResult = Result<CmdResult, CmdError>;

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

/// Scopes created by `block()`.
struct BlockPlan {
    variables: bool,
    policies: bool,
    diagnostics: bool,
    propagate: Vec<BString>,
}

/// Outcome of one loop-body pass (`cmForEachFunctionBlocker::invoke`).
struct Pass {
    restore: bool,
    stop: bool,
    flow: Option<Flow>,
}

fn same_raw_args(a: &[Argument], b: &[Argument]) -> bool {
    a.len() == b.len()
        && a.iter()
            .zip(b)
            .all(|(x, y)| x.text == y.text && x.kind == y.kind)
}

impl Interp {
    /// Run `f` with a backtrace frame but without a call scope (`elseif`, `while`
    /// condition re-evaluation, block-end checks).
    fn with_plain_frame<R>(
        &mut self,
        file: &Arc<[u8]>,
        line: u32,
        name: &[u8],
        f: impl FnOnce(&mut Self) -> R,
    ) -> R {
        let bt = self.command_backtrace(file, line, name);
        self.with_plain_backtrace(bt, line, f)
    }

    fn with_plain_backtrace<R>(
        &mut self,
        bt: Backtrace,
        line: u32,
        f: impl FnOnce(&mut Self) -> R,
    ) -> R {
        let saved = core::mem::replace(&mut self.bt, bt);
        let saved_line = core::mem::replace(&mut self.cur_line, line);
        let r = f(self);
        self.cur_line = saved_line;
        self.bt = saved;
        r
    }

    /// Run the block opener `cmd` under a call frame and trace it.
    fn run_opener(
        &mut self,
        code: &Code,
        cmd: &CommandInvocation,
        f: impl FnOnce(&mut Self) -> CmdResult,
    ) -> CmdResult {
        self.with_command_frame(&code.file, cmd.line, &cmd.name, |this| {
            if this.trace_enabled() && !this.fatal {
                this.trace_command(cmd, &code.file, false);
            }
            f(this)
        })
    }

    /// Trace the command at `idx` that is evaluated without its own call frame (`else`,
    /// `elseif`, and end commands).
    fn trace_unframed(&mut self, code: &Code, idx: usize) {
        if !self.trace_enabled() {
            return;
        }
        if let Some(cmd) = code.cmds.get(idx) {
            self.trace_command(cmd, &code.file, true);
        }
    }

    /// Run the commands in `start..end` as a block body.
    fn run_body(&mut self, code: &Code, start: usize, end: usize) -> CmdResult {
        if start >= end {
            return Ok(Flow::Normal);
        }
        self.run_code(code, start, end, BodyMode::Nested)
    }

    /// `cmFunctionBlocker::IsFunctionBlocked` end-of-block argument checks.
    fn check_block_end(&mut self, code: &Code, open: usize, opener_args: &[ExpandedArg]) {
        let end = code.end_of(open);
        let (Some(op), Some(close)) = (code.cmds.get(open), code.cmds.get(end)) else {
            return;
        };
        if end == open {
            return;
        }
        let context = |c: &CommandInvocation| -> BString {
            cat(&[
                &code.file,
                b":",
                c.line.to_string().as_bytes(),
                b" (",
                &c.name,
                b")",
            ])
        };
        let lower = op.lower.as_slice();
        let msg = if lower == b"block" {
            if close.args.is_empty() {
                return;
            }
            cat(&[
                b"A logical block closing on the line\n  ",
                &context(close),
                b"\nhas unexpected arguments.",
            ])
        } else {
            let matches = if close.args.is_empty() {
                true
            } else if lower == b"if" || lower == b"while" {
                same_raw_args(&op.args, &close.args)
            } else {
                let mut out = Vec::new();
                let expand = self.frontend.expand;
                let _ = expand(&close.args, self, &mut out);
                out.first()
                    .is_none_or(|first| Some(&first.value) == opener_args.first().map(|a| &a.value))
            };
            if matches {
                return;
            }
            cat(&[
                b"A logical block opening on the line\n  ",
                &context(op),
                b"\ncloses on the line\n  ",
                &context(close),
                b"\nwith mis-matching arguments.",
            ])
        };
        self.issue_here(
            MessageKind::AuthorWarning,
            Effect::None,
            Some(DiagCategory::Author),
            msg,
            true,
        );
    }

    /// Expand and evaluate a condition. On error, issue
    /// `<prefix>given arguments:\n  <args>\n<message>` as a fatal error.
    fn eval_reporting(&mut self, args: &[Argument], prefix: &[u8]) -> Result<bool, CmdError> {
        let expanded = self.expand(args)?;
        match self.eval_command_condition(&expanded) {
            Ok(v) => Ok(v),
            Err(e) => {
                let mut msg = prefix.to_vec();
                msg.extend_from_slice(b"given arguments:\n ");
                for a in &expanded {
                    msg.push(b' ');
                    msg.extend_from_slice(&escape_for_cmake(&a.value));
                }
                msg.push(b'\n');
                msg.extend_from_slice(&e.message);
                self.fatal_here(BString::from(msg));
                Err(CmdError::Reported)
            }
        }
    }

    // ----------------------------------------------------------------------------------
    // if / elseif / else / endif
    // ----------------------------------------------------------------------------------

    pub(crate) fn exec_if(&mut self, code: &Code, i: usize) -> BlockResult {
        let cmds = Arc::clone(&code.cmds);
        let Some(cmd) = cmds.get(i) else {
            return Ok(Ok(Flow::Normal));
        };
        let end = code.end_of(i);
        let mut truth = false;
        if let Err(e) = self.run_opener(code, cmd, |this| {
            truth = this.eval_reporting(&cmd.args, b"if ")?;
            Ok(Flow::Normal)
        }) {
            // `cmIfCommand` adds its block before it reports a nested error.
            return Ok(Err(e));
        }
        self.check_block_end(code, i, &[]);
        let result = self.run_if_branches(code, i, end, truth);
        self.trace_unframed(code, end);
        Ok(result)
    }

    fn run_if_branches(&mut self, code: &Code, i: usize, end: usize, truth: bool) -> CmdResult {
        let cmds = Arc::clone(&code.cmds);
        let branches = code.if_branches(i);
        let first_stop = branches.first().copied().unwrap_or(end);
        if truth {
            return self.run_body(code, i + 1, first_stop);
        }
        for (n, &b) in branches.iter().enumerate() {
            let stop = branches.get(n + 1).copied().unwrap_or(end);
            let Some(bc) = cmds.get(b) else { break };
            if bc.lower.as_slice() == b"else" {
                self.trace_unframed(code, b);
                return self.run_body(code, b + 1, stop);
            }
            self.trace_unframed(code, b);
            let taken = self.with_plain_frame(&code.file, bc.line, &bc.name, |this| {
                if bc.args.is_empty() {
                    this.issue_here(
                        MessageKind::AuthorWarning,
                        Effect::None,
                        Some(DiagCategory::Author),
                        "ELSEIF called with no arguments, it will be skipped.".into(),
                        false,
                    );
                }
                this.eval_reporting(&bc.args, b"")
            })?;
            if taken {
                return self.run_body(code, b + 1, stop);
            }
        }
        Ok(Flow::Normal)
    }

    // ----------------------------------------------------------------------------------
    // foreach / endforeach
    // ----------------------------------------------------------------------------------

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
                    if self.over_budget() {
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
                    if self.over_budget() {
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
                    if self.over_budget() {
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

    // ----------------------------------------------------------------------------------
    // while / endwhile
    // ----------------------------------------------------------------------------------

    pub(crate) fn exec_while(&mut self, code: &Code, i: usize) -> BlockResult {
        let cmds = Arc::clone(&code.cmds);
        let Some(cmd) = cmds.get(i) else {
            return Ok(Ok(Flow::Normal));
        };
        let end = code.end_of(i);
        self.run_opener(code, cmd, |_| {
            if cmd.args.is_empty() {
                Err(CmdError::Status(
                    "called with incorrect number of arguments".into(),
                ))
            } else {
                Ok(Flow::Normal)
            }
        })?;
        self.check_block_end(code, i, &[]);

        if let Some(top) = self.loop_blocks.last_mut() {
            *top += 1;
        }
        let result = self.run_while(code, i, end, cmd);
        if let Some(top) = self.loop_blocks.last_mut() {
            *top = top.saturating_sub(1);
        }
        self.trace_unframed(code, end);
        Ok(result)
    }

    fn run_while(
        &mut self,
        code: &Code,
        i: usize,
        end: usize,
        cmd: &CommandInvocation,
    ) -> CmdResult {
        let bt = self.command_backtrace(&code.file, cmd.line, &cmd.name);
        loop {
            if self.fatal || self.over_budget() {
                return Err(CmdError::Reported);
            }
            let outcome = self.with_plain_backtrace(bt.clone(), cmd.line, |this| {
                let expanded = this.expand(&cmd.args)?;
                Ok::<_, CmdError>((this.eval_command_condition(&expanded), expanded))
            });
            let (truth, expanded) = outcome?;
            match truth {
                Ok(true) => {}
                Ok(false) => return Ok(Flow::Normal),
                Err(e) => {
                    self.report_while_error(code, cmd, &expanded, &e.message);
                    return Ok(Flow::Normal);
                }
            }
            match self.run_body(code, i + 1, end) {
                Ok(Flow::Normal | Flow::Continue) => {}
                Ok(Flow::Break) => return Ok(Flow::Normal),
                Ok(ret @ Flow::Return { .. }) => return Ok(ret),
                Err(e) => {
                    if self.fatal {
                        return Err(e);
                    }
                }
            }
        }
    }

    /// Report a `while()` condition error (`CMP0130`).
    fn report_while_error(
        &mut self,
        code: &Code,
        cmd: &CommandInvocation,
        expanded: &[ExpandedArg],
        message: &[u8],
    ) {
        let status = self.policy_status(PolicyId::new(130));
        if status == PolicyStatus::Old {
            return;
        }
        let mut err = b"while() given incorrect arguments:\n ".to_vec();
        for a in expanded {
            err.push(b' ');
            err.extend_from_slice(&escape_for_cmake(&a.value));
        }
        err.push(b'\n');
        err.extend_from_slice(message);
        self.with_plain_frame(&code.file, cmd.line, &cmd.name, |this| {
            if status == PolicyStatus::Warn {
                this.issue_policy_warning(PolicyId::new(130), b"", &err);
            } else {
                this.fatal_here(BString::from(err));
            }
        });
    }

    // ----------------------------------------------------------------------------------
    // function / macro definitions
    // ----------------------------------------------------------------------------------

    pub(crate) fn exec_define(
        &mut self,
        code: &Code,
        i: usize,
        kind: UserCommandKind,
    ) -> BlockResult {
        let cmds = Arc::clone(&code.cmds);
        let Some(cmd) = cmds.get(i) else {
            return Ok(Ok(Flow::Normal));
        };
        let end = code.end_of(i);
        let mut values: Vec<BString> = Vec::new();
        self.run_opener(code, cmd, |this| {
            let args = this.expand(&cmd.args)?;
            if args.is_empty() {
                return Err(CmdError::Status(
                    "called with incorrect number of arguments".into(),
                ));
            }
            values = args.into_iter().map(|a| a.value).collect();
            Ok(Flow::Normal)
        })?;
        let opener: Vec<ExpandedArg> = values
            .first()
            .map(|v| ExpandedArg {
                value: v.clone(),
                kind: rcmake_syntax::ArgKind::Unquoted,
                line: cmd.line,
            })
            .into_iter()
            .collect();
        self.check_block_end(code, i, &opener);

        let mut values = values.into_iter();
        let Some(name) = values.next() else {
            return Ok(Ok(Flow::Normal));
        };
        if kind == UserCommandKind::Macro
            && self.policy_status(PolicyId::new(217)) != PolicyStatus::New
        {
            let dir = self.current_dir_id();
            self.project
                .dir_mut(dir)
                .props
                .append(BString::from("MACROS"), &name);
        }
        let user = UserCommand {
            kind,
            name,
            params: values.collect(),
            body: code.slice(i + 1, end),
            line: cmd.line,
            policies: *self.policies.top(),
            diags: *self.diags.top(),
        };
        let added = self.with_plain_frame(&code.file, cmd.line, &cmd.name, |this| {
            this.add_scripted_command(Arc::new(user))
        });
        self.trace_unframed(code, end);
        if added {
            Ok(Ok(Flow::Normal))
        } else {
            Ok(Err(CmdError::Reported))
        }
    }

    // ----------------------------------------------------------------------------------
    // block / endblock
    // ----------------------------------------------------------------------------------

    fn block_plan(&mut self, args: &[ExpandedArg]) -> Result<Option<BlockPlan>, CmdError> {
        let parser = KeywordParser::new()
            .keyword("SCOPE_FOR", Slot::MultiNonEmpty)
            .keyword("PROPAGATE", Slot::Multi);
        let parsed = parser.parse(args);
        if let Some(first) = parsed.unparsed().first() {
            self.fatal = true;
            return Err(CmdError::Status(cat(&[
                b"called with unsupported argument \"",
                first,
                b"\"",
            ])));
        }
        if let Some(text) = parsed.report_error_text() {
            self.fatal_here(text);
            return Ok(None);
        }
        let mut plan = BlockPlan {
            variables: false,
            policies: false,
            diagnostics: false,
            propagate: parsed.multi("PROPAGATE").to_vec(),
        };
        if parsed.seen("SCOPE_FOR") {
            for scope in parsed.multi("SCOPE_FOR") {
                match scope.as_slice() {
                    b"VARIABLES" => plan.variables = true,
                    b"POLICIES" => plan.policies = true,
                    b"DIAGNOSTICS" => plan.diagnostics = true,
                    _ => {
                        self.fatal = true;
                        return Err(CmdError::Status(cat(&[
                            b"SCOPE_FOR unsupported scope \"",
                            scope,
                            b"\"",
                        ])));
                    }
                }
            }
        } else {
            plan.variables = true;
            plan.policies = true;
            plan.diagnostics = true;
        }
        if !plan.variables && !plan.propagate.is_empty() {
            self.fatal = true;
            return Err(CmdError::Status(
                "PROPAGATE cannot be specified without a new scope for VARIABLES".into(),
            ));
        }
        Ok(Some(plan))
    }

    pub(crate) fn exec_block(&mut self, code: &Code, i: usize) -> BlockResult {
        let cmds = Arc::clone(&code.cmds);
        let Some(cmd) = cmds.get(i) else {
            return Ok(Ok(Flow::Normal));
        };
        let end = code.end_of(i);
        let mut plan = None;
        self.run_opener(code, cmd, |this| {
            let args = this.expand(&cmd.args)?;
            plan = this.block_plan(&args)?;
            Ok(Flow::Normal)
        })?;
        let Some(plan) = plan else {
            return Err(CmdError::Reported);
        };
        self.check_block_end(code, i, &[]);

        if plan.variables {
            self.push_snapshot();
            self.vars.push();
        }
        if plan.policies {
            self.policies.push_copy();
        }
        if plan.diagnostics {
            self.diags.push_copy();
        }

        let result = self.run_body(code, i + 1, end);
        if let Ok(Flow::Return {
            propagate: Some(names),
        }) = &result
        {
            self.raise_vars(names);
        }
        if plan.variables {
            self.raise_vars(&plan.propagate);
        }
        if plan.diagnostics {
            let _ = self.pop_diag();
        }
        if plan.policies {
            let _ = self.pop_policy();
        }
        if plan.variables {
            let report = !self.fatal;
            self.pop_snapshot(report);
            self.vars.pop();
        }
        self.trace_unframed(code, end);
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use std::{path::PathBuf, sync::Arc};

    use rcmake_host::{PureHost, Stream};

    use crate::{Interp, InterpConfig, RunMode};

    #[test]
    fn while_checks_cmp0218_and_deprecation_uses_the_set_value() {
        for (value, warn) in [
            (None, true),
            (Some(""), true),
            (Some("NOTFOUND"), true),
            (Some("X-NOTFOUND"), true),
            (Some("x-notfound"), false),
            (Some("OFF"), false),
        ] {
            let host = Arc::new(PureHost::default());
            let mut interp = Interp::new(
                InterpConfig {
                    host: host.clone(),
                    mode: RunMode::Script,
                    cmake_root: PathBuf::new(),
                    cmake_command: PathBuf::from("/usr/local/bin/cmake"),
                    cwd: PathBuf::from("/work"),
                    limits: Default::default(),
                    diag: Default::default(),
                    defines: Vec::new(),
                    script_args: Vec::new(),
                },
                |_| {},
            );
            if let Some(value) = value {
                interp.define(b"CMAKE_WARN_DEPRECATED", value.into());
            }
            let script = b"while(CMAKE_WARN_DEPRECATED)\n  break()\nendwhile()\nmessage(DEPRECATION legacy-warning)\n";
            assert!(interp.eval_code(script).is_ok());
            let stderr = host.stream_bytes(Stream::Stderr);
            let stderr = String::from_utf8_lossy(&stderr);
            assert!(stderr.contains("Policy CMP0218 is not set"), "{stderr}");
            assert_eq!(stderr.contains("legacy-warning"), warn, "{value:?}");
        }
    }
}
