use std::{path::Path, sync::Arc};

use rcmake_base::suggest::{Threshold, closest};
use rcmake_base::{BString, fold_command_name, truth};
use rcmake_diag::{
    Backtrace, DiagCategory, Diagnostic, Frame, FrameKind, Location, PolicyId, PolicyStatus,
};
use rcmake_expr::{CondEnv, CondError, ExpandEnv, ExpandedArg};
use rcmake_host::Host;
use rcmake_regex::{Captures, Regex, RegexError};
use rcmake_syntax::{ArgKind, Argument, CommandInvocation};

use super::BodyMode;
use crate::code::Code;
use crate::hooks::display_line;
use crate::interp::IssueClass;
use crate::profiler::{CommandSample, SampleKind};
use crate::util::cat;
use crate::{
    CmdError, CmdResult, Command, Cx, FLOW_CONTROL, Flow, HARD_RECURSION_CEILING, Interp, RunMode,
    UserCommand, UserCommandKind,
};

struct CommandExpansion<'a>(&'a mut Interp);

impl ExpandEnv for CommandExpansion<'_> {
    fn var(&mut self, name: &[u8]) -> Option<BString> {
        self.0.observe_replay_variable_access(name);
        let value = self.0.var(name);
        self.0.warn_deprecated_variable(name);
        value
    }

    fn cache_var(&mut self, name: &[u8]) -> Option<BString> {
        self.0.observe_replay_variable(name);
        self.0.cache_var(name)
    }

    fn env_var(&mut self, name: &[u8]) -> Option<BString> {
        self.0.observe_replay_env(name);
        self.0.env_var(name)
    }

    fn missing_reference(&mut self, name: &[u8]) {
        self.0.missing_reference(name);
    }

    fn current_line(&self) -> u32 {
        self.0.current_line()
    }
}

impl CondEnv for CommandExpansion<'_> {
    fn is_defined(&mut self, name: &[u8]) -> bool {
        self.0.observe_replay_variable(name);
        let defined = self.0.is_defined(name);
        if !defined {
            self.0.warn_deprecated_variable(name);
        }
        defined
    }

    fn cache_defined(&mut self, name: &[u8]) -> bool {
        self.0.observe_replay_variable(name);
        self.0.cache_defined(name)
    }

    fn env_defined(&mut self, name: &[u8]) -> bool {
        self.0.observe_replay_env(name);
        self.0.env_defined(name)
    }

    fn command_exists(&self, name: &[u8]) -> bool {
        self.0.command_exists(name)
    }

    fn policy_known(&self, name: &[u8]) -> bool {
        self.0.policy_known(name)
    }

    fn diagnostic_known(&self, name: &[u8]) -> bool {
        self.0.diagnostic_known(name)
    }

    fn target_exists(&self, name: &[u8]) -> bool {
        self.0.target_exists(name)
    }

    fn test_exists(&self, name: &[u8]) -> bool {
        self.0.test_exists(name)
    }

    fn rule_exists(&self, name: &[u8]) -> bool {
        self.0.rule_exists(name)
    }

    fn policy(&self, id: PolicyId) -> PolicyStatus {
        self.0.policy(id)
    }

    fn host(&self) -> &dyn Host {
        self.0.host()
    }

    fn base_dir(&self) -> &Path {
        self.0.base_dir()
    }

    fn regex(&mut self, pattern: &[u8]) -> Result<Arc<Regex>, RegexError> {
        self.0.regex(pattern)
    }

    fn set_match_vars(&mut self, captures: Option<(&Captures, &[u8])>) {
        self.0.set_match_vars(captures);
    }

    fn warn(&mut self, diag: Diagnostic) {
        self.0.warn(diag)
    }
}

