use std::sync::Arc;

use rcmake_base::BString;
use rcmake_diag::{PolicyId, PolicyStatus};
use rcmake_expr::ExpandedArg;
use rcmake_syntax::{ArgKind, Argument, CommandInvocation};

use super::BodyMode;
use crate::util::{cat, filename_path};
use crate::{CmdError, CmdResult, Flow, Interp, UserCommand};

impl Interp {
    fn push_call_scope(&mut self, u: &UserCommand, function: bool) {
        self.push_snapshot();
        if function {
            self.push_variable_boundary(None);
            self.loop_blocks.push(0);
            self.listfiles.push(Arc::clone(&u.body.file));
            self.function_locks.push(Default::default());
        }
        self.policies.push_weak(u.policies);
        self.diags.push_weak(u.diags);
    }

    fn pop_call_scope(&mut self, function: bool, report: bool) {
        let base = self.snapshot_bases();
        self.diags.pop_above(base.diag_base);
        self.policies.pop_above(base.policy_base);
        self.pop_snapshot(report);
        if function {
            self.vars.pop();
            self.loop_blocks.pop();
            self.function_locks.pop();
            self.listfiles.pop();
        }
    }

    /// Check the copies of the arguments that a call makes before it makes them.
    ///
    /// The joined `ARGV` and `ARGN` values must fit the value limit. For a function, the
    /// projected bindings (`ARGV<n>`, parameters, `ARGV`, `ARGN`, and the fixed variables)
    /// must fit the live variable budget. Report a fatal error and return `false` if not.
    fn admit_call_arguments(
        &mut self,
        u: &UserCommand,
        args: &[ExpandedArg],
        function: bool,
    ) -> bool {
        // Longest generated name (`ARGV` and a 20-digit index) plus the binding overhead.
        const PER_BINDING: usize = 24 + 64;
        let value_bytes = args
            .iter()
            .try_fold(0_usize, |sum, arg| sum.checked_add(arg.value.len()));
        let joined = value_bytes.and_then(|bytes| bytes.checked_add(args.len()));
        // CMP0219 doubles each backslash in the joined macro values.
        let escaped = if function {
            joined
        } else {
            args.iter()
                .map(|arg| arg.value.iter().filter(|&&byte| byte == b'\\').count())
                .try_fold(0_usize, usize::checked_add)
                .zip(joined)
                .and_then(|(backslashes, joined)| joined.checked_add(backslashes))
        };
        if escaped.is_none_or(|joined| joined > self.config.limits.max_value_bytes) {
            self.error_here("Variable value exceeded maximum allowed byte length.".into());
            self.fatal = true;
            return false;
        }
        if !function {
            return true;
        }
        let names = u
            .params
            .iter()
            .try_fold(0_usize, |sum, param| sum.checked_add(param.len()));
        let fixed = u
            .body
            .file
            .len()
            .checked_mul(2)
            .and_then(|bytes| bytes.checked_add(u.name.len()))
            .and_then(|bytes| bytes.checked_add(16 * PER_BINDING));
        let projected = match (value_bytes, joined, names, fixed) {
            (Some(values), Some(joined), Some(names), Some(fixed)) => {
                // ARGV<n> and the parameters copy each value. ARGV and ARGN copy the list.
                let copies = values.checked_mul(2).zip(joined.checked_mul(2));
                let bindings = args.len().checked_mul(2 * PER_BINDING);
                copies
                    .and_then(|(values, joined)| values.checked_add(joined))
                    .zip(bindings)
                    .and_then(|(copies, bindings)| copies.checked_add(bindings))
                    .and_then(|bytes| bytes.checked_add(names))
                    .and_then(|bytes| bytes.checked_add(fixed))
                    .and_then(|bytes| bytes.checked_add(self.vars.live_bytes()))
                    .and_then(|bytes| bytes.checked_add(self.temp_bytes))
            }
            _ => None,
        };
        if projected.is_none_or(|bytes| bytes > self.config.limits.max_live_variable_bytes) {
            self.report_variable_budget();
            return false;
        }
        true
    }

