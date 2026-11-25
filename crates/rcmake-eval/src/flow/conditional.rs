use std::sync::Arc;

use rcmake_base::{BString, shell::cmake_quote};
use rcmake_diag::{DiagCategory, Effect, MessageKind, PolicyId, PolicyStatus};
use rcmake_expr::ExpandedArg;
use rcmake_syntax::{Argument, CommandInvocation};

use super::BlockResult;
use crate::{CmdError, CmdResult, Flow, Interp, code::Code};

impl Interp {
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
                    cmake_quote(&mut msg, &a.value);
                }
                msg.push(b'\n');
                msg.extend_from_slice(&e.message);
                self.fatal_here(BString::from(msg));
                Err(CmdError::Reported)
            }
        }
    }

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
        let mut repeated = false;
        loop {
            if self.fatal || self.over_budget() || (repeated && !self.take_step()) {
                return Err(CmdError::Reported);
            }
            repeated = true;
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
            cmake_quote(&mut err, &a.value);
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
}