/// Error text of commands that only close or continue a block.
fn unexpected_text(lower: &[u8]) -> Option<&'static str> {
    Some(match lower {
        b"else" => {
            "An ELSE command was found outside of a proper IF ENDIF structure. Or its arguments did not match the opening IF command."
        }
        b"elseif" => "An ELSEIF command was found outside of a proper IF ENDIF structure.",
        b"endforeach" => {
            "An ENDFOREACH command was found outside of a proper FOREACH ENDFOREACH structure. Or its arguments did not match the opening FOREACH command."
        }
        b"endfunction" => {
            "An ENDFUNCTION command was found outside of a proper FUNCTION ENDFUNCTION structure. Or its arguments did not match the opening FUNCTION command."
        }
        b"endif" => {
            "An ENDIF command was found outside of a proper IF ENDIF structure. Or its arguments did not match the opening IF command."
        }
        b"endmacro" => {
            "An ENDMACRO command was found outside of a proper MACRO ENDMACRO structure. Or its arguments did not match the opening MACRO command."
        }
        b"endwhile" => {
            "An ENDWHILE command was found outside of a proper WHILE ENDWHILE structure. Or its arguments did not match the opening WHILE command."
        }
        b"endblock" => {
            "An ENDBLOCK command was found outside of a proper BLOCK ENDBLOCK structure."
        }
        _ => return None,
    })
}

impl Interp {
    pub(crate) fn command_exists(&self, name: &[u8]) -> bool {
        let lower = fold_command_name(name);
        FLOW_CONTROL.contains(&lower.as_slice())
            || self.scripted.contains_key(&lower)
            || self.registry.get_lower(&lower).is_some()
    }

    pub(crate) fn lookup_command(&self, lower: &[u8]) -> Option<Command> {
        if let Some(cmd) = self.scripted.get(lower) {
            return Some(cmd.clone());
        }
        self.registry
            .get_lower(lower)
            .map(|b| Command::Builtin(b.func, b.flags, b.raw))
    }

    /// Sorted names of all known commands (`cmState::GetCommandNames`).
    fn command_names(&self) -> Vec<BString> {
        let mut names: Vec<BString> = FLOW_CONTROL.iter().map(|n| BString::from(*n)).collect();
        names.extend(self.registry.names().cloned());
        names.extend(self.scripted.keys().cloned());
        names.sort();
        names.dedup();
        names
    }

    /// `cmState::AddScriptedCommand`.
    pub(crate) fn add_scripted_command(&mut self, cmd: Arc<UserCommand>) -> bool {
        let lower = fold_command_name(&cmd.name);
        if FLOW_CONTROL.contains(&lower.as_slice()) {
            self.fatal_here(cat(&[
                b"Built-in flow control command \"",
                &lower,
                b"\" cannot be overridden.",
            ]));
            return false;
        }
        if let Some(old) = self.lookup_command(&lower) {
            self.scripted.insert(cat(&[b"_", &lower]), old);
        }
        self.scripted.insert(lower, Command::User(cmd));
        true
    }

    pub(crate) fn recursion_limit(&self) -> u32 {
        let name = b"CMAKE_MAXIMUM_RECURSION_DEPTH";
        let environment;
        let value = if let Some(value) = self.lookup_var(name) {
            Some(value)
        } else {
            environment = self.config.host.env_get(name);
            environment.as_ref()
        };
        let configured = value
            .and_then(|v| {
                core::str::from_utf8(v.trim_ascii_start())
                    .ok()?
                    .parse::<u64>()
                    .ok()
            })
            .map_or(self.config.limits.max_recursion, |v| {
                u32::try_from(v).unwrap_or(u32::MAX)
            });
        configured.min(HARD_RECURSION_CEILING)
    }

    /// Get the current backtrace with a new command frame.
    pub(crate) fn command_backtrace(&self, file: &Arc<[u8]>, line: u32, name: &[u8]) -> Backtrace {
        self.bt.push(Frame {
            file: Arc::clone(file),
            line,
            name: Some(Arc::from(name)),
            kind: FrameKind::Command,
        })
    }

