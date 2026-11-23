use rcmake_base::{BString, truth};
use rcmake_eval::{CmdError, Cx, UserCommandKind};
use rcmake_expr::ExpandedArg;
use rcmake_syntax::ArgKind;

use super::{cat, status};

pub(super) struct Callback<'a> {
    command: &'a [u8],
    context: &'static [u8],
    output: BString,
}

impl<'a> Callback<'a> {
    pub(super) fn new(
        cx: &Cx<'_>,
        command: &'a [u8],
        context: &'static [u8],
        purpose: &str,
    ) -> Result<Self, CmdError> {
        if !cx.command_exists(command) {
            return Err(status(cat(&[
                context,
                b"unknown function \"",
                command,
                b"\".",
            ])));
        }
        if cx.user_command_kind(command) == Some(UserCommandKind::Macro) {
            return Err(status(cat(&[
                context,
                b"macro \"",
                command,
                b"\" may not be used here; define it as a function() instead.",
            ])));
        }
        let mut number = 0_u64;
        let output = loop {
            let candidate = BString::from(format!("_cmake_{purpose}_out_{number}_"));
            if !cx.var_defined(&candidate) {
                break candidate;
            }
            number += 1;
        };
        Ok(Self {
            command,
            context,
            output,
        })
    }

    pub(super) fn call<const N: usize>(
        &self,
        cx: &mut Cx<'_>,
        input: [&[u8]; N],
    ) -> Result<BString, CmdError> {
        let line = cx.current_line();
        let args = input
            .into_iter()
            .chain([self.output.as_slice()])
            .map(|value| ExpandedArg {
                value: value.into(),
                kind: ArgKind::Bracket,
                line,
            })
            .collect();
        let call = cx.call_command(self.command, args);
        let result = cx.var(&self.output);
        cx.unset_var(&self.output);
        call?;
        result.ok_or_else(|| {
            status(cat(&[
                self.context,
                b"function \"",
                self.command,
                b"\" did not set the output variable.",
            ]))
        })
    }

    pub(super) fn test<const N: usize>(
        &self,
        cx: &mut Cx<'_>,
        input: [&[u8]; N],
    ) -> Result<bool, CmdError> {
        self.call(cx, input).map(|value| truth::is_on(&value))
    }
}
