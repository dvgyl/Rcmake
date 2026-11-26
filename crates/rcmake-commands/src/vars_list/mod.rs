//! Variable, cache, command-line parsing, and list commands.

#![forbid(unsafe_code)]

use rcmake_eval::{CmdFlags, Registry};

use crate::util::{bytes, cat};

mod callbacks;
mod lists;
pub(crate) mod separate;
mod sort;
mod transform;
mod variables;

#[cfg(test)]
mod tests;

pub(crate) fn register(registry: &mut Registry) {
    let script = CmdFlags { scriptable: true };
    registry.add("set", variables::set, script);
    registry.add("unset", variables::unset, script);
    registry.add("option", variables::option, script);
    registry.add("mark_as_advanced", variables::mark_as_advanced, script);
    registry.add("separate_arguments", separate::separate_arguments, script);
    registry.add("list", lists::list, script);
}