    pub(crate) fn local_command_backtrace(&self) -> Backtrace {
        let count = self.depth.saturating_sub(self.dir_frame().trace_base) as usize;
        let frames: Vec<_> = self
            .bt
            .frames()
            .filter(|frame| frame.kind != FrameKind::ModuleInclude)
            .take(count)
            .cloned()
            .collect();
        frames
            .into_iter()
            .rev()
            .fold(Backtrace::default(), |trace, frame| trace.push(frame))
    }

    /// Run `f` with a call frame for `name` at `file:line` (`CallScope`), enforce the
    /// recursion limit, and convert command errors into diagnostics.
    pub(crate) fn with_command_frame(
        &mut self,
        file: &Arc<[u8]>,
        line: u32,
        name: &[u8],
        f: impl FnOnce(&mut Self) -> CmdResult,
    ) -> CmdResult {
        self.with_frame(file, line, name, None, f)
    }

    /// [`Self::with_command_frame`] with an optional deferred-call identifier
    /// (`cmListFileContext::DeferId`).
    fn with_frame(
        &mut self,
        file: &Arc<[u8]>,
        line: u32,
        name: &[u8],
        defer_id: Option<BString>,
        f: impl FnOnce(&mut Self) -> CmdResult,
    ) -> CmdResult {
        if !self.take_step() {
            return Err(CmdError::Reported);
        }
        let bt = self.command_backtrace(file, line, name);
        let saved_bt = core::mem::replace(&mut self.bt, bt);
        let saved_defer = core::mem::replace(&mut self.hooks.top_defer_id, defer_id);
        self.depth = self.depth.saturating_add(1);
        let saved_line = core::mem::replace(&mut self.cur_line, line);
        let saved_failed = core::mem::replace(&mut self.cmd_failed, false);

        let limit = self.recursion_limit();
        let audit = self
            .session_data::<crate::hermetic::HermeticContext>()
            .cloned();
        let _audit_site = audit.as_ref().map(|audit| {
            audit.enter(crate::hermetic::HermeticSite::new(
                &rcmake_host::path::to_native(file),
                line,
            ))
        });
        let result = if audit
            .as_ref()
            .is_some_and(crate::hermetic::HermeticContext::denied)
        {
            self.fatal = true;
            Err(CmdError::Reported)
        } else if self.depth > limit {
            self.recursion_exceeded = true;
            let local = self.local_command_backtrace();
            let backtrace = core::mem::replace(&mut self.bt, local);
            self.fatal_here(format!("Maximum recursion depth of {limit} exceeded").into());
            self.bt = backtrace;
            Err(CmdError::Reported)
        } else if let Some(profiler) = self.command_profiler.clone() {
            let name = fold_command_name(name);
            let kind = self.sample_kind(&name);
            let depth = self.depth;
            let start = self.config.host.monotonic();
            let result = f(self);
            let elapsed = self.config.host.monotonic().saturating_sub(start);
            profiler(&CommandSample {
                name: &name,
                file,
                line,
                depth,
                start,
                elapsed,
                kind,
            });
            result
        } else {
            f(self)
        };
        if audit
            .as_ref()
            .is_some_and(crate::hermetic::HermeticContext::denied)
        {
            self.fatal = true;
        }
        let result = if self.poll_cancel()
            || matches!(
                self.stop_reason,
                Some(crate::StopReason::StepBudgetExceeded { .. })
            ) {
            Err(CmdError::Reported)
        } else {
            self.finish_command(name, result)
        };

        self.cmd_failed = saved_failed;
        self.cur_line = saved_line;
        self.depth = self.depth.saturating_sub(1);
        self.hooks.top_defer_id = saved_defer;
        self.bt = saved_bt;
        result
    }

