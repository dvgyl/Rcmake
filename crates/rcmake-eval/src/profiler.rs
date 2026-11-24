//! Timing samples for commands and listfiles.

#![forbid(unsafe_code)]

use std::sync::Arc;
use std::time::Duration;

use crate::Interp;

/// The kind of execution that produced a timing sample.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SampleKind {
    /// Built-in command or evaluator control flow.
    Builtin,
    /// User-defined function.
    Function,
    /// User-defined macro.
    Macro,
    /// Listfile execution.
    ListFile,
}

/// Timing and source information for completed execution.
pub struct CommandSample<'a> {
    /// Command name or listfile label.
    pub name: &'a [u8],
    /// Source file path as bytes.
    pub file: &'a [u8],
    /// One-based source line, or zero when no line applies.
    pub line: u32,
    /// Nesting depth of the execution frame.
    pub depth: u32,
    /// Start time measured by the monotonic clock.
    pub start: Duration,
    /// Time elapsed from start to completion.
    pub elapsed: Duration,
    /// Kind of execution frame.
    pub kind: SampleKind,
}

/// Callback for a completed execution frame.
pub type CommandProfiler = dyn Fn(&CommandSample<'_>) + Send + Sync;

impl Interp {
    /// Set the callback for completed command executions.
    pub fn set_command_profiler(&mut self, profiler: Option<Arc<CommandProfiler>>) {
        self.command_profiler = profiler;
    }
}
