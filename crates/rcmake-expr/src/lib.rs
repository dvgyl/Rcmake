//! CMake 4.5.1 variable expansion (`${...}`, `$ENV{...}`, `$CACHE{...}`), escape decoding,
//! unquoted argument list splitting, `if()`/`while()` condition evaluation, and
//! `configure_file` `@VAR@` / `#cmakedefine` substitution.

#![forbid(unsafe_code)]

use std::path::Path;
use std::sync::Arc;

use rcmake_base::BString;
use rcmake_diag::{Diagnostic, PolicyId, PolicyStatus};
use rcmake_host::Host;
use rcmake_regex::{Captures, Regex, RegexError};
use rcmake_syntax::{ArgKind, Argument};
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

    /// Current 1-based listfile line number for `${CMAKE_CURRENT_LIST_LINE}`.
    fn current_line(&self) -> u32;
}

/// Error produced during variable or escape expansion.
#[derive(Clone, Debug, PartialEq, Eq, Error)]
#[error("{message}")]
pub struct ExpandError {
    /// Exact CMake expansion error text.
    pub message: BString,
}

/// Expand a slice of parsed [`Argument`]s into [`ExpandedArg`]s.
///
/// - [`ArgKind::Bracket`] passes through verbatim as 1 argument.
/// - [`ArgKind::Quoted`] decodes escapes and expands `${...}`/`$ENV{...}`/`$CACHE{...}` into 1 argument.
/// - [`ArgKind::Unquoted`] decodes escapes and expands variables, then splits on `;`
///   with `Empty::Drop` (each resulting element retains `ArgKind::Unquoted`).
pub fn expand_args(
    _args: &[Argument],
    _env: &mut dyn ExpandEnv,
    _out: &mut Vec<ExpandedArg>,
) -> Result<(), ExpandError> {
    Err(ExpandError {
        message: "expand_args not yet implemented (scheduled for M1-W1)".into(),
    })
}

/// Expand variables and escape sequences inside a raw byte string without list splitting.
pub fn expand_string(_src: &[u8], _env: &mut dyn ExpandEnv) -> Result<BString, ExpandError> {
    Err(ExpandError {
        message: "expand_string not yet implemented (scheduled for M1-W1)".into(),
    })
}

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

/// Evaluate an expanded `if()` / `while()` condition using CMake 4.5.1's multi-pass
/// reduction rules.
pub fn eval_condition(_args: &[ExpandedArg], _env: &mut dyn CondEnv) -> Result<bool, CondError> {
    Err(CondError {
        message: "eval_condition not yet implemented (scheduled for M1-W1)".into(),
    })
}

/// `configure_file` template substitution (`@VAR@`, `${VAR}`, `#cmakedefine`, `#cmakedefine01`).
pub mod configure {
    use rcmake_base::BString;

    use crate::{ExpandEnv, ExpandError};

    /// Options controlling `configure_file` expansion.
    #[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
    pub struct ConfigureOptions {
        /// Expand only `@VAR@` references (`@ONLY`).
        pub at_only: bool,
        /// Escape double quotes with backslashes (`ESCAPE_QUOTES`).
        pub escape_quotes: bool,
    }

    /// Perform `configure_file` template substitution over `input`.
    pub fn configure(
        _input: &[u8],
        _env: &mut dyn ExpandEnv,
        _options: ConfigureOptions,
    ) -> Result<BString, ExpandError> {
        Err(ExpandError {
            message: "configure not yet implemented (scheduled for M1-W1)".into(),
        })
    }
}
