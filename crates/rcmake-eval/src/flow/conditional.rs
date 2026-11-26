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
        let mut expanded = self.take_arg_buf(args.len());
        if let Err(err) = self.expand_into_buf(args, &mut expanded) {
            self.recycle_arg_buf(expanded);
            return Err(err);
        }
        let result = match self.eval_command_condition(&expanded) {
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
        };
        self.recycle_arg_buf(expanded);
        result
    }

    pub(crate) fn exec_if(&mut self, code: &Code, i: usize) -> BlockResult {
        let cmds = Arc::clone(&code.cmds);
        let Some(cmd) = cmds.get(i) else {
            return Ok(Ok(Flow::Normal));
        };
        let end = code.end_of(i);
        let mut truth = false;
        if let Err(e) = self.run_opener(code, i, cmd, |this| {
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
        let first_stop = code.next_branch(i);
        if truth {
            return self.run_body(code, i + 1, first_stop);
        }
        let cmds = Arc::clone(&code.cmds);
        let mut b = first_stop;
        while b < end {
            let stop = code.next_branch(b);
            let Some(bc) = cmds.get(b) else { break };
            if bc.lower.as_slice() == b"else" {
                self.trace_unframed(code, b);
                return self.run_body(code, b + 1, stop);
            }
            self.trace_unframed(code, b);
            let taken =
                self.with_plain_frame_arc(&code.file, bc.line, code.name_arc(b), |this| {
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
            b = stop;
        }
        Ok(Flow::Normal)
    }

    pub(crate) fn exec_while(&mut self, code: &Code, i: usize) -> BlockResult {
        let cmds = Arc::clone(&code.cmds);
        let Some(cmd) = cmds.get(i) else {
            return Ok(Ok(Flow::Normal));
        };
        let end = code.end_of(i);
        self.run_opener(code, i, cmd, |_| {
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
        let bt = self.command_backtrace_arc(&code.file, cmd.line, code.name_arc(i));
        let mut repeated = false;
        let mut expanded = self.take_arg_buf(cmd.args.len());
        let result = loop {
            if self.fatal || self.over_budget() || (repeated && !self.take_step()) {
                break Err(CmdError::Reported);
            }
            repeated = true;
            let outcome = self.with_plain_backtrace(bt.clone(), cmd.line, |this| {
                this.expand_into_buf(&cmd.args, &mut expanded)?;
                Ok::<_, CmdError>(this.eval_command_condition(&expanded))
            });
            let truth = match outcome {
                Ok(truth) => truth,
                Err(err) => break Err(err),
            };
            match truth {
                Ok(true) => {}
                Ok(false) => break Ok(Flow::Normal),
                Err(e) => {
                    self.report_while_error(code, i, cmd, &expanded, &e.message);
                    break Ok(Flow::Normal);
                }
            }
            match self.run_body(code, i + 1, end) {
                Ok(Flow::Normal | Flow::Continue) => {}
                Ok(Flow::Break) => break Ok(Flow::Normal),
                Ok(ret @ Flow::Return { .. }) => break Ok(ret),
                Err(e) => {
                    if self.fatal {
                        break Err(e);
                    }
                }
            }
        };
        self.recycle_arg_buf(expanded);
        result
    }

    /// Report a `while()` condition error (`CMP0130`).
    fn report_while_error(
        &mut self,
        code: &Code,
        i: usize,
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
        self.with_plain_frame_arc(&code.file, cmd.line, code.name_arc(i), |this| {
            if status == PolicyStatus::Warn {
                this.issue_policy_warning(PolicyId::new(130), b"", &err);
            } else {
                this.fatal_here(BString::from(err));
            }
        });
    }
}
