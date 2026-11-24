use std::sync::Arc;
use std::time::Duration;

use crate::Interp;

/// Callback for one command's name, file, line, monotonic start, and duration.
pub type CommandProfiler = dyn Fn(&[u8], &[u8], u32, Duration, Duration) + Send + Sync;

impl Interp {
    /// Set the callback for completed command executions.
    pub fn set_command_profiler(&mut self, profiler: Option<Arc<CommandProfiler>>) {
        self.command_profiler = profiler;
    }
}
