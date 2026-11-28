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
    /// Return true for quoted or bracket arguments. These are literal in `if()`.
    #[must_use]
    pub const fn is_quoted_or_bracket(&self) -> bool {
        matches!(self.kind, ArgKind::Quoted | ArgKind::Bracket)
    }
}

/// Environment queried during variable expansion.
pub trait ExpandEnv {
    /// Look up a normal variable (falling back to cache if normal binding is absent).
    fn var(&mut self, name: &[u8]) -> Option<BString>;

    /// Borrow a normal variable (falling back to cache) without cloning when possible.
    ///
    /// Implementations with variable watches must run the full watch path when a watch
    /// applies and only return a direct borrow when no watch or side effect can run.
    fn var_ref(&mut self, name: &[u8]) -> Option<std::borrow::Cow<'_, [u8]>> {
        self.var(name)
            .map(|value| std::borrow::Cow::Owned(value.into()))
    }

    /// Look up a cache variable directly (`$CACHE{...}`).
    fn cache_var(&mut self, name: &[u8]) -> Option<BString>;

    /// Borrow a cache variable directly (`$CACHE{...}`) without cloning when possible.
    fn cache_var_ref(&mut self, name: &[u8]) -> Option<std::borrow::Cow<'_, [u8]>> {
        self.cache_var(name)
            .map(|value| std::borrow::Cow::Owned(value.into()))
    }

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

    /// Limits for the expanded arguments of the current command call.
    ///
    /// `used_bytes` counts the arguments that the caller already expanded for the call.
    fn argument_budget(&self) -> ArgumentBudget {
        ArgumentBudget::default()
    }
}

/// Start of the message of an [`ExpandError`] for a resource limit.
const ARGUMENT_LIMIT: &str = "Command arguments expanded to more than ";

/// Count and byte limits for the expanded arguments of one command call.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ArgumentBudget {
    /// Maximum number of arguments.
    pub max_arguments: usize,
    /// Maximum total bytes of all argument values.
    pub max_bytes: usize,
    /// Number of arguments already expanded for the call.
    pub used_arguments: usize,
    /// Bytes of the arguments already expanded for the call.
    pub used_bytes: usize,
}

impl Default for ArgumentBudget {
    fn default() -> Self {
        let limits = rcmake_base::Limits::default();
        Self::new(
            limits.max_command_arguments,
            limits.max_command_argument_bytes,
        )
    }
}

impl ArgumentBudget {
    /// Permit at most `max_arguments` arguments and `max_bytes` bytes in total.
    #[must_use]
    pub const fn new(max_arguments: usize, max_bytes: usize) -> Self {
        Self {
            max_arguments,
            max_bytes,
            used_arguments: 0,
            used_bytes: 0,
        }
    }

    /// Reserve one argument of `len` bytes before the argument is copied.
    ///
    /// # Errors
    ///
    /// Return a resource limit error and reserve nothing if the call would pass a limit.
    pub fn take(&mut self, len: usize) -> Result<(), ExpandError> {
        if self.used_arguments >= self.max_arguments {
            return Err(ExpandError::resource_limit(format!(
                "{ARGUMENT_LIMIT}{} elements.",
                self.max_arguments
            )));
        }
        let Some(bytes) = self
            .used_bytes
            .checked_add(len)
            .filter(|&bytes| bytes <= self.max_bytes)
        else {
            return Err(ExpandError::resource_limit(format!(
                "{ARGUMENT_LIMIT}{} bytes.",
                self.max_bytes
            )));
        };
        self.used_arguments += 1;
        self.used_bytes = bytes;
        Ok(())
    }
}

/// Error produced during variable or escape expansion.
#[derive(Clone, Debug, PartialEq, Eq, Error)]
#[error("{message}")]
pub struct ExpandError {
    /// Exact CMake expansion error text.
    pub message: BString,
    /// Whether the input is invalid or a resource limit stopped the expansion.
    pub kind: ExpandErrorKind,
}

/// Kind of an [`ExpandError`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ExpandErrorKind {
    /// The text is not valid CMake syntax.
    #[default]
    Syntax,
    /// A resource limit stopped the expansion: the argument count or bytes of an
    /// [`ArgumentBudget`], the value size, or the nesting depth.
    ResourceLimit,
}

impl ExpandError {
    /// Error for invalid syntax.
    #[must_use]
    pub fn syntax(message: impl Into<BString>) -> Self {
        Self {
            message: message.into(),
            kind: ExpandErrorKind::Syntax,
        }
    }

    /// Error for a resource limit.
    #[must_use]
    pub fn resource_limit(message: impl Into<BString>) -> Self {
        Self {
            message: message.into(),
            kind: ExpandErrorKind::ResourceLimit,
        }
    }

    /// Return `true` if a resource limit stopped the expansion. The caller must stop the
    /// command at once.
    #[must_use]
    pub fn is_resource_limit(&self) -> bool {
        self.kind == ExpandErrorKind::ResourceLimit
    }
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
