use std::sync::Arc;

use rcmake_base::BString;
use rcmake_diag::{PolicyId, PolicyStatus};
use rcmake_expr::ExpandedArg;
use rcmake_syntax::{ArgKind, CommandInvocation};

use super::BodyMode;
use crate::util::{cat, filename_path};
use crate::{CmdError, CmdResult, Flow, Interp, UserCommand};

impl Interp {
    fn push_call_scope(&mut self, u: &UserCommand, function: bool) {
        self.push_snapshot();
        if function {
            self.vars.push();
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

    pub(super) fn call_function(
        &mut self,
        u: &Arc<UserCommand>,
        args: &[ExpandedArg],
    ) -> CmdResult {
        let values: Vec<BString> = args.iter().map(|a| a.value.clone()).collect();
        if values.len() < u.params.len() {
            return Err(CmdError::Status(cat(&[
                b"Function invoked with incorrect arguments for function named: ",
                &u.name,
            ])));
        }
        self.push_call_scope(u, true);

        self.define(b"ARGC", values.len().to_string().into());
        for (i, v) in values.iter().enumerate() {
            self.define(format!("ARGV{i}").as_bytes(), v.clone());
        }
        for (param, v) in u.params.iter().zip(values.iter()) {
            self.define(param, v.clone());
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

        let result = self.run_code(&u.body, 0, u.body.len(), BodyMode::Function);
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
        let mut values: Vec<BString> = args.iter().map(|a| a.value.clone()).collect();
        if values.len() < u.params.len() {
            return Err(CmdError::Status(cat(&[
                b"Macro invoked with incorrect arguments for macro named: ",
                &u.name,
            ])));
        }
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

        let formals: Vec<BString> = u.params.iter().map(|p| cat(&[b"${", p, b"}"])).collect();
        let argvs: Vec<BString> = (0..values.len())
            .map(|i| BString::from(format!("${{ARGV{i}}}")))
            .collect();
        let substitute = |text: &[u8]| -> BString {
            let mut s = text.to_vec();
            for (formal, value) in formals.iter().zip(values.iter()) {
                s = crate::util::replace_all(&s, formal, value);
            }
            s = crate::util::replace_all(&s, b"${ARGC}", argc.as_bytes());
            s = crate::util::replace_all(&s, b"${ARGN}", &argn);
            s = crate::util::replace_all(&s, b"${ARGV}", &argv);
            if contains_seq(&s, b"${ARGV") {
                for (pat, value) in argvs.iter().zip(values.iter()) {
                    s = crate::util::replace_all(&s, pat, value);
                }
            }
            BString::from(s)
        };
        let cmds: Vec<CommandInvocation> = u
            .body
            .cmds
            .iter()
            .map(|c| {
                let mut c = c.clone();
                for arg in &mut c.args {
                    if arg.kind != ArgKind::Bracket {
                        arg.text = substitute(&arg.text);
                    }
                }
                c
            })
            .collect();
        let body = u.body.with_cmds(cmds);

        self.push_call_scope(u, false);
        let result = self.run_code(&body, 0, body.len(), BodyMode::Macro);
        let report = result.is_ok() && !self.fatal;
        self.pop_call_scope(false, report);
        result
    }
}

fn contains_seq(hay: &[u8], needle: &[u8]) -> bool {
    !needle.is_empty() && hay.windows(needle.len()).any(|w| w == needle)
}
