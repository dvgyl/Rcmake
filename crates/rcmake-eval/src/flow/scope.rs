use std::sync::Arc;

use rcmake_base::BString;
use rcmake_diag::{PolicyId, PolicyStatus};
use rcmake_expr::ExpandedArg;

use super::BlockResult;
use crate::args::{KeywordParser, Slot};
use crate::util::cat;
use crate::{CmdError, Flow, Interp, UserCommand, UserCommandKind, code::Code};

/// Scopes created by `block()`.
struct BlockPlan {
    variables: bool,
    policies: bool,
    diagnostics: bool,
    propagate: Vec<BString>,
}

impl Interp {
    pub(crate) fn exec_define(
        &mut self,
        code: &Code,
        i: usize,
        kind: UserCommandKind,
    ) -> BlockResult {
        let cmds = Arc::clone(&code.cmds);
        let Some(cmd) = cmds.get(i) else {
            return Ok(Ok(Flow::Normal));
        };
        let end = code.end_of(i);
        let mut values: Vec<BString> = Vec::new();
        self.run_opener(code, i, cmd, |this| {
            let args = this.expand(&cmd.args)?;
            if args.is_empty() {
                return Err(CmdError::Status(
                    "called with incorrect number of arguments".into(),
                ));
            }
            values = args.into_iter().map(|a| a.value).collect();
            Ok(Flow::Normal)
        })?;
        let opener: Vec<ExpandedArg> = values
            .first()
            .map(|v| ExpandedArg {
                value: v.clone(),
                kind: rcmake_syntax::ArgKind::Unquoted,
                line: cmd.line,
            })
            .into_iter()
            .collect();
        self.check_block_end(code, i, &opener);

        let mut values = values.into_iter();
        let Some(name) = values.next() else {
            return Ok(Ok(Flow::Normal));
        };
        if kind == UserCommandKind::Macro
            && self.policy_status(PolicyId::new(217)) != PolicyStatus::New
        {
            let dir = self.current_dir_id();
            self.project
                .dir_mut(dir)
                .props
                .append(BString::from("MACROS"), &name);
        }
        let params: Vec<BString> = values.collect();
        let formal_patterns = if kind == UserCommandKind::Macro {
            params.iter().map(|p| cat(&[b"${", p, b"}"])).collect()
        } else {
            Vec::new()
        };
        let body = code.slice(i + 1, end);
        let file_dir = crate::util::filename_path(&body.file);
        let user = UserCommand {
            kind,
            name,
            params,
            formal_patterns,
            body,
            file_dir,
            line: cmd.line,
            line_str: cmd.line.to_string().into(),
            policies: *self.policies.top(),
            diags: *self.diags.top(),
        };
        let added = self.with_plain_frame_arc(&code.file, cmd.line, code.name_arc(i), |this| {
            this.add_scripted_command(Arc::new(user))
        });
        self.trace_unframed(code, end);
        if added {
            Ok(Ok(Flow::Normal))
        } else {
            Ok(Err(CmdError::Reported))
        }
    }

    fn block_plan(&mut self, args: &[ExpandedArg]) -> Result<Option<BlockPlan>, CmdError> {
        let parser = KeywordParser::new()
            .keyword("SCOPE_FOR", Slot::MultiNonEmpty)
            .keyword("PROPAGATE", Slot::Multi);
        let parsed = parser.parse(args);
        if let Some(first) = parsed.unparsed().first() {
            self.fatal = true;
            return Err(CmdError::Status(cat(&[
                b"called with unsupported argument \"",
                first,
                b"\"",
            ])));
        }
        if let Some(text) = parsed.report_error_text() {
            self.fatal_here(text);
            return Ok(None);
        }
        let mut plan = BlockPlan {
            variables: false,
            policies: false,
            diagnostics: false,
            propagate: parsed.multi("PROPAGATE").to_vec(),
        };
        if parsed.seen("SCOPE_FOR") {
            for scope in parsed.multi("SCOPE_FOR") {
                match scope.as_slice() {
                    b"VARIABLES" => plan.variables = true,
                    b"POLICIES" => plan.policies = true,
                    b"DIAGNOSTICS" => plan.diagnostics = true,
                    _ => {
                        self.fatal = true;
                        return Err(CmdError::Status(cat(&[
                            b"SCOPE_FOR unsupported scope \"",
                            scope,
                            b"\"",
                        ])));
                    }
                }
            }
        } else {
            plan.variables = true;
            plan.policies = true;
            plan.diagnostics = true;
        }
        if !plan.variables && !plan.propagate.is_empty() {
            self.fatal = true;
            return Err(CmdError::Status(
                "PROPAGATE cannot be specified without a new scope for VARIABLES".into(),
            ));
        }
        Ok(Some(plan))
    }

    pub(crate) fn exec_block(&mut self, code: &Code, i: usize) -> BlockResult {
        let cmds = Arc::clone(&code.cmds);
        let Some(cmd) = cmds.get(i) else {
            return Ok(Ok(Flow::Normal));
        };
        let end = code.end_of(i);
        let mut plan = None;
        self.run_opener(code, i, cmd, |this| {
            let args = this.expand(&cmd.args)?;
            plan = this.block_plan(&args)?;
            Ok(Flow::Normal)
        })?;
        let Some(plan) = plan else {
            return Err(CmdError::Reported);
        };
        self.check_block_end(code, i, &[]);

        if plan.variables {
            self.push_snapshot();
            self.push_variable_boundary(None);
        }
        if plan.policies {
            self.policies.push_copy();
        }
        if plan.diagnostics {
            self.diags.push_copy();
        }

        let result = self.run_body(code, i + 1, end);
        if let Ok(Flow::Return {
            propagate: Some(names),
        }) = &result
        {
            self.raise_vars(names);
        }
        if plan.variables {
            self.raise_vars(&plan.propagate);
        }
        if plan.diagnostics {
            let _ = self.pop_diag();
        }
        if plan.policies {
            let _ = self.pop_policy();
        }
        if plan.variables {
            let report = !self.fatal;
            self.pop_snapshot(report);
            self.vars.pop();
        }
        self.trace_unframed(code, end);
        Ok(result)
    }
}
