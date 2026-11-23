//! Cross-platform `cmake -E` built-in command-line utilities (`copy`, `rm`, `cat`, `tar`,
//! `md5sum`/`sha*sum`, `env`, `chdir`, `compare_files`, `touch`, `time`, `capabilities`, etc.).

#![forbid(unsafe_code)]

use std::path::Path;

use rcmake_base::BString;
use rcmake_host::{Host, Stream};
use rcmake_json::Value;

/// Context passed into `cmake -E <subcommand> [args...]`.
pub struct ToolCtx<'a> {
    /// Host abstraction for FS, env, process, clock, and stdio.
    pub host: &'a dyn Host,
    /// Current working directory.
    pub cwd: &'a Path,
    /// Executable path displayed in `Usage: <exe> -E ...`.
    pub exe_display: &'a [u8],
    /// Callback returning the `cmake -E capabilities` JSON document.
    pub capabilities: &'a dyn Fn() -> Value,
}

/// Dispatch a `cmake -E` invocation (`args` starts with the subcommand name after `-E`).
#[must_use]
pub fn run(args: &[BString], cx: &ToolCtx<'_>) -> i32 {
    let _ = args;
    cx.host.write_out(
        Stream::Stderr,
        b"CMake Error: -E tools not yet implemented (scheduled for M1-W2)\n",
    );
    1
}
