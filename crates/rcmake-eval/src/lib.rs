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
use std::path::PathBuf;
use std::sync::Arc;

use rcmake_base::{BString, Limits, fold_command_name};
use rcmake_diag::{
    Backtrace, DiagEngine, DiagOptions, DiagState, PolicyMap, RenderOptions, SourceMap,
};
use rcmake_expr::{CondEnv, CondError, ExpandEnv, ExpandError, ExpandedArg};
use rcmake_host::Host;
use rcmake_model::{CacheType, DirId, ProjectModel, PropertyMap};
use rcmake_regex::RegexCache;
use rcmake_syntax::{Argument, CommandInvocation};

/// Shared keyword argument parser helpers (`cmArgumentParser`-style) for command families.
pub mod args;

mod access;
mod builtins;
mod code;
mod configure_log;
mod cx;
mod env;
mod file_lock;
mod flow;
pub mod hermetic;
mod hooks;
mod interp;
pub mod journal;
mod md5;
mod profiler;
mod provenance;
mod scope;
mod services;
mod stack;
mod util;

#[cfg(test)]
mod tests;

pub use configure_log::FindEvent;
pub use cx::Cx;
pub use file_lock::FileLockScope;
pub use hooks::{TraceConfig, TraceFormat, WatchAccess};
pub use interp::StopReason;
pub use profiler::{CommandProfiler, CommandSample, SampleKind};
pub use provenance::{ValuePreview, VarEvent, VarLedger, VarOp, VarWrite};
pub use scope::Binding;
pub use services::{
    CancelToken, FetchLockMode, HermeticMode, LedgerLevel, ProbeCacheMode, Services, Site,
    SpeedOptions,
};

use code::Code;
use scope::VarStack;
use stack::LayeredStack;

