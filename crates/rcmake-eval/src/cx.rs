use std::path::{Path, PathBuf};
use std::sync::Arc;

use rcmake_base::{BString, Limits, fold_command_name};
use rcmake_diag::{
    Backtrace, DiagCategory, DiagEngine, DiagState, Effect, MessageKind, PolicyError, PolicyId,
    PolicyMap, PolicyStatus,
};
use rcmake_expr::{CondError, ExpandError, ExpandedArg};
use rcmake_host::Host;
use rcmake_model::{Cache, DirId, GlobalState, ProjectModel, PropertyMap};
use rcmake_regex::{Captures, Regex, RegexError};
use rcmake_syntax::Argument;

use crate::{
    Binding, CmdError, CmdResult, Command, IncludeOptions, Interp, LogLevel, RunMode,
    SubdirOptions, TraceConfig, UserCommandKind,
};

/// Execution context passed to every built-in command (`BuiltinFn`).
pub struct Cx<'a> {
    pub(super) interp: &'a mut Interp,
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
        self.interp.vars.get(name).is_some() || self.interp.project.cache.contains(name)
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
        names.extend(self.interp.project.cache.iter().map(|(k, _)| k.clone()));
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
        &mut self.interp.project.cache
    }

    /// Mutable access to [`GlobalState`].
    pub fn global(&mut self) -> &mut GlobalState {
        &mut self.interp.project.global
    }

    /// Mutable access to [`ProjectModel`].
    pub fn project(&mut self) -> &mut ProjectModel {
        &mut self.interp.project
    }

    /// Mutable access to the plain properties of the current directory. Usage
    /// requirements and computed properties go through [`ProjectModel::get_prop`] and
    /// [`ProjectModel::set_prop`] with [`Self::current_dir_id`].
    pub fn directory_props(&mut self) -> &mut PropertyMap {
        let id = self.interp.current_dir_id();
        &mut self.interp.project.dir_mut(id).props
    }

    /// The directory being processed ([`DirId::ROOT`] in script mode).
    #[must_use]
    pub fn current_dir_id(&self) -> DirId {
        self.interp.current_dir_id()
    }

    /// Process a subdirectory (`add_subdirectory()`): create the directory in the
    /// [`ProjectModel`] and run `<src>/CMakeLists.txt` with `<bin>` as its binary
    /// directory. Relative paths resolve against the current source and binary
    /// directories.
    ///
    /// The child starts with a copy of the current variables, so `set(... PARENT_SCOPE)`
    /// and `return(PROPAGATE)` in the child update the caller. Policy, diagnostic, and
    /// loop-block scopes are isolated. The child's deferred calls run before this call
    /// returns, and [`rcmake_model::Directory::final_policies`] records the child's
    /// policies at its end.
    ///
    /// Issues the error `The source directory ... does not contain a CMakeLists.txt
    /// file.` and returns `Err(CmdError::Reported)` when the listfile is missing.
    /// Processing continues after that error, as in CMake.
    pub fn run_subdirectory(
        &mut self,
        src: &Path,
        bin: &Path,
        options: SubdirOptions,
    ) -> Result<DirId, CmdError> {
        self.interp.run_subdirectory(src, bin, options)
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
        self.interp.current_binary_dir()
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

    /// Query a policy's status in the parent policy scope (`cmake_policy(GET ... PARENT_SCOPE)`).
    /// Returns `None` when at the root policy scope.
    #[must_use]
    pub fn parent_policy(&self, id: PolicyId) -> Option<PolicyStatus> {
        self.interp.policies.parent().map(|map| {
            if id.info().is_some_and(|i| i.removed_old) {
                PolicyStatus::New
            } else {
                map.get(id)
            }
        })
    }

    /// Check whether to display find diagnostics for the given output variable.
    #[must_use]
    pub fn find_variable_debug(&self, name: &[u8]) -> bool {
        self.var_is_on(b"CMAKE_FIND_DEBUG_MODE")
            || self.interp.find_debug_variables.iter().any(|item| item == name)
    }

    /// Check whether to display find diagnostics for the given package.
    #[must_use]
    pub fn find_package_debug(&self, name: &[u8]) -> bool {
        self.var_is_on(b"CMAKE_FIND_DEBUG_MODE")
            || self.interp.find_debug_packages.iter().any(|item| item == name)
    }

    /// Enable tracing until the next matching `cmake_language(TRACE OFF)`.
    pub fn push_trace(&mut self, expand: bool) {
        self.interp.trace_stack.push(expand);
    }

    /// Restore the previous trace setting. Return false if the stack is empty.
    pub fn pop_trace(&mut self) -> bool {
        self.interp.trace_stack.pop().is_some()
    }

    /// Return true while this directory runs its deferred calls.
    #[must_use]
    pub fn running_deferred(&self) -> bool {
        self.interp.running_deferred
    }

    /// Active command tracing configuration (`--trace`), if enabled.
    #[must_use]
    pub fn trace_config(&self) -> Option<&TraceConfig> {
        self.interp.trace.as_ref()
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
        if id == crate::interp::CMP0198 {
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

    /// `variable_watch(<var> [<command>])`: add a watch. Every call adds a new watch,
    /// also for the same variable and command.
    ///
    /// On each access to `var`, the watch calls `command` with the arguments
    /// `<var> <access> <value> <CMAKE_CURRENT_LIST_FILE> <LISTFILE_STACK>` (see
    /// [`WatchAccess`](crate::WatchAccess)).
    /// Without a command (`None` or empty), the watch prints
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

    /// `true` while a directory listfile runs in configure mode, so that deferred calls
    /// can be queued for the end of the directory.
    #[must_use]
    pub fn defer_active(&self) -> bool {
        self.interp.dir_frame().defer.is_some()
    }

    /// A new unique deferred-call identifier (`__0`, `__1`, ...).
    pub fn defer_new_id(&mut self) -> BString {
        self.interp.new_defer_id()
    }

    /// Queue `command` with raw `args` (see [`Self::raw_args`]) for the end of the
    /// selected directory. The arguments are expanded when the call runs. The call keeps
    /// the file and line of the current command, and runs with the policies that are
    /// active at the end of the directory. Returns `false` when no queue is active
    /// (`DEFER CALL may not be scheduled in directory ... at this time.`).
    pub fn defer_call(
        &mut self,
        dir: DirId,
        id: BString,
        command: &[u8],
        args: &[Argument],
    ) -> bool {
        self.interp.queue_deferred(dir, id, command, args)
    }

    /// Identifiers of the queued calls that are not canceled, in queue order. `None`
    /// when no queue is active.
    #[must_use]
    pub fn defer_get_call_ids(&self, dir: DirId) -> Option<Vec<BString>> {
        self.interp.deferred_ids(dir)
    }

    /// `[command, args...]` of the queued call `id` (raw argument text), or an empty
    /// list if `id` is unknown. `None` when no queue is active.
    #[must_use]
    pub fn defer_get_call(&self, dir: DirId, id: &[u8]) -> Option<Vec<BString>> {
        self.interp.deferred_call(dir, id)
    }

    /// Cancel every queued call with identifier `id`. Returns `false` when no queue is
    /// active.
    pub fn defer_cancel_call(&mut self, dir: DirId, id: &[u8]) -> bool {
        self.interp.cancel_deferred(dir, id)
    }
}
