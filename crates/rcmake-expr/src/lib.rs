//! CMake 4.5.1 variable expansion (`${...}`, `$ENV{...}`, `$CACHE{...}`), escape decoding,
//! unquoted argument list splitting, `if()`/`while()` condition evaluation, and
//! `configure_file` `@VAR@` / `#cmakedefine` substitution.

#![forbid(unsafe_code)]

mod condition;
mod expand;
mod number;

use std::path::Path;
use std::sync::Arc;

use rcmake_base::BString;
use rcmake_diag::{Diagnostic, PolicyId, PolicyStatus};
use rcmake_host::Host;
use rcmake_regex::{Captures, Regex, RegexError};
use rcmake_syntax::ArgKind;
use thiserror::Error;

/// One expanded command argument retaining its original [`ArgKind`] delimiter so `if()`
/// can distinguish quoted/bracket literals from unquoted auto-dereferenceable tokens
/// (`CMP0054` is unconditionally `NEW` in CMake 4.x).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExpandedArg {
    /// Expanded byte value.
    pub value: BString,
    /// Original delimiter classification.
    pub kind: ArgKind,
    /// 1-based line number where the source argument began.
    pub line: u32,
}

impl ExpandedArg {
    /// Convenience predicate: `true` for quoted and bracket arguments (literal in `if()`).
    #[must_use]
    pub const fn is_quoted_or_bracket(&self) -> bool {
        matches!(self.kind, ArgKind::Quoted | ArgKind::Bracket)
    }
}

/// Environment queried during variable expansion.
pub trait ExpandEnv {
    /// Look up a normal variable (falling back to cache if normal binding is absent).
    fn var(&mut self, name: &[u8]) -> Option<BString>;

    /// Look up a cache variable directly (`$CACHE{...}`).
    fn cache_var(&mut self, name: &[u8]) -> Option<BString>;

    /// Look up a host environment variable (`$ENV{...}`).
    fn env_var(&mut self, name: &[u8]) -> Option<BString>;

    /// Report a reference whose lookup did not return a value.
    fn missing_reference(&mut self, _name: &[u8]) {}

    /// Current 1-based listfile line number for `${CMAKE_CURRENT_LIST_LINE}`.
    fn current_line(&self) -> u32;

    /// Path to the listfile that contains the command being expanded.
    ///
    /// Evaluators can override this method to use the command's source path
    /// when it differs from `CMAKE_CURRENT_LIST_FILE`.
    fn current_list_file(&mut self) -> Option<BString> {
        self.var(b"CMAKE_CURRENT_LIST_FILE")
    }
}

/// Error produced during variable or escape expansion.
#[derive(Clone, Debug, PartialEq, Eq, Error)]
#[error("{message}")]
pub struct ExpandError {
    /// Exact CMake expansion error text.
    pub message: BString,
}

pub use expand::{expand_args, expand_string};

/// Full execution context required by CMake 4.5.1's `if()` / `while()` condition evaluator.
pub trait CondEnv: ExpandEnv {
    /// `if(DEFINED name)` (normal or cache binding exists, including defined empty).
    fn is_defined(&mut self, name: &[u8]) -> bool;

    /// `if(DEFINED CACHE{name})`.
    fn cache_defined(&mut self, name: &[u8]) -> bool;

    /// `if(DEFINED ENV{name})`.
    fn env_defined(&mut self, name: &[u8]) -> bool;

    /// `if(COMMAND name)` (case-insensitive builtin or user function/macro).
    fn command_exists(&self, name: &[u8]) -> bool;

    /// `if(POLICY CMPnnnn)` (policy exists in CMake 4.5.1 registry `CMP0000`..=`CMP0226`).
    fn policy_known(&self, name: &[u8]) -> bool;

    /// `if(DIAGNOSTIC CMD_...)` (diagnostic category exists in CMake 4.5.1).
    fn diagnostic_known(&self, name: &[u8]) -> bool;

    /// `if(TARGET name)`.
    fn target_exists(&self, name: &[u8]) -> bool;

    /// `if(TEST name)`.
    fn test_exists(&self, name: &[u8]) -> bool;

    /// `if(RULE name)` (added in CMake 4.5).
    fn rule_exists(&self, name: &[u8]) -> bool;

    /// Query active policy status (`CMP0139`, `CMP0222`, `CMP0223`, `CMP0186`, etc.).
    fn policy(&self, id: PolicyId) -> PolicyStatus;

    /// Access [`Host`] for filesystem predicates (`EXISTS`, `IS_DIRECTORY`, `IS_NEWER_THAN`, etc.).
    fn host(&self) -> &dyn Host;

    /// Current directory for resolving relative paths in filesystem predicates.
    fn base_dir(&self) -> &Path;

    /// Compile or fetch cached CMake regex.
    fn regex(&mut self, pattern: &[u8]) -> Result<Arc<Regex>, RegexError>;

    /// Update `CMAKE_MATCH_0..9` and `CMAKE_MATCH_COUNT` in the current scope.
    fn set_match_vars(&mut self, captures: Option<(&Captures, &[u8])>);

    /// Emit a non-fatal warning during condition evaluation.
    fn warn(&mut self, diag: Diagnostic);
}

/// Error produced when evaluating a malformed `if()` or `while()` condition.
#[derive(Clone, Debug, PartialEq, Eq, Error)]
#[error("{message}")]
pub struct CondError {
    /// Exact CMake condition diagnostic text.
    pub message: BString,
}

pub use condition::eval_condition;

/// CMake template substitution for `configure_file`.
pub mod configure;

#[cfg(test)]
mod tests;