    /// Define the argument variables of a function call. Stop at the first fatal error.
    fn bind_function_arguments(&mut self, u: &UserCommand, values: &[BString]) -> bool {
        self.define(b"ARGC", values.len().to_string().into());
        for (i, v) in values.iter().enumerate() {
            if self.fatal {
                return false;
            }
            self.define(format!("ARGV{i}").as_bytes(), v.clone());
        }
        for (param, v) in u.params.iter().zip(values.iter()) {
            if self.fatal {
                return false;
            }
            self.define(param, v.clone());
        }
        if self.fatal {
            return false;
        }
        let argn = values.get(u.params.len()..).unwrap_or_default();
        self.define(b"ARGV", rcmake_base::list::join(values.iter()));
        self.define(b"ARGN", rcmake_base::list::join(argn.iter()));
        self.define(b"ARGNC", argn.len().to_string().into());
        self.define(b"CMAKE_CURRENT_FUNCTION", u.name.clone());
        let file = BString::from(&u.body.file[..]);
        self.define(b"CMAKE_CURRENT_FUNCTION_LIST_DIR", filename_path(&file));
        self.define(b"CMAKE_CURRENT_FUNCTION_LIST_FILE", file);
        self.define(
            b"CMAKE_CURRENT_FUNCTION_LIST_LINE",
            u.line.to_string().into(),
        );
        !self.fatal
    }

    pub(super) fn call_function(
        &mut self,
        u: &Arc<UserCommand>,
        args: &[ExpandedArg],
    ) -> CmdResult {
        if args.len() < u.params.len() {
            return Err(CmdError::Status(cat(&[
                b"Function invoked with incorrect arguments for function named: ",
                &u.name,
            ])));
        }
        if !self.admit_call_arguments(u, args, true) {
            return Err(CmdError::Reported);
        }
        let values: Vec<BString> = args.iter().map(|a| a.value.clone()).collect();
        self.push_call_scope(u, true);

        let result = if self.bind_function_arguments(u, &values) {
            self.run_code(&u.body, 0, u.body.len(), BodyMode::Function)
        } else {
            Err(CmdError::Reported)
        };
        if let Ok(Flow::Return {
            propagate: Some(names),
        }) = &result
        {
            self.raise_vars(names);
        }
        let report = result.is_ok() && !self.fatal;
        self.pop_call_scope(true, report);
        result.map(|_| Flow::Normal)
    }

    /// `cmMakefile::CheckCMP0219`.
    pub(crate) fn check_cmp0219(&mut self, callee: &BString, values: &[BString]) -> PolicyStatus {
        let status = self.policy_status(PolicyId::new(219));
        let has_backslash = values.iter().any(|v| v.contains(&b'\\'));
        if status == PolicyStatus::Warn
            && has_backslash
            && self.warned_cmp0219.insert(callee.clone())
        {
            return PolicyStatus::Warn;
        }
        if status == PolicyStatus::New {
            PolicyStatus::New
        } else {
            PolicyStatus::Old
        }
    }

    pub(crate) fn issue_cmp0219_warning(&mut self, callee: &[u8], values: &[BString]) {
        let mut old_args = Vec::new();
        for v in values.iter().filter(|v| v.contains(&b'\\')) {
            if !old_args.is_empty() {
                old_args.push(b'\n');
            }
            old_args.extend_from_slice(b" \"");
            old_args.extend_from_slice(v);
            old_args.push(b'"');
        }
        let new_args = crate::util::replace_all(&old_args, b"\\", b"\\\\");
        let postface = cat(&[
            b"Command \"",
            callee,
            b"\" called with arguments containing backslashes.\nSince the policy is not set, backslashes in the arguments:\n",
            &old_args,
            b"\nwill be interpreted as escape sequences for compatibility.\nSet the policy to NEW to instead pass\n",
            &new_args,
            b"\nso that argument parsing will preserve the original values.",
        ]);
        self.issue_policy_warning(PolicyId::new(219), b"", &postface);
    }

