use std::sync::Arc;

use rcmake_base::suggest::{Threshold, closest};
use rcmake_base::{BString, fold_command_name};
use rcmake_diag::{Backtrace, Frame, FrameKind};
use rcmake_expr::ExpandedArg;
use rcmake_syntax::{ArgKind, Argument, CommandInvocation};

use super::BodyMode;
use crate::code::Code;
use crate::hooks::display_line;
use crate::util::cat;
use crate::{
    CmdError, CmdResult, Command, Cx, FLOW_CONTROL, Flow, HARD_RECURSION_CEILING, Interp, RunMode,
    UserCommand, UserCommandKind,
};

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

    pub(super) fn recursion_limit(&self) -> u32 {
        let configured = self
            .lookup_var(b"CMAKE_MAXIMUM_RECURSION_DEPTH")
            .filter(|v| !v.is_empty() && v.iter().all(u8::is_ascii_digit))
            .and_then(|v| core::str::from_utf8(v).ok()?.parse::<u64>().ok())
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
        let bt = self.command_backtrace(file, line, name);
        let saved_bt = core::mem::replace(&mut self.bt, bt);
        let saved_defer = core::mem::replace(&mut self.top_defer_id, defer_id);
        self.depth = self.depth.saturating_add(1);
        let saved_line = core::mem::replace(&mut self.cur_line, line);
        let saved_failed = core::mem::replace(&mut self.cmd_failed, false);

        let limit = self.recursion_limit();
        let result = if self.depth > limit {
            self.fatal_here(format!("Maximum recursion depth of {limit} exceeded").into());
            Err(CmdError::Reported)
        } else {
            f(self)
        };
        let result = self.finish_command(name, result);

        self.cmd_failed = saved_failed;
        self.cur_line = saved_line;
        self.depth = self.depth.saturating_sub(1);
        self.top_defer_id = saved_defer;
        self.bt = saved_bt;
        result
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
            if let Err(e) = expand(core::slice::from_ref(arg), self, &mut out) {
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
            return self.invoke(command, &[]);
        }
        let args = self.expand(&cmd.args)?;
        if self.running_deferred
            && lower == b"enable_language"
            && matches!(command, Command::Builtin(..))
        {
            self.error_here("Languages may not be enabled during deferred execution.".into());
            return Err(CmdError::Reported);
        }
        self.invoke(command, &args)
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

    fn invoke(&mut self, command: Command, args: &[ExpandedArg]) -> CmdResult {
        match command {
            Command::Builtin(f, flags, _) => {
                if self.config.mode == RunMode::Script && !flags.scriptable {
                    return Err(CmdError::Status("command is not scriptable".into()));
                }
                // Install rules use this setting at the call site.
                let first_install = self.project.installs.len();
                let result = f(&mut Cx { interp: self }, args);
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
        let result = self.with_command_frame(&file, line, name, |this| this.invoke(command, &args));
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
        let mut i = start;
        while i < end {
            if self.fatal || self.over_budget() {
                return Err(CmdError::Reported);
            }
            let (result, next) = self.exec_at(code, i);
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
            b"foreach" => self.exec_foreach(code, i),
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
