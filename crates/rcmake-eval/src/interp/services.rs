use crate::{Interp, VarLedger, VarOp, VarWrite};

/// Reason an evaluator session stopped before completion.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StopReason {
    /// The caller requested cancellation.
    Cancelled,
    /// The evaluator used all available steps.
    StepBudgetExceeded {
        /// Maximum number of steps.
        limit: u64,
    },
    /// A denied process caused a fatal command failure.
    DeniedProcess,
}

impl Interp {
    /// Get the reason the evaluator stopped, if it stopped for a session limit or denial.
    #[must_use]
    pub fn stop_reason(&self) -> Option<StopReason> {
        self.stop_reason
    }

    /// Take the variable history, if this session collected it.
    pub fn take_var_ledger(&mut self) -> Option<VarLedger> {
        self.var_ledger.take()
    }

    pub(crate) fn stop(&mut self, reason: StopReason) {
        self.stop_reason.get_or_insert(reason);
        self.fatal = true;
    }

    pub(crate) fn poll_cancel(&mut self) -> bool {
        if !self.fatal
            && self
                .config
                .services
                .cancel
                .as_ref()
                .is_some_and(crate::CancelToken::is_cancelled)
        {
            self.stop(StopReason::Cancelled);
        }
        self.stop_reason == Some(StopReason::Cancelled)
    }

    pub(crate) fn take_step(&mut self) -> bool {
        if self.fatal || self.poll_cancel() {
            return false;
        }
        if let Some(limit) = self.config.services.step_budget {
            if self.steps >= limit {
                self.stop(StopReason::StepBudgetExceeded { limit });
                return false;
            }
            self.steps += 1;
        }
        true
    }

    pub(crate) fn record_var_write(&mut self, name: &[u8], op: VarOp, new: Option<&[u8]>) {
        let Some(ledger) = &mut self.var_ledger else {
            return;
        };
        let parent = op == VarOp::ParentScope;
        let Some((scope_depth, scope_id, directory)) = self.vars.scope_identity(parent) else {
            return;
        };
        let old_value = match op {
            VarOp::CacheSet | VarOp::CacheDeclare | VarOp::CacheUnset => {
                self.project.cache.get(name).map(|e| &e.value)
            }
            _ => self.vars.get_from(name, parent),
        };
        self.write_seq = self.write_seq.saturating_add(1);
        let binding_dir = if matches!(
            op,
            VarOp::CacheSet | VarOp::CacheDeclare | VarOp::CacheUnset
        ) {
            None
        } else {
            directory
        };
        ledger.record_at(
            name,
            VarWrite {
                seq: self.write_seq,
                op,
                scope_depth,
                scope_id,
                old_value: old_value.map(|v| v.as_slice()),
                new_value: new,
                backtrace: &self.bt,
            },
            binding_dir,
        );
    }
}
