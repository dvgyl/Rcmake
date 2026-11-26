//! Execute CMake block commands.

mod conditional;
mod foreach;
mod scope;

pub(crate) use foreach::ForeachFrame;
pub use foreach::ForeachLookahead;

#[cfg(test)]
mod tests;

use std::sync::Arc;

use rcmake_base::BString;
use rcmake_diag::{Backtrace, DiagCategory, Effect, MessageKind};
use rcmake_expr::ExpandedArg;
use rcmake_syntax::{Argument, CommandInvocation};

use crate::{CmdError, CmdResult, Flow, Interp};
use crate::{code::Code, interp::BodyMode, util::cat};

/// Result of a block command. The outer `Err` means that the opener command failed and
/// did not open a block, so the caller runs the body as ordinary commands. The inner
/// result is the result of the block.
pub(crate) type BlockResult = Result<CmdResult, CmdError>;

fn same_raw_args(a: &[Argument], b: &[Argument]) -> bool {
    a.len() == b.len()
        && a.iter()
            .zip(b)
            .all(|(x, y)| x.text == y.text && x.kind == y.kind)
}

impl Interp {
    /// Run `f` with a backtrace frame but without a call scope (`elseif`, `while`
    /// condition re-evaluation, block-end checks).
    fn with_plain_frame_arc<R>(
        &mut self,
        file: &Arc<[u8]>,
        line: u32,
        name: Arc<[u8]>,
        f: impl FnOnce(&mut Self) -> R,
    ) -> R {
        let bt = self.command_backtrace_arc(file, line, name);
        self.with_plain_backtrace(bt, line, f)
    }

    fn with_plain_backtrace<R>(
        &mut self,
        bt: Backtrace,
        line: u32,
        f: impl FnOnce(&mut Self) -> R,
    ) -> R {
        let audit = self.hermetic.clone();
        let _audit_site = audit.as_ref().zip(bt.top()).map(|(audit, frame)| {
            audit.enter(crate::hermetic::HermeticSite::new(
                &rcmake_host::path::to_native(&frame.file),
                line,
            ))
        });
        let saved = core::mem::replace(&mut self.bt, bt);
        let saved_line = core::mem::replace(&mut self.cur_line, line);
        let r = f(self);
        if audit
            .as_ref()
            .is_some_and(crate::hermetic::HermeticContext::denied)
        {
            self.fatal = true;
        }
        self.cur_line = saved_line;
        self.bt = saved;
        r
    }

    /// Run the block opener at `idx` of `code` under a call frame and trace it.
    fn run_opener(
        &mut self,
        code: &Code,
        idx: usize,
        cmd: &CommandInvocation,
        f: impl FnOnce(&mut Self) -> CmdResult,
    ) -> CmdResult {
        self.with_command_frame_arc(&code.file, cmd.line, code.name_arc(idx), |this| {
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
    ///
    /// A body runs after the command frame of its opener returns, so block nesting has its
    /// own depth limit: the same limit as command recursion, and the native stack guard.
    fn run_body(&mut self, code: &Code, start: usize, end: usize) -> CmdResult {
        if start >= end {
            return Ok(Flow::Normal);
        }
        let limit = self.recursion_limit();
        if self.block_depth >= limit || crate::stack_guard::exhausted() {
            self.fatal_here(
                format!("Maximum nesting depth of {limit} exceeded by structured blocks.").into(),
            );
            return Err(CmdError::Reported);
        }
        self.block_depth += 1;
        let result = self.run_code(code, start, end, BodyMode::Nested);
        self.block_depth -= 1;
        result
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
}