    pub(super) fn call_macro(&mut self, u: &Arc<UserCommand>, args: &[ExpandedArg]) -> CmdResult {
        if args.len() < u.params.len() {
            return Err(CmdError::Status(cat(&[
                b"Macro invoked with incorrect arguments for macro named: ",
                &u.name,
            ])));
        }
        if !self.admit_call_arguments(u, args, false) {
            return Err(CmdError::Reported);
        }
        // Reserve the argument copies before they are made: the values, `ARGN`, and `ARGV`,
        // with room for CMP0219 to double each backslash. Nested calls share the reservation.
        let Some(copies) = macro_copy_bytes(args) else {
            self.report_variable_budget();
            return Err(CmdError::Reported);
        };
        if !self.reserve_temp(copies) {
            return Err(CmdError::Reported);
        }
        let result = self.run_macro(u, args);
        self.release_temp(copies);
        result
    }

    /// Copy the arguments, substitute them into the body, and run it. The caller holds the
    /// reservation for the argument copies.
    fn run_macro(&mut self, u: &UserCommand, args: &[ExpandedArg]) -> CmdResult {
        let mut values: Vec<BString> = args.iter().map(|a| a.value.clone()).collect();
        let argc = values.len().to_string();
        let mut argn =
            rcmake_base::list::join(values.get(u.params.len()..).unwrap_or_default().iter());
        let mut argv = rcmake_base::list::join(values.iter());
        match self.check_cmp0219(&u.name, &values) {
            PolicyStatus::New => {
                for v in &mut values {
                    *v = crate::util::escape_backslashes(v);
                }
                argn = crate::util::escape_backslashes(&argn);
                argv = crate::util::escape_backslashes(&argv);
            }
            PolicyStatus::Warn => self.issue_cmp0219_warning(&u.name, &values),
            PolicyStatus::Old => {}
        }

        // Reserve the replacement patterns (`${<param>}` and `${ARGV<n>}`) before they
        // are built.
        let patterns = macro_pattern_bytes(&u.params, values.len());
        let Some(mut reserved) = patterns.filter(|&bytes| self.reserve_temp(bytes)) else {
            if patterns.is_none() {
                self.report_variable_budget();
            }
            return Err(CmdError::Reported);
        };
        let formals: Vec<BString> = u.params.iter().map(|p| cat(&[b"${", p, b"}"])).collect();
        let argvs: Vec<BString> = (0..values.len())
            .map(|i| BString::from(format!("${{ARGV{i}}}")))
            .collect();
        // Each substituted argument must fit the value limit and the free part of the live
        // variable budget. The size is checked before each copy is made.
        let max_value = self.config.limits.max_value_bytes;
        let substitute = |text: &[u8], room: usize| -> Option<BString> {
            use crate::util::replace_all_bounded as replace;
            let max = max_value.min(room);
            if text.len() > max {
                return None;
            }
            let mut s = text.to_vec();
            for (formal, value) in formals.iter().zip(values.iter()) {
                s = replace(&s, formal, value, max)?;
            }
            s = replace(&s, b"${ARGC}", argc.as_bytes(), max)?;
            s = replace(&s, b"${ARGN}", &argn, max)?;
            s = replace(&s, b"${ARGV}", &argv, max)?;
            if contains_seq(&s, b"${ARGV") {
                for (pat, value) in argvs.iter().zip(values.iter()) {
                    s = replace(&s, pat, value, max)?;
                }
            }
            Some(BString::from(s))
        };
        // Reserve the slots of the copied command array before it is allocated.
        let slots = u
            .body
            .cmds
            .len()
            .checked_mul(size_of::<CommandInvocation>());
        if slots.is_none_or(|slots| !self.reserve_temp(slots)) {
            self.release_temp(reserved);
            if slots.is_none() {
                self.report_variable_budget();
            }
            return Err(CmdError::Reported);
        }
        reserved += slots.unwrap_or_default();
        let mut cmds: Vec<CommandInvocation> = Vec::with_capacity(u.body.cmds.len());
        let mut complete = true;
        'commands: for command in u.body.cmds.iter() {
            // Reserve the parts of the copy that do not change before the copy is built:
            // both spellings of the name, the bracket arguments, and the argument slots.
            let fixed = command
                .args
                .iter()
                .filter(|arg| arg.kind == ArgKind::Bracket)
                .map(|arg| arg.text.len())
                .chain([
                    command.name.len(),
                    command.lower.len(),
                    command.args.len().saturating_mul(size_of::<Argument>()),
                ])
                .fold(0_usize, usize::saturating_add);
            if !self.reserve_temp(fixed) {
                complete = false;
                break;
            }
            reserved += fixed;
            let mut args = Vec::with_capacity(command.args.len());
            for arg in &command.args {
                let text = if arg.kind == ArgKind::Bracket {
                    arg.text.clone()
                } else {
                    let text = substitute(&arg.text, self.temp_room());
                    let Some(text) = text.filter(|text| self.reserve_temp(text.len())) else {
                        complete = false;
                        break 'commands;
                    };
                    reserved += text.len();
                    text
                };
                args.push(Argument {
                    kind: arg.kind,
                    text,
                    line: arg.line,
                    span: arg.span.clone(),
                });
            }
            cmds.push(CommandInvocation {
                name: command.name.clone(),
                lower: command.lower.clone(),
                line: command.line,
                close_line: command.close_line,
                span: command.span.clone(),
                args,
            });
        }
        if !complete {
            self.release_temp(reserved);
            if !self.fatal {
                self.error_here(
                    "Macro arguments expanded to more than the memory limit allows.".into(),
                );
                self.fatal = true;
            }
            return Err(CmdError::Reported);
        }
        let body = u.body.with_cmds(cmds);

