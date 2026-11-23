//! Core CMake 4.5.1 interpreter runtime, variable/policy/diagnostic scope stacks,
//! control flow (`if`, `foreach`, `while`, `function`, `macro`, `block`, `return`,
//! `break`, `continue`), `include`, `message`, and the builtin [`Cx`] API (ADR-005).
//!
//! # Architecture
//!
//! - [`Interp`] owns four independent stacks: variable scopes, policies, diagnostics,
//!   and the execution [`Backtrace`].
//! - The executor runs a flat command list. Block structure (`if`/`endif` and so on) is
//!   computed once per listfile, so block bodies run without copying.
//! - Built-in commands receive expanded arguments through [`BuiltinFn`] and talk to the
//!   interpreter only through [`Cx`].
//! - Argument expansion and condition evaluation go through `rcmake_expr::expand_args` and
//!   `rcmake_expr::eval_condition`. [`Interp`] implements [`ExpandEnv`] and [`CondEnv`].

#![forbid(unsafe_code)]

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use rcmake_base::{BString, Limits, fold_command_name};
use rcmake_diag::{
    Backtrace, DiagCategory, DiagEngine, DiagOptions, DiagState, Effect, MessageKind, PolicyError,
    PolicyId, PolicyMap, PolicyStatus, RenderOptions, SourceMap,
};
use rcmake_expr::{CondEnv, CondError, ExpandEnv, ExpandError, ExpandedArg};
use rcmake_host::Host;
use rcmake_model::{Cache, CacheType, GlobalState, ProjectModel, PropertyMap};
use rcmake_regex::{Captures, Regex, RegexCache, RegexError};
use rcmake_syntax::{Argument, CommandInvocation};

/// Shared keyword argument parser helpers (`cmArgumentParser`-style) for command families.
pub mod args;

mod builtins;
mod code;
mod env;
mod exec;
mod flow;
mod hooks;
mod md5;
mod scope;
mod stack;
mod util;

#[cfg(test)]
mod tests;

pub use hooks::{TraceConfig, TraceFormat, WatchAccess};
pub use scope::Binding;

use code::Code;
use scope::VarStack;
use stack::LayeredStack;

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
    /// Renders `<command> <text>` as an error (`cmExecutionStatus::SetError`). The command
    /// name is spelled as written at the call site, as in CMake.
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
    /// Commands that expand their own arguments ([`Self::add_raw`]).
    raw: BTreeSet<BString>,
}

impl Registry {
    /// Register a built-in command under its canonical lowercase name.
    pub fn add(&mut self, name: &'static str, func: BuiltinFn, flags: CmdFlags) {
        let lower = fold_command_name(name.as_bytes());
        self.raw.remove(&lower);
        self.builtins.insert(lower, (func, flags));
    }

    /// Register a built-in command that expands its own arguments, as CMake commands
    /// that take `cmListFileArgument`s do (`cmake_language`, so that
    /// `cmake_language(DEFER CALL)` keeps the call arguments unexpanded).
    ///
    /// The evaluator does not expand the arguments of such a command and passes an
    /// empty argument slice. The command reads [`Cx::raw_args`] and expands them with
    /// [`Cx::expand_raw_args`].
    pub fn add_raw(&mut self, name: &'static str, func: BuiltinFn, flags: CmdFlags) {
        let lower = fold_command_name(name.as_bytes());
        self.raw.insert(lower.clone());
        self.builtins.insert(lower, (func, flags));
    }

    /// `true` if the command was registered with [`Self::add_raw`].
    #[must_use]
    pub fn takes_raw_args(&self, name: &[u8]) -> bool {
        self.raw.contains(&fold_command_name(name))
    }

    /// Look up a built-in command by any-case name.
    #[must_use]
    pub fn get(&self, name: &[u8]) -> Option<(BuiltinFn, CmdFlags)> {
        let lower = fold_command_name(name);
        self.builtins.get(&lower).copied()
    }

