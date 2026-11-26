use std::sync::atomic::{AtomicBool, Ordering};

use rcmake_base::BString;

use crate::Interp;
use crate::scope::RECURSION_LIMIT;

impl Interp {
    /// Restrict command-line find diagnostics to the named variables and packages.
    pub fn set_find_debug_filters(&mut self, variables: Vec<BString>, packages: Vec<BString>) {
        self.find_debug_variables = variables;
        self.find_debug_packages = packages;
    }

    /// Track the named variables and reset their access status.
    pub fn track_variable_reads(&mut self, names: impl IntoIterator<Item = BString>) {
        self.variable_access
            .extend(names.into_iter().map(|name| (name, AtomicBool::new(false))));
        self.refresh_variable_tracking();
    }

    /// Stop tracking the named variables.
    pub fn untrack_variable_reads(&mut self, names: impl IntoIterator<Item = BString>) {
        for name in names {
            self.variable_access.remove(&name);
        }
        self.refresh_variable_tracking();
    }

    fn refresh_variable_tracking(&mut self) {
        self.tracks_recursion_limit = self.variable_access.contains_key(RECURSION_LIMIT);
        let unread = self
            .variable_access
            .values()
            .filter(|used| !used.load(Ordering::Relaxed))
            .count();
        self.unread_tracked.store(unread, Ordering::Relaxed);
    }

    /// Return tracked variables with no normal read, write, or removal.
    pub fn unread_variables(&self) -> impl Iterator<Item = &[u8]> + '_ {
        self.variable_access
            .iter()
            .filter(|(_, used)| !used.load(Ordering::Relaxed))
            .map(|(name, _)| name.as_slice())
    }

    pub(crate) fn record_variable_access(&self, name: &[u8]) {
        if self.trace_expanding || self.unread_tracked.load(Ordering::Relaxed) == 0 {
            return;
        }
        if let Some(used) = self.variable_access.get(name)
            && !used.swap(true, Ordering::Relaxed)
        {
            self.unread_tracked.fetch_sub(1, Ordering::Relaxed);
        }
    }
}