    fn sample_kind(&self, name: &[u8]) -> SampleKind {
        match self.scripted.get(name) {
            Some(Command::User(command)) => match command.kind {
                UserCommandKind::Function => SampleKind::Function,
                UserCommandKind::Macro => SampleKind::Macro,
            },
            _ => SampleKind::Builtin,
        }
    }

    fn finish_command(&mut self, name: &[u8], result: CmdResult) -> CmdResult {
        let failed = match result {
            Err(CmdError::Status(text)) => {
                self.error_here(cat(&[name, b" ", &text]));
                true
            }
            Err(CmdError::Reported) => true,
            Ok(flow) => {
                if !self.cmd_failed {
                    return Ok(flow);
                }
                true
            }
        };
        if failed && self.config.mode == RunMode::Script {
            self.fatal = true;
        }
        Err(CmdError::Reported)
    }

    /// Expand arguments with `rcmake_expr::expand_args`, one argument at a time
    /// (`cmMakefile::ExpandArguments`). An expansion error is fatal and reads
    /// `Syntax error in cmake code at <file>:<line> when parsing string <arg> <error>`.
    pub(crate) fn expand(&mut self, args: &[Argument]) -> Result<Vec<ExpandedArg>, CmdError> {
        let mut out = Vec::new();
        let expand = self.frontend.expand;
        let mut failed = false;
        for arg in args {
            // Short literal arguments need no variable lookups, escape processing,
            // or list splitting. Keep their original kind and line number.
            let text = arg.text.as_slice();
            if text.len() <= 64
                && !text.iter().any(|&b| {
                    matches!(b, b'$' | b'\\') || (b == b';' && arg.kind == ArgKind::Unquoted)
                })
            {
                if !text.is_empty() || arg.kind != ArgKind::Unquoted {
                    out.push(ExpandedArg {
                        value: arg.text.clone(),
                        kind: arg.kind,
                        line: arg.line,
                    });
                }
                continue;
            }
            if let Err(e) = expand(
                core::slice::from_ref(arg),
                &mut CommandExpansion(self),
                &mut out,
            ) {
                let body = self.syntax_error_text(arg, e.message);
                self.fatal_here(body);
                failed = true;
            }
        }
        if failed {
            Err(CmdError::Reported)
        } else {
            Ok(out)
        }
    }

    pub(crate) fn eval_command_condition(
        &mut self,
        args: &[ExpandedArg],
    ) -> Result<bool, CondError> {
        self.observe_replay_condition(args);
        (self.frontend.cond)(args, &mut CommandExpansion(self))
    }

    /// `ExpandVariablesInStringImpl` error text for an argument of the command on top of
    /// the backtrace.
    fn syntax_error_text(&self, arg: &Argument, message: BString) -> BString {
        const PREFIX: &[u8] = b"Syntax error in cmake code ";
        if message.starts_with(PREFIX) {
            return message;
        }
        let mut out = PREFIX.to_vec();
        if let Some(top) = self.bt.top() {
            out.extend_from_slice(b"at\n  ");
            out.extend_from_slice(&top.file);
            out.push(b':');
            out.extend_from_slice(display_line(arg.line).as_bytes());
            out.push(b'\n');
        }
        out.extend_from_slice(b"when parsing string\n  ");
        out.extend_from_slice(&arg.text);
        out.push(b'\n');
        out.extend_from_slice(&message);
        BString::from(out)
    }

    /// Execute the ordinary command at index `i` of `code`.
    pub(crate) fn exec_command(&mut self, code: &Code, i: usize) -> CmdResult {
        let Some(cmd) = code.cmds.get(i) else {
            return Ok(Flow::Normal);
        };
        let saved = self.cur_cmd.replace((Arc::clone(&code.cmds), i));
        let result = self.with_command_frame(&code.file, cmd.line, &cmd.name, |this| {
            this.dispatch(cmd, &code.file)
        });
        self.cur_cmd = saved;
        result
    }