        self.push_call_scope(u, false);
        let result = self.run_code(&body, 0, body.len(), BodyMode::Macro);
        let report = result.is_ok() && !self.fatal;
        self.pop_call_scope(false, report);
        drop(body);
        self.release_temp(reserved);
        result
    }
}

/// Bytes of the macro replacement patterns: `${<param>}` for each parameter and
/// `${ARGV<n>}` for each argument.
fn macro_pattern_bytes(params: &[BString], args: usize) -> Option<usize> {
    // `${ARGV}` and a 20-digit index.
    const ARGV_PATTERN: usize = 7 + 20;
    let formals = params.iter().try_fold(0_usize, |sum, param| {
        sum.checked_add(param.len())?.checked_add(3)
    })?;
    formals.checked_add(args.checked_mul(ARGV_PATTERN)?)
}

/// Upper bound of the bytes that a macro call copies from its arguments: the values,
/// `ARGN`, and `ARGV`, with each backslash doubled by CMP0219.
fn macro_copy_bytes(args: &[ExpandedArg]) -> Option<usize> {
    let values = args
        .iter()
        .try_fold(0_usize, |sum, arg| sum.checked_add(arg.value.len()))?;
    let backslashes = args
        .iter()
        .map(|arg| arg.value.iter().filter(|&&byte| byte == b'\\').count())
        .try_fold(0_usize, usize::checked_add)?;
    let joined = values.checked_add(args.len())?.checked_add(backslashes)?;
    values
        .checked_add(backslashes)?
        .checked_add(joined.checked_mul(2)?)
}

fn contains_seq(hay: &[u8], needle: &[u8]) -> bool {
    !needle.is_empty() && hay.windows(needle.len()).any(|w| w == needle)
}