    /// All registered lowercase names in sorted order.
    pub fn names(&self) -> impl Iterator<Item = &BString> {
        self.builtins.keys()
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
    /// Script arguments (`CMAKE_ARGV0..N` / `CMAKE_ARGC`): the complete command line,
    /// starting with the `cmake` executable.
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

/// Message log level (`--log-level`, `CMAKE_MESSAGE_LOG_LEVEL`), from least to most
/// verbose.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum LogLevel {
    /// `ERROR`.
    Error,
    /// `WARNING`.
    Warning,
    /// `NOTICE`.
    Notice,
    /// `STATUS` (default).
    Status,
    /// `VERBOSE`.
    Verbose,
    /// `DEBUG`.
    Debug,
    /// `TRACE`.
    Trace,
}

impl LogLevel {
    /// Parse a log level name (ASCII case-insensitive).
    #[must_use]
    pub fn parse(name: &[u8]) -> Option<Self> {
        let upper = name.to_ascii_uppercase();
        match upper.as_slice() {
            b"ERROR" => Some(Self::Error),
            b"WARNING" => Some(Self::Warning),
            b"NOTICE" => Some(Self::Notice),
            b"STATUS" => Some(Self::Status),
            b"VERBOSE" => Some(Self::Verbose),
            b"DEBUG" => Some(Self::Debug),
            b"TRACE" => Some(Self::Trace),
            _ => None,
        }
    }
}

/// Kind of a user-defined command.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UserCommandKind {
    /// Defined with `function()`.
    Function,
    /// Defined with `macro()`.
    Macro,
}

/// Commands that `function()` and `macro()` can not override.
pub(crate) const FLOW_CONTROL: [&[u8]; 17] = [
    b"block",
    b"break",
    b"continue",
    b"else",
    b"elseif",
    b"endblock",
    b"endforeach",
    b"endfunction",
    b"endif",
    b"endmacro",
    b"endwhile",
    b"foreach",
    b"function",
    b"if",
    b"macro",
    b"return",
    b"while",
];

/// A recorded `function()` or `macro()` definition.
#[derive(Debug)]
pub(crate) struct UserCommand {
    pub(crate) kind: UserCommandKind,
    /// Name as written in the definition (`Args[0]`).
    pub(crate) name: BString,
    /// Formal parameter names.
    pub(crate) params: Vec<BString>,
    pub(crate) body: Code,
    /// Line of the `function()`/`macro()` command.
    pub(crate) line: u32,
    /// Policy settings recorded at definition time.
    pub(crate) policies: PolicyMap,
    /// Diagnostic settings recorded at definition time.
    pub(crate) diags: DiagState,
}

/// A command entry in the scripted command table.
#[derive(Clone)]
pub(crate) enum Command {
    /// A built-in command. The `bool` is `true` for a command that expands its own
    /// arguments ([`Registry::add_raw`]).
    Builtin(BuiltinFn, CmdFlags, bool),
    User(Arc<UserCommand>),
}

/// Policy/diagnostic stack lengths recorded when a snapshot (file, call, block) starts.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Snapshot {
    pub(crate) policy_base: usize,
    pub(crate) diag_base: usize,
}

type ExpandFn =
    fn(&[Argument], &mut dyn ExpandEnv, &mut Vec<ExpandedArg>) -> Result<(), ExpandError>;
type CondFn = fn(&[ExpandedArg], &mut dyn CondEnv) -> Result<bool, CondError>;
type ExpandStringFn = fn(&[u8], &mut dyn ExpandEnv) -> Result<BString, ExpandError>;

/// Argument expansion and condition evaluation entry points (`rcmake-expr`).
#[derive(Clone, Copy)]
pub(crate) struct Frontend {
    pub(crate) expand: ExpandFn,
    pub(crate) expand_string: ExpandStringFn,
    pub(crate) cond: CondFn,
}

impl Default for Frontend {
    fn default() -> Self {
        Self {
            expand: rcmake_expr::expand_args,
            expand_string: rcmake_expr::expand_string,
            cond: rcmake_expr::eval_condition,
        }
    }
}

/// Hard ceiling for the recursion limit, so that a large
/// `CMAKE_MAXIMUM_RECURSION_DEPTH` can not exhaust the native stack.
pub const HARD_RECURSION_CEILING: u32 = 20_000;

