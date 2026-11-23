//! Core CMake 4.5.1 interpreter runtime, variable/policy/diagnostic scope stacks,
//! control flow (`if`, `foreach`, `while`, `function`, `macro`, `block`, `return`,
//! `break`, `continue`), `include`, `message`, and the builtin [`Cx`] API (ADR-005).

#![forbid(unsafe_code)]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use rcmake_base::{BString, Limits, fold_command_name};
use rcmake_diag::{
    Backtrace, DiagCategory, DiagOptions, DiagState, Effect, MessageKind, PolicyId, PolicyMap,
    PolicyStatus,
};
use rcmake_expr::{CondError, ExpandedArg};
use rcmake_host::Host;
use rcmake_model::{Cache, CacheType, GlobalState, ProjectModel};
use rcmake_regex::{Captures, Regex, RegexError};

/// Shared keyword argument parser helpers (`cmArgumentParser`-style) for command families.
pub mod args;

/// Signature of every built-in CMake command registered in [`Registry`].
pub type BuiltinFn = fn(&mut Cx<'_>, &[ExpandedArg]) -> CmdResult;

/// Result returned by every built-in command.
pub type CmdResult = Result<Flow, CmdError>;

/// Structured control-flow transfer emitted by commands (`break`, `continue`, `return`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Flow {
    /// Continue normal sequential execution.
    Normal,
    /// Break out of the innermost `foreach` or `while` loop.
    Break,
    /// Jump to the next iteration of the innermost `foreach` or `while` loop.
    Continue,
    /// Return from the innermost `function`, `include` file, or directory, optionally
    /// propagating variable names across variable scopes (`CMP0140`).
    Return {
        /// Variable names passed to `return(PROPAGATE ...)`.
        propagate: Option<Vec<BString>>,
    },
}

/// Error returned by a built-in command.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CmdError {
    /// Renders `<lowercase-command> <text>` as a fatal error (`cmExecutionStatus::SetError`).
    Status(BString),
    /// The command already issued its own diagnostic(s) through [`Cx::issue`].
    Reported,
}

/// Metadata flags for a registered command.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CmdFlags {
    /// `true` if this command is allowed in `-P` script mode.
    pub scriptable: bool,
}

/// Registry of built-in commands indexed by ASCII-lowercased name.
#[derive(Default)]
pub struct Registry {
    builtins: BTreeMap<BString, (BuiltinFn, CmdFlags)>,
}

impl Registry {
    /// Register a built-in command under its canonical lowercase name.
    pub fn add(&mut self, name: &'static str, func: BuiltinFn, flags: CmdFlags) {
        self.builtins
            .insert(fold_command_name(name.as_bytes()), (func, flags));
    }

    /// Look up a built-in command by any-case name.
    #[must_use]
    pub fn get(&self, name: &[u8]) -> Option<(BuiltinFn, CmdFlags)> {
        let lower = fold_command_name(name);
        self.builtins.get(&lower).copied()
    }
}

/// Execution mode of an [`Interp`] session.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RunMode {
    /// Standalone script mode (`cmake -P <script>`).
    Script,
    /// Project configure mode (`CMakeLists.txt`).
    Configure,
}

/// A `-D<VAR>[:<TYPE>]=<VALUE>` CLI define applied before script execution.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CacheDefine {
    /// Variable name.
    pub name: BString,
    /// Optional explicit cache type (defaults to `Uninitialized`).
    pub ty: Option<CacheType>,
    /// Value bytes.
    pub value: BString,
}

/// Configuration used to initialize an [`Interp`] session.
#[derive(Clone)]
pub struct InterpConfig {
    /// Host abstraction for FS, env, processes, stdio, and clock.
    pub host: Arc<dyn Host>,
    /// Script vs Configure execution mode.
    pub mode: RunMode,
    /// Path to `share/cmake-4.5` containing vendored `Modules/` and `Templates/`.
    pub cmake_root: PathBuf,
    /// Path to the `cmake` executable (`CMAKE_COMMAND`).
    pub cmake_command: PathBuf,
    /// Initial working directory.
    pub cwd: PathBuf,
    /// Execution safety limits.
    pub limits: Limits,
    /// Initial diagnostic options (`-W*`, `--sarif-output`, etc.).
    pub diag: DiagOptions,
    /// Pre-script `-D` definitions in CLI order.
    pub defines: Vec<CacheDefine>,
    /// Script arguments (`CMAKE_ARGV0..N` / `CMAKE_ARGC`).
    pub script_args: Vec<BString>,
}

/// Options for [`Cx::include_file`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct IncludeOptions {
    /// Ignore missing file without error (`OPTIONAL`).
    pub optional: bool,
    /// Do not push a policy scope (`NO_POLICY_SCOPE`).
    pub no_policy_scope: bool,
    /// Do not push a diagnostic scope (`NO_DIAGNOSTIC_SCOPE`).
    pub no_diagnostic_scope: bool,
}

/// Outcome of running a CMake script or configure session.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RunOutcome {
    /// Process exit code (`0` on success, `1` on error).
    pub exit_code: i32,
}

/// Core CMake 4.5.1 interpreter (full implementation lands in Milestone 1 Wave 1).
pub struct Interp {
    config: InterpConfig,
    cache: Cache,
    global: GlobalState,
    project: ProjectModel,
    policies: PolicyMap,
    diag_state: DiagState,
}