    /// Execute the deferred call in `code` (one command) with identifier `id`.
    pub(crate) fn exec_deferred(&mut self, code: &Code, id: BString) {
        let Some(cmd) = code.cmds.first() else {
            return;
        };
        let saved = self.cur_cmd.replace((Arc::clone(&code.cmds), 0));
        let vars = self.push_list_file_vars(&code.file);
        let _ = self.with_frame(&code.file, cmd.line, &cmd.name, Some(id), |this| {
            this.dispatch(cmd, &code.file)
        });
        self.pop_list_file_vars(vars);
        self.cur_cmd = saved;
    }

    fn dispatch(&mut self, cmd: &CommandInvocation, file: &[u8]) -> CmdResult {
        if self.fatal {
            return Ok(Flow::Normal);
        }
        let lower = cmd.lower.as_slice();
        let command = self.lookup_command(lower);
        if self.trace_enabled() && (command.is_some() || unexpected_text(lower).is_some()) {
            self.trace_command(cmd, file, false);
        }
        if let Some(text) = unexpected_text(lower) {
            return Err(CmdError::Status(text.into()));
        }
        let Some(command) = command else {
            self.unknown_command(&cmd.name);
            return Err(CmdError::Reported);
        };
        if matches!(command, Command::Builtin(_, _, true)) {
            return self.invoke(lower, command, &[]);
        }
        let args = self.expand(&cmd.args)?;
        if self.replay.observations.is_some()
            && !crate::journal::valid_expansion(lower, &cmd.args, &args)
        {
            self.mark_replay_opaque();
        }
        if self
            .session_data::<crate::hermetic::HermeticContext>()
            .is_some_and(crate::hermetic::HermeticContext::denied)
        {
            self.fatal = true;
            return Err(CmdError::Reported);
        }
        if self.hooks.running_deferred
            && lower == b"enable_language"
            && matches!(command, Command::Builtin(..))
        {
            self.error_here("Languages may not be enabled during deferred execution.".into());
            return Err(CmdError::Reported);
        }
        self.invoke_observed(lower, command, &args)
    }

    fn unknown_command(&mut self, name: &[u8]) {
        let mut msg = cat(&[b"Unknown CMake command \"", name, b"\"."]);
        let names = self.command_names();
        if let Some(s) = closest(name, &names, Threshold::CmakeCommand) {
            msg.extend_from_slice(b"  Did you mean: \"");
            msg.extend_from_slice(s);
            msg.extend_from_slice(b"\"?");
        }
        self.fatal_here(msg);
    }

    fn invoke_observed(
        &mut self,
        name: &[u8],
        command: Command,
        args: &[ExpandedArg],
    ) -> CmdResult {
        if let Some(observer) = &self.expanded_observer
            && !matches!(command, Command::Builtin(_, _, true))
            && observer.names.contains(&name)
        {
            (observer.notify)(&mut self.cx(), args);
            if self.fatal {
                return Err(CmdError::Reported);
            }
        }
        self.invoke(name, command, args)
    }

