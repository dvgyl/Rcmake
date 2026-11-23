//! The `file()` and `cmake_path()` commands.

#![forbid(unsafe_code)]

use rcmake_base::BString;
use rcmake_diag::{DiagCategory, Effect, MessageKind};
use rcmake_eval::{CmdError, CmdFlags, Cx, Flow, Registry};

mod archive;
mod copy;
mod file;
mod glob;
mod lexical;
mod mutation;
mod path_cmd;
mod path_hash;
mod reading;
mod strings;

#[cfg(test)]
mod tests;

pub(crate) fn register(registry: &mut Registry) {
    let flags = CmdFlags { scriptable: true };
    registry.add("file", file::run, flags);
    registry.add("cmake_path", path_cmd::run, flags);
}

type Result<T = Flow> = core::result::Result<T, CmdError>;

fn error(message: impl Into<BString>) -> CmdError {
    CmdError::Status(message.into())
}

fn warning(cx: &mut Cx<'_>, message: impl Into<BString>) {
    cx.issue(
        MessageKind::AuthorWarning,
        Effect::None,
        Some(DiagCategory::Author),
        message.into(),
    );
}

fn arg<'a>(args: &mut impl Iterator<Item = &'a [u8]>, option: &[u8]) -> Result<&'a [u8]> {
    args.next().ok_or_else(|| {
        error(format!(
            "{} requires a value",
            String::from_utf8_lossy(option)
        ))
    })
}

fn unknown(subcommand: &[u8]) -> CmdError {
    error([b"does not recognize sub-command ", subcommand].concat())
}

fn set(cx: &mut Cx<'_>, name: &[u8], value: impl Into<BString>) -> Result {
    let value = value.into();
    if value.len() > cx.limits().max_value_bytes {
        return Err(error("output exceeds the variable size limit"));
    }
    cx.set_var(name, value);
    Ok(Flow::Normal)
}
