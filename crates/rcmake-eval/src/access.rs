use std::sync::atomic::{AtomicBool, Ordering};

use rcmake_base::BString;

use crate::Interp;

impl Interp {
    /// Track the named variables and reset their access status.
    pub fn track_variable_reads(&mut self, names: impl IntoIterator<Item = BString>) {
        self.variable_access
            .extend(names.into_iter().map(|name| (name, AtomicBool::new(false))));
    }

    /// Stop tracking the named variables.
    pub fn untrack_variable_reads(&mut self, names: impl IntoIterator<Item = BString>) {
        for name in names {
            self.variable_access.remove(&name);
        }
    }

    /// Return tracked variables with no normal read, write, or removal.
    pub fn unread_variables(&self) -> impl Iterator<Item = &[u8]> + '_ {
        self.variable_access
            .iter()
            .filter(|(_, used)| !used.load(Ordering::Relaxed))
            .map(|(name, _)| name.as_slice())
    }

    pub(crate) fn record_variable_access(&self, name: &[u8]) {
        if !self.trace_expanding
            && let Some(used) = self.variable_access.get(name)
        {
            used.store(true, Ordering::Relaxed);
        }
    }
}