impl Interp {
    /// Create a new interpreter with registered command families.
    #[must_use]
    pub fn new(config: InterpConfig, register: fn(&mut Registry)) -> Self {
        let mut reg = Registry::default();
        register(&mut reg);
        let mut cache = Cache::default();
        for def in &config.defines {
            cache.insert(
                def.name.clone(),
                rcmake_model::CacheEntry {
                    value: def.value.clone(),
                    ty: def.ty.unwrap_or(CacheType::Uninitialized),
                    help: BString::default(),
                    advanced: false,
                    props: Default::default(),
                },
            );
        }
        let diag_state = config.diag.initial_state;
        Self {
            config,
            cache,
            global: GlobalState::default(),
            project: ProjectModel::default(),
            policies: PolicyMap::default(),
            diag_state,
        }
    }

    /// Execute a standalone `-P` script at `path`.
    pub fn run_script(&mut self, _path: &Path) -> RunOutcome {
        RunOutcome { exit_code: 1 }
    }
}

/// Execution context passed to every built-in command (`BuiltinFn`).
pub struct Cx<'a> {
    interp: &'a mut Interp,
}

impl Cx<'_> {
    /// Access the [`Host`].
    #[must_use]
    pub fn host(&self) -> &dyn Host {
        self.interp.config.host.as_ref()
    }

    /// Current persistent [`Backtrace`].
    #[must_use]
    pub fn backtrace(&self) -> Backtrace {
        Backtrace::default()
    }

    /// Active [`RunMode`].
    #[must_use]
    pub fn mode(&self) -> RunMode {
        self.interp.config.mode
    }

    /// Look up a normal variable (falling back to cache).
    pub fn var(&mut self, name: &[u8]) -> Option<BString> {
        self.interp.cache.get(name).map(|e| e.value.clone())
    }

    /// Set a normal variable in the current scope.
    pub fn set_var(&mut self, _name: &[u8], _value: BString) {}

    /// Unset a normal variable in the current scope.
    pub fn unset_var(&mut self, _name: &[u8]) {}

    /// Set (`Some`) or unset (`None`) a variable in the immediate parent scope (raise-then-set).
    pub fn set_parent_scope(&mut self, _name: &[u8], _value: Option<BString>) {}

    /// Mutable access to the [`Cache`].
    pub fn cache(&mut self) -> &mut Cache {
        &mut self.interp.cache
    }

    /// Mutable access to [`GlobalState`].
    pub fn global(&mut self) -> &mut GlobalState {
        &mut self.interp.global
    }

    /// Mutable access to [`ProjectModel`].
    pub fn project(&mut self) -> &mut ProjectModel {
        &mut self.interp.project
    }

    /// Query a policy's active status.
    #[must_use]
    pub fn policy(&self, id: PolicyId) -> PolicyStatus {
        self.interp.policies.get(id)
    }

    /// Mutable access to the top of the policy stack.
    pub fn policies_mut(&mut self) -> &mut PolicyMap {
        &mut self.interp.policies
    }

    /// Query a diagnostic category's active state.
    #[must_use]
    pub fn diag_state(&self) -> &DiagState {
        &self.interp.diag_state
    }

    /// Mutable access to the top of the diagnostic stack.
    pub fn diag_state_mut(&mut self) -> &mut DiagState {
        &mut self.interp.diag_state
    }

    /// Issue a diagnostic at the current call site.
    pub fn issue(
        &mut self,
        _kind: MessageKind,
        _effect: Effect,
        _category: Option<DiagCategory>,
        _body: BString,
    ) {
    }

    /// Resolve a potentially relative path against the active working/source directory.
    #[must_use]
    pub fn resolve_path(&self, p: &[u8]) -> PathBuf {
        let native = rcmake_host::path::to_native(p);
        if native.is_absolute() {
            native
        } else {
            self.interp.config.cwd.join(native)
        }
    }

    /// Compile or retrieve a cached regex.
    pub fn regex(&mut self, pat: &[u8]) -> Result<Arc<Regex>, RegexError> {
        Regex::new(pat).map(Arc::new)
    }

    /// Update `CMAKE_MATCH_0..9` and `CMAKE_MATCH_COUNT`.
    pub fn set_match_vars(&mut self, _caps: Option<(&Captures, &[u8])>) {}

    /// Evaluate an `if()` condition in the current context.
    pub fn eval_condition(&mut self, _args: &[ExpandedArg]) -> Result<bool, CondError> {
        Err(CondError {
            message: "not yet implemented".into(),
        })
    }

    /// Invoke another command by name (used by `list()` callbacks and `cmake_language(CALL)`).
    pub fn call_command(&mut self, _name: &[u8], _args: Vec<ExpandedArg>) -> CmdResult {
        Err(CmdError::Status("not yet implemented".into()))
    }

    /// Include and execute a CMake file.
    pub fn include_file(&mut self, _path: &Path, _options: IncludeOptions) -> CmdResult {
        Err(CmdError::Status("not yet implemented".into()))
    }

    /// Parse and execute virtual CMake code (`cmake_language(EVAL CODE ...)`).
    pub fn eval_code(&mut self, _code: &[u8]) -> CmdResult {
        Err(CmdError::Status("not yet implemented".into()))
    }
}