    fn invoke(&mut self, name: &[u8], command: Command, args: &[ExpandedArg]) -> CmdResult {
        match command {
            Command::Builtin(f, flags, _) => {
                if self.config.mode == RunMode::Script && !flags.scriptable {
                    return Err(CmdError::Status("command is not scriptable".into()));
                }
                let guard = self
                    .registry
                    .guards
                    .get(name)
                    .or_else(|| self.registry.guards.get(name.strip_prefix(b"_")?));
                if let Some(check) = guard.copied() {
                    check(&mut self.cx(), args)?;
                }
                if matches!(name, b"mark_as_advanced" | b"_mark_as_advanced") {
                    self.warn_missing_advanced_entries(args);
                }
                let legacy_deprecation = if matches!(name, b"cmake_policy" | b"_cmake_policy")
                    && matches!(args, [op, policy, status] if op.value == "SET" && status.value == "OLD"
                        && PolicyId::parse(&policy.value).is_some_and(|id| id.number() <= 161))
                    && self.policy_status(PolicyId::new(218)) != PolicyStatus::New
                {
                    let state = *self.diag_top();
                    let action = self.legacy_deprecation_action(false);
                    self.diag_top_mut()
                        .set(DiagCategory::Deprecated, action, false);
                    Some(state)
                } else {
                    None
                };
                // Install rules use this setting at the call site.
                let first_install = self.project.installs.len();
                let result = f(&mut Cx { interp: self }, args);
                if let Some(state) = legacy_deprecation {
                    *self.diag_top_mut() = state;
                }
                if result.is_ok()
                    && matches!(name, b"set" | b"_set" | b"unset" | b"_unset")
                    && let Some(arg) = args.first()
                {
                    self.warn_deprecated_variable(&arg.value);
                }
                if first_install < self.project.installs.len()
                    && self
                        .bt
                        .top()
                        .and_then(|frame| frame.name.as_deref())
                        .is_some_and(|name| name.eq_ignore_ascii_case(b"install"))
                    && let Some(level) = self.lookup_var(b"CMAKE_INSTALL_MESSAGE").cloned()
                {
                    for index in first_install..self.project.installs.len() {
                        self.project.global.props.set(
                            format!("_RCMAKE_INSTALL_{index}_MESSAGE").into(),
                            level.clone(),
                        );
                    }
                }
                result
            }
            Command::User(u) => match u.kind {
                UserCommandKind::Function => self.call_function(&u, args),
                UserCommandKind::Macro => self.call_macro(&u, args),
            },
        }
    }

    fn warn_missing_advanced_entries(&mut self, args: &[ExpandedArg]) {
        let id = PolicyId::new(102);
        if self.policy_status(id) != PolicyStatus::Warn
            || !self
                .lookup_var(b"CMAKE_POLICY_WARNING_CMP0102")
                .map_or_else(|| self.trace_enabled(), |value| truth::is_on(value))
        {
            return;
        }
        let skip = args
            .first()
            .is_some_and(|arg| matches!(arg.value.as_slice(), b"CLEAR" | b"FORCE"));
        let mut warned = std::collections::BTreeSet::new();
        for arg in args.iter().skip(usize::from(skip)) {
            let name = arg.value.as_slice();
            if self.project.cache.contains(name) || !warned.insert(name) {
                continue;
            }
            self.issue_at(
                IssueClass::policy(id),
                Location::Backtrace(self.bt.clone()),
                cat(&[
                    b"Policy CMP0102 is not set: The variable named \"",
                    name,
                    b"\" is not in the cache. This results in an empty cache entry which is no longer created when policy CMP0102 is set to NEW. Run \"cmake --help-policy CMP0102\" for policy details. Use the cmake_policy command to set the policy and suppress this warning.",
                ]),
                true,
            );
        }
    }

    /// `Cx::call_command`: run a command with pre-expanded arguments under a frame.
    pub(crate) fn call_command(&mut self, name: &[u8], args: Vec<ExpandedArg>) -> CmdResult {
        let lower = fold_command_name(name);
        if crate::code::opener_end(&lower).is_some() || unexpected_text(&lower).is_some() {
            return Err(CmdError::Status(cat(&[
                b"invalid command specified: ",
                name,
            ])));
        }
        let file = self
            .bt
            .top()
            .map_or_else(|| Arc::from(&b""[..]), |f| Arc::clone(&f.file));
        let line = self.cur_line;
        let Some(command) = self.lookup_command(&lower) else {
            let saved = core::mem::take(&mut self.bt);
            let result = self.with_command_frame(&file, line, name, |this| {
                this.unknown_command(name);
                Err(CmdError::Reported)
            });
            self.bt = saved;
            return result;
        };
        // A command that expands its own arguments reads them from `cur_cmd`. The
        // arguments are expanded already, so pass them as bracket arguments.
        let raw = matches!(command, Command::Builtin(_, _, true)).then(|| {
            let args = args
                .iter()
                .map(|a| Argument {
                    kind: ArgKind::Bracket,
                    text: a.value.clone(),
                    line,
                    span: 0..0,
                })
                .collect();
            let cmd = CommandInvocation {
                name: BString::from(name),
                lower: lower.clone(),
                line,
                close_line: line,
                span: 0..0,
                args,
            };
            (Arc::from(vec![cmd]), 0)
        });
        let args = if raw.is_some() { Vec::new() } else { args };
        let saved = core::mem::replace(&mut self.cur_cmd, raw);
        let result = self.with_command_frame(&file, line, name, |this| {
            this.invoke_observed(&lower, command, &args)
        });
        self.cur_cmd = saved;
        result
    }

