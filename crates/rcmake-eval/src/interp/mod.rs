//! List files, command dispatch, and interpreter scopes.

mod calls;
mod defaults;
mod diagnostics;
mod dispatch;
mod listfile;
mod scope;
mod services;
mod source;
pub use services::StopReason;

pub(crate) use listfile::CMP0198;

/// How a command sequence reacts to failed commands and control-flow transfers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum BodyMode {
    /// Listfile: continue after failures; stop at `return()`; ignore `break()`/`continue()`.
    File,
    /// Function body: stop at the first failure; stop at `return()`.
    Function,
    /// Macro body: stop at the first failure; forward every transfer.
    Macro,
    /// `if`/`foreach`/`while`/`block` body: continue after failures; forward transfers.
    Nested,
}