/// Signature of every built-in CMake command registered in [`Registry`].
pub type BuiltinFn = fn(&mut Cx<'_>, &[ExpandedArg]) -> CmdResult;

/// Check expanded arguments before a built-in command runs.
pub type BuiltinGuard = fn(&mut Cx<'_>, &[ExpandedArg]) -> Result<(), CmdError>;

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

impl CmdError {
    /// Create an error that the evaluator reports with the command name.
    pub fn status(message: impl Into<BString>) -> Self {
        Self::Status(message.into())
    }
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
    builtins: BTreeMap<BString, Builtin>,
    guards: BTreeMap<BString, BuiltinGuard>,
}

#[derive(Clone, Copy)]
struct Builtin {
    func: BuiltinFn,
    flags: CmdFlags,
    raw: bool,
    replay: bool,
}

impl Registry {
    /// Set the guard for a built-in command.
    pub fn guard(&mut self, name: &'static str, check: BuiltinGuard) {
        self.guards
            .insert(fold_command_name(name.as_bytes()), check);
    }

    /// Register a built-in command under its canonical lowercase name.
    ///
    /// A later registration of the same name replaces the earlier one (also one from
    /// [`Self::add_raw`]). `rcmake-ctest` uses this to override `add_test`,
    /// `set_tests_properties`, `subdirs`, and other commands after
    /// `rcmake_commands::register_all`.
    pub fn add(&mut self, name: &'static str, func: BuiltinFn, flags: CmdFlags) {
        self.insert(name, func, flags, false);
    }

    /// Register a built-in command that expands its own arguments, as CMake commands
    /// that take `cmListFileArgument`s do (`cmake_language`, so that
    /// `cmake_language(DEFER CALL)` keeps the call arguments unexpanded).
    ///
    /// The evaluator does not expand the arguments of such a command and passes an
    /// empty argument slice. The command reads [`Cx::raw_args`] and expands them with
    /// [`Cx::expand_raw_args`].
    pub fn add_raw(&mut self, name: &'static str, func: BuiltinFn, flags: CmdFlags) {
        self.insert(name, func, flags, true);
    }

    fn insert(&mut self, name: &str, func: BuiltinFn, flags: CmdFlags, raw: bool) {
        self.builtins.insert(
            fold_command_name(name.as_bytes()),
            Builtin {
                func,
                flags,
                raw,
                replay: false,
            },
        );
    }

    /// `true` if the command was registered with [`Self::add_raw`].
    #[must_use]
    pub fn takes_raw_args(&self, name: &[u8]) -> bool {
        self.get_lower(&fold_command_name(name))
            .is_some_and(|b| b.raw)
    }

    /// Look up a built-in command by any-case name.
    #[must_use]
    pub fn get(&self, name: &[u8]) -> Option<(BuiltinFn, CmdFlags)> {
        self.get_lower(&fold_command_name(name))
            .map(|b| (b.func, b.flags))
    }

    fn get_lower(&self, name: &[u8]) -> Option<Builtin> {
        self.builtins.get(name).copied()
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
    /// Optional services for this session.
    pub services: Arc<Services>,
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

/// Options for [`Cx::run_subdirectory`] (`add_subdirectory()` keywords).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SubdirOptions {
    /// `EXCLUDE_FROM_ALL`.
    pub exclude_from_all: bool,
    /// `SYSTEM`.
    pub system: bool,
}

/// One active directory on [`Interp`]'s directory stack.
#[derive(Debug)]
pub(crate) struct DirFrame {
    /// The directory in the [`ProjectModel`] arena.
    pub(crate) id: DirId,
    pub(crate) trace_base: u32,
    /// Deferred-call queue of the directory listfile being processed (`None` outside
    /// a directory listfile).
    pub(crate) defer: Option<Vec<hooks::DeferredCall>>,
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
    /// Project model: cache, global state, and the directory/target arenas.
    project: ProjectModel,
    /// The root directory frame (script mode, or the top-level `CMakeLists.txt`).
    root_frame: DirFrame,
    /// Active subdirectory frames (`add_subdirectory()`), innermost last.
    dir_stack: Vec<DirFrame>,
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
    recursion_exceeded: bool,
    check_system_vars: bool,
    package_file_depth: usize,
    unwinding_package: bool,
    /// `LoopBlockCounter`: one counter per function/directory barrier.
    loop_blocks: Vec<u32>,
    fatal: bool,
    stop_reason: Option<StopReason>,
    steps: u64,
    var_ledger: Option<VarLedger>,
    write_seq: u64,
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
    /// Optional explicit exit code requested by `cmake_language(EXIT <code>)`.
    exit_override: Option<i32>,
    frontend: Frontend,
    /// Variable watches by variable name (`cmVariableWatch`).
    watches: BTreeMap<BString, Vec<hooks::Watch>>,
    variable_access: BTreeMap<BString, std::sync::atomic::AtomicBool>,
    next_watch_id: u64,
    /// Command tracing configuration (`--trace*`).
    trace: Option<TraceConfig>,
    command_profiler: Option<Arc<CommandProfiler>>,
    /// Configure log events for this run.
    configure_log: Vec<u8>,
    /// Expansion settings for nested `cmake_language(TRACE ON)` calls.
    trace_stack: Vec<bool>,
    find_debug_variables: Vec<BString>,
    find_debug_packages: Vec<BString>,
    next_defer_id: u64,
    running_deferred: bool,
    /// Identifier of the deferred call whose frame is on top of the backtrace.
    top_defer_id: Option<BString>,
    /// The listfile command being executed (raw arguments for [`Cx::raw_args`]).
    cur_cmd: Option<(Arc<[CommandInvocation]>, usize)>,
    command_end: usize,
    session_data: services::SessionData,
    expanded_observer: Option<services::ExpandedObserver>,
    parse_cache: Option<code::ParseCache>,
    replay_session: Option<journal::ReplaySession>,
    replay_hits: u64,
    replayed_listfiles: BTreeSet<PathBuf>,
    executed_listfiles: Option<BTreeSet<PathBuf>>,
    replay_observations: Option<journal::ReplayObservations>,
    /// `--trace-expand` is expanding the arguments of a traced command.
    trace_expanding: bool,
    /// Listfile of each file and function call scope (`LISTFILE_STACK`).
    listfiles: Vec<Arc<[u8]>>,
    file_locks: Vec<file_lock::ScopeLocks>,
    function_locks: Vec<file_lock::ScopeLocks>,
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
        let mut project = ProjectModel::default();
        for def in &config.defines {
            project.cache.insert(
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
        let initial_dir = rcmake_host::path::from_native(&config.cwd);
        project.set_root_dirs(initial_dir.clone(), initial_dir);
        let var_ledger = (!matches!(config.services.provenance, LedgerLevel::Off)).then(|| {
            project.set_prop_ledger(Some(Box::default()));
            VarLedger::new(config.services.provenance.clone())
        });
        let mut interp = Self {
            var_ledger,
            write_seq: 0,
            stop_reason: None,
            steps: 0,
            config,
            project,
            root_frame: DirFrame {
                id: DirId::ROOT,
                trace_base: 0,
                defer: None,
            },
            dir_stack: Vec::new(),
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
            recursion_exceeded: false,
            check_system_vars: false,
            package_file_depth: 0,
            unwinding_package: false,
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
            exit_override: None,
            frontend: Frontend::default(),
            watches: BTreeMap::new(),
            variable_access: BTreeMap::new(),
            next_watch_id: 0,
            trace: None,
            command_profiler: None,
            configure_log: Vec::new(),
            trace_stack: Vec::new(),
            find_debug_variables: Vec::new(),
            find_debug_packages: Vec::new(),
            next_defer_id: 0,
            running_deferred: false,
            top_defer_id: None,
            cur_cmd: None,
            command_end: 0,
            session_data: services::SessionData::default(),
            expanded_observer: None,
            parse_cache: None,
            replay_session: None,
            replay_hits: 0,
            replayed_listfiles: BTreeSet::new(),
            executed_listfiles: None,
            replay_observations: None,
            trace_expanding: false,
            listfiles: Vec::new(),
            file_locks: Vec::new(),
            function_locks: Vec::new(),
        };
        interp.init_default_vars();
        interp
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

    /// Explicit exit code from `cmake_language(EXIT)`, if the script requested one.
    #[must_use]
    pub fn requested_exit_code(&self) -> Option<i32> {
        self.exit_override
    }

    /// Exit code for the current state (`1` if any error was issued, or explicit `cmake_language(EXIT)` code).
    #[must_use]
    pub fn exit_code(&self) -> i32 {
        self.exit_override
            .unwrap_or_else(|| i32::from(self.fatal || self.engine.error_occurred()))
    }

    /// Include system files in variable usage diagnostics.
    pub fn set_check_system_vars(&mut self, check: bool) {
        self.check_system_vars = check;
    }

    /// Return whether a command exceeded the recursion limit.
    pub fn recursion_exceeded(&self) -> bool {
        self.recursion_exceeded
    }

    /// Start a nested configure with the caller's command recursion depth.
    pub fn inherit_recursion_depth(&mut self, depth: u32) {
        self.depth = depth;
        self.root_frame.trace_base = depth;
    }
}
