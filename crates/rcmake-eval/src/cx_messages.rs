//! Diagnostics that a command reports without failing.

use rcmake_base::BString;
use rcmake_diag::{DiagCategory, Effect, MessageKind};

use crate::Cx;

impl Cx<'_> {
    /// Issue a diagnostic but let the current command succeed (`cmMakefile::IssueMessage`
    /// followed by `return true`). A script continues after such an error.
    pub fn issue_and_continue(
        &mut self,
        kind: MessageKind,
        effect: Effect,
        category: Option<DiagCategory>,
        body: BString,
    ) {
        self.interp.issue_here(kind, effect, category, body, false);
    }
}