/// Core CMake 4.5.1 interpreter.
pub struct Interp {
    config: InterpConfig,
    cache: Cache,
    global: GlobalState,
    project: ProjectModel,
    /// Current directory properties (`include_guard(DIRECTORY)`, `MACROS`).
    dir_props: PropertyMap,
    registry: Registry,
    /// User-defined commands and renamed (`_name`) previous definitions.
    scripted: BTreeMap<BString, Command>,
    vars: VarStack,
    policies: LayeredStack<PolicyMap>,
    diags: LayeredStack<DiagState>,
    snapshots: Vec<Snapshot>,
    engine: DiagEngine,
    render: RenderOptions,
    bt: Backtrace,
    depth: u32,
    /// `LoopBlockCounter`: one counter per function/directory barrier.
    loop_blocks: Vec<u32>,
    fatal: bool,
    /// The command being executed issued an error (`SetNestedError`).
    cmd_failed: bool,
    cur_line: u32,
    sources: SourceMap,
    regex_cache: RegexCache,
    checks: Vec<BString>,
    warned_cmp0219: BTreeSet<BString>,
    log_level: Option<LogLevel>,
    /// `--log-context`.
    show_log_context: bool,
    /// Host monotonic time at session start (wall-clock budget).
    started: core::time::Duration,
    /// True source directory for the active directory frame (not affected by user `set()`).
    source_dir: PathBuf,
    /// True binary directory for the active directory frame (not affected by user `set()`).
    binary_dir: PathBuf,
    /// Optional explicit exit code requested by `cmake_language(EXIT <code>)`.
    exit_override: Option<i32>,
    frontend: Frontend,
    /// Variable watches by variable name (`cmVariableWatch`).
    watches: BTreeMap<BString, Vec<hooks::Watch>>,
    next_watch_id: u64,
    /// Command tracing configuration (`--trace*`).
    trace: Option<TraceConfig>,
    /// Deferred-call queue of the directory listfile being processed.
    defer: Option<Vec<hooks::DeferredCall>>,
    next_defer_id: u64,
    /// Identifier of the deferred call whose frame is on top of the backtrace.
    top_defer_id: Option<BString>,
    /// The listfile command being executed (raw arguments for [`Cx::raw_args`]).
    cur_cmd: Option<(Arc<[CommandInvocation]>, usize)>,
    /// `--trace-expand` is expanding the arguments of a traced command.
    trace_expanding: bool,
    /// Listfile of each file and function call scope (`LISTFILE_STACK`).
    listfiles: Vec<Arc<[u8]>>,
}

impl Interp {
    /// Create a new interpreter with registered command families.
    ///
    /// The core commands owned by this crate (`message`, `include`, `include_guard`,
    /// `break`, `continue`, `return`) are registered first. Then `register` adds the
    /// other command families.
    #[must_use]
    pub fn new(config: InterpConfig, register: fn(&mut Registry)) -> Self {
        let mut registry = Registry::default();
        builtins::register_core(&mut registry);
        register(&mut registry);
        let mut cache = Cache::default();
        for def in &config.defines {
            cache.insert(
                def.name.clone(),
                rcmake_model::CacheEntry {
                    value: def.value.clone(),
                    ty: def.ty.unwrap_or(CacheType::Uninitialized),
                    help: BString::default(),
                    advanced: false,
                    props: PropertyMap::default(),
                },
            );
        }
        let diag_state = config.diag.initial_state;
        let engine = DiagEngine::new(config.diag.clone());
        let render = RenderOptions {
            relative_base: Some(rcmake_host::path::from_native(&config.cwd)),
            ..RenderOptions::default()
        };
        let started = config.host.monotonic();
        let initial_dir = config.cwd.clone();
        let mut interp = Self {
            config,
            cache,
            global: GlobalState::default(),
            project: ProjectModel::default(),
            dir_props: PropertyMap::default(),
            registry,
            scripted: BTreeMap::new(),
            vars: VarStack::default(),
            policies: LayeredStack::new(PolicyMap::default()),
            diags: LayeredStack::new(diag_state),
            snapshots: vec![Snapshot {
                policy_base: 1,
                diag_base: 1,
            }],
            engine,
            render,
            bt: Backtrace::default(),
            depth: 0,
            loop_blocks: vec![0],
            fatal: false,
            cmd_failed: false,
            cur_line: 0,
            sources: SourceMap::default(),
            regex_cache: RegexCache::default(),
            checks: Vec::new(),
            warned_cmp0219: BTreeSet::new(),
            log_level: None,
            show_log_context: false,
            started,
            source_dir: initial_dir.clone(),
            binary_dir: initial_dir,
            exit_override: None,
            frontend: Frontend::default(),
            watches: BTreeMap::new(),
            next_watch_id: 0,
            trace: None,
            defer: None,
            next_defer_id: 0,
            top_defer_id: None,
            cur_cmd: None,
            trace_expanding: false,
            listfiles: Vec::new(),
        };
        interp.init_default_vars();
        interp
    }

