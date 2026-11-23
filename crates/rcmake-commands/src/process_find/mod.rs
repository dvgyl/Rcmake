//! Process execution and filesystem search commands.

#![forbid(unsafe_code)]

use rcmake_base::BString;
use rcmake_diag::{Effect, MessageKind};
use rcmake_eval::{CmdError, CmdFlags, Cx, Registry};

mod environment;
mod execute;
mod find;
#[cfg(test)]
mod tests;

pub(crate) fn register(registry: &mut Registry) {
    let flags = CmdFlags { scriptable: true };
    registry.add("execute_process", execute::execute_process, flags);
    registry.add("find_file", find::find_file, flags);
    registry.add("find_path", find::find_path, flags);
    registry.add("find_program", find::find_program, flags);
    registry.add("find_library", find::find_library, flags);
}

fn diagnostic(cx: &mut Cx<'_>, text: BString) -> CmdError {
    cx.issue(MessageKind::Error, Effect::Fatal, None, text);
    CmdError::Reported
}

fn text(parts: &[&[u8]]) -> BString {
    BString::from(parts.concat())
}
