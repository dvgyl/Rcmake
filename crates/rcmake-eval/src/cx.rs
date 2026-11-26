use std::path::{Path, PathBuf};
use std::sync::Arc;

use rcmake_base::{BString, Limits, fold_command_name};
use rcmake_diag::{Backtrace, PolicyError, PolicyId, PolicyMap, PolicyStatus};
use rcmake_host::Host;
use rcmake_model::{DirId, GlobalState, ProjectModel, PropertyMap};
use rcmake_syntax::CommandInvocation;

mod cache;
mod defer;
mod diag;
mod execution;
mod language_scope;
mod variables;

use crate::{Binding, CmdError, Command, Interp, RunMode, TraceConfig, UserCommandKind};

/// Execution context passed to every built-in command (`BuiltinFn`).
pub struct Cx<'a> {
    pub(super) interp: &'a mut Interp,
}

impl Cx<'_> {
    /// Clone the session host for a child evaluator.
    #[must_use]
    pub fn host_arc(&self) -> Arc<dyn Host> {
        Arc::clone(&self.interp.config.host)
    }

    /// Get the optional services for this session.
    #[must_use]
    pub fn services(&self) -> &crate::Services {
        &self.interp.config.services
    }

    /// Get the session data of type `T`.
    #[must_use]
    pub fn session_data<T: std::any::Any + Send + Sync>(&self) -> Option<&T> {
        self.interp.session_data()
    }

    /// Get or lazily initialize mutable session data of type `T`.
    pub fn session_data_mut_or_init<T: std::any::Any + Send + Sync + Default>(
        &mut self,
    ) -> Option<&mut T> {
        self.interp.session_data_mut_or_init()
    }

    /// Get the configured path to the bundled modules and templates.
    #[must_use]
    pub fn cmake_root(&self) -> &Path {
        &self.interp.config.cmake_root
    }

    /// Get the defining listfile of a function or macro.
    #[must_use]
    pub fn user_command_file(&self, name: &[u8]) -> Option<&[u8]> {
        match self.interp.scripted.get(name)? {
            crate::Command::User(command) => Some(&command.body.file),
            crate::Command::Builtin(..) => None,
        }
    }

    /// Return whether a built-in implements the specified lowercase command.
    #[must_use]
    pub fn is_builtin(&self, name: &[u8]) -> bool {
        matches!(
            self.interp.lookup_command(name),
            Some(crate::Command::Builtin(..))
        )
    }

    /// Get the current raw command and the rest of its active sequence.
    #[must_use]
    pub fn lookahead(&self) -> Option<(&CommandInvocation, &[CommandInvocation])> {
        let (commands, index) = self.interp.cur_cmd.as_ref()?;
        let current = commands.get(*index)?;
        let remaining = commands
            .get(index.saturating_add(1)..self.interp.command_end)
            .unwrap_or_default();
        Some((current, remaining))
    }

    /// Read-only view of the innermost active `foreach()` loop, if any.
    #[must_use]
    pub fn foreach_lookahead(&self) -> Option<crate::ForeachLookahead<'_>> {
        self.interp
            .foreach_frames
            .last()
            .map(crate::ForeachLookahead::new)
    }

    /// Return whether the currently executing command is directly inside the innermost
    /// active `foreach()` loop body (not inside a macro or included file called from it).
    #[must_use]
    pub fn cur_cmd_in_foreach(&self) -> bool {
        let (Some((cmds, index)), Some(frame)) = (
            self.interp.cur_cmd.as_ref(),
            self.interp.foreach_frames.last(),
        ) else {
            return false;
        };
        frame.contains_command(cmds, *index)
    }

    /// Number of active variable scopes (directory, function, and `block()` scopes).
    #[must_use]
    pub fn var_scope_depth(&self) -> usize {
        self.interp.vars.depth()
    }

    /// Look up a normal or cache variable without triggering watches, usage tracking,
    /// or replay observations (for side-effect-free speculative lookahead).
    #[must_use]
    pub fn peek_var(&self, name: &[u8]) -> Option<&BString> {
        self.interp.vars.get(name).or_else(|| {
            self.interp
                .project
                .cache
                .get(name)
                .map(|entry| &entry.value)
        })
    }

    /// Look up a cache entry without triggering watches, usage tracking, or replay
    /// observations.
    #[must_use]
    pub fn peek_cache_var(&self, name: &[u8]) -> Option<&BString> {
        self.interp
            .project
            .cache
            .get(name)
            .map(|entry| &entry.value)
    }

    /// Read-only access to [`ProjectModel`] (for side-effect-free speculative lookahead).
    #[must_use]
    pub fn project_ref(&self) -> &ProjectModel {
        &self.interp.project
    }

    /// Return whether tracing, profiling, or variable watches are active.
    #[must_use]
    pub fn observes_commands(&self) -> bool {
        self.interp.has_command_observers() || !self.interp.hooks.watches.is_empty()
    }

    /// Stop after a denied process caused the current command to fail fatally.
    pub fn stop_for_denied_process(&mut self) {
        self.interp.stop(crate::StopReason::DeniedProcess);
    }

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

    /// Command recursion depth across active makefiles.
    pub fn recursion_depth(&self) -> u32 {
        self.interp.depth
    }

    /// Account for recursion in a nested configure called at the current depth.
    pub fn note_nested_recursion_limit(&mut self, max_start_depth: u32) {
        self.interp.note_recursion_limit(max_start_depth);
    }

    /// Active command recursion limit.
    pub fn recursion_limit(&self) -> u32 {
        self.interp.recursion_limit()
    }

    /// Report that a nested configure stopped at the command recursion limit.
    pub fn fail_nested_recursion(&mut self) {
        self.interp.recursion_exceeded = true;
        self.interp
            .fatal_here("Failed to configure test project build system.".into());
    }

    /// Execution limits.
    #[must_use]
    pub fn limits(&self) -> &Limits {
        &self.interp.config.limits
    }

    /// Look up a normal variable (falling back to cache). Notifies variable watches
    /// (`READ_ACCESS` or `UNKNOWN_READ_ACCESS`).
    pub fn var(&mut self, name: &[u8]) -> Option<BString> {
        self.interp.observe_replay_variable_access(name);
        self.interp.read_var(name)
    }

    /// Borrow a normal or cache variable value without cloning when no watch or
    /// deprecation warning applies. Notifies variable watches identically to [`Self::var`].
    pub fn with_var<R>(&mut self, name: &[u8], f: impl FnOnce(Option<&[u8]>) -> R) -> R {
        self.interp.observe_replay_variable_access(name);
        if !self.interp.needs_var_watch(name) {
            let value = self.interp.lookup_var(name).map(|v| v.as_slice());
            return f(value);
        }
        let value = self.interp.read_var(name);
        f(value.as_ref().map(|v| v.as_slice()))
    }

    /// Append `;`-separated list items to `name` in the current scope, mutating an
    /// existing local binding in place when no watch or provenance ledger applies.
    ///
    /// # Errors
    ///
    /// Returns [`CmdError::Status`] if the joined value exceeds `limits.max_value_bytes`.
    pub fn append_list_var(
        &mut self,
        name: &[u8],
        items: &[rcmake_expr::ExpandedArg],
    ) -> Result<(), CmdError> {
        if items.is_empty() {
            return Ok(());
        }
        self.interp.observe_replay_variable_access(name);
        self.interp.record_variable_access(name);
        if !self.interp.needs_var_watch(name) && self.interp.var_ledger.is_none() {
            let max_value = self.interp.config.limits.max_value_bytes;
            let live_limit = self.interp.live_limit();
            match self
                .interp
                .vars
                .append_local_in_place(name, items, max_value, live_limit)
            {
                crate::scope::AppendOutcome::Appended => {
                    self.interp.note_replay_var_write(name);
                    return Ok(());
                }
                crate::scope::AppendOutcome::ValueTooLarge => {
                    return Err(CmdError::status(
                        "Variable value exceeded maximum allowed byte length.",
                    ));
                }
                crate::scope::AppendOutcome::BudgetExceeded => {
                    self.interp.report_variable_budget();
                    return Ok(());
                }
                crate::scope::AppendOutcome::NotLocal => {}
            }
        }
        let raw = self.interp.read_var(name);
        let existing_len = raw.as_ref().map_or(0, |raw| raw.len());
        let separators = items.len() - 1 + usize::from(existing_len > 0);
        let joined = items
            .iter()
            .try_fold(existing_len, |len, item| len.checked_add(item.value.len()))
            .and_then(|len| len.checked_add(separators));
        let Some(joined) = joined.filter(|&len| len <= self.interp.config.limits.max_value_bytes)
        else {
            return Err(CmdError::status(
                "Variable value exceeded maximum allowed byte length.",
            ));
        };
        let mut existing = raw.unwrap_or_default();
        let has_value = !existing.is_empty();
        existing.reserve(joined.saturating_sub(existing_len));
        for (i, item) in items.iter().enumerate() {
            if has_value || i != 0 {
                existing.push(b';');
            }
            existing.extend_from_slice(&item.value);
        }
        self.set_var(name, existing);
        Ok(())
    }

    /// Look up a normal variable only (no cache fallback).
    #[must_use]
    pub fn var_normal_only(&self, name: &[u8]) -> Option<BString> {
        self.interp.observe_replay_variable(name);
        self.interp.vars.get(name).cloned()
    }

    /// Innermost normal binding of `name` (`None` if no scope binds it).
    #[must_use]
    pub fn binding(&self, name: &[u8]) -> Option<Binding> {
        self.interp.observe_replay_variable(name);
        self.interp.vars.lookup(name).cloned()
    }

    /// `if(DEFINED name)`: a normal binding or a cache entry exists.
    #[must_use]
    pub fn var_defined(&self, name: &[u8]) -> bool {
        self.interp.observe_replay_variable(name);
        self.interp.vars.get(name).is_some() || self.interp.project.cache.contains(name)
    }

    /// Value of `name` or an empty string (`GetSafeDefinition`).
    #[must_use]
    pub fn var_or_empty(&self, name: &[u8]) -> BString {
        self.interp.observe_replay_variable_access(name);
        self.interp.lookup_var(name).cloned().unwrap_or_default()
    }

    /// `true` if the value of `name` is a true constant (`cmMakefile::IsOn`).
    #[must_use]
    pub fn var_is_on(&self, name: &[u8]) -> bool {
        self.interp.observe_replay_variable_access(name);
        self.interp
            .lookup_var(name)
            .is_some_and(|v| rcmake_base::truth::is_on(v))
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
        self.interp.note_replay_var_write(name);
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
        self.interp.note_replay_var_write(name);
        self.interp.undefine(name);
    }

    /// Set (`Some`) or unset (`None`) a variable in the immediate parent scope (raise-then-set).
    ///
    /// At the outermost scope this issues the `CMD_AUTHOR` warning
    /// `Cannot set "<name>": current scope has no parent.`. Otherwise this notifies
    /// variable watches (`MODIFIED_ACCESS`).
    pub fn set_parent_scope(&mut self, name: &[u8], value: Option<BString>) {
        self.interp.mark_replay_opaque();
        let _ = self.interp.raise_var(name, value);
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

    /// `CMAKE_CURRENT_LIST_FILE`.
    #[must_use]
    pub fn current_list_file(&self) -> BString {
        self.var_or_empty(b"CMAKE_CURRENT_LIST_FILE")
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
    /// (`CMP0000` through `CMP0065`) are always `NEW`.
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

    /// Enable tracing until the next matching `cmake_language(TRACE OFF)`.
    pub fn push_trace(&mut self, expand: bool) {
        self.interp.trace_stack.push(expand);
    }

    /// Restore the previous trace setting. Return false if the stack is empty.
    pub fn pop_trace(&mut self) -> bool {
        self.interp.trace_stack.pop().is_some()
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
        self.interp.update_policies(|map| map.set(id, status))?;
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
            .update_policies(|map| map.apply_version(version))?;
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
}