    /// Execute a standalone `-P` script at `path`.
    pub fn run_script(&mut self, path: &Path) -> RunOutcome {
        self.run_script_impl(path)
    }

    /// Execute a project configure session (`source_dir/CMakeLists.txt` with `binary_dir`
    /// and optional `-C` preload scripts).
    pub fn run_configure(
        &mut self,
        source_dir: &Path,
        binary_dir: &Path,
        preload_scripts: &[PathBuf],
    ) -> RunOutcome {
        self.run_configure_impl(source_dir, binary_dir, preload_scripts)
    }

    /// Execute one `-C` initial cache script in the current session.
    pub fn run_preload_script(&mut self, path: &Path) -> bool {
        self.run_preload_script_impl(path)
    }

    /// Execution context for embedding code (CLI setup, presets, tests).
    pub fn cx(&mut self) -> Cx<'_> {
        Cx { interp: self }
    }

    /// Set the `--log-level` override (`None` uses `CMAKE_MESSAGE_LOG_LEVEL`).
    pub fn set_log_level(&mut self, level: Option<LogLevel>) {
        self.log_level = level;
    }

    /// Show `CMAKE_MESSAGE_CONTEXT` in messages (`--log-context`).
    pub fn set_log_context(&mut self, show: bool) {
        self.show_log_context = show;
    }

    /// Turn command tracing on (`Some`) or off (`None`).
    ///
    /// Each traced command writes one line before it runs. End commands (`endif`,
    /// `endforeach`, and so on) write their line after the block. With
    /// [`TraceFormat::JsonV1`], this call first writes the version line
    /// `{"version":{"major":1,"minor":2}}`. With [`TraceConfig::redirect`], this call
    /// truncates the output file.
    pub fn set_trace_config(&mut self, config: Option<TraceConfig>) {
        self.install_trace(config);
    }

    /// Active trace configuration.
    #[must_use]
    pub fn trace_config(&self) -> Option<&TraceConfig> {
        self.trace.as_ref()
    }

    /// Diagnostic engine with error flags and recorded SARIF results
    /// ([`DiagEngine::sarif_json`]).
    #[must_use]
    pub fn diag_engine(&self) -> &DiagEngine {
        &self.engine
    }

    /// `true` once a fatal error stopped processing.
    #[must_use]
    pub fn fatal_occurred(&self) -> bool {
        self.fatal
    }

    /// Exit code for the current state (`1` if any error was issued, or explicit `cmake_language(EXIT)` code).
    #[must_use]
    pub fn exit_code(&self) -> i32 {
        self.exit_override
            .unwrap_or_else(|| i32::from(self.fatal || self.engine.error_occurred()))
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
        self.interp.bt.clone()
    }

    /// Active [`RunMode`].
    #[must_use]
    pub fn mode(&self) -> RunMode {
        self.interp.config.mode
    }

    /// Execution limits.
    #[must_use]
    pub fn limits(&self) -> &Limits {
        &self.interp.config.limits
    }

    /// Look up a normal variable (falling back to cache). Notifies variable watches
    /// (`READ_ACCESS` or `UNKNOWN_READ_ACCESS`).
    pub fn var(&mut self, name: &[u8]) -> Option<BString> {
        self.interp.read_var(name)
    }

    /// Look up a normal variable only (no cache fallback).
    #[must_use]
    pub fn var_normal_only(&self, name: &[u8]) -> Option<BString> {
        self.interp.vars.get(name).cloned()
    }

    /// Innermost normal binding of `name` (`None` if no scope binds it).
    #[must_use]
    pub fn binding(&self, name: &[u8]) -> Option<Binding> {
        self.interp.vars.lookup(name).cloned()
    }

    /// `if(DEFINED name)`: a normal binding or a cache entry exists.
    #[must_use]
    pub fn var_defined(&self, name: &[u8]) -> bool {
        self.interp.vars.get(name).is_some() || self.interp.cache.contains(name)
    }

    /// Value of `name` or an empty string (`GetSafeDefinition`).
    #[must_use]
    pub fn var_or_empty(&self, name: &[u8]) -> BString {
        self.interp.lookup_var(name).cloned().unwrap_or_default()
    }

    /// `true` if the value of `name` is a true constant (`cmMakefile::IsOn`).
    #[must_use]
    pub fn var_is_on(&self, name: &[u8]) -> bool {
        self.interp
            .lookup_var(name)
            .is_some_and(|v| rcmake_base::truth::is_on(v))
    }

    /// All visible variable names plus cache names, sorted (`VARIABLES` property).
    #[must_use]
    pub fn variable_names(&self) -> Vec<BString> {
        let mut names = self.interp.vars.visible_names();
        names.extend(self.interp.cache.iter().map(|(k, _)| k.clone()));
        names.sort();
        names.dedup();
        names
    }

    /// Set a normal variable in the current scope, enforcing `limits.max_value_bytes`.
    /// Notifies variable watches (`MODIFIED_ACCESS`).
    pub fn set_var(&mut self, name: &[u8], value: BString) {
        if value.len() > self.interp.config.limits.max_value_bytes {
            self.interp
                .error_here(b"Variable value exceeded maximum allowed byte length.".into());
            self.interp.fatal = true;
            return;
        }
        self.interp.define(name, value);
    }

    /// Request immediate termination with exit code `code` (`cmake_language(EXIT <code>)`).
    pub fn request_exit(&mut self, code: i32) {
        self.interp.exit_override = Some(code);
        self.interp.fatal = true;
    }

    /// All registered builtin + user-defined command names, sorted (`COMMANDS` property).
    #[must_use]
    pub fn registered_command_names(&self) -> Vec<BString> {
        let mut names: Vec<BString> = self.interp.registry.names().cloned().collect();
        names.extend(self.interp.scripted.keys().cloned());
        for kw in [
            "if",
            "elseif",
            "else",
            "endif",
            "foreach",
            "endforeach",
            "while",
            "endwhile",
            "function",
            "endfunction",
            "macro",
            "endmacro",
            "block",
            "endblock",
        ] {
            names.push(BString::from(kw));
        }
        names.sort();
        names.dedup();
        names
    }

    /// Unset a normal variable in the current scope. Notifies variable watches
    /// (`REMOVED_ACCESS`).
    pub fn unset_var(&mut self, name: &[u8]) {
        self.interp.undefine(name);
    }

    /// Set (`Some`) or unset (`None`) a variable in the immediate parent scope (raise-then-set).
    ///
    /// At the outermost scope this issues the `CMD_AUTHOR` warning
    /// `Cannot set "<name>": current scope has no parent.`. Otherwise this notifies
    /// variable watches (`MODIFIED_ACCESS`).
    pub fn set_parent_scope(&mut self, name: &[u8], value: Option<BString>) {
        self.interp.raise_var(name, value);
    }

    /// Raise each name to the parent scope with its current value, or unset it in the
    /// parent if it is not a normal variable (`cmMakefile::RaiseScope`).
    pub fn raise_scope(&mut self, names: &[BString]) {
        self.interp.raise_vars(names);
    }

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

    /// Mutable access to the current directory properties.
    pub fn directory_props(&mut self) -> &mut PropertyMap {
        &mut self.interp.dir_props
    }

    /// `CMAKE_CURRENT_LIST_FILE`.
    #[must_use]
    pub fn current_list_file(&self) -> BString {
        self.var_or_empty(b"CMAKE_CURRENT_LIST_FILE")
    }

    /// `CMAKE_CURRENT_LIST_DIR`.
    #[must_use]
    pub fn current_list_dir(&self) -> BString {
        self.var_or_empty(b"CMAKE_CURRENT_LIST_DIR")
    }

    /// `CMAKE_CURRENT_SOURCE_DIR` as a native path.
    #[must_use]
    pub fn current_source_dir(&self) -> PathBuf {
        self.interp.current_source_dir()
    }

    /// `CMAKE_CURRENT_BINARY_DIR` as a native path (backed by true directory state, not
    /// user-mutated variables).
    #[must_use]
    pub fn current_binary_dir(&self) -> PathBuf {
        self.interp.binary_dir.clone()
    }

    /// Line of the command being executed.
    #[must_use]
    pub fn current_line(&self) -> u32 {
        self.interp.cur_line
    }

    /// Query a policy's active status. Policies whose `OLD` behavior was removed
    /// (`CMP0000`–`CMP0065`) are always `NEW`.
    #[must_use]
    pub fn policy(&self, id: PolicyId) -> PolicyStatus {
        self.interp.policy_status(id)
    }

    /// Mutable access to the top of the policy stack (no weak write-through; prefer
    /// [`Self::set_policy`]).
    pub fn policies_mut(&mut self) -> &mut PolicyMap {
        self.interp.policies_top_mut()
    }

    /// `cmake_policy(SET)`: update the top policy entry and the entries of enclosing
    /// function/macro calls up to the first strong entry.
    pub fn set_policy(&mut self, id: PolicyId, status: PolicyStatus) -> Result<(), PolicyError> {
        self.interp.policies.try_update(|map| map.set(id, status))?;
        if id == exec::CMP0198 {
            self.interp.update_parent_list_file_var();
        }
        Ok(())
    }

    /// `cmake_policy(VERSION)` / `cmake_minimum_required()`: apply a policy version with
    /// weak write-through.
    pub fn apply_policy_version(
        &mut self,
        version: &rcmake_base::version::Version,
    ) -> Result<(), PolicyError> {
        self.interp
            .policies
            .try_update(|map| map.apply_version(version))?;
        self.interp.update_parent_list_file_var();
        Ok(())
    }

    /// `cmake_minimum_required(VERSION <v>)`: set `CMAKE_MINIMUM_REQUIRED_VERSION` and
    /// apply the policy version.
    pub fn set_cmake_minimum_required(
        &mut self,
        text: &[u8],
        version: &rcmake_base::version::Version,
    ) -> Result<(), PolicyError> {
        self.apply_policy_version(version)?;
        self.set_var(b"CMAKE_MINIMUM_REQUIRED_VERSION", BString::from(text));
        Ok(())
    }

    /// `cmake_policy(PUSH)`.
    pub fn push_policy_scope(&mut self) {
        self.interp.policies.push_copy();
    }

    /// `cmake_policy(POP)`. Issues `cmake_policy POP without matching PUSH` as a fatal
    /// error and returns `Err(CmdError::Reported)` when the current snapshot has no
    /// matching push.
    pub fn pop_policy_scope(&mut self) -> Result<(), CmdError> {
        self.interp.pop_policy()
    }

    /// Query a diagnostic category's active state.
    #[must_use]
    pub fn diag_state(&self) -> &DiagState {
        self.interp.diag_top()
    }

    /// Mutable access to the top of the diagnostic stack (no weak write-through; prefer
    /// [`Self::update_diag_state`]).
    pub fn diag_state_mut(&mut self) -> &mut DiagState {
        self.interp.diag_top_mut()
    }

    /// Update the diagnostic state with weak write-through (`cmake_diagnostic(SET)`).
    pub fn update_diag_state(&mut self, update: impl FnMut(&mut DiagState)) {
        self.interp.diags.update(update);
    }

    /// `cmake_diagnostic(PUSH)`.
    pub fn push_diag_scope(&mut self) {
        self.interp.diags.push_copy();
    }

    /// `cmake_diagnostic(POP)`. Issues `cmake_diagnostic POP without matching PUSH` as a
    /// fatal error when the current snapshot has no matching push.
    pub fn pop_diag_scope(&mut self) -> Result<(), CmdError> {
        self.interp.pop_diag()
    }

    /// Diagnostic engine (error flags and SARIF results).
    #[must_use]
    pub fn diag_engine(&self) -> &DiagEngine {
        &self.interp.engine
    }

    /// Issue a diagnostic at the current call site.
    ///
    /// - With a `category`, the active diagnostic state selects ignore, warning, error, or
    ///   fatal error (`IssueDiagnostic`).
    /// - Without a category, `effect` decides: [`Effect::SetError`] marks the current
    ///   command as failed and [`Effect::Fatal`] also stops processing.
    pub fn issue(
        &mut self,
        kind: MessageKind,
        effect: Effect,
        category: Option<DiagCategory>,
        body: BString,
    ) {
        self.interp.issue_here(kind, effect, category, body, true);
    }

    /// Issue the standard `Policy CMPnnnn is not set: ...` warning (`CMD_POLICY`),
    /// with optional text before and after it.
    pub fn issue_policy_warning(&mut self, id: PolicyId, preface: &[u8], postface: &[u8]) {
        self.interp.issue_policy_warning(id, preface, postface);
    }

    /// Mark processing as stopped (`cmSystemTools::SetFatalErrorOccurred`).
    pub fn set_fatal(&mut self) {
        self.interp.fatal = true;
    }

    /// `true` once a fatal error stopped processing.
    #[must_use]
    pub fn fatal_occurred(&self) -> bool {
        self.interp.fatal
    }

    /// Active message log level.
    #[must_use]
    pub fn log_level(&self) -> LogLevel {
        self.interp.log_level()
    }

    /// `if(COMMAND name)`: built-in, flow-control, or user-defined command exists.
    #[must_use]
    pub fn command_exists(&self, name: &[u8]) -> bool {
        self.interp.command_exists(name)
    }

    /// Kind of a user-defined command (`None` for built-ins and unknown names).
    #[must_use]
    pub fn user_command_kind(&self, name: &[u8]) -> Option<UserCommandKind> {
        match self.interp.scripted.get(&fold_command_name(name)) {
            Some(Command::User(u)) => Some(u.kind),
            _ => None,
        }
    }

    /// Resolve a potentially relative path against the active working/source directory.
    #[must_use]
    pub fn resolve_path(&self, p: &[u8]) -> PathBuf {
        let native = rcmake_host::path::to_native(p);
        if native.is_absolute() {
            native
        } else {
            self.interp.current_source_dir().join(native)
        }
    }

    /// Compile or retrieve a cached regex.
    pub fn regex(&mut self, pat: &[u8]) -> Result<Arc<Regex>, RegexError> {
        self.interp.regex_cache.get_or_compile(pat)
    }

    /// Update `CMAKE_MATCH_0..9` and `CMAKE_MATCH_COUNT`. `None` clears previous matches.
    pub fn set_match_vars(&mut self, caps: Option<(&Captures, &[u8])>) {
        self.interp.store_matches(caps);
    }

    /// Evaluate an `if()` condition in the current context.
    pub fn eval_condition(&mut self, args: &[ExpandedArg]) -> Result<bool, CondError> {
        let cond = self.interp.frontend.cond;
        cond(args, self.interp)
    }

    /// Expand variable references and escapes in `src` without list splitting.
    pub fn expand_string(&mut self, src: &[u8]) -> Result<BString, ExpandError> {
        let expand = self.interp.frontend.expand_string;
        expand(src, self.interp)
    }

    /// Invoke another command by name with already expanded arguments (used by `list()`
    /// callbacks and `cmake_language(CALL)`).
    pub fn call_command(&mut self, name: &[u8], args: Vec<ExpandedArg>) -> CmdResult {
        self.interp.call_command(name, args)
    }

    /// Include and execute a CMake file (`include()` without module lookup).
    ///
    /// Returns `Err(CmdError::Status)` with CMake's text when the file is missing or can
    /// not be read, unless `options.optional` is set.
    pub fn include_file(&mut self, path: &Path, options: IncludeOptions) -> CmdResult {
        self.interp.include_file(path, options)
    }

    /// Parse and execute virtual CMake code (`cmake_language(EVAL CODE ...)`).
    pub fn eval_code(&mut self, code: &[u8]) -> CmdResult {
        self.interp.eval_code(code)
    }

    /// Raw (unexpanded) arguments of the listfile command being executed. Empty for a
    /// command run through [`Self::call_command`].
    ///
    /// Commands that delay expansion (`cmake_language(DEFER CALL)`) use this together
    /// with [`Self::expand_raw_args`].
    #[must_use]
    pub fn raw_args(&self) -> &[Argument] {
        self.interp
            .cur_cmd
            .as_ref()
            .and_then(|(cmds, i)| cmds.get(*i))
            .map_or(&[], |c| c.args.as_slice())
    }

    /// Expand raw arguments as command arguments are expanded. Reports expansion errors
    /// and returns `Err(CmdError::Reported)` after an error.
    pub fn expand_raw_args(&mut self, args: &[Argument]) -> Result<Vec<ExpandedArg>, CmdError> {
        self.interp.expand(args)
    }

    // ----------------------------------------------------------------------------------
    // Variable watches (`variable_watch()`).
    // ----------------------------------------------------------------------------------

    /// `variable_watch(<var> [<command>])`: add a watch. Every call adds a new watch,
    /// also for the same variable and command.
    ///
    /// On each access to `var`, the watch calls `command` with the arguments
    /// `<var> <access> <value> <CMAKE_CURRENT_LIST_FILE> <LISTFILE_STACK>` (see
    /// [`WatchAccess`]). Without a command (`None` or empty), the watch prints
    /// `Variable "<var>" was accessed using <access> with value "<value>".` as a log
    /// message. A callback does not run again while it is running. `CMP0219` applies to
    /// the callback arguments. The command checks `CMAKE_CURRENT_LIST_FILE` itself.
    pub fn add_variable_watch(&mut self, var: &[u8], command: Option<&[u8]>) {
        self.interp.add_watch(var, command.unwrap_or_default());
    }

    /// Remove the watches of `var` that call `command` (all watches of `var` when
    /// `command` is `None`). Returns `true` if a watch was removed.
    pub fn remove_variable_watch(&mut self, var: &[u8], command: Option<&[u8]>) -> bool {
        self.interp.remove_watch(var, command)
    }

    /// Callback commands of the watches of `var` in registration order. An empty entry
    /// is a watch without a command.
    #[must_use]
    pub fn variable_watches(&self, var: &[u8]) -> Vec<BString> {
        self.interp.watch_commands(var)
    }

    // ----------------------------------------------------------------------------------
    // Deferred calls (`cmake_language(DEFER)`).
    // ----------------------------------------------------------------------------------

    /// `true` while a directory listfile runs in configure mode, so that deferred calls
    /// can be queued for the end of the directory.
    #[must_use]
    pub fn defer_active(&self) -> bool {
        self.interp.defer.is_some()
    }

    /// A new unique deferred-call identifier (`__0`, `__1`, ...).
    pub fn defer_new_id(&mut self) -> BString {
        self.interp.new_defer_id()
    }

    /// Queue `command` with raw `args` (see [`Self::raw_args`]) for the end of the
    /// current directory. The arguments are expanded when the call runs. The call keeps
    /// the file and line of the current command, and runs with the policies that are
    /// active at the end of the directory. Returns `false` when no queue is active
    /// (`DEFER CALL may not be scheduled in directory ... at this time.`).
    pub fn defer_call(&mut self, id: BString, command: &[u8], args: &[Argument]) -> bool {
        self.interp.queue_deferred(id, command, args)
    }

    /// Identifiers of the queued calls that are not canceled, in queue order. `None`
    /// when no queue is active.
    #[must_use]
    pub fn defer_get_call_ids(&self) -> Option<Vec<BString>> {
        self.interp.deferred_ids()
    }

    /// `[command, args...]` of the queued call `id` (raw argument text), or an empty
    /// list if `id` is unknown. `None` when no queue is active.
    #[must_use]
    pub fn defer_get_call(&self, id: &[u8]) -> Option<Vec<BString>> {
        self.interp.deferred_call(id)
    }

    /// Cancel every queued call with identifier `id`. Returns `false` when no queue is
    /// active.
    pub fn defer_cancel_call(&mut self, id: &[u8]) -> bool {
        self.interp.cancel_deferred(id)
    }
}
