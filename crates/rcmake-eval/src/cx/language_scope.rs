use rcmake_base::truth;
use rcmake_diag::PolicyId;

use crate::{CmdError, Cx};

impl Cx<'_> {
    /// Capture language setup bindings and propagate them according to CMP0220.
    ///
    /// # Errors
    ///
    /// Returns the error from the setup callback.
    pub fn with_language_scope(
        &mut self,
        setup: impl FnOnce(&mut Self) -> Result<(), CmdError>,
    ) -> Result<(), CmdError> {
        self.interp.mark_replay_opaque();
        self.interp.vars.push();
        setup(self).inspect_err(|_| self.interp.vars.pop())?;
        let limit = self.interp.live_limit();
        let Ok(warn) = self.interp.vars.propagate_capture(limit) else {
            self.interp.report_variable_budget();
            return Err(CmdError::Reported);
        };
        if warn
            && self
                .interp
                .lookup_var(b"CMAKE_POLICY_WARNING_CMP0220")
                .map_or_else(|| self.interp.trace_enabled(), |value| truth::is_on(value))
        {
            self.issue_policy_warning(PolicyId::new(220), b"", b"For compatibility with older versions of CMake, the language configuration set by this call is not propagated to the enclosing variable scopes.");
        }
        Ok(())
    }
}