    /// Run commands `start..end` of `code`.
    pub(crate) fn run_code(
        &mut self,
        code: &Code,
        start: usize,
        end: usize,
        mode: BodyMode,
    ) -> CmdResult {
        if mode == BodyMode::File
            && !self.fatal
            && !code.file.is_empty()
            && let Some(paths) = &mut self.replay.executed_listfiles
        {
            paths.insert(rcmake_host::path::to_native(&code.file));
        }
        let mut i = start;
        while i < end {
            if self.fatal || self.over_budget() {
                return Err(CmdError::Reported);
            }
            let saved_end = core::mem::replace(&mut self.command_end, end);
            let (result, next) = self.exec_at(code, i);
            self.command_end = saved_end;
            i = next;
            if self.fatal {
                return Err(CmdError::Reported);
            }
            match result {
                Ok(Flow::Normal) => {}
                Ok(flow @ Flow::Return { .. }) => return Ok(flow),
                Ok(flow) => {
                    if mode != BodyMode::File {
                        return Ok(flow);
                    }
                }
                Err(e) => {
                    if matches!(mode, BodyMode::Function | BodyMode::Macro) {
                        return Err(e);
                    }
                }
            }
        }
        Ok(Flow::Normal)
    }

    /// Stop execution if the optional wall-clock budget has expired.
    pub(crate) fn over_budget(&mut self) -> bool {
        if self.poll_cancel() {
            return true;
        }
        let Some(budget) = self.config.limits.wall_budget else {
            return false;
        };
        if self.config.host.monotonic().saturating_sub(self.started) <= budget {
            return false;
        }
        self.fatal_here("Execution exceeded the configured wall-clock budget.".into());
        true
    }

    /// Execute the command at `i` and return the index of the next command.
    fn exec_at(&mut self, code: &Code, i: usize) -> (CmdResult, usize) {
        let Some(cmd) = code.cmds.get(i) else {
            return (Ok(Flow::Normal), i + 1);
        };
        let result = match cmd.lower.as_slice() {
            b"if" => self.exec_if(code, i),
            b"foreach" => {
                let name = cmd.args.first().map(|arg| arg.text.as_slice());
                let before = name
                    .filter(|_| self.replay.observations.is_some())
                    .and_then(|name| self.vars.lookup(name))
                    .cloned();
                let result = self.exec_foreach(code, i);
                if let Some(name) = name
                    && before.as_ref() != self.vars.lookup(name)
                    && let Some(observed) = &mut self.replay.observations
                {
                    observed.vars.insert(name.into());
                }
                result
            }
            b"while" => self.exec_while(code, i),
            b"function" => self.exec_define(code, i, UserCommandKind::Function),
            b"macro" => self.exec_define(code, i, UserCommandKind::Macro),
            b"block" => self.exec_block(code, i),
            _ => return (self.exec_command(code, i), i + 1),
        };
        match result {
            Ok(result) => (result, code.end_of(i).max(i) + 1),
            // The opener failed and did not open a block: run its body as ordinary
            // commands, so that the end command reports its own error.
            Err(e) => (Err(e), i + 1),
        }
    }
}
